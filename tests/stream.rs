//! Router tests of `stream` (`docs/design.md` sections 3.9 and 4): the
//! original file of every format, ranges, `If-Range`, conditional requests,
//! HEAD and POST, the parameters that select a transcode, and the files that
//! the method does not serve.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::header::HeaderName;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use http_body_util::BodyExt;
use rusqlite::params;
use wn_server::api::params::Params;
use wn_server::config::Transcoding;
use wn_server::db::Db;
use wn_server::db::queries::tracks::{self, Track};
use wn_server::media::stream::{StreamPlan, StreamRequest, decide};

use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, ScannedServer, TestServer,
    body_bytes, body_string, content_type, subsonic_response, urlencode,
};

/// Parameters of a request, and request headers: names and values.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The value of `Access-Control-Expose-Headers` of the API route.
const EXPOSED: &str = "Content-Range, Accept-Ranges, Content-Length";

/// An ID of the form of a track ID that no track has.
const NO_TRACK: &str = "tr-00000000000000000000000000000000";

/// A request to `/rest/<name>` with the HTTP method `method`, with `params`
/// and the password of the owner as its parameters and with `headers`. The
/// parameters are the form body of a POST and the query string of every
/// other method.
fn request(method: Method, name: &str, params: Pairs, headers: Pairs) -> Request<Body> {
    let mut all = params.to_vec();
    all.extend([
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
    ]);
    let query = urlencode(&all);
    let mut request = if method == Method::POST {
        Request::post(format!("/rest/{name}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
    } else {
        Request::builder()
            .method(method.clone())
            .uri(format!("/rest/{name}?{query}"))
    };
    for (header, value) in headers {
        request = request.header(*header, *value);
    }
    let body = if method == Method::POST {
        Body::from(query)
    } else {
        Body::empty()
    };
    request.body(body).expect("a request")
}

/// Sends `GET /rest/stream` for the track `id` with `params` and `headers`.
async fn stream(server: &TestServer, id: &str, params: Pairs<'_>, headers: Pairs<'_>) -> Response {
    stream_as(server, Method::GET, id, params, headers).await
}

/// Sends a `stream` request for the track `id` with the HTTP method
/// `method`.
async fn stream_as(
    server: &TestServer,
    method: Method,
    id: &str,
    params: Pairs<'_>,
    headers: Pairs<'_>,
) -> Response {
    let mut all = vec![("id", id)];
    all.extend_from_slice(params);
    server
        .request(request(method, "stream", &all, headers))
        .await
}

/// Sends `GET /rest/stream` for the track `id` with `params`, `headers` and
/// `f=json`. Checks that the response is the envelope of an error and no
/// response of a file. Returns the code and the message.
async fn stream_error(
    server: &TestServer,
    id: &str,
    params: Pairs<'_>,
    headers: Pairs<'_>,
) -> (u64, String) {
    let mut all = params.to_vec();
    all.push(("f", "json"));
    error_of(stream(server, id, &all, headers).await).await
}

/// Checks that `response` is the JSON envelope of an error and carries no
/// header of a file. Returns the code and the message.
async fn error_of(response: Response) -> (u64, String) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    for name in [
        header::ACCEPT_RANGES,
        header::CONTENT_RANGE,
        header::CONTENT_DISPOSITION,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        assert_eq!(header_of(&response, &name), None, "{name}");
    }
    let envelope = subsonic_response(&body_string(response).await);
    assert_eq!(envelope["status"], "failed", "{envelope}");
    let code = envelope["error"]["code"].as_u64().expect("an error code");
    let message = envelope["error"]["message"]
        .as_str()
        .expect("an error message");
    (code, message.to_owned())
}

/// The value of the header `name` of `response`.
fn header_of<'a>(response: &'a Response, name: &HeaderName) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().expect("an ASCII header value"))
}

/// Checks that `response` is HTTP 200 with the whole of `file` as its body,
/// the content type `mime` and the headers of a file.
async fn assert_whole_file(response: Response, file: &[u8], mime: &str, case: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{case}");
    assert_eq!(content_type(&response), mime, "{case}");
    assert_eq!(
        header_of(&response, &header::CONTENT_LENGTH),
        Some(file.len().to_string().as_str()),
        "{case}"
    );
    assert_eq!(
        header_of(&response, &header::ACCEPT_RANGES),
        Some("bytes"),
        "{case}"
    );
    assert_eq!(header_of(&response, &header::CONTENT_RANGE), None, "{case}");
    assert_eq!(body_bytes(response).await, file, "{case}");
}

/// Reads the row of the track `id`.
async fn row(db: &Db, id: &str) -> Track {
    let id = id.to_owned();
    db.read(move |conn| tracks::get_track(conn, &id))
        .await
        .expect("read the track")
        .expect("the track exists")
}

/// Reads the row of the track at the relative path `rel_path`.
async fn row_at(db: &Db, rel_path: &str) -> Track {
    let rel_path = rel_path.to_owned();
    let id: String = db
        .read(move |conn| {
            let id = conn.query_row(
                "SELECT id FROM tracks WHERE rel_path = ?1",
                [rel_path],
                |row| row.get(0),
            )?;
            Ok(id)
        })
        .await
        .expect("a track at the path");
    row(db, &id).await
}

/// The row of the track `id` of `server` and the bytes of its file.
async fn row_and_file(server: &ScannedServer, id: &str) -> (Track, Vec<u8>) {
    let track = row(server.db(), id).await;
    let file = fs::read(server.path(&track.rel_path)).expect("read the file of the track");
    (track, file)
}

/// The time `seconds` Unix seconds.
fn unix_time(seconds: i64) -> SystemTime {
    let distance = Duration::from_secs(seconds.unsigned_abs());
    if seconds >= 0 {
        UNIX_EPOCH + distance
    } else {
        UNIX_EPOCH - distance
    }
}

/// Sets the modification time of the file at `path`.
fn set_modified(path: &Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(time))
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

/// Decodes the percent escapes of `text`.
fn percent_decode(text: &str) -> String {
    let mut bytes = Vec::new();
    let mut rest = text.as_bytes();
    while let [first, tail @ ..] = rest {
        if *first == b'%' {
            let hex = std::str::from_utf8(&tail[..2]).expect("two hex digits");
            bytes.push(u8::from_str_radix(hex, 16).expect("two hex digits"));
            rest = &tail[2..];
        } else {
            bytes.push(*first);
            rest = tail;
        }
    }
    String::from_utf8(bytes).expect("a UTF-8 name")
}

/// Splits a `Content-Disposition` value into the plain file name and the
/// decoded value of `filename*`.
fn disposition(value: &str) -> (&str, String) {
    let (plain, encoded) = value
        .strip_prefix("inline; filename=\"")
        .and_then(|rest| rest.split_once("\"; filename*=UTF-8''"))
        .unwrap_or_else(|| panic!("not the form of the header: {value}"));
    assert!(
        plain
            .chars()
            .all(|c| matches!(c, ' '..='~') && c != '"' && c != '\\'),
        "{value}"
    );
    assert!(
        encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~%".contains(c)),
        "{value}"
    );
    (plain, percent_decode(encoded))
}

/// Returns a WAV file with `data_len` bytes of samples: mono, 8 kHz, 16-bit
/// PCM. The samples are a fixed sequence in which no part repeats, so a
/// part of the file is found at one offset only.
fn wav(data_len: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((36 + data_len).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes()); // PCM
    bytes.extend(1u16.to_le_bytes()); // channels
    bytes.extend(8000u32.to_le_bytes()); // sample rate
    bytes.extend(16000u32.to_le_bytes()); // bytes per second
    bytes.extend(2u16.to_le_bytes()); // bytes per frame
    bytes.extend(16u16.to_le_bytes()); // bits per sample
    bytes.extend(b"data");
    bytes.extend(data_len.to_le_bytes());
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let end = bytes.len() + data_len as usize;
    while bytes.len() < end {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend(state.to_le_bytes());
    }
    bytes.truncate(end);
    bytes
}

/// The size of the samples of the file of [`wav_server`]: 1 MiB, which is
/// 16 chunks of the 64 KiB that the file service reads at a time.
const WAV_DATA_LEN: u32 = 1024 * 1024;

/// A server over a library of one WAV file of [`WAV_DATA_LEN`] bytes of
/// samples. Returns the server, the ID of the track and the path of the
/// file.
async fn wav_server() -> (ScannedServer, String, PathBuf) {
    let library = LibraryBuilder::new().build();
    let path = library.root.path().join("harbour.wav");
    fs::write(&path, wav(WAV_DATA_LEN)).expect("write the WAV file");
    let server = TestServer::scanned(library).await;
    let id = row_at(server.db(), "harbour.wav").await.id;
    (server, id, path)
}

#[tokio::test]
async fn stream_returns_the_original_file_of_every_format() {
    let server = TestServer::standard().await;
    let track = &server.track;
    // The content type is the one of the track row (`docs/design.md`
    // section 1.3). The FLAC file is lossless, and the response holds its
    // bytes unchanged, as it holds the bytes of the lossy files.
    for (id, suffix, mime) in [
        (&track.low_tide, "flac", "audio/flac"),
        (&track.meridian, "mp3", "audio/mpeg"),
        (&track.upstream, "m4a", "audio/mp4"),
        (&track.open_road, "ogg", "audio/ogg"),
        (&track.last_train, "opus", "audio/ogg"),
    ] {
        let (row, file) = row_and_file(&server, id).await;
        assert_eq!(row.suffix, suffix);
        assert_eq!(row.content_type, mime);
        assert_eq!(row.size, file.len() as u64);

        let response = stream(&server, id, &[], &[]).await;
        assert!(header_of(&response, &header::ETAG).is_some(), "{suffix}");
        assert!(
            header_of(&response, &header::LAST_MODIFIED).is_some(),
            "{suffix}"
        );
        assert_whole_file(response, &file, mime, suffix).await;
    }
}

#[tokio::test]
async fn stream_over_post_and_with_the_view_suffix_returns_the_same_file() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (_, file) = row_and_file(&server, id).await;

    // The helper of the test server sends a token in the form body.
    let response = server.post_form("stream", &[("id", id)]).await;
    assert_whole_file(response, &file, "audio/flac", "POST with a token").await;

    for method in [Method::GET, Method::POST] {
        for name in ["stream", "stream.view"] {
            let case = format!("{method} {name}");
            let response = server
                .request(request(method.clone(), name, &[("id", id)], &[]))
                .await;
            assert_whole_file(response, &file, "audio/flac", &case).await;

            // A range of a POST is the range of the file, as for a GET.
            let ranged = request(
                method.clone(),
                name,
                &[("id", id)],
                &[("range", "bytes=100-199")],
            );
            let response = server.request(ranged).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{case}");
            assert_eq!(body_bytes(response).await, &file[100..200], "{case}");
        }
    }
}

#[tokio::test]
async fn stream_response_carries_the_cors_headers_of_the_route() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let size = row(server.db(), id).await.size;
    // The whole file, a part, a range that no byte satisfies and an error.
    let unsatisfiable = format!("bytes={size}-");
    for (range, status) in [
        (None, StatusCode::OK),
        (Some("bytes=100-199"), StatusCode::PARTIAL_CONTENT),
        (
            Some(unsatisfiable.as_str()),
            StatusCode::RANGE_NOT_SATISFIABLE,
        ),
    ] {
        let headers: Vec<(&str, &str)> = range.map(|range| ("range", range)).into_iter().collect();
        let response = stream(&server, id, &[], &headers).await;
        assert_eq!(response.status(), status);
        assert_eq!(
            header_of(&response, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some("*"),
            "{status}"
        );
        assert_eq!(
            header_of(&response, &header::ACCESS_CONTROL_EXPOSE_HEADERS),
            Some(EXPOSED),
            "{status}"
        );
    }
    let response = stream(&server, NO_TRACK, &[], &[]).await;
    assert_eq!(
        header_of(&response, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some("*")
    );
}

#[tokio::test]
async fn range_request_returns_the_part_of_the_file() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (_, file) = row_and_file(&server, id).await;
    let size = file.len();
    assert!(size > 1000, "the fixture has {size} bytes");

    let past_the_end = format!("bytes=100-{}", size + 5000);
    let long_suffix = format!("bytes=-{}", size + 1);
    let whole_suffix = format!("bytes=-{size}");
    // The header, and the first and the last byte of the part.
    let cases = [
        ("bytes=100-199", 100, 199),
        ("bytes=0-0", 0, 0),
        ("bytes=0-", 0, size - 1),
        ("bytes=100-", 100, size - 1),
        ("bytes=-100", size - 100, size - 1),
        ("bytes=-1", size - 1, size - 1),
        // A range that ends after the file ends at its last byte.
        (past_the_end.as_str(), 100, size - 1),
        // A suffix of the length of the file, and a longer one, is the
        // whole file.
        (whole_suffix.as_str(), 0, size - 1),
        (long_suffix.as_str(), 0, size - 1),
    ];
    for (range, first, last) in cases {
        let response = stream(&server, id, &[], &[("range", range)]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(content_type(&response), "audio/flac", "{range}");
        assert_eq!(
            header_of(&response, &header::CONTENT_RANGE),
            Some(format!("bytes {first}-{last}/{size}").as_str()),
            "{range}"
        );
        assert_eq!(
            header_of(&response, &header::CONTENT_LENGTH),
            Some((last - first + 1).to_string().as_str()),
            "{range}"
        );
        assert_eq!(
            header_of(&response, &header::ACCEPT_RANGES),
            Some("bytes"),
            "{range}"
        );
        assert!(
            header_of(&response, &header::CONTENT_DISPOSITION).is_some(),
            "{range}"
        );
        assert_eq!(body_bytes(response).await, &file[first..=last], "{range}");
    }
}

#[tokio::test]
async fn range_that_no_byte_satisfies_is_http_416() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let size = row(server.db(), id).await.size;
    for range in [
        format!("bytes={size}-"),
        format!("bytes={}-{}", size + 10, size + 20),
        "bytes=-0".to_owned(),
    ] {
        let response = stream(&server, id, &[], &[("range", &range)]).await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{range}"
        );
        assert_eq!(
            header_of(&response, &header::CONTENT_RANGE),
            Some(format!("bytes */{size}").as_str()),
            "{range}"
        );
        assert_eq!(
            header_of(&response, &header::ACCEPT_RANGES),
            Some("bytes"),
            "{range}"
        );
        assert!(body_bytes(response).await.is_empty(), "{range}");
    }
}

#[tokio::test]
async fn request_for_several_ranges_gets_the_whole_file() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (_, file) = row_and_file(&server, id).await;
    for range in ["bytes=0-9,100-199", "bytes=0-9, -10"] {
        let response = stream(&server, id, &[], &[("range", range)]).await;
        assert_whole_file(response, &file, "audio/flac", range).await;
    }
}

#[tokio::test]
async fn if_range_selects_between_the_part_and_the_whole_file() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (_, file) = row_and_file(&server, id).await;
    let size = file.len();

    let first = stream(&server, id, &[], &[]).await;
    let tag = header_of(&first, &header::ETAG).expect("a tag").to_owned();
    let date = header_of(&first, &header::LAST_MODIFIED)
        .expect("a date")
        .to_owned();
    assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");
    let unsatisfiable = format!("bytes={size}-");

    // The tag or the date of the file on disk: the client holds a part of
    // this version, and it gets the part it asks for.
    for validator in [tag.as_str(), date.as_str()] {
        let headers = [("range", "bytes=100-199"), ("if-range", validator)];
        let response = stream(&server, id, &[], &headers).await;
        assert_eq!(
            response.status(),
            StatusCode::PARTIAL_CONTENT,
            "{validator}"
        );
        assert_eq!(body_bytes(response).await, &file[100..200], "{validator}");

        let headers = [("range", unsatisfiable.as_str()), ("if-range", validator)];
        let response = stream(&server, id, &[], &headers).await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{validator}"
        );
    }

    // The tag or the date of another version, a weak tag and a value that
    // is neither: the range is not read, and the response is the whole file.
    let weak = format!("W/{tag}");
    for validator in [
        "\"0.00000000-0\"",
        weak.as_str(),
        "Thu, 01 Jan 1970 00:00:00 GMT",
        "yesterday",
    ] {
        for range in ["bytes=100-199", "bytes=-100", unsatisfiable.as_str()] {
            let case = format!("{validator} {range}");
            let headers = [("range", range), ("if-range", validator)];
            let response = stream(&server, id, &[], &headers).await;
            assert_whole_file(response, &file, "audio/flac", &case).await;

            let post = stream_as(&server, Method::POST, id, &[], &headers).await;
            assert_whole_file(post, &file, "audio/flac", &case).await;
        }
    }

    // `If-Range` without `Range` selects nothing.
    let response = stream(&server, id, &[], &[("if-range", "\"0.00000000-0\"")]).await;
    assert_whole_file(response, &file, "audio/flac", "no range").await;
}

#[tokio::test]
async fn request_for_the_version_that_the_client_holds_is_http_304() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (_, file) = row_and_file(&server, id).await;

    let first = stream(&server, id, &[], &[]).await;
    let tag = header_of(&first, &header::ETAG).expect("a tag").to_owned();
    let date = header_of(&first, &header::LAST_MODIFIED)
        .expect("a date")
        .to_owned();

    for headers in [
        vec![("if-none-match", tag.as_str())],
        vec![("if-modified-since", date.as_str())],
        // The condition is read before the range.
        vec![("if-none-match", tag.as_str()), ("range", "bytes=100-199")],
    ] {
        let response = stream(&server, id, &[], &headers).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{headers:?}");
        assert_eq!(
            header_of(&response, &header::ETAG),
            Some(tag.as_str()),
            "{headers:?}"
        );
        assert!(body_bytes(response).await.is_empty(), "{headers:?}");
    }

    for headers in [
        [("if-none-match", "\"0.00000000-0\"")],
        [("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT")],
    ] {
        let response = stream(&server, id, &[], &headers).await;
        assert_whole_file(response, &file, "audio/flac", headers[0].0).await;
    }
}

#[tokio::test]
async fn head_request_returns_the_headers_of_the_file_and_no_body() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let (track, _) = row_and_file(&server, id).await;
    let get = stream(&server, id, &[], &[]).await;

    // A range is defined for GET, so a HEAD with a range describes the
    // whole file.
    for headers in [vec![], vec![("range", "bytes=100-199")]] {
        let response = stream_as(&server, Method::HEAD, id, &[], &headers).await;
        assert_eq!(response.status(), StatusCode::OK, "{headers:?}");
        for name in [
            header::CONTENT_TYPE,
            header::CONTENT_LENGTH,
            header::ACCEPT_RANGES,
            header::CONTENT_DISPOSITION,
            header::ETAG,
            header::LAST_MODIFIED,
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
        ] {
            assert_eq!(
                header_of(&response, &name),
                header_of(&get, &name),
                "{name} {headers:?}"
            );
        }
        assert_eq!(
            header_of(&response, &header::CONTENT_LENGTH),
            Some(track.size.to_string().as_str())
        );
        assert_eq!(header_of(&response, &header::CONTENT_RANGE), None);
        assert!(body_bytes(response).await.is_empty(), "{headers:?}");
    }
}

/// Sets `bitrate_kbps` of the track `id`.
async fn set_bitrate(db: &Db, id: &str, bitrate_kbps: u32) {
    let id = id.to_owned();
    db.write(move |conn| {
        conn.execute(
            "UPDATE tracks SET bitrate_kbps = ?1 WHERE id = ?2",
            params![bitrate_kbps, id],
        )?;
        Ok(())
    })
    .await
    .expect("set the bitrate");
}

#[tokio::test]
async fn request_that_selects_the_original_file_gets_it_whatever_the_other_parameters() {
    let server = TestServer::standard().await;
    let flac = server.track.low_tide.as_str();
    let (_, flac_file) = row_and_file(&server, flac).await;
    let cases: [Pairs; 8] = [
        &[("format", "raw")],
        &[("maxBitRate", "0")],
        &[("format", "raw"), ("maxBitRate", "128")],
        &[("format", ""), ("maxBitRate", "")],
        &[("timeOffset", "30")],
        &[("estimateContentLength", "true")],
        &[("format", "raw"), ("timeOffset", "30")],
        &[
            ("format", "raw"),
            ("maxBitRate", "64"),
            ("timeOffset", "30"),
            ("estimateContentLength", "true"),
        ],
    ];
    for params in cases {
        let case = format!("{params:?}");
        let response = stream(&server, flac, params, &[]).await;
        assert_whole_file(response, &flac_file, "audio/flac", &case).await;
        let response = stream_as(&server, Method::POST, flac, params, &[]).await;
        assert_whole_file(response, &flac_file, "audio/flac", &case).await;
    }

    // A lossy track whose bitrate does not exceed the limit.
    let mp3 = server.track.meridian.as_str();
    let (_, mp3_file) = row_and_file(&server, mp3).await;
    set_bitrate(server.db(), mp3, 32).await;
    for limit in ["320", "32", "8"] {
        let response = stream(&server, mp3, &[("maxBitRate", limit)], &[]).await;
        assert_whole_file(response, &mp3_file, "audio/mpeg", limit).await;
    }
    // A range of such a request is a range of the original file.
    let headers = [("range", "bytes=100-199")];
    let response = stream(&server, mp3, &[("maxBitRate", "320")], &headers).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(response).await, &mp3_file[100..200]);
}

/// The plan of `decide` for the track `id` and a request with `params`,
/// with the transcoding settings of a test server.
async fn plan(server: &TestServer, id: &str, params: Pairs<'_>) -> StreamPlan {
    let track = row(server.db(), id).await;
    let request = StreamRequest::from_params(&Params::from_query(&urlencode(params)))
        .expect("the parameters of a stream request");
    decide(&track, &request, &Transcoding::default()).expect("a plan")
}

#[tokio::test]
async fn request_that_selects_a_transcode_gets_the_original_file() {
    let server = TestServer::standard().await;
    let flac = server.track.low_tide.as_str();
    let mp3 = server.track.meridian.as_str();
    let (flac_row, flac_file) = row_and_file(&server, flac).await;
    let (mp3_row, mp3_file) = row_and_file(&server, mp3).await;
    // A limit of 1 kbps below the bitrate that the scan read from the file.
    let below = |row: &Track| (row.bitrate_kbps.expect("a bitrate") - 1).to_string();
    let (below_flac, below_mp3) = (below(&flac_row), below(&mp3_row));

    let cases: [(&str, &[u8], &str, Pairs); 9] = [
        (flac, &flac_file, "audio/flac", &[("format", "opus")]),
        (flac, &flac_file, "audio/flac", &[("format", "mp3")]),
        (
            flac,
            &flac_file,
            "audio/flac",
            &[("maxBitRate", &below_flac)],
        ),
        // A lossless file with a limit above its bitrate.
        (flac, &flac_file, "audio/flac", &[("maxBitRate", "320")]),
        (
            flac,
            &flac_file,
            "audio/flac",
            &[("format", "mp3"), ("maxBitRate", &below_flac)],
        ),
        (
            flac,
            &flac_file,
            "audio/flac",
            &[
                ("format", "opus"),
                ("maxBitRate", "128"),
                ("timeOffset", "30"),
                ("estimateContentLength", "true"),
            ],
        ),
        (mp3, &mp3_file, "audio/mpeg", &[("maxBitRate", &below_mp3)]),
        (mp3, &mp3_file, "audio/mpeg", &[("format", "opus")]),
        (mp3, &mp3_file, "audio/mpeg", &[("format", "mp3")]),
    ];
    for (id, file, mime, params) in cases {
        let case = format!("{mime} {params:?}");
        let size = file.len();
        // The rules select a transcode for the request, and the response
        // is the original file with its own content type.
        let selected = plan(&server, id, params).await;
        assert!(
            matches!(selected, StreamPlan::Transcode { .. }),
            "{case}: {selected:?}"
        );
        for method in [Method::GET, Method::POST] {
            let response = stream_as(&server, method, id, params, &[]).await;
            assert!(
                header_of(&response, &header::CONTENT_DISPOSITION).is_some(),
                "{case}"
            );
            assert_whole_file(response, file, mime, &case).await;
        }

        // A range of such a request is a range of the original file.
        for (range, first, last) in [
            ("bytes=100-199", 100, 199),
            ("bytes=100-", 100, size - 1),
            ("bytes=-100", size - 100, size - 1),
        ] {
            for method in [Method::GET, Method::POST] {
                let headers = [("range", range)];
                let response = stream_as(&server, method, id, params, &headers).await;
                assert_eq!(
                    response.status(),
                    StatusCode::PARTIAL_CONTENT,
                    "{case} {range}"
                );
                assert_eq!(content_type(&response), mime, "{case} {range}");
                assert_eq!(
                    header_of(&response, &header::CONTENT_RANGE),
                    Some(format!("bytes {first}-{last}/{size}").as_str()),
                    "{case} {range}"
                );
                assert_eq!(
                    header_of(&response, &header::ACCEPT_RANGES),
                    Some("bytes"),
                    "{case} {range}"
                );
                assert_eq!(
                    body_bytes(response).await,
                    &file[first..=last],
                    "{case} {range}"
                );
            }
        }
        let unsatisfiable = format!("bytes={size}-");
        let headers = [("range", unsatisfiable.as_str())];
        let response = stream(&server, id, params, &headers).await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{case}"
        );

        // A HEAD request describes the original file.
        let head = stream_as(&server, Method::HEAD, id, params, &[]).await;
        assert_eq!(head.status(), StatusCode::OK, "{case}");
        assert_eq!(content_type(&head), mime, "{case}");
        assert_eq!(
            header_of(&head, &header::CONTENT_LENGTH),
            Some(size.to_string().as_str()),
            "{case}"
        );
    }

    // The validators of such a response are those of the file: a part is
    // continued with `If-Range`, and a copy is confirmed with
    // `If-None-Match`.
    let params = [("format", "opus"), ("maxBitRate", "128")];
    let first = stream(&server, flac, &params, &[]).await;
    let tag = header_of(&first, &header::ETAG).expect("a tag").to_owned();
    let headers = [("range", "bytes=100-199"), ("if-range", tag.as_str())];
    let response = stream(&server, flac, &params, &headers).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(body_bytes(response).await, &flac_file[100..200]);
    let headers = [("range", "bytes=100-199"), ("if-range", "\"0.00000000-0\"")];
    let response = stream(&server, flac, &params, &headers).await;
    assert_whole_file(response, &flac_file, "audio/flac", "another version").await;
    let response = stream(&server, flac, &params, &[("if-none-match", tag.as_str())]).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn parameter_that_names_no_format_or_no_number_is_error_0() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    for format in ["flac", "ogg", "RAW", "../raw"] {
        let (code, message) = stream_error(&server, id, &[("format", format)], &[]).await;
        assert_eq!(
            (code, message.as_str()),
            (0, "parameter format must be raw, opus or mp3"),
            "{format}"
        );
    }
    for (name, value) in [
        ("maxBitRate", "high"),
        ("maxBitRate", "-1"),
        ("timeOffset", "1.5"),
        ("estimateContentLength", "yes"),
    ] {
        let (code, message) = stream_error(&server, id, &[(name, value)], &[]).await;
        assert_eq!(code, 0, "{name}={value}");
        assert!(message.contains(name), "{message}");
    }
}

#[tokio::test]
async fn id_that_names_no_track_is_error_70_and_a_missing_id_is_error_10() {
    let server = TestServer::standard().await;
    let (_, file) = row_and_file(&server, &server.track.low_tide).await;

    let response = server
        .request(request(Method::GET, "stream", &[("f", "json")], &[]))
        .await;
    assert_eq!(
        error_of(response).await,
        (10, "required parameter is missing: id".to_owned())
    );

    // An ID of another kind, a track ID that no row has, and values that
    // are no ID: a path of a file of the library among them. None of them
    // reaches the database or the filesystem as a path.
    let absolute = server
        .path("Aurora Field/Glass Harbour/01 Low Tide.flac")
        .to_str()
        .expect("a Unicode path")
        .to_owned();
    let upper = server.track.low_tide.to_uppercase();
    let padded = format!("{} ", server.track.low_tide);
    for id in [
        NO_TRACK,
        server.album.glass_harbour.as_str(),
        server.artist.aurora_field.as_str(),
        "",
        "1",
        "tr-",
        upper.as_str(),
        padded.as_str(),
        "Aurora Field/Glass Harbour/01 Low Tide.flac",
        "../Aurora Field/Glass Harbour/01 Low Tide.flac",
        absolute.as_str(),
        "tr-../../../../../../etc/passwd",
        "/etc/passwd",
        "%",
        "tr-0000000000000000000000000000000' OR '1'='1",
    ] {
        assert_eq!(
            stream_error(&server, id, &[], &[]).await,
            (70, NOT_FOUND.to_owned()),
            "{id}"
        );
        let post = stream_as(&server, Method::POST, id, &[("f", "json")], &[]).await;
        assert_eq!(error_of(post).await, (70, NOT_FOUND.to_owned()), "{id}");
    }

    let xml = server.get_xml("stream", &[("id", NO_TRACK)]).await;
    assert!(
        xml.contains(r#"<error code="70" message="not found"/>"#),
        "{xml}"
    );

    // The ID of the track is served by the same server.
    let response = stream(&server, &server.track.low_tide, &[], &[]).await;
    assert_whole_file(response, &file, "audio/flac", "the track").await;
}

#[tokio::test]
async fn stream_without_a_user_is_the_authentication_error_and_download_stays_disabled() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();

    let anonymous = [("id", id), ("f", "json")];
    let response = server.get_raw_noauth("stream", &anonymous).await;
    assert_eq!(error_of(response).await.0, 10);
    let wrong = [
        ("id", id),
        ("u", OWNER),
        ("p", "wrong"),
        ("v", API_VERSION),
        ("c", CLIENT),
        ("f", "json"),
    ];
    let response = server.get_raw_noauth("stream", &wrong).await;
    assert_eq!(error_of(response).await.0, 40);

    // `download` of the track that `stream` serves is error 50.
    for method in [Method::GET, Method::POST, Method::HEAD] {
        let params = [("id", id), ("f", "json")];
        let response = server
            .request(request(method.clone(), "download", &params, &[]))
            .await;
        if method == Method::HEAD {
            assert_eq!(content_type(&response), "application/json");
            assert_eq!(header_of(&response, &header::ACCEPT_RANGES), None);
            assert!(body_bytes(response).await.is_empty());
        } else {
            assert_eq!(
                error_of(response).await,
                (50, "download is disabled on this server".to_owned()),
                "{method}"
            );
        }
    }
}

#[tokio::test]
async fn track_without_a_file_is_error_70() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.clone();
    let before = row(server.db(), &id).await;
    assert!(server.path(&before.rel_path).is_file());

    // The scanner gives a track that it keeps without a file this path.
    let placeholder = tracks::placeholder_path(&id);
    let relinked = id.clone();
    server
        .db()
        .write(move |conn| {
            tracks::relink_track(
                conn,
                &relinked,
                before.folder_id,
                &placeholder,
                before.mtime,
                before.size,
            )
        })
        .await
        .expect("give the track its placeholder path");
    assert!(!row(server.db(), &id).await.has_file());

    assert_eq!(
        stream_error(&server, &id, &[], &[]).await,
        (70, NOT_FOUND.to_owned())
    );
    // The answer is the same for a request that selects a transcode.
    assert_eq!(
        stream_error(&server, &id, &[("format", "opus")], &[]).await,
        (70, NOT_FOUND.to_owned())
    );
}

#[tokio::test]
async fn file_deleted_after_the_scan_is_error_70() {
    let server = TestServer::standard_with_own_files().await;
    let id = server.track.low_tide.as_str();
    let (track, file) = row_and_file(&server, id).await;
    let response = stream(&server, id, &[], &[]).await;
    assert_whole_file(response, &file, "audio/flac", "before").await;

    fs::remove_file(server.path(&track.rel_path)).expect("delete the file");
    assert_eq!(
        stream_error(&server, id, &[], &[]).await,
        (70, NOT_FOUND.to_owned())
    );
    assert_eq!(
        stream_error(&server, id, &[], &[("range", "bytes=0-99")]).await,
        (70, NOT_FOUND.to_owned())
    );
    let post = stream_as(&server, Method::POST, id, &[("f", "json")], &[]).await;
    assert_eq!(error_of(post).await, (70, NOT_FOUND.to_owned()));

    // The other tracks of the album are served.
    let (_, other) = row_and_file(&server, &server.track.breakwater).await;
    let response = stream(&server, &server.track.breakwater, &[], &[]).await;
    assert_whole_file(response, &other, "audio/flac", "another track").await;
}

#[tokio::test]
async fn file_replaced_after_the_scan_is_not_served_until_the_next_scan() {
    let server = TestServer::standard_with_own_files().await;
    let id = server.track.low_tide.as_str();
    let (before, original) = row_and_file(&server, id).await;
    let path = server.path(&before.rel_path);
    let scanned_at = unix_time(before.mtime);
    let not_served = || async {
        assert_eq!(
            stream_error(&server, id, &[], &[]).await,
            (70, NOT_FOUND.to_owned())
        );
        assert_eq!(
            stream_error(&server, id, &[], &[("range", "bytes=0-99")]).await,
            (70, NOT_FOUND.to_owned())
        );
        let head = stream_as(&server, Method::HEAD, id, &[], &[]).await;
        assert_eq!(header_of(&head, &header::ACCEPT_RANGES), None);
        assert_eq!(content_type(&head), "application/xml");
    };

    // The file of another track takes the path. It has another size, and
    // the time of the file that the scan saw.
    let (_, other) = row_and_file(&server, &server.track.meridian).await;
    assert_ne!(other.len(), original.len());
    fs::write(&path, &other).expect("replace the file");
    set_modified(&path, scanned_at);
    not_served().await;

    // A file of the same size at another time, as a retag that keeps the
    // size leaves it.
    let mut edited = original.clone();
    *edited.last_mut().expect("a file with bytes") ^= 0xff;
    fs::write(&path, &edited).expect("replace the file");
    set_modified(&path, unix_time(before.mtime + 60));
    not_served().await;

    // The size and the time of the row: the file of the row is back.
    fs::write(&path, &original).expect("restore the file");
    set_modified(&path, scanned_at);
    let response = stream(&server, id, &[], &[]).await;
    assert_whole_file(response, &original, "audio/flac", "restored").await;

    // After a scan the row describes the file at the path, and the ID
    // serves that file.
    fs::write(&path, &edited).expect("replace the file");
    set_modified(&path, unix_time(before.mtime + 60));
    not_served().await;
    let summary = server.rescan().await;
    assert_eq!(summary.changed, 1, "{summary:?}");
    let after = row(server.db(), id).await;
    assert_eq!(after.mtime, before.mtime + 60);
    let response = stream(&server, id, &[], &[]).await;
    assert_whole_file(response, &edited, "audio/flac", "after the scan").await;
}

#[tokio::test]
async fn files_that_exchange_paths_are_not_served_under_the_id_of_the_other() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path().to_path_buf();
    let glass_harbour = root.join("Aurora Field").join("Glass Harbour");
    let low_tide_path = glass_harbour.join("01 Low Tide.flac");
    let breakwater_path = glass_harbour.join("02 Breakwater.flac");
    // The two files have times that differ, as files of a library have.
    set_modified(&low_tide_path, unix_time(1_600_000_000));
    set_modified(&breakwater_path, unix_time(1_600_000_100));
    let low_tide_file = fs::read(&low_tide_path).unwrap();
    let breakwater_file = fs::read(&breakwater_path).unwrap();
    assert_ne!(low_tide_file, breakwater_file);

    let server = TestServer::scanned(library).await;
    let catalogue = server.catalogue().await;
    let low_tide = catalogue.song("Low Tide").track.id.clone();
    let breakwater = catalogue.song("Breakwater").track.id.clone();
    let response = stream(&server, &low_tide, &[], &[]).await;
    assert_whole_file(response, &low_tide_file, "audio/flac", "before").await;

    // Each file is now at the path of the other, with its own time.
    let spare = glass_harbour.join("spare");
    fs::rename(&low_tide_path, &spare).unwrap();
    fs::rename(&breakwater_path, &low_tide_path).unwrap();
    fs::rename(&spare, &breakwater_path).unwrap();
    for id in [&low_tide, &breakwater] {
        assert_eq!(
            stream_error(&server, id, &[], &[]).await,
            (70, NOT_FOUND.to_owned())
        );
    }

    // The scan gives each row the path of its file, and each ID serves the
    // bytes of its own track again.
    let summary = server.rescan().await;
    assert_eq!(summary.moved, 2, "{summary:?}");
    let response = stream(&server, &low_tide, &[], &[]).await;
    assert_whole_file(response, &low_tide_file, "audio/flac", "Low Tide").await;
    let response = stream(&server, &breakwater, &[], &[]).await;
    assert_whole_file(response, &breakwater_file, "audio/flac", "Breakwater").await;
}

#[tokio::test]
async fn row_whose_path_leaves_the_music_folder_is_not_served() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.clone();
    let (track, file) = row_and_file(&server, &id).await;

    // A file outside the music folder with the size and the time of the
    // row, and a relative path that leads to it. The scanner stores no such
    // path.
    let outside = tempfile::tempdir().expect("a directory outside the library");
    assert_eq!(outside.path().parent(), server.root().parent());
    let secret = outside.path().join("secret.flac");
    fs::write(&secret, vec![b'S'; file.len()]).expect("write the file outside");
    set_modified(&secret, unix_time(track.mtime));
    let directory = outside.path().file_name().and_then(|name| name.to_str());
    let rel_path = format!("../{}/secret.flac", directory.expect("a Unicode name"));
    assert_eq!(fs::read(server.path(&rel_path)).unwrap().len(), file.len());

    let (relinked, path) = (id.clone(), rel_path.clone());
    server
        .db()
        .write(move |conn| {
            tracks::relink_track(
                conn,
                &relinked,
                track.folder_id,
                &path,
                track.mtime,
                track.size,
            )
        })
        .await
        .expect("change the path of the row");
    assert_eq!(row(server.db(), &id).await.rel_path, rel_path);

    assert_eq!(
        stream_error(&server, &id, &[], &[]).await,
        (70, NOT_FOUND.to_owned())
    );
    assert_eq!(
        stream_error(&server, &id, &[], &[("range", "bytes=0-9")]).await,
        (70, NOT_FOUND.to_owned())
    );
}

#[tokio::test]
async fn file_whose_time_has_no_http_date_is_error_0() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .build();
    let path = library.files[0].clone();
    set_modified(&path, unix_time(-10));
    if fs::metadata(&path).unwrap().modified().unwrap() >= UNIX_EPOCH {
        // The filesystem keeps no time before 1970.
        return;
    }
    let server = TestServer::scanned(library).await;
    let track = server.catalogue().await.song("Low Tide").track.clone();
    assert_eq!(track.mtime, -10);

    let message =
        "the modification time of the file of the track is outside the years 1970 to 9999";
    for method in [Method::GET, Method::POST] {
        let response = stream_as(&server, method, &track.id, &[("f", "json")], &[]).await;
        assert_eq!(error_of(response).await, (0, message.to_owned()));
    }

    // With a time that has an HTTP date, the next scan makes the file one
    // that the server serves.
    set_modified(&path, unix_time(1_600_000_000));
    server.rescan().await;
    let file = fs::read(&path).unwrap();
    let response = stream(&server, &track.id, &[], &[]).await;
    assert_whole_file(response, &file, "audio/flac", "after the scan").await;
}

#[tokio::test]
async fn content_disposition_holds_the_file_name_of_the_track() {
    let server = TestServer::standard().await;
    let response = stream(&server, &server.track.low_tide, &[], &[]).await;
    assert_eq!(
        header_of(&response, &header::CONTENT_DISPOSITION),
        Some("inline; filename=\"01 Low Tide.flac\"; filename*=UTF-8''01%20Low%20Tide.flac")
    );
}

#[tokio::test]
async fn content_disposition_of_a_file_name_outside_ascii_is_a_valid_header() {
    // The names are valid on Windows. The title is in the tags and in the
    // file name; the header holds the file name.
    let library = LibraryBuilder::new()
        .track("Señal Única", "Años Luz", "Canción Ünïca 日本", 7, "flac")
        .build();
    let file = fs::read(&library.files[0]).unwrap();
    let server = TestServer::scanned(library).await;
    let track = server
        .catalogue()
        .await
        .song("Canción Ünïca 日本")
        .track
        .clone();
    // The name as the filesystem returned it to the scanner, which may be
    // another Unicode form than the one the test wrote.
    let name = track.rel_path.rsplit('/').next().expect("a file name");
    assert!(!name.is_ascii());

    let response = stream(&server, &track.id, &[], &[]).await;
    let value = header_of(&response, &header::CONTENT_DISPOSITION)
        .expect("the header")
        .to_owned();
    let (plain, exact) = disposition(&value);
    assert_eq!(exact, name);
    let expected: String = name
        .chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .collect();
    assert_eq!(plain, expected);
    assert_whole_file(response, &file, "audio/flac", "a name outside ASCII").await;
}

// A double quote and a backslash are no part of a file name on Windows.
#[cfg(unix)]
#[tokio::test]
async fn content_disposition_of_a_file_name_with_a_quote_is_a_valid_header() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .build();
    let name = "01 \"Low\" \\ Tide; 100%.flac";
    let path = library.files[0].with_file_name(name);
    fs::rename(&library.files[0], &path).unwrap();
    let file = fs::read(&path).unwrap();
    let server = TestServer::scanned(library).await;
    let track = server.catalogue().await.song("Low Tide").track.clone();
    assert_eq!(track.rel_path, format!("Aurora Field/Glass Harbour/{name}"));

    let response = stream(&server, &track.id, &[], &[]).await;
    let value = header_of(&response, &header::CONTENT_DISPOSITION)
        .expect("the header")
        .to_owned();
    assert_eq!(
        value,
        concat!(
            "inline; filename=\"01 _Low_ _ Tide; 100%.flac\"; ",
            "filename*=UTF-8''01%20%22Low%22%20%5C%20Tide%3B%20100%25.flac"
        )
    );
    assert_eq!(disposition(&value).1, name);
    assert_whole_file(response, &file, "audio/flac", "a name with a quote").await;
}

#[tokio::test]
async fn large_file_is_read_in_chunks_as_the_client_takes_them() {
    let (server, id, path) = wav_server().await;
    let file = fs::read(&path).unwrap();

    let response = stream(&server, &id, &[], &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "audio/wav");
    assert_eq!(
        header_of(&response, &header::CONTENT_LENGTH),
        Some(file.len().to_string().as_str())
    );
    let mut body = response.into_body();
    let mut chunks = Vec::new();
    let mut received = Vec::new();
    while let Some(frame) = body.frame().await {
        let data = frame.expect("a frame").into_data().expect("a data frame");
        chunks.push(data.len());
        received.extend_from_slice(&data);
    }
    assert!(received == file, "the body is the file");
    // The body never holds the file as one piece.
    let largest = chunks.iter().copied().max().expect("a chunk");
    assert!(chunks.len() >= 4, "{chunks:?}");
    assert!(largest <= file.len() / 4, "{chunks:?}");

    // A part in the middle of the file.
    let headers = [("range", "bytes=500000-899999")];
    let response = stream(&server, &id, &[], &headers).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(body_bytes(response).await == file[500_000..900_000]);
}

/// The number of file descriptors of this process that are open on the file
/// at `path`.
///
/// The entries of `/dev/fd` are the open descriptors. On Linux the state of
/// an entry is the state of the open file. On macOS it is not, and opening
/// the entry duplicates the descriptor, whose state is then read.
#[cfg(unix)]
fn open_descriptors(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;

    let file = fs::metadata(path).expect("the state of the file");
    fs::read_dir("/dev/fd")
        .expect("list the open descriptors")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            // A descriptor that another thread closed meanwhile, or one that
            // cannot be opened for reading, is not the file.
            #[cfg(target_os = "linux")]
            let state = fs::metadata(entry.path());
            #[cfg(not(target_os = "linux"))]
            let state = fs::File::open(entry.path()).and_then(|open| open.metadata());
            state.ok()
        })
        .filter(|open| open.dev() == file.dev() && open.ino() == file.ino())
        .count()
}

// The open files of a process are listed through `/dev/fd`, which Windows
// does not have.
#[cfg(unix)]
#[tokio::test]
async fn response_that_the_client_drops_leaves_no_open_file_and_no_task() {
    let (server, id, path) = wav_server().await;
    let size = fs::metadata(&path).unwrap().len() as usize;
    let tasks = || {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    };
    let tasks_before = tasks();
    assert_eq!(open_descriptors(&path), 0);

    for headers in [vec![], vec![("range", "bytes=1000-")]] {
        let response = stream(&server, &id, &[], &headers).await;
        let mut body = response.into_body();
        let first = body
            .frame()
            .await
            .expect("a frame")
            .expect("a frame")
            .into_data()
            .expect("a data frame");
        // The client has taken the start of the body, and the response
        // reads the rest from the open file.
        assert!(first.len() < size / 4, "{}", first.len());
        assert_eq!(open_descriptors(&path), 1, "{headers:?}");

        // The client disconnects: the server drops the body.
        drop(body);
        let closed = async {
            while open_descriptors(&path) > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(1), closed)
            .await
            .expect("the file is closed within one second");
        assert_eq!(tasks(), tasks_before, "{headers:?}");
    }

    // A response that is dropped unread, and the first response of a
    // request whose `If-Range` names another version.
    drop(stream(&server, &id, &[], &[]).await);
    let headers = [("range", "bytes=1000-"), ("if-range", "\"0.00000000-0\"")];
    let response = stream(&server, &id, &[], &headers).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(open_descriptors(&path), 1);
    drop(response);
    // A HEAD request and a request that is an error open no file. The
    // format names no target, which is error 0.
    let head = stream_as(&server, Method::HEAD, &id, &[], &[]).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(open_descriptors(&path), 0);
    assert_eq!(
        stream_error(&server, &id, &[("format", "flac")], &[])
            .await
            .0,
        0
    );
    assert_eq!(open_descriptors(&path), 0);
    assert_eq!(tasks(), tasks_before);
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege that a test run does not have.
#[cfg(unix)]
mod links {
    use std::os::unix::fs::symlink;

    use tokio_util::sync::CancellationToken;
    use wn_server::config::Config;
    use wn_server::library::scanner::{self, ScanOptions};

    use super::*;

    #[tokio::test]
    async fn track_behind_a_link_out_of_the_music_folder_is_not_served() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        let outside = tempfile::tempdir().expect("a directory outside the library");

        // The file of one track is outside the folder, and a link is at its
        // path.
        let link = root
            .join("Aurora Field")
            .join("Glass Harbour")
            .join("01 Low Tide.flac");
        let target = outside.path().join("01 Low Tide.flac");
        fs::rename(&link, &target).unwrap();
        symlink(&target, &link).unwrap();
        // The directory of one artist is outside the folder, behind a link.
        let link = root.join("Marlow Quartet");
        let target = outside.path().join("Marlow Quartet");
        fs::rename(&link, &target).unwrap();
        symlink(&target, &link).unwrap();

        // The scan follows the links and gives each file its row.
        let server = TestServer::scanned(library).await;
        let catalogue = server.catalogue().await;
        assert_eq!(catalogue.songs.len(), 10);
        for title in ["Low Tide", "Upstream", "Delta"] {
            let track = &catalogue.song(title).track;
            assert!(track.has_file(), "{title}");
            assert!(server.path(&track.rel_path).is_file(), "{title}");
            assert_eq!(
                stream_error(&server, &track.id, &[], &[]).await,
                (70, NOT_FOUND.to_owned()),
                "{title}"
            );
            assert_eq!(
                stream_error(&server, &track.id, &[], &[("range", "bytes=0-99")]).await,
                (70, NOT_FOUND.to_owned()),
                "{title}"
            );
            let post = stream_as(
                &server,
                Method::POST,
                &track.id,
                &[("f", "json"), ("format", "raw")],
                &[],
            )
            .await;
            assert_eq!(error_of(post).await, (70, NOT_FOUND.to_owned()), "{title}");
        }

        // A file that is in the folder is served.
        let track = &catalogue.song("Breakwater").track;
        let file = fs::read(server.path(&track.rel_path)).unwrap();
        let response = stream(&server, &track.id, &[], &[]).await;
        assert_whole_file(response, &file, "audio/flac", "Breakwater").await;
    }

    #[tokio::test]
    async fn link_that_takes_the_path_of_a_file_after_the_scan_is_not_followed_out() {
        let server = TestServer::standard_with_own_files().await;
        let id = server.track.low_tide.as_str();
        let (track, file) = row_and_file(&server, id).await;
        let path = server.path(&track.rel_path);

        // A file outside the music folder with the size and the time of the
        // row, so that the state of the path equals the row.
        let outside = tempfile::tempdir().expect("a directory outside the library");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, vec![b'S'; file.len()]).unwrap();
        set_modified(&secret, unix_time(track.mtime));
        fs::remove_file(&path).unwrap();
        symlink(&secret, &path).unwrap();
        let state = fs::metadata(&path).unwrap();
        assert_eq!(state.len(), track.size);
        assert_eq!(state.modified().unwrap(), unix_time(track.mtime));

        for headers in [vec![], vec![("range", "bytes=0-99")]] {
            assert_eq!(
                stream_error(&server, id, &[], &headers).await,
                (70, NOT_FOUND.to_owned()),
                "{headers:?}"
            );
        }
        let head = stream_as(&server, Method::HEAD, id, &[], &[]).await;
        assert_eq!(header_of(&head, &header::ACCEPT_RANGES), None);
        assert_eq!(content_type(&head), "application/xml");
    }

    #[tokio::test]
    async fn track_behind_a_link_inside_the_music_folder_is_served() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        let target = root
            .join("Aurora Field")
            .join("Glass Harbour")
            .join("01 Low Tide.flac");
        let file = fs::read(&target).unwrap();
        // A relative link to a file and a link to a directory. The targets
        // are in the music folder.
        fs::create_dir(root.join("Favourites")).unwrap();
        symlink(
            "../Aurora Field/Glass Harbour/01 Low Tide.flac",
            root.join("Favourites").join("low tide.flac"),
        )
        .unwrap();
        symlink(root.join("Aurora Field"), root.join("Linked Artist")).unwrap();

        // The scan gives each path of the file a row.
        let server = TestServer::scanned(library).await;
        for rel_path in [
            "Aurora Field/Glass Harbour/01 Low Tide.flac",
            "Favourites/low tide.flac",
            "Linked Artist/Glass Harbour/01 Low Tide.flac",
        ] {
            let track = row_at(server.db(), rel_path).await;
            let response = stream(&server, &track.id, &[], &[]).await;
            assert_whole_file(response, &file, "audio/flac", rel_path).await;
            let headers = [("range", "bytes=100-199")];
            let response = stream(&server, &track.id, &[], &headers).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{rel_path}");
            assert_eq!(body_bytes(response).await, &file[100..200], "{rel_path}");
        }
    }

    #[tokio::test]
    async fn music_folder_behind_a_link_is_served() {
        let library = LibraryBuilder::standard().build();
        let links = tempfile::tempdir().expect("a directory for the link");
        let folder = links.path().join("music");
        symlink(library.root.path(), &folder).unwrap();
        let file = fs::read(&library.files[0]).unwrap();

        // The config names the link as the music folder, and the scan reads
        // the folder through the link.
        let mut config = Config::default();
        config.library.folders = vec![folder.clone()];
        let server = TestServer::bare(config).await;
        let db = server.db().clone();
        let summary = tokio::task::spawn_blocking(move || {
            let cancel = CancellationToken::new();
            scanner::scan(&db, &[folder], &cancel, false, ScanOptions::default())
        })
        .await
        .expect("the scan does not panic")
        .expect("the scan reads and writes the database");
        assert_eq!(summary.errors, [] as [&str; 0]);

        let track = row_at(server.db(), "Aurora Field/Glass Harbour/01 Low Tide.flac").await;
        let response = stream(&server, &track.id, &[], &[]).await;
        assert_whole_file(response, &file, "audio/flac", "a folder behind a link").await;
    }
}
