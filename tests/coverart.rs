//! Router tests of `getCoverArt` (`docs/design.md` sections 3.11 and 4).
//!
//! The cover file and the embedded picture of the standard library are 2 × 2
//! pixels. No cover is enlarged, so a test of scaling builds a library with a
//! larger cover file. A size is served at its step: the smallest of 80, 160,
//! 320, 640, 1280 and 2000 pixels that is not smaller than the size.
//!
//! The tests of the cover service are in `tests/covers.rs` and in
//! `src/library/covers.rs`. This file covers what the method adds: the
//! parameters, the headers, the conditional request and the errors. The unit
//! tests in `src/api/covers.rs` check the files of the cover cache, which the
//! test server does not show.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::Response;
use image::{ImageFormat, Rgb, RgbImage};
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::db::queries::{albums, tracks};

use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, PICTURE_PNG, ScannedServer,
    TestLibrary, TestServer, body_bytes, body_string, content_type, form_request, get_request,
    subsonic_response, urlencode,
};

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of the error for a `size` that is no integer in the range.
const INVALID_SIZE: &str = "parameter size must be an integer from 0 to 4294967295";

/// The value of `Cache-Control` of a response with a cover.
const CACHE_CONTROL: &str = "public, max-age=86400";

/// The `subsonic-response` object of the error `code` with `message`.
fn failed(code: u32, message: &str) -> Value {
    json!({
        "status": "failed",
        "version": "1.16.1",
        "type": "wn-server",
        "serverVersion": VERSION,
        "openSubsonic": true,
        "error": { "code": code, "message": message },
    })
}

/// The XML body of the error `code` with `message`.
fn xml_failed(code: u32, message: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true">"#,
            r#"<error code="{}" message="{}"/></subsonic-response>"#
        ),
        VERSION, code, message
    )
}

/// `params` followed by the password of the owner, `v` and `c`. A request
/// that a test builds itself carries them.
fn as_owner<'a>(params: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut params = params.to_vec();
    params.extend([
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
    ]);
    params
}

/// What a response holds: the status, the headers and the body.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    async fn of(response: Response) -> Self {
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            body: body_bytes(response).await,
        }
    }

    /// The value of the header `name`.
    fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("an ASCII header value"))
    }

    /// The value of `ETag`.
    fn etag(&self) -> &str {
        self.header(header::ETAG).expect("the response has an ETag")
    }

    /// Checks that the response carries a cover: HTTP 200, the content type
    /// `content_type`, the length of the body, the cache headers with the
    /// entity tag `etag`, and the CORS headers of the route.
    fn assert_cover(&self, content_type: &str, etag: &str, case: &str) {
        assert_eq!(self.status, StatusCode::OK, "{case}");
        assert_eq!(
            self.header(header::CONTENT_TYPE),
            Some(content_type),
            "{case}"
        );
        assert_eq!(
            self.header(header::CONTENT_LENGTH),
            Some(self.body.len().to_string().as_str()),
            "{case}"
        );
        assert!(!self.body.is_empty(), "{case}");
        assert_eq!(
            self.header(header::CACHE_CONTROL),
            Some(CACHE_CONTROL),
            "{case}"
        );
        assert_eq!(self.header(header::ETAG), Some(etag), "{case}");
        self.assert_cors(case);
    }

    /// Checks that the response is HTTP 304 for the entity tag `etag`: no
    /// body, no content type, and the cache headers.
    fn assert_not_modified(&self, etag: &str, case: &str) {
        assert_eq!(self.status, StatusCode::NOT_MODIFIED, "{case}");
        assert!(self.body.is_empty(), "{case}");
        assert_eq!(self.header(header::CONTENT_TYPE), None, "{case}");
        assert_eq!(
            self.header(header::CACHE_CONTROL),
            Some(CACHE_CONTROL),
            "{case}"
        );
        assert_eq!(self.header(header::ETAG), Some(etag), "{case}");
        self.assert_cors(case);
    }

    /// Checks that the response is the JSON envelope of the error `code`
    /// with HTTP 200, and that it has no header of a cover.
    fn assert_error(&self, code: u32, message: &str, case: &str) {
        assert_eq!(self.status, StatusCode::OK, "{case}");
        assert_eq!(
            self.header(header::CONTENT_TYPE),
            Some("application/json"),
            "{case}"
        );
        let body = String::from_utf8(self.body.clone()).expect("a UTF-8 body");
        assert_eq!(subsonic_response(&body), failed(code, message), "{case}");
        assert_eq!(self.header(header::ETAG), None, "{case}");
        assert_eq!(self.header(header::CACHE_CONTROL), None, "{case}");
        self.assert_cors(case);
    }

    /// Checks the CORS headers that every response of the route has.
    fn assert_cors(&self, case: &str) {
        assert_eq!(
            self.header(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some("*"),
            "{case}"
        );
        assert_eq!(
            self.header(header::ACCESS_CONTROL_EXPOSE_HEADERS),
            Some("Content-Range, Accept-Ranges, Content-Length"),
            "{case}"
        );
    }
}

/// Sends `GET /rest/getCoverArt` with `params` and the auth defaults.
async fn cover(server: &TestServer, params: &[(&str, &str)]) -> Answer {
    Answer::of(server.get_raw("getCoverArt", params).await).await
}

/// Sends `GET /rest/getCoverArt` with `params`, the auth defaults and the
/// request header `If-None-Match: <value>`.
async fn conditional(server: &TestServer, params: &[(&str, &str)], value: &str) -> Answer {
    let headers = [("If-None-Match", value)];
    Answer::of(
        server
            .get_with_headers("getCoverArt", params, &headers)
            .await,
    )
    .await
}

/// The entity tag of the cover `id` at `size` whose source has the
/// modification time `mtime`, with its quotes. `size` is 0 for the image as
/// it is stored, and otherwise the step that a size is served at.
fn etag(id: &str, size: u32, mtime: i64) -> String {
    format!("\"{id}-{size}-{mtime}\"")
}

/// Returns the path of the file `name` in the directory of the album `album`
/// of `artist` in `library`.
fn file(library: &TestLibrary, artist: &str, album: &str, name: &str) -> PathBuf {
    library.root.path().join(artist).join(album).join(name)
}

/// Writes an image of `width` × `height` pixels to `path`. The extension of
/// the path selects the encoding. `shade` is the blue value of every pixel,
/// so two images of one size differ.
fn write_image(path: &Path, width: u32, height: u32, shade: u8) {
    RgbImage::from_fn(width, height, |x, y| Rgb([x as u8, y as u8, shade]))
        .save(path)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// Returns the modification time of the file at `path` in Unix seconds.
fn mtime(path: &Path) -> i64 {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|error| panic!("read the time of {}: {error}", path.display()));
    let seconds = modified
        .duration_since(UNIX_EPOCH)
        .expect("a time after 1970")
        .as_secs();
    i64::try_from(seconds).expect("a time that fits")
}

/// Sets the modification time of the file at `path` to `seconds` after 1970.
fn set_mtime(path: &Path, seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

/// Checks that `bytes` are a JPEG and returns its width and height.
fn jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(
        image::guess_format(bytes).expect("a known image format"),
        ImageFormat::Jpeg
    );
    let image = image::load_from_memory(bytes).expect("decode the JPEG");
    (image.width(), image.height())
}

/// A server over a library of one album, "Glass Harbour" by Aurora Field,
/// with one track and a cover file `name` of `width` × `height` pixels.
/// Returns the server, the ID of the album and the path of the cover file.
async fn server_with_cover(
    name: &str,
    width: u32,
    height: u32,
) -> (ScannedServer, String, PathBuf) {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .build();
    let cover_file = file(&library, "Aurora Field", "Glass Harbour", name);
    write_image(&cover_file, width, height, 128);
    let server = TestServer::scanned(library).await;
    let album = server.catalogue().await.album("Glass Harbour").id.clone();
    (server, album, cover_file)
}

/// Gives the track `id` its placeholder path, as the scanner does for a
/// track that it keeps without a file.
async fn remove_file(server: &TestServer, id: &str) {
    let id = id.to_owned();
    server
        .db()
        .write(move |conn| {
            let track = tracks::get_track(conn, &id)?.expect("the track exists");
            let placeholder = tracks::placeholder_path(&id);
            tracks::relink_track(
                conn,
                &id,
                track.folder_id,
                &placeholder,
                track.mtime,
                track.size,
            )
        })
        .await
        .expect("give the track a placeholder path");
}

/// Polls `request` until it has returned `Pending` `waits` times, and
/// returns `None` then: the request is at that point of waiting, and the
/// caller ends it by dropping it. Returns the output of a request that
/// finished before that.
///
/// Between two polls the function waits until the request is woken, so every
/// poll finds the result of what the request waited for.
async fn advance<F: Future>(mut request: Pin<&mut F>, waits: usize) -> Option<F::Output> {
    let mut left = waits;
    std::future::poll_fn(|cx| match request.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(Some(output)),
        Poll::Pending => {
            left -= 1;
            if left == 0 {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        }
    })
    .await
}

#[tokio::test]
async fn album_with_a_cover_file_returns_the_file_and_a_jpeg_of_at_most_the_step() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");
    let time = mtime(&cover_file);

    for name in ["getCoverArt", "getCoverArt.view"] {
        // Without a size the body is the file, byte for byte.
        let original = Answer::of(server.get_raw(name, &[("id", id)]).await).await;
        original.assert_cover("image/jpeg", &etag(id, 0, time), name);
        assert_eq!(original.body, stored, "{name}");

        // With a size the body is a JPEG whose longer side has the step of
        // that size, which is 80 for 64.
        let params = [("id", id), ("size", "64")];
        let resized = Answer::of(server.get_raw(name, &params).await).await;
        resized.assert_cover("image/jpeg", &etag(id, 80, time), name);
        assert_eq!(jpeg_dimensions(&resized.body), (80, 40), "{name}");
        assert_ne!(resized.body, stored, "{name}");

        // The second request returns the same cover.
        let again = Answer::of(server.get_raw(name, &params).await).await;
        again.assert_cover("image/jpeg", &etag(id, 80, time), name);
        assert_eq!(again.body, resized.body, "{name}");
    }

    // A cover within the step keeps its size and is encoded as a JPEG.
    let larger = cover(&server, &[("id", id), ("size", "500")]).await;
    larger.assert_cover("image/jpeg", &etag(id, 640, time), "size=500");
    assert_eq!(jpeg_dimensions(&larger.body), (128, 64));
}

#[tokio::test]
async fn sizes_of_one_step_get_one_cover_with_one_entity_tag() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 640, 320).await;
    let id = album.as_str();
    let tag = etag(id, 320, mtime(&cover_file));

    // 290, 300 and 310 are sizes of the step 320. The cover is 320 pixels
    // wide, which is more than each of them asks for.
    let first = cover(&server, &[("id", id), ("size", "290")]).await;
    first.assert_cover("image/jpeg", &tag, "290");
    assert_eq!(jpeg_dimensions(&first.body), (320, 160));
    for size in ["300", "310", "161", "320"] {
        let answer = cover(&server, &[("id", id), ("size", size)]).await;
        answer.assert_cover("image/jpeg", &tag, size);
        assert_eq!(answer.body, first.body, "{size}");

        // A client that holds the cover of one size holds the cover of
        // every size of the step.
        let answer = conditional(&server, &[("id", id), ("size", size)], &tag).await;
        answer.assert_not_modified(&tag, size);
    }

    // The sizes next to the step get other covers.
    for (size, step, width) in [("160", 160, 160), ("321", 640, 640), ("1000", 1280, 640)] {
        let answer = cover(&server, &[("id", id), ("size", size)]).await;
        answer.assert_cover("image/jpeg", &etag(id, step, mtime(&cover_file)), size);
        assert_eq!(jpeg_dimensions(&answer.body), (width, width / 2), "{size}");
        let answer = conditional(&server, &[("id", id), ("size", size)], &tag).await;
        assert_eq!(answer.status, StatusCode::OK, "{size}");
    }
}

#[tokio::test]
async fn standard_library_covers_have_the_content_type_of_their_format() {
    let server = TestServer::standard().await;

    // "Glass Harbour" has the file `cover.jpg` of 2 × 2 pixels.
    let cover_file = server
        .root()
        .join("Aurora Field")
        .join("Glass Harbour")
        .join("cover.jpg");
    let id = server.album.glass_harbour.as_str();
    let time = mtime(&cover_file);
    let original = cover(&server, &[("id", id)]).await;
    original.assert_cover("image/jpeg", &etag(id, 0, time), "cover.jpg");
    assert_eq!(
        original.body,
        fs::read(&cover_file).expect("read the cover")
    );
    let resized = cover(&server, &[("id", id), ("size", "64")]).await;
    resized.assert_cover("image/jpeg", &etag(id, 80, time), "cover.jpg at 64");
    assert_eq!(jpeg_dimensions(&resized.body), (2, 2));

    // "Slow Rivers" has no image file. Its cover is the PNG picture of its
    // first track, and the time is the time of that track.
    let slow_rivers = server.root().join("Marlow Quartet").join("Slow Rivers");
    let first_track = slow_rivers.join("01 Upstream.m4a");
    let id = server.album.slow_rivers.as_str();
    let time = mtime(&first_track);
    let original = cover(&server, &[("id", id)]).await;
    original.assert_cover("image/png", &etag(id, 0, time), "embedded");
    assert_eq!(original.body, PICTURE_PNG);
    let resized = cover(&server, &[("id", id), ("size", "64")]).await;
    resized.assert_cover("image/jpeg", &etag(id, 80, time), "embedded at 64");
    assert_eq!(jpeg_dimensions(&resized.body), (2, 2));

    // The ID of a track is the ID of the picture in its own file.
    for (id, name) in [
        (&server.track.upstream, "01 Upstream.m4a"),
        (&server.track.delta, "02 Delta.m4a"),
    ] {
        let time = mtime(&slow_rivers.join(name));
        let original = cover(&server, &[("id", id)]).await;
        original.assert_cover("image/png", &etag(id, 0, time), name);
        assert_eq!(original.body, PICTURE_PNG, "{name}");
        // The size 1 is served at the smallest step, and the picture of
        // 2 × 2 pixels is not made smaller for it.
        let resized = cover(&server, &[("id", id), ("size", "1")]).await;
        resized.assert_cover("image/jpeg", &etag(id, 80, time), name);
        assert_eq!(jpeg_dimensions(&resized.body), (2, 2), "{name}");
    }
}

/// The value of `coverArt` of `element`, or `None` when it has none.
fn cover_art(element: &Value) -> Option<String> {
    element
        .get("coverArt")
        .map(|id| id.as_str().expect("coverArt is a string").to_owned())
}

/// Checks that `getCoverArt` returns an image for `id`, without a size and
/// with one.
async fn assert_has_cover(server: &TestServer, id: &str) {
    let original = cover(server, &[("id", id)]).await;
    assert_eq!(original.status, StatusCode::OK, "{id}");
    let content_type = original.header(header::CONTENT_TYPE).expect("a type");
    assert!(
        ["image/jpeg", "image/png", "image/webp"].contains(&content_type),
        "{id}: {content_type}"
    );
    let format = image::guess_format(&original.body).expect("a known image format");
    assert_eq!(format.to_mime_type(), content_type, "{id}");
    image::load_from_memory(&original.body).expect("decode the cover");

    let resized = cover(server, &[("id", id), ("size", "32")]).await;
    assert_eq!(resized.status, StatusCode::OK, "{id}");
    assert_eq!(resized.header(header::CONTENT_TYPE), Some("image/jpeg"));
    jpeg_dimensions(&resized.body);
}

#[tokio::test]
async fn cover_art_ids_of_the_browse_methods_are_accepted() {
    let server = TestServer::standard().await;
    let albums = [
        &server.album.glass_harbour,
        &server.album.paper_maps,
        &server.album.slow_rivers,
        &server.album.summer_sampler,
    ];

    // The `coverArt` of every album and of every song that `getAlbum`
    // returns, and the IDs of the albums and the songs that have none.
    let mut named = BTreeSet::new();
    let mut without = Vec::new();
    for album_id in albums {
        let response = server.get_json("getAlbum", &[("id", album_id)]).await;
        let album = &response["album"];
        let songs = album["song"].as_array().expect("the songs of the album");
        assert!(!songs.is_empty(), "{album_id}");
        for element in std::iter::once(album).chain(songs) {
            match cover_art(element) {
                Some(id) => {
                    named.insert(id);
                }
                None => {
                    let id = element["id"].as_str().expect("an ID");
                    without.push(id.to_owned());
                }
            }
        }

        // `getSong` names the same cover as the song in the album.
        for song in songs {
            let id = song["id"].as_str().expect("an ID");
            let response = server.get_json("getSong", &[("id", id)]).await;
            assert_eq!(cover_art(&response["song"]), cover_art(song), "{id}");
        }
    }
    // `getArtist` names the same cover of an album as `getAlbum`.
    for artist_id in [
        &server.artist.aurora_field,
        &server.artist.marlow_quartet,
        &server.artist.various_artists,
    ] {
        let response = server.get_json("getArtist", &[("id", artist_id)]).await;
        // An artist has no `coverArt`.
        assert_eq!(cover_art(&response["artist"]), None);
        let listed = response["artist"]["album"].as_array().expect("the albums");
        for album in listed {
            let id = album["id"].as_str().expect("an ID").to_owned();
            assert_eq!(cover_art(album).is_some(), named.contains(&id), "{id}");
            if let Some(cover_id) = cover_art(album) {
                assert_eq!(cover_id, id);
            }
        }
    }

    // The standard library has two albums with a cover, and the cover of
    // each of their songs is the cover of the album.
    let expected: BTreeSet<String> = [&server.album.glass_harbour, &server.album.slow_rivers]
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(named, expected);
    for id in &named {
        assert_has_cover(&server, id).await;
    }

    // An album and a song without `coverArt` have no cover: two albums and
    // their five songs.
    assert_eq!(without.len(), 7, "{without:?}");
    for id in &without {
        let answer = cover(&server, &[("id", id), ("f", "json")]).await;
        answer.assert_error(70, NOT_FOUND, id);
    }
}

/// Adds every value of a `coverArt` field of `value`, at any depth, to
/// `found`.
fn collect_cover_art(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                if name == "coverArt" {
                    let id = field.as_str().expect("coverArt is a string");
                    found.insert(id.to_owned());
                } else {
                    collect_cover_art(field, found);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_cover_art(item, found);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn cover_art_ids_of_the_list_search_star_and_playlist_methods_are_accepted() {
    let server = TestServer::standard().await;
    let glass_harbour = server.album.glass_harbour.as_str();
    let slow_rivers = server.album.slow_rivers.as_str();
    let delta = server.track.delta.as_str();
    let meridian = server.track.meridian.as_str();

    // The user stars albums, songs and an artist with and without a cover,
    // and makes a playlist whose first song has a cover.
    let stars = [
        ("albumId", glass_harbour),
        ("albumId", slow_rivers),
        ("albumId", server.album.paper_maps.as_str()),
        ("id", server.track.night_ferry.as_str()),
        ("id", delta),
        ("id", meridian),
        ("artistId", server.artist.aurora_field.as_str()),
    ];
    let response = server.get_json("star", &stars).await;
    assert_eq!(response["status"], "ok", "{response}");
    let songs = [("name", "Mix"), ("songId", delta), ("songId", meridian)];
    let response = server.get_json("createPlaylist", &songs).await;
    assert_eq!(response["status"], "ok", "{response}");
    let playlist = response["playlist"]["id"]
        .as_str()
        .expect("the ID of the playlist")
        .to_owned();

    let requests: [(&str, &[(&str, &str)]); 13] = [
        ("getAlbumList", &[("type", "alphabeticalByName")]),
        ("getAlbumList2", &[("type", "alphabeticalByName")]),
        ("getAlbumList", &[("type", "starred")]),
        ("getAlbumList2", &[("type", "starred")]),
        ("getRandomSongs", &[("size", "500")]),
        ("getSongsByGenre", &[("genre", "Jazz")]),
        ("getSongsByGenre", &[("genre", "Electronic")]),
        ("search2", &[("query", "")]),
        ("search3", &[("query", "")]),
        ("getStarred", &[]),
        ("getStarred2", &[]),
        ("getPlaylists", &[]),
        ("getPlaylist", &[("id", &playlist)]),
    ];
    let mut named = BTreeSet::new();
    for (method, params) in requests {
        let response = server.get_json(method, params).await;
        assert_eq!(response["status"], "ok", "{method} {params:?}: {response}");
        let mut found = BTreeSet::new();
        collect_cover_art(&response, &mut found);
        // Every answer holds an item with a cover.
        assert!(!found.is_empty(), "{method} {params:?}: {response}");
        named.extend(found);
    }

    // The covers are the ones of the two albums that have one, as the
    // browse methods name them. A playlist names the cover of its first
    // song and has no ID of its own as `coverArt`.
    let expected: BTreeSet<String> = [glass_harbour, slow_rivers]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(named, expected);
    for id in &named {
        assert_has_cover(&server, id).await;
    }
    // The ID of the playlist itself has no cover.
    let answer = cover(&server, &[("id", &playlist), ("f", "json")]).await;
    answer.assert_error(70, NOT_FOUND, "the playlist");
}

#[tokio::test]
async fn cover_art_id_of_a_song_whose_album_has_no_cover_is_the_track_id() {
    let server = TestServer::standard().await;
    let album_id = server.album.slow_rivers.clone();
    // The album loses its stored cover, as after a scan that did not see
    // its directory. Its tracks keep their pictures.
    let id = album_id.clone();
    server
        .db()
        .write(move |conn| albums::set_cover(conn, &id, None, None))
        .await
        .expect("clear the cover of the album");

    let response = server.get_json("getAlbum", &[("id", &album_id)]).await;
    assert_eq!(cover_art(&response["album"]), None);
    let songs = response["album"]["song"].as_array().expect("the songs");
    assert_eq!(songs.len(), 2);
    for song in songs {
        // The song names its own ID, and the method returns its picture.
        let id = song["id"].as_str().expect("an ID");
        assert_eq!(cover_art(song).as_deref(), Some(id));
        assert_has_cover(&server, id).await;
        let original = cover(&server, &[("id", id)]).await;
        assert_eq!(original.header(header::CONTENT_TYPE), Some("image/png"));
        assert_eq!(original.body, PICTURE_PNG);
    }

    // The album itself has no cover now.
    let answer = cover(&server, &[("id", &album_id), ("f", "json")]).await;
    answer.assert_error(70, NOT_FOUND, "the album");
}

#[tokio::test]
async fn if_none_match_with_the_etag_of_the_cover_is_http_304() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let time = mtime(&cover_file);

    for size in [None, Some("64")] {
        let mut params = vec![("id", id)];
        params.extend(size.map(|size| ("size", size)));
        let first = cover(&server, &params).await;
        assert_eq!(first.status, StatusCode::OK);
        let tag = first.etag().to_owned();
        // The size 64 is served at the step 80.
        let served = size.map_or(0, |_| 80);
        assert_eq!(tag, etag(id, served, time));

        // The tag as it was sent, in the weak form, in a list, and `*`.
        let weak = format!("W/{tag}");
        let listed = format!("\"another\", {tag}");
        let both = format!("W/\"another\",{weak}");
        for value in [
            tag.as_str(),
            weak.as_str(),
            listed.as_str(),
            both.as_str(),
            "*",
        ] {
            let answer = conditional(&server, &params, value).await;
            answer.assert_not_modified(&tag, &format!("{size:?} {value}"));
        }

        // A tag that is not the tag of the cover gets the cover.
        let other_time = etag(id, served, time + 1);
        let other_size = etag(id, served + 1, time);
        let unquoted = tag.trim_matches('"');
        let others = [
            "\"another\"",
            other_time.as_str(),
            other_size.as_str(),
            unquoted,
            "",
        ];
        for value in others {
            let answer = conditional(&server, &params, value).await;
            answer.assert_cover("image/jpeg", &tag, &format!("{size:?} {value}"));
            assert_eq!(answer.body, first.body, "{size:?} {value}");
        }
    }

    // The tag of the resized cover is not the tag of the file.
    let answer = conditional(&server, &[("id", id)], &etag(id, 80, time)).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, fs::read(&cover_file).expect("read the cover"));
}

#[tokio::test]
async fn etag_changes_when_a_scan_stores_a_new_cover() {
    let server = TestServer::standard_with_own_files().await;
    let id = server.album.glass_harbour.as_str();
    let cover_file = server
        .root()
        .join("Aurora Field")
        .join("Glass Harbour")
        .join("cover.jpg");

    let before = cover(&server, &[("id", id)]).await;
    let old_tag = before.etag().to_owned();
    let old_sized = cover(&server, &[("id", id), ("size", "8")]).await;

    // The cover file is replaced by another image with a later time.
    let later = u64::try_from(mtime(&cover_file)).expect("a time after 1970") + 10;
    write_image(&cover_file, 16, 8, 200);
    set_mtime(&cover_file, later);
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [""; 0]);
    let later = i64::try_from(later).expect("a time that fits");

    let after = cover(&server, &[("id", id)]).await;
    after.assert_cover("image/jpeg", &etag(id, 0, later), "the new cover");
    assert_ne!(after.etag(), old_tag);
    assert_eq!(after.body, fs::read(&cover_file).expect("read the cover"));
    assert_ne!(after.body, before.body);

    // A client that holds the old cover gets the new one.
    let answer = conditional(&server, &[("id", id)], &old_tag).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, after.body);
    let answer = conditional(&server, &[("id", id)], after.etag()).await;
    answer.assert_not_modified(after.etag(), "the new tag");

    // The same holds for a resized cover.
    let params = [("id", id), ("size", "8")];
    let sized = cover(&server, &params).await;
    sized.assert_cover("image/jpeg", &etag(id, 80, later), "the new cover at 8");
    assert_eq!(jpeg_dimensions(&sized.body), (16, 8));
    let answer = conditional(&server, &params, old_sized.etag()).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.body, sized.body);
}

#[tokio::test]
async fn album_in_disc_directories_returns_the_cover_file_of_their_parent_directory() {
    // Each disc of the album has a directory of its own, and the cover file
    // is in the directory of the album, which holds both.
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side A", 1, "flac")
        .disc(1)
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .build();
    let album_dir = library.root.path().join("Aurora Field").join("Long Play");
    for (name, disc) in [("01 Side A.flac", "CD1"), ("01 Side C.flac", "CD2")] {
        fs::create_dir(album_dir.join(disc)).expect("create the disc directory");
        fs::rename(album_dir.join(name), album_dir.join(disc).join(name)).expect("move the track");
    }
    let cover_file = album_dir.join("cover.jpg");
    write_image(&cover_file, 16, 8, 128);
    let server = TestServer::scanned(library).await;
    let album_id = server.catalogue().await.album("Long Play").id.clone();
    let id = album_id.as_str();

    // The album and its songs name the cover of the album.
    let response = server.get_json("getAlbum", &[("id", id)]).await;
    assert_eq!(cover_art(&response["album"]).as_deref(), Some(id));
    let songs = response["album"]["song"].as_array().expect("the songs");
    assert_eq!(songs.len(), 2);
    for song in songs {
        assert_eq!(cover_art(song).as_deref(), Some(id));
    }

    let time = mtime(&cover_file);
    let original = cover(&server, &[("id", id)]).await;
    original.assert_cover("image/jpeg", &etag(id, 0, time), "the file");
    assert_eq!(
        original.body,
        fs::read(&cover_file).expect("read the cover")
    );
    let params = [("id", id), ("size", "8")];
    let resized = cover(&server, &params).await;
    resized.assert_cover("image/jpeg", &etag(id, 80, time), "the file at 8");
    assert_eq!(jpeg_dimensions(&resized.body), (16, 8));

    // Without the file the album has no cover after the next scan.
    fs::remove_file(&cover_file).expect("remove the cover file");
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [""; 0]);
    let response = server.get_json("getAlbum", &[("id", id)]).await;
    assert_eq!(cover_art(&response["album"]), None);
    let answer = cover(&server, &[("id", id), ("f", "json")]).await;
    answer.assert_error(70, NOT_FOUND, "the album without the file");
}

#[tokio::test]
async fn id_without_a_cover_is_error_70_in_json_and_xml() {
    let server = TestServer::standard().await;
    let album = server.album.glass_harbour.as_str();

    // Values that do not have the shape of an ID.
    let upper = album.to_uppercase();
    let long = format!("{album}0");
    let short = &album[..album.len() - 1];
    let leading_space = format!(" {album}");
    let trailing_space = format!("{album} ");
    let with_path = format!("{album}/../cover.jpg");
    let ids = [
        // An ID of each kind that no row has.
        "al-ffffffffffffffffffffffffffffffff",
        "tr-ffffffffffffffffffffffffffffffff",
        "ar-ffffffffffffffffffffffffffffffff",
        "pl-ffffffffffffffffffffffffffffffff",
        // An artist has no image.
        server.artist.aurora_field.as_str(),
        server.artist.various_artists.as_str(),
        // Albums without a cover file and without a picture.
        server.album.paper_maps.as_str(),
        server.album.summer_sampler.as_str(),
        // Tracks without a picture. The album of the first one has a cover
        // file, which is the cover of the album and not of the track.
        server.track.low_tide.as_str(),
        server.track.meridian.as_str(),
        server.track.open_road.as_str(),
        "",
        "1",
        "0",
        "-1",
        "../x",
        "../Glass Harbour/cover.jpg",
        "cover.jpg",
        "/etc/passwd",
        "' OR 1=1 --",
        "%",
        &album[..3],
        &album[3..],
        upper.as_str(),
        long.as_str(),
        short,
        leading_space.as_str(),
        trailing_space.as_str(),
        with_path.as_str(),
    ];

    for id in ids {
        for size in [None, Some("0"), Some("64")] {
            let mut params = vec![("id", id)];
            params.extend(size.map(|size| ("size", size)));
            for name in ["getCoverArt", "getCoverArt.view"] {
                assert_eq!(
                    server.get_json(name, &params).await,
                    failed(70, NOT_FOUND),
                    "{name} {id:?} {size:?}"
                );
            }
            assert_eq!(
                server.get_xml("getCoverArt", &params).await,
                xml_failed(70, NOT_FOUND),
                "{id:?} {size:?}"
            );
        }

        // A conditional request for a cover that the server does not have
        // gets the error and not HTTP 304.
        let params = [("id", id), ("f", "json")];
        for value in ["*", "\"al-ffffffffffffffffffffffffffffffff-0-0\""] {
            let answer = conditional(&server, &params, value).await;
            answer.assert_error(70, NOT_FOUND, &format!("{id:?} {value}"));
        }
    }

    // The album itself has a cover.
    assert_eq!(
        cover(&server, &[("id", album)]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn track_without_a_file_has_no_cover_and_gives_none_to_its_album() {
    let server = TestServer::standard().await;
    let upstream = server.track.upstream.as_str();
    let album = server.album.slow_rivers.as_str();
    assert_eq!(cover(&server, &[("id", upstream)]).await.body, PICTURE_PNG);
    assert_eq!(cover(&server, &[("id", album)]).await.body, PICTURE_PNG);

    // The file with the picture stays on disk. The row has no file.
    remove_file(&server, upstream).await;
    for id in [upstream, album] {
        for size in [None, Some("64")] {
            let mut params = vec![("id", id), ("f", "json")];
            params.extend(size.map(|size| ("size", size)));
            let answer = cover(&server, &params).await;
            answer.assert_error(70, NOT_FOUND, &format!("{id} {size:?}"));
        }
    }

    // The other track of the album has its file and its picture.
    let delta = cover(&server, &[("id", &server.track.delta)]).await;
    assert_eq!(delta.status, StatusCode::OK);
    assert_eq!(delta.body, PICTURE_PNG);
}

#[tokio::test]
async fn request_without_an_id_is_error_10() {
    let server = TestServer::standard().await;
    let message = "required parameter is missing: id";
    let album = server.album.glass_harbour.as_str();
    for name in ["getCoverArt", "getCoverArt.view"] {
        // The name of a parameter is compared as it is written.
        for params in [
            &[][..],
            &[("size", "64")],
            &[("ID", album)],
            &[("size", "x")],
        ] {
            assert_eq!(
                server.get_json(name, params).await,
                failed(10, message),
                "{name} {params:?}"
            );
        }
        assert_eq!(
            server.get_xml(name, &[]).await,
            xml_failed(10, message),
            "{name}"
        );
    }
}

#[tokio::test]
async fn size_selects_the_stored_image_or_a_jpeg_of_at_most_2000_pixels() {
    // The cover is wider than the largest size that the server returns.
    let (server, album, cover_file) = server_with_cover("cover.png", 2048, 2).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");
    let time = mtime(&cover_file);

    // No size, an empty size and the size 0 are the image as it is stored.
    for params in [
        vec![("id", id)],
        vec![("id", id), ("size", "")],
        vec![("id", id), ("size", "0")],
        vec![("id", id), ("size", "00")],
    ] {
        let answer = cover(&server, &params).await;
        answer.assert_cover("image/png", &etag(id, 0, time), &format!("{params:?}"));
        assert_eq!(answer.body, stored, "{params:?}");
    }

    // The step of a size is the longer side of the JPEG: the smallest step
    // that is not smaller than the size.
    for (size, step) in [
        ("1", 80),
        ("64", 80),
        ("80", 80),
        ("81", 160),
        ("300", 320),
        ("640", 640),
        ("641", 1280),
        ("1281", 2000),
        ("1999", 2000),
        ("2000", 2000),
    ] {
        let answer = cover(&server, &[("id", id), ("size", size)]).await;
        answer.assert_cover("image/jpeg", &etag(id, step, time), size);
        let (width, height) = jpeg_dimensions(&answer.body);
        assert_eq!(width, step, "{size}");
        assert!((1..=2).contains(&height), "{size}: {height}");
    }

    // A size over 2000 counts as 2000: the cover and its tag are the ones
    // of the size 2000.
    let largest = cover(&server, &[("id", id), ("size", "2000")]).await;
    for size in ["2001", "5000", "65536", "4294967295"] {
        let answer = cover(&server, &[("id", id), ("size", size)]).await;
        answer.assert_cover("image/jpeg", &etag(id, 2000, time), size);
        assert_eq!(answer.body, largest.body, "{size}");
        assert_eq!(jpeg_dimensions(&answer.body).0, 2000, "{size}");

        let answer = conditional(&server, &[("id", id), ("size", size)], largest.etag()).await;
        answer.assert_not_modified(largest.etag(), size);
    }

    // A value that is no integer from 0 to 4294967295 is error 0, for an
    // ID that has a cover and for one that has none.
    for size in [
        "-1",
        "-0",
        "abc",
        "1.5",
        "64.0",
        "64px",
        "0x40",
        " 64",
        "1e3",
        "4294967296",
        "99999999999999999999",
        // Values that would leave a directory if they were part of a path.
        "../x",
        "64/../../x",
        "..\\x",
        "/etc/passwd",
        "64\0",
    ] {
        for id in [id, "al-ffffffffffffffffffffffffffffffff", "../x"] {
            let params = [("id", id), ("size", size)];
            assert_eq!(
                server.get_json("getCoverArt", &params).await,
                failed(0, INVALID_SIZE),
                "{id} {size:?}"
            );
            assert_eq!(
                server.get_xml("getCoverArt", &params).await,
                xml_failed(0, INVALID_SIZE),
                "{id} {size:?}"
            );
        }
    }
}

#[tokio::test]
async fn head_request_gets_the_headers_of_the_cover_without_the_body() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let time = mtime(&cover_file);
    let head = |params: &[(&str, &str)], tag: Option<&str>| {
        let path = format!("/rest/getCoverArt?{}", urlencode(&as_owner(params)));
        let mut request = Request::head(path);
        if let Some(tag) = tag {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        request.body(Body::empty()).expect("a request")
    };

    for size in [None, Some("64")] {
        let mut params = vec![("id", id)];
        params.extend(size.map(|size| ("size", size)));
        let get = cover(&server, &params).await;
        let answer = Answer::of(server.request(head(&params, None)).await).await;
        let case = format!("{size:?}");

        // The headers are the ones of the GET request, the length included.
        assert_eq!(answer.status, StatusCode::OK, "{case}");
        assert!(answer.body.is_empty(), "{case}");
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::ETAG,
            header::CACHE_CONTROL,
        ] {
            assert_eq!(answer.header(name.clone()), get.header(name), "{case}");
        }
        assert_eq!(
            answer.header(header::CONTENT_LENGTH),
            Some(get.body.len().to_string().as_str()),
            "{case}"
        );
        answer.assert_cors(&case);

        // A conditional HEAD request is answered as a conditional GET. The
        // size 64 is served at the step 80.
        let tag = etag(id, size.map_or(0, |_| 80), time);
        let request = head(&params, Some(&tag));
        let answer = Answer::of(server.request(request).await).await;
        answer.assert_not_modified(&tag, &case);
    }

    // An error is the envelope without its body.
    let params = [("id", "../x"), ("f", "json")];
    let answer = Answer::of(server.request(head(&params, None)).await).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.header(header::CONTENT_TYPE),
        Some("application/json")
    );
    assert_eq!(answer.header(header::ETAG), None);
    assert!(answer.body.is_empty());
}

#[tokio::test]
async fn post_with_a_form_body_is_answered_as_a_get() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let time = mtime(&cover_file);
    let stored = fs::read(&cover_file).expect("read the cover file");

    for name in ["getCoverArt", "getCoverArt.view"] {
        let answer = Answer::of(server.post_form(name, &[("id", id)]).await).await;
        answer.assert_cover("image/jpeg", &etag(id, 0, time), name);
        assert_eq!(answer.body, stored, "{name}");

        let form = [("id", id), ("size", "64")];
        let answer = Answer::of(server.post_form(name, &form).await).await;
        answer.assert_cover("image/jpeg", &etag(id, 80, time), name);
        assert_eq!(jpeg_dimensions(&answer.body), (80, 40), "{name}");
        let get = cover(&server, &form).await;
        assert_eq!(answer.body, get.body, "{name}");

        // The errors have the format that the form selects.
        let form = [("id", "../x"), ("f", "json")];
        let answer = Answer::of(server.post_form(name, &form).await).await;
        answer.assert_error(70, NOT_FOUND, name);
        let form = [("id", id), ("size", "abc"), ("f", "json")];
        let answer = Answer::of(server.post_form(name, &form).await).await;
        answer.assert_error(0, INVALID_SIZE, name);
        let response = server.post_form(name, &[("size", "64")]).await;
        assert_eq!(content_type(&response), "application/xml", "{name}");
        assert_eq!(
            body_string(response).await,
            xml_failed(10, "required parameter is missing: id"),
            "{name}"
        );
    }

    // A conditional POST is answered as a conditional GET.
    let tag = etag(id, 80, time);
    let form = as_owner(&[("id", id), ("size", "64")]);
    let mut request = form_request("/rest/getCoverArt", &form);
    let value = tag.parse().expect("a header value");
    request.headers_mut().insert(header::IF_NONE_MATCH, value);
    let answer = Answer::of(server.request(request).await).await;
    answer.assert_not_modified(&tag, "POST");
}

#[tokio::test]
async fn range_and_if_modified_since_are_not_read() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");
    let tag = etag(id, 0, mtime(&cover_file));

    // The answer is the whole cover with HTTP 200, not a part of it and not
    // HTTP 304. `If-Match` and `If-Range` with the tag change nothing.
    for (name, value) in [
        ("Range", "bytes=0-9"),
        ("Range", "bytes=999999-"),
        ("If-Modified-Since", "Fri, 31 Dec 9999 23:59:59 GMT"),
        ("If-Match", "\"another\""),
        ("If-Range", tag.as_str()),
    ] {
        let response = server
            .get_with_headers("getCoverArt", &[("id", id)], &[(name, value)])
            .await;
        let answer = Answer::of(response).await;
        answer.assert_cover("image/jpeg", &tag, name);
        assert_eq!(answer.body, stored, "{name}");
        assert_eq!(answer.header(header::CONTENT_RANGE), None, "{name}");
    }
}

#[tokio::test]
async fn request_without_a_user_gets_the_authentication_error_and_no_cover() {
    let server = TestServer::standard().await;
    let id = server.album.glass_harbour.as_str();
    let v = ("v", API_VERSION);
    let c = ("c", CLIENT);

    let cases: [(&[(&str, &str)], Value); 3] = [
        (
            &[("id", id), v, c],
            failed(10, "required parameter is missing: u"),
        ),
        (
            &[("id", id), v, c, ("u", OWNER), ("p", "wrong")],
            failed(40, "wrong username or password"),
        ),
        (
            &[("id", id), v, c, ("u", "nobody"), ("p", OWNER_PASSWORD)],
            failed(40, "wrong username or password"),
        ),
    ];
    for (params, expected) in cases {
        // `get_json_noauth` checks the status and the JSON content type.
        let response = server.get_json_noauth("getCoverArt", params).await;
        assert_eq!(response, expected, "{params:?}");

        // A conditional request without a user is not HTTP 304.
        let mut params = params.to_vec();
        params.push(("f", "json"));
        let request = get_request("getCoverArt", &params, &[("If-None-Match", "*")]);
        let answer = Answer::of(server.request(request).await).await;
        assert_eq!(answer.status, StatusCode::OK, "{params:?}");
        assert_eq!(answer.header(header::ETAG), None, "{params:?}");
        assert_eq!(
            answer.header(header::CONTENT_TYPE),
            Some("application/json")
        );
    }
}

/// The runtime of a test has one thread. A handler that kept that thread
/// while it waits for the cover service would stop the task of the service
/// that makes the cover, and no request of this test would be answered.
#[tokio::test]
async fn requests_at_the_same_time_are_all_answered() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 640, 320).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");
    let get = |size: &'static str| {
        let server = &server;
        async move {
            let mut params = vec![("id", id)];
            if !size.is_empty() {
                params.push(("size", size));
            }
            cover(server, &params).await
        }
    };

    // Three steps, and two sizes of the step 80.
    let answers = tokio::join!(
        get("80"),
        get("160"),
        get(""),
        get("80"),
        get("320"),
        get("160"),
        get(""),
        get("64"),
    );
    let (a80, a160, a0, b80, a320, b160, b0, c80) = answers;
    for (answer, width) in [(&a80, 80), (&a160, 160), (&a320, 320)] {
        assert_eq!(answer.status, StatusCode::OK, "{width}");
        assert_eq!(jpeg_dimensions(&answer.body), (width, width / 2));
    }
    // The requests for one step get one cover.
    assert_eq!(b80.body, a80.body);
    assert_eq!(c80.body, a80.body);
    assert_eq!(b160.body, a160.body);
    for answer in [&a0, &b0] {
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, stored);
    }
}

/// A client that disconnects drops the future of its request. The test
/// drops a request at each point at which it waits: for the user, for the
/// cover row, for the cache and for the cover. The cover service holds one
/// source image at a time, so a request that kept its place would keep the
/// next request waiting, and the test would not end.
///
/// The number of the points differs from run to run: a read that ends before
/// the request is polled again is no point of waiting. A request for a
/// resized cover that the cache does not hold always waits once, for the task
/// that makes the cover.
///
/// The unit tests in `src/api/covers.rs` check that the cover of a dropped
/// request is not made. They read the cover cache, which the test server
/// does not show.
#[tokio::test]
async fn request_that_ends_early_does_not_keep_the_next_request_waiting() {
    let (server, album, cover_file) = server_with_cover("cover.jpg", 128, 64).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");

    for sized in [false, true] {
        let mut ended = 0;
        for waits in 1.. {
            // A resized cover, or the image as it is stored. The sizes are
            // sizes of the step 160, so the requests ask for one cover. The
            // cache does not hold it while every request for it was dropped
            // before its cover was made.
            let size = (100 + waits).to_string();
            let mut params = vec![("id", id)];
            if sized {
                params.push(("size", &size));
            }
            let request = get_request("getCoverArt", &as_owner(&params), &[]);
            let mut pending = Box::pin(server.request(request));
            let finished = advance(pending.as_mut(), waits).await;
            drop(pending);

            // The next requests are answered: the stored image, which waits
            // for the one place of the service itself, and a resized cover
            // of another step, which the first of these requests makes.
            let original = cover(&server, &[("id", id)]).await;
            assert_eq!(original.status, StatusCode::OK, "{sized} {waits}");
            assert_eq!(original.body, stored, "{sized} {waits}");
            let next = (1000 + 100 * usize::from(sized) + waits).to_string();
            let resized = cover(&server, &[("id", id), ("size", &next)]).await;
            assert_eq!(resized.status, StatusCode::OK, "{sized} {waits}");
            assert_eq!(jpeg_dimensions(&resized.body), (128, 64));

            match finished {
                Some(response) => {
                    assert_eq!(response.status(), StatusCode::OK, "{sized} {waits}");
                    break;
                }
                None => ended += 1,
            }
        }
        if sized {
            assert!(ended >= 1, "no request was dropped while it waited");
        }
    }
}

/// A link that changed after the scan leads the stored path of a cover out
/// of the music folder. `stream` refuses the file of a track behind such a
/// link, and `getCoverArt` refuses the cover (`docs/design.md` section 3.11).
#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn cover_below_a_directory_link_out_of_the_music_folder_is_error_70() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().expect("create a directory");
    let (server, album, cover_file) = server_with_cover("cover.jpg", 160, 80).await;
    let id = album.as_str();
    let stored = fs::read(&cover_file).expect("read the cover file");

    // The cover is served while its file is in the music folder. The cache
    // of the server holds the cover of the step 80 after that.
    let original = cover(&server, &[("id", id)]).await;
    assert_eq!(original.body, stored);
    let resized = cover(&server, &[("id", id), ("size", "80")]).await;
    assert_eq!(jpeg_dimensions(&resized.body), (80, 40));

    // The directory of the album becomes a link to a directory outside the
    // music folder. The cover file has its path, and it is a regular file.
    let album_dir = cover_file.parent().expect("the directory of the album");
    let moved = outside.path().join("Glass Harbour");
    fs::rename(album_dir, &moved).expect("move the directory");
    symlink(&moved, album_dir).expect("create the link");
    assert_eq!(
        fs::read(&cover_file).expect("read through the link"),
        stored
    );

    // The file, a cover that the cache holds and a cover that it does not
    // hold are all error 70.
    for size in [None, Some("0"), Some("80"), Some("64"), Some("320")] {
        let mut params = vec![("id", id), ("f", "json")];
        params.extend(size.map(|size| ("size", size)));
        let answer = cover(&server, &params).await;
        answer.assert_error(70, NOT_FOUND, &format!("{size:?}"));

        // A client that holds the cover gets the error and not HTTP 304.
        let answer = conditional(&server, &params, resized.etag()).await;
        answer.assert_error(70, NOT_FOUND, &format!("conditional {size:?}"));
    }

    // The cover is served again when the directory is back in the folder.
    fs::remove_file(album_dir).expect("remove the link");
    fs::rename(&moved, album_dir).expect("move the directory back");
    let again = cover(&server, &[("id", id), ("size", "80")]).await;
    assert_eq!(again.body, resized.body);
}
