//! Tests of the response models, the annotation lookup and the browse
//! queries on a scanned library, and of the test servers that scan one
//! (`TestServer::standard`, `TestServer::standard_with_own_files` and
//! `TestServer::scanned`).
//!
//! The unit tests in `src/api/model.rs` compare the documents of every model
//! byte for byte, with rows that the tests write. The tests here build the
//! models from the rows of a real scan.

mod common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use common::{
    LibraryBuilder, OWNER, ScannedServer, StandardServer, TestServer, body_bytes, body_string,
    get_request,
};
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::api::envelope::{Format, SubsonicResponse};
use wn_server::api::model;
use wn_server::db::queries::annotations::{self, Annotation};
use wn_server::db::queries::browse::{self, Album, Artist, Genre, Song};
use wn_server::db::queries::{tracks, users};
use wn_server::ids::{self, Kind};

/// The relative paths of the tracks of the standard library, in the order
/// the builder adds them.
const LOW_TIDE: &str = "Aurora Field/Glass Harbour/01 Low Tide.flac";
const MERIDIAN: &str = "Aurora Field/Paper Maps/01 Meridian.mp3";
const UPSTREAM: &str = "Marlow Quartet/Slow Rivers/01 Upstream.m4a";
const OPEN_ROAD: &str = "Tessa Lund/Summer Sampler/01 Open Road.ogg";
const LAST_TRAIN: &str = "Aurora Field/Summer Sampler/02 Last Train.opus";

fn unix_now() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    i64::try_from(now.as_secs()).expect("the time fits")
}

async fn song(server: &ScannedServer, id: &str) -> Song {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::track(conn, &id))
        .await
        .expect("read the track")
        .expect("the track exists")
}

async fn album(server: &ScannedServer, id: &str) -> Album {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::album(conn, &id))
        .await
        .expect("read the album")
        .expect("the album exists")
}

async fn artist(server: &ScannedServer, id: &str) -> Artist {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::artist(conn, &id))
        .await
        .expect("read the artist")
        .expect("the artist exists")
}

async fn songs_of(server: &ScannedServer, album_id: &str) -> Vec<Song> {
    let album_id = album_id.to_owned();
    server
        .db()
        .read(move |conn| browse::tracks_of_album(conn, &album_id))
        .await
        .expect("read the tracks")
}

/// The ID of the owner account of the server.
async fn owner_id(server: &TestServer) -> i64 {
    server
        .db()
        .read(|conn| users::get_by_name(conn, OWNER))
        .await
        .expect("read the owner")
        .expect("the server has an owner")
        .user
        .id
}

/// The annotations of the owner for `ids`.
async fn annotations_of(server: &TestServer, ids: &[&str]) -> HashMap<String, Annotation> {
    let user_id = owner_id(server).await;
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    server
        .db()
        .read(move |conn| annotations::get_many(conn, user_id, &ids))
        .await
        .expect("read the annotations")
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|song| song.track.title.as_str()).collect()
}

fn to_json(model: impl serde::Serialize) -> Value {
    serde_json::to_value(model).expect("the model serialises")
}

#[tokio::test]
async fn the_standard_server_is_ready_in_under_one_second() {
    // The first server of the test binary may write the files of the
    // library. The second one finds them.
    for _ in 0..2 {
        let start = Instant::now();
        let server = TestServer::standard().await;
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        assert_eq!(server.catalogue().await.songs.len(), 10);
    }
}

#[tokio::test]
async fn the_standard_server_exposes_the_ids_of_its_artists_albums_and_tracks() {
    let server = TestServer::standard().await;
    let artists = [
        &server.artist.aurora_field,
        &server.artist.marlow_quartet,
        &server.artist.various_artists,
    ];
    let albums = [
        &server.album.glass_harbour,
        &server.album.paper_maps,
        &server.album.slow_rivers,
        &server.album.summer_sampler,
    ];
    let tracks = [
        &server.track.low_tide,
        &server.track.breakwater,
        &server.track.night_ferry,
        &server.track.meridian,
        &server.track.contour,
        &server.track.legend,
        &server.track.upstream,
        &server.track.delta,
        &server.track.open_road,
        &server.track.last_train,
    ];
    let kind = |id: &String| ids::parse(id).map(|(kind, _)| kind);
    assert!(artists.iter().all(|id| kind(id) == Some(Kind::Artist)));
    assert!(albums.iter().all(|id| kind(id) == Some(Kind::Album)));
    assert!(tracks.iter().all(|id| kind(id) == Some(Kind::Track)));
    let distinct: HashSet<&String> = artists
        .iter()
        .chain(&albums)
        .chain(&tracks)
        .copied()
        .collect();
    assert_eq!(distinct.len(), 17);

    // The fields name every row of the library, and each names its own.
    let catalogue = server.catalogue().await;
    assert_eq!(catalogue.artists.len(), 3);
    assert_eq!(catalogue.albums.len(), 4);
    assert_eq!(catalogue.songs.len(), 10);
    assert_eq!(catalogue.folders.len(), 1);
    assert_eq!(catalogue.folders[0].id, server.folder_id);
    assert_eq!(
        artist(&server, &server.artist.various_artists).await.name,
        "Various Artists"
    );
    assert_eq!(
        album(&server, &server.album.paper_maps).await.name,
        "Paper Maps"
    );
    let paths = [
        (&server.track.low_tide, LOW_TIDE),
        (&server.track.meridian, MERIDIAN),
        (&server.track.upstream, UPSTREAM),
        (&server.track.open_road, OPEN_ROAD),
        (&server.track.last_train, LAST_TRAIN),
    ];
    for (id, rel_path) in paths {
        assert_eq!(song(&server, id).await.track.rel_path, rel_path);
        assert!(server.path(rel_path).is_file(), "{rel_path}");
    }
    assert_eq!(server.files().len(), 10);
    assert_eq!(server.files()[0], server.path(LOW_TIDE));
}

#[tokio::test]
async fn the_standard_server_answers_requests_as_the_owner() {
    let server = TestServer::standard().await;
    let response = server.get_json("getUser", &[("username", OWNER)]).await;
    assert_eq!(response["status"], "ok");
    // The owner reads the one music folder of the library.
    assert_eq!(response["user"]["folder"], json!([server.folder_id]));
}

#[tokio::test]
async fn standard_servers_share_the_files_and_have_a_database_each() {
    let first = TestServer::standard().await;
    let second = TestServer::standard().await;
    assert_eq!(first.root(), second.root());

    // The scan of each server gave the rows IDs of its own.
    assert_ne!(first.track.low_tide, second.track.low_tide);
    let user_id = owner_id(&first).await;
    let id = first.track.low_tide.clone();
    first
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO annotations (user_id, item_id, item_type, starred_at)
                 VALUES (?1, ?2, 'track', 100)",
                params![user_id, id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let all = "SELECT count(*) FROM annotations";
    let count = |server: &StandardServer| {
        let db = server.db().clone();
        async move {
            db.read(move |conn| Ok(conn.query_row(all, [], |row| row.get::<_, i64>(0))?))
                .await
                .unwrap()
        }
    };
    assert_eq!(count(&first).await, 1);
    assert_eq!(count(&second).await, 0);
}

#[tokio::test]
async fn a_server_with_its_own_files_follows_a_renamed_file_on_a_rescan() {
    let shared = TestServer::standard().await;
    let server = TestServer::standard_with_own_files().await;
    assert_ne!(server.root(), shared.root());

    let renamed = "Aurora Field/Glass Harbour/01 High Tide.flac";
    fs::rename(server.path(LOW_TIDE), server.path(renamed)).unwrap();
    let summary = server.rescan().await;
    assert_eq!(
        (summary.seen, summary.moved, summary.added, summary.removed),
        (10, 1, 0, 0)
    );
    assert_eq!(summary.errors, [] as [&str; 0]);
    // The track keeps its ID at its new path.
    assert_eq!(
        song(&server, &server.track.low_tide).await.track.rel_path,
        renamed
    );
    // The files of the shared library are as they were.
    assert!(shared.path(LOW_TIDE).is_file());
}

#[tokio::test]
async fn a_scanned_server_serves_the_library_of_the_test() {
    let library = LibraryBuilder::new()
        .track("The Beatles", "Help!", "Help!", 1, "mp3")
        .track("2Pac", "100%", "_", 1, "flac")
        .build();
    let root = library.root.path().to_path_buf();
    let server = TestServer::scanned(library).await;
    assert_eq!(server.root(), root);

    let catalogue = server.catalogue().await;
    assert_eq!(catalogue.artists.len(), 2);
    assert_eq!(catalogue.artist("2Pac").album_count, 1);
    assert_eq!(catalogue.album("100%").artist_name, "2Pac");
    assert_eq!(catalogue.song("_").track.rel_path, "2Pac/100%/01 _.flac");
    assert_eq!(catalogue.song("Help!").track.folder_id, server.folder_id);
    // The server answers requests.
    assert_eq!(server.get_json("ping", &[]).await["status"], "ok");
}

#[tokio::test]
async fn get_with_headers_sends_the_auth_defaults_and_the_headers() {
    let server = TestServer::standard().await;
    // `ping` needs a user, so the answer shows that the request carried the
    // auth defaults.
    let response = server
        .get_with_headers("ping", &[("f", "json")], &[("Range", "bytes=0-1")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response).await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(r#""status":"ok""#), "{text}");

    let plain = server.get_with_headers("ping", &[], &[]).await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert!(body_string(plain).await.contains(r#"status="ok""#));

    // The request that the method sends, without the auth defaults.
    let request = get_request(
        "stream",
        &[("id", "tr 1")],
        &[("Range", "bytes=0-1"), ("If-None-Match", "\"x\"")],
    );
    assert_eq!(request.method(), "GET");
    assert_eq!(request.uri(), "/rest/stream?id=tr%201");
    assert_eq!(request.headers()["range"], "bytes=0-1");
    assert_eq!(request.headers()["if-none-match"], "\"x\"");
    assert_eq!(request.headers().len(), 2);
}

/// The properties of the file of one track of the standard library, as the
/// `Child` element reports them. Every fixture is mono and lasts about half
/// a second, which is one second after rounding.
struct FileFields {
    id: String,
    path: &'static str,
    suffix: &'static str,
    content_type: &'static str,
    bit_rate: u32,
    bit_depth: u8,
    sampling_rate: u32,
}

#[tokio::test]
async fn the_child_of_a_track_of_each_format_reports_the_properties_of_its_file() {
    let server = TestServer::standard().await;
    let field =
        |id: &String, path, suffix, content_type, bit_rate, bit_depth, sampling_rate| FileFields {
            id: id.clone(),
            path,
            suffix,
            content_type,
            bit_rate,
            bit_depth,
            sampling_rate,
        };
    let track = &server.track;
    // A lossy format has no bit depth, and the element reports 0.
    let cases = [
        field(
            &track.low_tide,
            LOW_TIDE,
            "flac",
            "audio/flac",
            58,
            16,
            22_050,
        ),
        field(
            &track.meridian,
            MERIDIAN,
            "mp3",
            "audio/mpeg",
            34,
            0,
            22_050,
        ),
        field(&track.upstream, UPSTREAM, "m4a", "audio/mp4", 34, 0, 22_050),
        field(
            &track.open_road,
            OPEN_ROAD,
            "ogg",
            "audio/ogg",
            24,
            0,
            22_050,
        ),
        field(
            &track.last_train,
            LAST_TRAIN,
            "opus",
            "audio/ogg",
            28,
            0,
            24_000,
        ),
    ];
    for case in cases {
        let child = model::child(&song(&server, &case.id).await, None);
        let size = fs::metadata(server.path(case.path)).unwrap().len();
        assert_eq!(child.path.as_deref(), Some(case.path));
        assert_eq!(child.suffix.as_deref(), Some(case.suffix), "{}", case.path);
        assert_eq!(
            child.content_type.as_deref(),
            Some(case.content_type),
            "{}",
            case.path
        );
        assert_eq!(child.bit_rate, Some(case.bit_rate), "{}", case.path);
        assert_eq!(child.bit_depth, Some(case.bit_depth), "{}", case.path);
        assert_eq!(
            child.sampling_rate,
            Some(case.sampling_rate),
            "{}",
            case.path
        );
        assert_eq!(child.channel_count, Some(1), "{}", case.path);
        assert_eq!(child.duration, Some(1), "{}", case.path);
        assert_eq!(child.size, Some(size), "{}", case.path);
    }
}

#[tokio::test]
async fn the_child_of_a_flac_track_is_this_document() {
    let before = unix_now();
    let server = TestServer::standard().await;
    let after = unix_now();
    let low_tide = song(&server, &server.track.low_tide).await;
    let created_at = low_tide.track.created_at;
    assert!((before..=after).contains(&created_at), "{created_at}");
    let created = model::iso8601(created_at);
    let size = fs::metadata(server.path(LOW_TIDE)).unwrap().len();

    let child = model::child(&low_tide, None);
    let album_id = &server.album.glass_harbour;
    let artist_id = &server.artist.aurora_field;
    let id = &server.track.low_tide;
    assert_eq!(
        to_json(&child),
        json!({
            "id": id,
            "parent": album_id,
            "isDir": false,
            "title": "Low Tide",
            "album": "Glass Harbour",
            "artist": "Aurora Field",
            "track": 1,
            "year": 2019,
            "genre": "Electronic",
            // The album has the file `cover.jpg`.
            "coverArt": album_id,
            "size": size,
            "contentType": "audio/flac",
            "suffix": "flac",
            "duration": 1,
            "bitRate": 58,
            "bitDepth": 16,
            "samplingRate": 22050,
            "channelCount": 1,
            "path": "Aurora Field/Glass Harbour/01 Low Tide.flac",
            // The track has no disc number tag.
            "discNumber": 1,
            "created": created,
            "albumId": album_id,
            "artistId": artist_id,
            "type": "music",
            "mediaType": "song",
            "playCount": 0,
        })
    );

    // The same element in XML: every field is an attribute.
    let response = SubsonicResponse::ok("song", to_json(&child));
    let (content_type, xml) = response.render(Format::Xml);
    assert_eq!(content_type, "application/xml");
    let expected = format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{version}""#,
            r#" openSubsonic="true"><song album="Glass Harbour" albumId="{album_id}""#,
            r#" artist="Aurora Field" artistId="{artist_id}" bitDepth="16" bitRate="58""#,
            r#" channelCount="1" contentType="audio/flac" coverArt="{album_id}""#,
            r#" created="{created}" discNumber="1" duration="1" genre="Electronic""#,
            r#" id="{id}" isDir="false" mediaType="song" parent="{album_id}""#,
            r#" path="Aurora Field/Glass Harbour/01 Low Tide.flac" playCount="0""#,
            r#" samplingRate="22050" size="{size}" suffix="flac" title="Low Tide""#,
            r#" track="1" type="music" year="2019"/></subsonic-response>"#
        ),
        version = wn_server::VERSION,
        album_id = album_id,
        artist_id = artist_id,
        created = created,
        id = id,
        size = size,
    );
    assert_eq!(xml, expected);
}

#[tokio::test]
async fn the_child_of_a_track_on_a_disc_reports_the_disc_number() {
    let server = TestServer::standard().await;
    let disc = |id: &String| {
        let id = id.clone();
        let server = &server;
        async move { model::child(&song(server, &id).await, None).disc_number }
    };
    assert_eq!(disc(&server.track.meridian).await, Some(1));
    assert_eq!(disc(&server.track.legend).await, Some(2));
    // No disc number tag: disc 1.
    assert_eq!(disc(&server.track.upstream).await, Some(1));
}

#[tokio::test]
async fn the_child_of_a_compilation_track_names_its_track_artist() {
    let server = TestServer::standard().await;
    let open_road = model::child(&song(&server, &server.track.open_road).await, None);
    assert_eq!(open_road.artist.as_deref(), Some("Tessa Lund"));
    assert_eq!(open_road.album.as_deref(), Some("Summer Sampler"));
    assert_eq!(
        open_road.parent.as_ref(),
        Some(&server.album.summer_sampler)
    );
    assert_eq!(
        open_road.album_id.as_ref(),
        Some(&server.album.summer_sampler)
    );
    // The album artist is "Various Artists". "Tessa Lund" has no artist row.
    assert_eq!(
        open_road.artist_id.as_ref(),
        Some(&server.artist.various_artists)
    );
    let last_train = model::child(&song(&server, &server.track.last_train).await, None);
    assert_eq!(last_train.artist.as_deref(), Some("Aurora Field"));
    assert_eq!(
        last_train.artist_id.as_ref(),
        Some(&server.artist.various_artists)
    );
}

#[tokio::test]
async fn cover_art_ids_follow_the_covers_that_the_scan_found() {
    let server = TestServer::standard().await;
    let album_cover = |id: &String| {
        let id = id.clone();
        let server = &server;
        async move { model::album(&album(server, &id).await, None).cover_art }
    };
    let song_cover = |id: &String| {
        let id = id.clone();
        let server = &server;
        async move { model::child(&song(server, &id).await, None).cover_art }
    };
    let glass_harbour = &server.album.glass_harbour;
    let slow_rivers = &server.album.slow_rivers;

    // "Glass Harbour" has the file `cover.jpg`, and "Slow Rivers" has an
    // embedded picture: the ID of the album is the cover of the album and of
    // its tracks.
    assert_eq!(
        album_cover(glass_harbour).await.as_ref(),
        Some(glass_harbour)
    );
    assert_eq!(
        song_cover(&server.track.night_ferry).await.as_ref(),
        Some(glass_harbour)
    );
    assert_eq!(album_cover(slow_rivers).await.as_ref(), Some(slow_rivers));
    assert_eq!(
        song_cover(&server.track.delta).await.as_ref(),
        Some(slow_rivers)
    );
    // The other two albums have no cover.
    assert_eq!(album_cover(&server.album.paper_maps).await, None);
    assert_eq!(song_cover(&server.track.contour).await, None);
    assert_eq!(album_cover(&server.album.summer_sampler).await, None);
    assert_eq!(song_cover(&server.track.open_road).await, None);

    // The directory form of an album has the cover of the album.
    let directory = model::child_dir_for_album(&album(&server, slow_rivers).await, None);
    assert_eq!(directory.cover_art.as_ref(), Some(slow_rivers));
    assert_eq!(
        directory.parent.as_ref(),
        Some(&server.artist.marlow_quartet)
    );
}

#[tokio::test]
async fn the_album_of_the_standard_library_is_this_document() {
    let server = TestServer::standard().await;
    let row = album(&server, &server.album.paper_maps).await;
    let created = model::iso8601(row.created_at);
    let songs: Vec<model::Child> = songs_of(&server, &server.album.paper_maps)
        .await
        .iter()
        .map(|song| model::child(song, None))
        .collect();
    let element = model::album_with_songs(&row, None, songs.clone());
    let mut expected = json!({
        "id": server.album.paper_maps,
        "name": "Paper Maps",
        "artist": "Aurora Field",
        "artistId": server.artist.aurora_field,
        "songCount": 3,
        // Three MP3 fixtures of 575 ms each.
        "duration": 2,
        "created": created,
        "year": 2021,
        "genre": "Electronic",
        "playCount": 0,
    });
    expected["song"] = to_json(&songs);
    assert_eq!(to_json(&element), expected);
    assert_eq!(songs.len(), 3);
    assert_eq!(element.song_count, 3);

    // Each album of the library, as an entry of a list.
    let entry = |id: &String| {
        let id = id.clone();
        let server = &server;
        async move { model::album(&album(server, &id).await, None) }
    };
    let glass_harbour = entry(&server.album.glass_harbour).await;
    assert_eq!((glass_harbour.song_count, glass_harbour.duration), (3, 2));
    assert_eq!(glass_harbour.year, Some(2019));
    let slow_rivers = entry(&server.album.slow_rivers).await;
    assert_eq!((slow_rivers.song_count, slow_rivers.duration), (2, 1));
    assert_eq!(slow_rivers.artist, "Marlow Quartet");
    assert_eq!(slow_rivers.genre.as_deref(), Some("Jazz"));
    let summer_sampler = entry(&server.album.summer_sampler).await;
    assert_eq!((summer_sampler.song_count, summer_sampler.duration), (2, 1));
    assert_eq!(summer_sampler.artist, "Various Artists");
    assert_eq!(summer_sampler.artist_id, server.artist.various_artists);
}

#[tokio::test]
async fn list_artists_returns_the_album_artists_with_their_album_counts() {
    let server = TestServer::standard().await;
    let folder_id = server.folder_id;
    let (all, in_folder, elsewhere) = server
        .db()
        .read(move |conn| {
            Ok((
                browse::list_artists(conn, None)?,
                browse::list_artists(conn, Some(folder_id))?,
                browse::list_artists(conn, Some(folder_id + 1))?,
            ))
        })
        .await
        .unwrap();
    let rows: Vec<(&str, &str, u32)> = all
        .iter()
        .map(|artist| (artist.id.as_str(), artist.name.as_str(), artist.album_count))
        .collect();
    assert_eq!(
        rows,
        [
            (server.artist.aurora_field.as_str(), "Aurora Field", 2),
            (server.artist.marlow_quartet.as_str(), "Marlow Quartet", 1),
            (server.artist.various_artists.as_str(), "Various Artists", 1),
        ]
    );
    assert_eq!(in_folder, all);
    assert_eq!(elsewhere, []);

    // The elements of `getArtists`, grouped by letter.
    let indexes = model::index(&all, |row| model::artist(row, None));
    assert_eq!(
        to_json(&indexes),
        json!([
            { "name": "A", "artist": [
                { "id": server.artist.aurora_field, "name": "Aurora Field", "albumCount": 2 },
            ]},
            { "name": "M", "artist": [
                { "id": server.artist.marlow_quartet, "name": "Marlow Quartet", "albumCount": 1 },
            ]},
            { "name": "V", "artist": [
                { "id": server.artist.various_artists, "name": "Various Artists", "albumCount": 1 },
            ]},
        ])
    );
}

#[tokio::test]
async fn albums_of_artist_returns_the_albums_of_one_artist_by_year() {
    let server = TestServer::standard().await;
    let of = |id: &String| {
        let id = id.clone();
        let db = server.db().clone();
        async move {
            db.read(move |conn| browse::albums_of_artist(conn, &id))
                .await
                .unwrap()
        }
    };
    let aurora_field = of(&server.artist.aurora_field).await;
    let ids: Vec<&String> = aurora_field.iter().map(|album| &album.id).collect();
    // 2019, then 2021. The compilation that holds a track of the artist is
    // an album of "Various Artists".
    assert_eq!(ids, [&server.album.glass_harbour, &server.album.paper_maps]);
    let various = of(&server.artist.various_artists).await;
    assert_eq!(various.len(), 1);
    assert_eq!(various[0].id, server.album.summer_sampler);
    assert!(various[0].compilation);
    assert!(!aurora_field[0].compilation);
    // An ID of another kind is the ID of no artist.
    assert_eq!(of(&server.album.glass_harbour).await, []);
}

#[tokio::test]
async fn tracks_of_album_orders_by_disc_then_track() {
    let server = TestServer::standard().await;
    // The file names start with `01`, `02` and `01`. The first two tracks
    // are on disc 1 and the third is on disc 2.
    let paper_maps = songs_of(&server, &server.album.paper_maps).await;
    assert_eq!(titles(&paper_maps), ["Meridian", "Contour", "Legend"]);
    let numbers: Vec<_> = paper_maps
        .iter()
        .map(|song| (song.track.disc_no, song.track.track_no))
        .collect();
    assert_eq!(
        numbers,
        [(Some(1), Some(1)), (Some(1), Some(2)), (Some(2), Some(1))]
    );

    let glass_harbour = songs_of(&server, &server.album.glass_harbour).await;
    assert_eq!(
        titles(&glass_harbour),
        ["Low Tide", "Breakwater", "Night Ferry"]
    );
    let summer_sampler = songs_of(&server, &server.album.summer_sampler).await;
    assert_eq!(titles(&summer_sampler), ["Open Road", "Last Train"]);
    assert_eq!(songs_of(&server, &server.track.low_tide).await, []);
}

#[tokio::test]
async fn genres_count_the_songs_and_the_albums() {
    let server = TestServer::standard().await;
    let genres = server.db().read(browse::genres).await.unwrap();
    let genre = |name: &str, song_count, album_count| Genre {
        name: name.to_owned(),
        song_count,
        album_count,
    };
    assert_eq!(
        genres,
        [
            genre("Electronic", 6, 2),
            genre("Jazz", 2, 1),
            genre("Pop", 2, 1)
        ]
    );
    let elements: Vec<model::Genre> = genres.iter().map(model::genre).collect();
    assert_eq!(
        to_json(&elements),
        json!([
            { "value": "Electronic", "songCount": 6, "albumCount": 2 },
            { "value": "Jazz", "songCount": 2, "albumCount": 1 },
            { "value": "Pop", "songCount": 2, "albumCount": 1 },
        ])
    );
}

#[tokio::test]
async fn folders_returns_the_music_folder_of_the_library() {
    let server = TestServer::standard().await;
    let folders = server.db().read(browse::folders).await.unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].id, server.folder_id);
    assert_eq!(folders[0].path, server.root());
    let name = server.root().file_name().unwrap().to_str().unwrap();
    assert_eq!(
        to_json(model::music_folder(&folders[0])),
        json!({ "id": server.folder_id, "name": name })
    );
}

#[tokio::test]
async fn rows_by_ids_are_in_the_order_of_the_ids() {
    let server = TestServer::standard().await;
    let track_ids = vec![
        server.track.last_train.clone(),
        server.track.low_tide.clone(),
        server.album.paper_maps.clone(),
        server.track.last_train.clone(),
    ];
    let album_ids = vec![
        server.album.summer_sampler.clone(),
        server.album.glass_harbour.clone(),
    ];
    let artist_ids = vec![
        server.artist.various_artists.clone(),
        server.artist.aurora_field.clone(),
    ];
    let (songs, albums, artists) = server
        .db()
        .read(move |conn| {
            Ok((
                browse::tracks_by_ids(conn, &track_ids)?,
                browse::albums_by_ids(conn, &album_ids)?,
                browse::artists_by_ids(conn, &artist_ids)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(titles(&songs), ["Last Train", "Low Tide", "Last Train"]);
    let names: Vec<&str> = albums.iter().map(|album| album.name.as_str()).collect();
    assert_eq!(names, ["Summer Sampler", "Glass Harbour"]);
    let names: Vec<&str> = artists.iter().map(|artist| artist.name.as_str()).collect();
    assert_eq!(names, ["Various Artists", "Aurora Field"]);
}

#[tokio::test]
async fn file_path_is_the_path_of_the_file_on_disk() {
    let server = TestServer::standard().await;
    let track = song(&server, &server.track.upstream).await.track;
    let path = server
        .db()
        .read(move |conn| browse::file_path(conn, &track))
        .await
        .unwrap()
        .expect("the track has a file");
    assert_eq!(path, server.path(UPSTREAM));
    assert!(path.is_file());
}

#[tokio::test]
async fn get_many_is_empty_on_a_new_database_and_returns_an_inserted_annotation() {
    let server = TestServer::standard().await;
    let low_tide = server.track.low_tide.as_str();
    let glass_harbour = server.album.glass_harbour.as_str();
    let aurora_field = server.artist.aurora_field.as_str();
    let ids = [low_tide, glass_harbour, aurora_field];
    assert_eq!(annotations_of(&server, &ids).await, HashMap::new());

    let user_id = owner_id(&server).await;
    let rows = [
        (
            low_tide.to_owned(),
            "track",
            Some(1_700_000_000_i64),
            Some(5),
            2,
            Some(1_700_000_500_i64),
        ),
        (glass_harbour.to_owned(), "album", None, Some(3), 0, None),
        (
            aurora_field.to_owned(),
            "artist",
            Some(1_700_000_000),
            None,
            0,
            None,
        ),
    ];
    server
        .db()
        .write(move |conn| {
            for (item_id, item_type, starred_at, rating, play_count, last_played_at) in rows {
                conn.execute(
                    "INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating,
                       play_count, last_played_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        user_id,
                        item_id,
                        item_type,
                        starred_at,
                        rating,
                        play_count,
                        last_played_at
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let found = annotations_of(&server, &ids).await;
    assert_eq!(found.len(), 3);
    assert_eq!(
        found[low_tide],
        Annotation {
            starred_at: Some(1_700_000_000),
            rating: Some(5),
            play_count: 2,
            last_played_at: Some(1_700_000_500),
        }
    );

    // The constructors show the annotation of each item.
    let child = model::child(&song(&server, low_tide).await, found.get(low_tide));
    assert_eq!(child.starred.as_deref(), Some("2023-11-14T22:13:20Z"));
    assert_eq!(child.user_rating, Some(5));
    assert_eq!(child.play_count, 2);
    assert_eq!(child.played.as_deref(), Some("2023-11-14T22:21:40Z"));
    // The album has the rating of its row and the plays of its track.
    let album_row = album(&server, glass_harbour).await;
    let album = model::album(&album_row, found.get(glass_harbour));
    assert_eq!(
        (album.starred, album.user_rating, album.play_count),
        (None, Some(3), 2)
    );
    assert_eq!(album.played.as_deref(), Some("2023-11-14T22:21:40Z"));
    // The album as a directory reports the same plays.
    let directory = model::child_dir_for_album(&album_row, found.get(glass_harbour));
    assert_eq!(directory.play_count, 2);
    assert_eq!(directory.played, album.played);
    let artist = model::artist(
        &artist(&server, aurora_field).await,
        found.get(aurora_field),
    );
    assert_eq!(artist.starred.as_deref(), Some("2023-11-14T22:13:20Z"));

    // A track that the user did not annotate has no entry.
    let other = server.track.breakwater.as_str();
    assert_eq!(annotations_of(&server, &[other]).await, HashMap::new());
    let child = model::child(&song(&server, other).await, None);
    assert_eq!(
        (child.starred, child.user_rating, child.play_count),
        (None, None, 0)
    );
}

/// Gives the track `id` its placeholder path, as the scanner does for a
/// track that it keeps without a file.
async fn remove_file(server: &ScannedServer, id: &str) {
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
        .unwrap();
}

#[tokio::test]
async fn a_track_without_a_file_keeps_its_row_and_has_no_path() {
    let server = TestServer::standard().await;
    let upstream = server.track.upstream.as_str();
    let before = model::child(&song(&server, upstream).await, None);
    remove_file(&server, upstream).await;

    let row = song(&server, upstream).await;
    assert!(!row.track.has_file());
    let child = model::child(&row, None);
    assert_eq!(child.path, None);
    assert_eq!(
        child,
        model::Child {
            path: None,
            ..before
        }
    );
    // The cover is the cover of the album, which the scan stored.
    assert_eq!(child.cover_art.as_ref(), Some(&server.album.slow_rivers));
    let document = serde_json::to_string(&child).unwrap();
    assert!(!document.contains("\"path\""), "{document}");
    assert!(!document.contains(&format!("/{upstream}")), "{document}");

    // The album lists the track, and its count agrees.
    let slow_rivers = songs_of(&server, &server.album.slow_rivers).await;
    assert_eq!(titles(&slow_rivers), ["Upstream", "Delta"]);
    assert_eq!(
        album(&server, &server.album.slow_rivers).await.song_count,
        2
    );
    // The track has no path on disk.
    let track = row.track.clone();
    let path = server
        .db()
        .read(move |conn| browse::file_path(conn, &track))
        .await
        .unwrap();
    assert_eq!(path, None);
}

#[tokio::test]
async fn an_artist_whose_tracks_have_no_file_is_in_no_folder() {
    let server = TestServer::standard().await;
    remove_file(&server, &server.track.upstream).await;
    let folder_id = server.folder_id;
    let in_folder = |server: &StandardServer| {
        let db = server.db().clone();
        async move {
            let artists = db
                .read(move |conn| browse::list_artists(conn, Some(folder_id)))
                .await
                .unwrap();
            artists
                .into_iter()
                .map(|artist| artist.name)
                .collect::<Vec<_>>()
        }
    };
    // One track of the artist still has a file.
    assert_eq!(
        in_folder(&server).await,
        ["Aurora Field", "Marlow Quartet", "Various Artists"]
    );
    remove_file(&server, &server.track.delta).await;
    assert_eq!(
        in_folder(&server).await,
        ["Aurora Field", "Various Artists"]
    );
    // The artist row stays, so the list without a folder holds it.
    let all = server
        .db()
        .read(|conn| browse::list_artists(conn, None))
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
}
