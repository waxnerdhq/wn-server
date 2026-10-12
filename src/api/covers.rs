//! The cover method `getCoverArt` (`docs/design.md` sections 3.11 and 4).
//!
//! The method answers with the bytes of an image and not with the envelope.
//! An error is an envelope, as for every other method.
//!
//! # Values from the client
//!
//! `id` and `size` come from the client. The handler checks their form and
//! passes them to [`Covers::get`]. It builds no path and opens no file. The
//! service resolves the ID through the database, and it owns the steps of
//! the size, the limits on the bytes of a source and on the memory of a
//! decoded image, the rule that no symbolic link is followed to an image,
//! and the rule that no source outside the music folders is read.
//!
//! - `id` is required: an absent one is error 10. A value that
//!   [`ids::parse`] rejects is error 70, and it reaches neither the service
//!   nor the header of a response.
//! - `size` is an integer from 0 to 4294967295. An empty value counts as
//!   absent. Any other value is error 0 that names the parameter: a negative
//!   number, a number with a fraction, a larger number and text.
//! - Error 70 answers every request for which the service has no cover: an
//!   ID that no row has, the ID of an artist or of a playlist, an album
//!   without a cover, a track without an embedded picture or without a file,
//!   and a source that the service does not serve. The server has no
//!   placeholder image.
//!
//! # Caching
//!
//! A response with a cover carries `Cache-Control: public, max-age=86400`
//! and an `ETag`. A request whose `If-None-Match` names that entity tag gets
//! HTTP 304 without a body. The handler reads the cover before it compares
//! the header, because the entity tag holds the modification time that the
//! service returns with the cover. A cover that is gone is therefore error
//! 70 for a conditional request as well.
//!
//! # Requests that end early
//!
//! The service decodes on blocking threads and holds one source image at a
//! time. The handler awaits the service inside the future of the request and
//! starts no task. When the client ends the request, the future is dropped:
//! a request that waits for the service leaves the queue, and a cover that
//! waits for its turn is not made.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;

use super::AppState;
use super::envelope::ApiError;
use super::params::Params;
use crate::auth::extract::Authed;
use crate::ids;
use crate::library::covers::{self, Cover, CoverError, Covers};

/// The value of `Cache-Control` of a response with a cover: every cache may
/// store the response and reuse it for one day, in seconds, without a
/// request.
const CACHE_CONTROL: &str = "public, max-age=86400";

/// `getCoverArt`: the cover with the ID `id`, which is the ID of an album or
/// of a track, as the `coverArt` of an album or of a song names it.
///
/// Without `size`, and with `size=0`, the body is the image as its source
/// stores it, with the content type of its format: `image/jpeg`, `image/png`
/// or `image/webp`. With another `size` the body is a JPEG at the step of
/// the size: the smallest of 80, 160, 320, 640, 1280 and 2000 pixels that is
/// not smaller than `size`, and 2000 pixels for a larger `size`. Its longer
/// side is at most the step, and it is not enlarged. The JPEG has the
/// orientation applied that the Exif metadata of a JPEG source declares; the
/// image as its source stores it keeps its bytes. The content type is the
/// one that the service reports.
///
/// The response has `Content-Length`, `Cache-Control` and `ETag`. A request
/// whose `If-None-Match` names the entity tag gets HTTP 304 without a body.
///
/// The request is a GET, a HEAD or a POST with a form body. The method of
/// the request changes nothing in the answer: the router removes the body of
/// the answer to a HEAD request. `Range` and `If-Modified-Since` are not
/// read.
///
/// # Errors
///
/// Error 10 answers a request without `id`, error 0 a `size` that is no
/// integer from 0 to 4294967295, and error 70 an `id` for which the server
/// has no cover (see the module comment). The checks run in that order, and
/// a request gets the error of the first one that it fails.
pub async fn get_cover_art(
    State(state): State<AppState>,
    _user: Authed,
    params: Params,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    cover_response(&state.covers, &params, &headers).await
}

/// Answers a `getCoverArt` request with the parameters `params` and the
/// request headers `headers` from `covers`.
///
/// The future awaits [`Covers::get`] itself. Dropping it ends the wait of
/// the request for the service.
async fn cover_response(
    covers: &Covers,
    params: &Params,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let id = params.req_str("id")?;
    let size = params.opt_u32("size")?;
    // The service rejects such a value as well. The check is here so that
    // only a value of the form of an ID is part of an entity tag and of a
    // log line.
    if ids::parse(id).is_none() {
        return Err(ApiError::NotFound);
    }
    let cover = covers
        .get(id, size)
        .await
        .map_err(|error| unavailable(id, &error))?
        .ok_or(ApiError::NotFound)?;

    let tag = entity_tag(id, size, cover.mtime);
    if names_tag(headers, &tag) {
        Ok(not_modified(&tag))
    } else {
        Ok(image(cover, &tag))
    }
}

/// Logs why the service returned no result for the cover `id` and returns
/// the error of the request, which has code 0.
fn unavailable(id: &str, error: &CoverError) -> ApiError {
    match error {
        CoverError::Db(error) => {
            tracing::error!(cover = id, %error, "cannot read the library");
            ApiError::Other("the server cannot read its library".to_owned())
        }
        CoverError::Unfinished => {
            tracing::warn!(cover = id, "the work on the cover ended without a result");
            ApiError::Other("the server did not finish the cover".to_owned())
        }
    }
}

/// Returns the entity tag of the cover `id` for a request with `size`,
/// without the quotes: `<id>-0-<mtime>` for the image as its source stores
/// it, which a request without `size` and with `size=0` gets, and
/// `<id>-<step>-<mtime>-<version>` for a resized cover.
///
/// `<step>` is the step of the size ([`covers::served_size`]). Two requests
/// that get the same bytes have the same tag, so every size of one step has
/// one tag. `mtime` is the modification time that the last scan stored for
/// the source of the cover. `<version>` is [`covers::CACHE_VERSION`]. The
/// four parts of the tag of a resized cover are the ones in the name of its
/// cache file. The tag of a resized cover that an older version of the
/// server made is therefore not the tag of the cover now, and a client that
/// holds such a cover gets the new one and not HTTP 304.
///
/// `id` has the form that [`ids::parse`] accepts.
fn entity_tag(id: &str, size: Option<u32>, mtime: i64) -> String {
    match covers::served_size(size) {
        None => format!("{id}-0-{mtime}"),
        Some(step) => format!("{id}-{step}-{mtime}-{}", covers::CACHE_VERSION),
    }
}

/// Returns whether an `If-None-Match` header of `headers` names the entity
/// tag `tag` of a cover that the server has.
fn names_tag(headers: &HeaderMap, tag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        // A value with a byte outside visible ASCII names no tag of the
        // server.
        .filter_map(|value| value.to_str().ok())
        .any(|value| lists_tag(value, tag))
}

/// Returns whether `value`, the value of one `If-None-Match` header, names
/// the entity tag `tag`. `tag` has no quotes.
///
/// The value is `*`, which names every tag, or a list of entity tags with
/// commas between them. An entity tag is text in double quotes, with `W/` in
/// front of it when it is weak. The comparison is the weak one of RFC 9110
/// section 13.1.2: `W/` is not part of what is compared. The list ends at
/// the first item that is no entity tag, and the items after it name
/// nothing.
fn lists_tag(value: &str, tag: &str) -> bool {
    if value.trim() == "*" {
        return true;
    }
    let mut rest = value;
    loop {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let quoted = rest.strip_prefix("W/").unwrap_or(rest);
        let item = quoted
            .strip_prefix('"')
            .and_then(|text| text.split_once('"'));
        let Some((listed, after)) = item else {
            return false;
        };
        if listed == tag {
            return true;
        }
        rest = after;
    }
}

/// The response that carries `cover` with the entity tag `tag`.
fn image(cover: Cover, tag: &str) -> Response {
    let length = cover.bytes.len();
    let mut response = Response::new(Body::from(cover.bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(cover.content_type),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    insert_cache_headers(headers, tag);
    response
}

/// The response to a conditional request that names the entity tag `tag`:
/// HTTP 304 without a body, with the headers that tell a cache how long the
/// cover that it holds stays fresh.
fn not_modified(tag: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    insert_cache_headers(response.headers_mut(), tag);
    response
}

/// Sets `Cache-Control` and `ETag` in `headers`. `tag` has no quotes.
fn insert_cache_headers(headers: &mut HeaderMap, tag: &str) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    let quoted = HeaderValue::from_str(&format!("\"{tag}\""))
        .expect("an ID, a size and a time are visible ASCII");
    headers.insert(header::ETAG, quoted);
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::Poll;

    use axum::body;
    use image::{ImageFormat, Rgb, RgbImage};
    use tempfile::TempDir;

    use super::*;
    use crate::db::queries::{albums, artists, folders};
    use crate::db::{Db, DbError};

    const ALBUM: &str = "al-0123456789abcdef0123456789abcdef";

    /// The modification time of the cover that the tests store.
    const MTIME: i64 = 1_700_000_000;

    /// The sides of the cover that the tests store, in pixels.
    const SIDE: u32 = 8;

    /// A library in a temporary directory: a memory database, the cover
    /// service, and one album whose cover is the image file `cover.png` of
    /// [`SIDE`] × [`SIDE`] pixels with the time [`MTIME`]. The directory
    /// `music` of the temporary directory holds the file and is the one
    /// music folder. The cache directory, `cache/covers`, does not exist
    /// until the service writes to it.
    struct Library {
        dir: TempDir,
        db: Db,
        covers: Covers,
        album_id: String,
        /// The ID of the artist of the album.
        artist_id: String,
        /// The bytes of `cover.png`.
        image: Vec<u8>,
    }

    impl Library {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let music = dir.path().join("music");
            fs::create_dir(&music).expect("create the music folder");
            let cover = music.join("cover.png");
            RgbImage::from_fn(SIDE, SIDE, |x, y| Rgb([x as u8 * 16, y as u8 * 16, 128]))
                .save_with_format(&cover, ImageFormat::Png)
                .expect("write the cover file");
            let image = fs::read(&cover).expect("read the cover file");

            let db = Db::open_memory().expect("a memory database");
            let folder = music.clone();
            let artist_id = db
                .write(move |conn| {
                    folders::sync_folders(conn, &[folder])?;
                    artists::upsert_by_key(conn, "Aurora Field")
                })
                .await
                .expect("insert the folder and the artist");
            let cache_dir = dir.path().join("cache").join("covers");
            let covers = Covers::new(cache_dir, db.clone(), vec![music]);
            let mut library = Self {
                dir,
                db,
                covers,
                album_id: String::new(),
                artist_id,
                image,
            };
            library.album_id = library.album("Glass Harbour").await;
            library
        }

        /// Inserts the album `title` with `cover.png` as its cover and
        /// returns its ID. A title that an album has returns that album.
        async fn album(&self, title: &str) -> String {
            let cover = self.dir.path().join("music").join("cover.png");
            let source = cover.to_str().expect("a Unicode path").to_owned();
            let (artist, title) = (self.artist_id.clone(), title.to_owned());
            self.db
                .write(move |conn| {
                    let album =
                        albums::upsert_by_key(conn, &artist, &title, "Aurora Field", false)?;
                    albums::set_cover(conn, &album, Some(&source), Some(MTIME))?;
                    Ok(album)
                })
                .await
                .expect("insert the album")
        }

        fn cache_dir(&self) -> PathBuf {
            self.dir.path().join("cache").join("covers")
        }

        /// The path of the cache file of the cover of the album at the step
        /// `size`.
        fn cache_file(&self, size: u32) -> PathBuf {
            self.cache_file_of(&self.album_id, size)
        }

        /// The path of the cache file of the cover `id` at the step `size`.
        fn cache_file_of(&self, id: &str, size: u32) -> PathBuf {
            self.cache_dir().join(format!("{id}-{size}-{MTIME}-2.jpg"))
        }

        /// The names of the entries of the directory `path`, in order.
        fn names_in(path: &std::path::Path) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(path)
                .expect("read the directory")
                .map(|entry| entry.expect("a directory entry").file_name())
                .map(|name| name.into_string().expect("a Unicode name"))
                .collect();
            names.sort();
            names
        }

        /// The names of the entries of the temporary directory, which holds
        /// the music folder and, when the service wrote a cover, `cache`.
        fn entries(&self) -> Vec<String> {
            Self::names_in(self.dir.path())
        }

        /// The parameters of a request for the cover `id` at `size`.
        fn query(id: &str, size: u32) -> Params {
            Params::from_query(&format!("id={id}&size={size}"))
        }

        /// Answers a request with the query string `query` and no header.
        async fn call(&self, query: &str) -> Result<Response, ApiError> {
            let params = Params::from_query(query);
            cover_response(&self.covers, &params, &HeaderMap::new()).await
        }
    }

    /// The headers of a request with one `If-None-Match` header for each
    /// item of `values`.
    fn if_none_match(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(
                header::IF_NONE_MATCH,
                HeaderValue::from_str(value).expect("a header value"),
            );
        }
        headers
    }

    /// The value of the header `name` of `response`.
    fn header_of(response: &Response, name: header::HeaderName) -> Option<&str> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("an ASCII header value"))
    }

    async fn body_of(response: Response) -> Vec<u8> {
        body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body")
            .to_vec()
    }

    /// Polls `request` until it has returned `Pending` `waits` times, and
    /// returns `None` then: the request is at that point of waiting, and the
    /// caller ends it by dropping it. Returns the output of a request that
    /// finished before that.
    ///
    /// Between two polls the function waits until the request is woken, so
    /// every poll finds the result of what the request waited for.
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

    #[test]
    fn entity_tag_holds_the_id_the_served_size_the_time_and_the_version() {
        // The image as it is stored has the size 0 and no version.
        assert_eq!(
            entity_tag(ALBUM, None, MTIME),
            "al-0123456789abcdef0123456789abcdef-0-1700000000"
        );
        assert_eq!(
            entity_tag(ALBUM, Some(0), MTIME),
            entity_tag(ALBUM, None, MTIME)
        );
        // A size has the tag of its step, so the sizes of one step have one
        // tag. The tag ends with the version of the resized covers.
        assert_eq!(covers::CACHE_VERSION, 2);
        let step_320 = "al-0123456789abcdef0123456789abcdef-320-1700000000-2";
        for size in [161, 290, 300, 310, 320] {
            assert_eq!(entity_tag(ALBUM, Some(size), MTIME), step_320, "{size}");
        }
        assert_ne!(entity_tag(ALBUM, Some(160), MTIME), step_320);
        assert_ne!(entity_tag(ALBUM, Some(321), MTIME), step_320);
        for step in covers::SIZE_STEPS {
            assert_eq!(
                entity_tag(ALBUM, Some(step), MTIME),
                format!("{ALBUM}-{step}-{MTIME}-2")
            );
        }
        // A size over the largest step counts as that step.
        let largest = "al-0123456789abcdef0123456789abcdef-2000-1700000000-2";
        for size in [1281, 1999, 2000, 2001, 5000, u32::MAX] {
            assert_eq!(entity_tag(ALBUM, Some(size), MTIME), largest, "{size}");
        }
        assert_ne!(entity_tag(ALBUM, Some(1280), MTIME), largest);
        // A file can have a time before 1970.
        assert_eq!(
            entity_tag(ALBUM, Some(1), -5),
            "al-0123456789abcdef0123456789abcdef-80--5-2"
        );
        // The tag of a resized cover of the version before, which had no
        // version, is not a tag of this version.
        let before = format!("{ALBUM}-320-{MTIME}");
        assert!(!lists_tag(
            &format!("\"{before}\""),
            &entity_tag(ALBUM, Some(320), MTIME)
        ));
    }

    #[test]
    fn if_none_match_names_a_tag_in_a_list_in_the_weak_form_or_with_a_star() {
        let named = [
            r#""a-1-2""#,
            r#"W/"a-1-2""#,
            r#"  "a-1-2"  "#,
            r#""x", "a-1-2""#,
            r#""x","a-1-2","y""#,
            r#"W/"x" , W/"a-1-2""#,
            "\t\"x\",\t\"a-1-2\"",
            // A comma is a character of an entity tag.
            r#""x,y", "a-1-2""#,
            "*",
            " * ",
        ];
        for value in named {
            assert!(lists_tag(value, "a-1-2"), "{value:?}");
        }

        let not_named = [
            "",
            " ",
            r#""""#,
            r#""a-1-3""#,
            r#""A-1-2""#,
            r#""a-1-2 ""#,
            r#""x", "y""#,
            // The tag without quotes is no entity tag.
            "a-1-2",
            r#"w/"a-1-2""#,
            r#""a-1-2"#,
            r#"a-1-2""#,
            // The list ends at the item that is no entity tag.
            r#"x, "a-1-2""#,
            r#"*, "x""#,
            "**",
            // The tag is not found inside another tag.
            r#""x a-1-2 y""#,
        ];
        for value in not_named {
            assert!(!lists_tag(value, "a-1-2"), "{value:?}");
        }
    }

    #[test]
    fn if_none_match_is_read_from_every_header_of_that_name() {
        assert!(!names_tag(&HeaderMap::new(), "a-1-2"));
        assert!(names_tag(&if_none_match(&[r#""a-1-2""#]), "a-1-2"));
        assert!(names_tag(
            &if_none_match(&[r#""x""#, r#""a-1-2""#]),
            "a-1-2"
        ));
        assert!(!names_tag(&if_none_match(&[r#""x""#, r#""y""#]), "a-1-2"));

        // A header with a byte outside ASCII names no tag, and the header
        // after it is read.
        let mut headers = HeaderMap::new();
        let opaque = HeaderValue::from_bytes(b"\"a-1-2\xff\"").expect("a header value");
        headers.append(header::IF_NONE_MATCH, opaque);
        assert!(!names_tag(&headers, "a-1-2"));
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"a-1-2\""));
        assert!(names_tag(&headers, "a-1-2"));

        // Another header of the request names no tag.
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"a-1-2\""));
        headers.insert(header::IF_RANGE, HeaderValue::from_static("\"a-1-2\""));
        assert!(!names_tag(&headers, "a-1-2"));
    }

    #[tokio::test]
    async fn cover_has_the_type_the_length_and_the_cache_headers() {
        let library = Library::new().await;
        let id = library.album_id.as_str();

        let response = library.call(&format!("id={id}")).await.expect("a cover");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            Some("image/png")
        );
        let length = library.image.len().to_string();
        assert_eq!(
            header_of(&response, header::CONTENT_LENGTH),
            Some(length.as_str())
        );
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            Some("public, max-age=86400")
        );
        let tag = format!("\"{id}-0-{MTIME}\"");
        assert_eq!(header_of(&response, header::ETAG), Some(tag.as_str()));
        assert_eq!(body_of(response).await, library.image);

        // A resized cover is a JPEG, and it is the file of the cache. Its
        // tag and its file have the step of the size and the version of the
        // resized covers.
        let response = library
            .call(&format!("id={id}&size=64"))
            .await
            .expect("a resized cover");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            Some("image/jpeg")
        );
        let tag = format!("\"{id}-80-{MTIME}-2\"");
        assert_eq!(header_of(&response, header::ETAG), Some(tag.as_str()));
        let cached = fs::read(library.cache_file(80)).expect("read the cache file");
        assert_eq!(
            header_of(&response, header::CONTENT_LENGTH),
            Some(cached.len().to_string().as_str())
        );
        assert_eq!(body_of(response).await, cached);
    }

    #[tokio::test]
    async fn if_none_match_with_the_tag_of_the_cover_is_http_304_without_a_body() {
        let library = Library::new().await;
        let params = Params::from_query(&format!("id={}", library.album_id));
        let tag = format!("\"{}-0-{MTIME}\"", library.album_id);

        let weak = format!("W/{tag}");
        let listed = format!("\"x\", {tag}");
        for value in [tag.as_str(), weak.as_str(), listed.as_str(), "*"] {
            let headers = if_none_match(&[value]);
            let response = cover_response(&library.covers, &params, &headers)
                .await
                .expect("an answer");
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
            assert_eq!(header_of(&response, header::ETAG), Some(tag.as_str()));
            assert_eq!(
                header_of(&response, header::CACHE_CONTROL),
                Some("public, max-age=86400")
            );
            // The answer describes no body.
            assert_eq!(header_of(&response, header::CONTENT_TYPE), None);
            assert_eq!(header_of(&response, header::CONTENT_LENGTH), None);
            assert!(body_of(response).await.is_empty(), "{value}");
        }

        // The tag of another size, of another time and of another cover is
        // not the tag of this cover.
        let other_size = format!("\"{}-80-{MTIME}\"", library.album_id);
        let other_time = format!("\"{}-0-{}\"", library.album_id, MTIME + 1);
        let other_cover = format!("\"{ALBUM}-0-{MTIME}\"");
        for value in [other_size, other_time, other_cover] {
            let headers = if_none_match(&[&value]);
            let response = cover_response(&library.covers, &params, &headers)
                .await
                .expect("a cover");
            assert_eq!(response.status(), StatusCode::OK, "{value}");
            assert_eq!(body_of(response).await, library.image, "{value}");
        }
    }

    #[tokio::test]
    async fn value_that_is_no_id_is_error_70_and_creates_no_file() {
        let library = Library::new().await;
        let upper = library.album_id.to_uppercase();
        let long = format!("{}0", library.album_id);
        let traversal = format!("../../music/cover.png%00{}", library.album_id);
        let values = [
            "",
            "1",
            "../x",
            "..%2Fx",
            "../music/cover",
            "%2E%2E%2Fmusic%2Fcover.png",
            "cover.png",
            "/etc/passwd",
            "al-",
            "al-..%2F..%2Fmusic%2Fcover",
            "%27%20OR%201%3D1%20--",
            upper.as_str(),
            long.as_str(),
            traversal.as_str(),
        ];
        for value in values {
            for size in ["", "&size=0", "&size=64"] {
                let query = format!("id={value}{size}");
                let error = library.call(&query).await.expect_err("no cover");
                assert_eq!(error, ApiError::NotFound, "{query}");
            }
        }
        // The service made no cache directory, and no file is next to it.
        assert_eq!(library.entries(), ["music"]);
        assert!(!library.cache_dir().exists());

        // The cover of the album is served after that, and a resized cover
        // is the one file of the cache.
        let query = format!("id={}&size=64", library.album_id);
        library.call(&query).await.expect("a resized cover");
        assert_eq!(library.entries(), ["cache", "music"]);
        let cached: Vec<PathBuf> = fs::read_dir(library.cache_dir())
            .expect("read the cache directory")
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        assert_eq!(cached, [library.cache_file(80)]);
    }

    /// `id` and `size` are the two values of a request. The test sends
    /// values that would name a file outside the cache directory if they
    /// were part of a path, and checks which files exist afterwards.
    #[tokio::test]
    async fn hostile_id_or_size_makes_no_file_outside_the_cache_directory() {
        let library = Library::new().await;
        let id = library.album_id.as_str();
        // A file next to the cache and the music folder, which a path built
        // from `..` would reach.
        let secret = library.dir.path().join("secret.jpg");
        fs::write(&secret, b"secret").expect("write the file");

        // Sizes that are no number are refused before the cover is read.
        let invalid =
            ApiError::Other("parameter size must be an integer from 0 to 4294967295".to_owned());
        for size in [
            "..%2F..%2Fsecret",
            "..%2F..%2Fmusic%2Fcover.png",
            "64%2F..%2F..%2Fsecret",
            "%2Fetc%2Fpasswd",
            "..%5C..%5Csecret",
            "64%00",
            "64.jpg",
            "tmp.0",
            "-64",
        ] {
            let query = format!("id={id}&size={size}");
            let error = library.call(&query).await.expect_err("no cover");
            assert_eq!(error, invalid, "{query}");
        }
        // IDs that are no ID are refused with every size.
        for value in [
            "..%2F..%2Fsecret",
            "al-..%2F..%2F..%2Fsecret",
            "%2Fetc%2Fpasswd",
            "..%5C..%5Csecret",
        ] {
            for size in ["1", "64", "2000", "4294967295"] {
                let query = format!("id={value}&size={size}");
                let error = library.call(&query).await.expect_err("no cover");
                assert_eq!(error, ApiError::NotFound, "{query}");
            }
        }
        assert_eq!(library.entries(), ["music", "secret.jpg"]);

        // Every number is a size. The cache gets one file for each step that
        // was asked for, in the cache directory and with the name of the
        // step.
        for size in ["1", "64", "80", "81", "2000", "2001", "4294967295"] {
            let query = format!("id={id}&size={size}");
            let response = library.call(&query).await.expect("a resized cover");
            assert_eq!(response.status(), StatusCode::OK, "{query}");
        }
        assert_eq!(library.entries(), ["cache", "music", "secret.jpg"]);
        assert_eq!(
            Library::names_in(&library.dir.path().join("cache")),
            ["covers"]
        );
        let mut cached: Vec<PathBuf> = fs::read_dir(library.cache_dir())
            .expect("read the cache directory")
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        cached.sort();
        let mut expected = [80, 160, 2000].map(|step| library.cache_file(step));
        expected.sort();
        assert_eq!(cached, expected);
        // The music folder holds the cover file as it was, and no other
        // file.
        let music = library.dir.path().join("music");
        assert_eq!(Library::names_in(&music), ["cover.png"]);
        assert_eq!(
            fs::read(music.join("cover.png")).expect("read the cover file"),
            library.image
        );
        assert_eq!(fs::read(&secret).expect("read the file"), b"secret");
    }

    #[tokio::test]
    async fn parameters_are_checked_before_the_cover_is_looked_up() {
        let library = Library::new().await;
        let id = library.album_id.as_str();
        let missing = ApiError::MissingParam("id".to_owned());
        let invalid =
            ApiError::Other("parameter size must be an integer from 0 to 4294967295".to_owned());

        for query in ["", "size=64", "ID=1", "size=abc"] {
            assert_eq!(library.call(query).await.expect_err("no id"), missing);
        }
        for size in [
            "-1",
            "abc",
            "1.5",
            "64px",
            "0x40",
            "4294967296",
            "+-1",
            "%20",
        ] {
            // The size is checked for an ID that has a cover and for a value
            // that is no ID.
            for id in [id, "../x"] {
                let query = format!("id={id}&size={size}");
                let error = library.call(&query).await.expect_err("no cover");
                assert_eq!(error, invalid, "{query}");
            }
        }
        // No request made a cover.
        assert_eq!(library.entries(), ["music"]);
    }

    #[tokio::test]
    async fn database_that_cannot_be_read_is_error_0() {
        let library = Library::new().await;
        library
            .db
            .write(|conn| Ok(conn.execute_batch("DROP TABLE albums")?))
            .await
            .expect("drop the table");
        let query = format!("id={}", library.album_id);
        let error = library.call(&query).await.expect_err("no cover");
        // The message holds nothing of the statement that failed.
        assert_eq!(
            error,
            ApiError::Other("the server cannot read its library".to_owned())
        );
    }

    #[test]
    fn error_of_the_service_is_error_0() {
        assert_eq!(
            unavailable(ALBUM, &CoverError::Db(DbError::Cancelled)),
            ApiError::Other("the server cannot read its library".to_owned())
        );
        assert_eq!(
            unavailable(ALBUM, &CoverError::Unfinished),
            ApiError::Other("the server did not finish the cover".to_owned())
        );
    }

    /// A client that disconnects drops the future of its request. The test
    /// drops a request at each point at which it waits, and checks that the
    /// cover of that request is not made afterwards.
    ///
    /// The request waits for the row of the cover, for the cache and for the
    /// task of the service that makes the cover. The runtime of the test has
    /// one thread, and the request is dropped in the poll in which it starts
    /// that task. The task therefore first runs after the request is
    /// dropped. It then finds that no request waits for its cover, and it
    /// decodes nothing.
    ///
    /// A handler that passed the call of the service to a task of its own
    /// would fail the test: that task would go on after the request is
    /// dropped, and the cover would be made.
    #[tokio::test]
    async fn request_that_ends_while_it_waits_leaves_no_cover_to_be_made() {
        let library = Library::new().await;
        let headers = HeaderMap::new();

        let mut ended = Vec::new();
        for waits in 1.. {
            // Every request asks for the cover of an album of its own, which
            // the cache does not hold. The sizes of a step share one cache
            // file, so a size of its own would not be a cover of its own.
            let album = library.album(&format!("Dropped {waits}")).await;
            let params = Library::query(&album, 64);
            let mut request = Box::pin(cover_response(&library.covers, &params, &headers));
            let finished = advance(request.as_mut(), waits).await;
            drop(request);

            // A request for the cover of another album is answered. Its
            // cover is made after everything that the dropped request left
            // behind.
            let other = library.album(&format!("Answered {waits}")).await;
            let control = Library::query(&other, 64);
            let response = cover_response(&library.covers, &control, &headers)
                .await
                .expect("a cover");
            assert_eq!(response.status(), StatusCode::OK, "after {waits} waits");
            assert!(library.cache_file_of(&other, 80).is_file());

            match finished {
                // The request was not dropped before its end, so its cover
                // is in the cache.
                Some(response) => {
                    let response = response.expect("a cover");
                    assert_eq!(response.status(), StatusCode::OK);
                    assert!(library.cache_file_of(&album, 80).is_file());
                    break;
                }
                None => ended.push(album),
            }
        }

        for album in &ended {
            assert!(
                !library.cache_file_of(album, 80).exists(),
                "the cover of the album {album} was made after its request ended"
            );
        }
        // The request waits at least once: for the task that makes its
        // cover. A read of the database or of the cache that ends before the
        // request is polled again is no point of waiting, so the number of
        // the points differs from run to run.
        assert!(!ended.is_empty(), "no request was dropped while it waited");
    }
}
