//! Router tests of the `stream` requests that select a transcode
//! (`docs/design.md` section 3.9).
//!
//! The transcoder of a test server is the `FakeTranscoder` of
//! `tests/common`, so no test of this file starts ffmpeg. A test sets the
//! bytes and the errors of the transcoder and reads the jobs that reached
//! it. The unit tests in `src/media/transcode.rs` cover the transcoder that
//! runs a process.
//!
//! Two tests open a socket on `127.0.0.1` with a port that the system
//! chooses, because they check what a connection does: a client that
//! disconnects, and a server that `run_server` starts without ffmpeg.

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::header::HeaderName;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use rusqlite::params;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wn_server::config::{Config, DataDir};
use wn_server::db::Db;
use wn_server::db::queries::tracks::{self, Track};
use wn_server::db::queries::users;
use wn_server::library::scanner::{self, ScanOptions};
use wn_server::media::stream::Target;
use wn_server::media::transcode::TranscodeJob;
use wn_server::server::{ServerOptions, run_server};

use common::transcoder::{FAKE_CHUNK, FAKE_TRANSCODE, FakeFailure};
use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, ScannedServer, StandardServer,
    TestServer, body_bytes, body_string, captured_log, content_type, subsonic_response, urlencode,
};

/// Parameters of a request, and request headers: names and values.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The longest time a test waits for something that a property of the
/// server bounds: the answer of a transcoder that is busy, the end of a
/// body, the drop of a stream. A passing run waits some milliseconds.
const SECOND: Duration = Duration::from_secs(1);

/// The longest time a test waits for a server on a socket to start, to
/// answer or to end. The wait ends with its event, so the limit does not
/// delay a passing run. It ends a test whose server hangs, as the limit of
/// `tests/shutdown.rs` does.
const LIMIT: Duration = Duration::from_secs(10);

/// The parameters `params` and the password of the owner, as a query
/// string.
fn query(params: Pairs) -> String {
    let mut all = params.to_vec();
    all.extend([
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
    ]);
    urlencode(&all)
}

/// A `stream` request with the HTTP method `method` for the track `id`,
/// with `params` and `headers`. The parameters are the form body of a POST
/// and the query string of every other method.
fn request(method: Method, name: &str, id: &str, params: Pairs, headers: Pairs) -> Request<Body> {
    let mut all = vec![("id", id)];
    all.extend_from_slice(params);
    let query = query(&all);
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

/// Sends `GET /rest/stream` for the track `id` with `params`.
async fn stream(server: &TestServer, id: &str, params: Pairs<'_>) -> Response {
    stream_as(server, Method::GET, id, params, &[]).await
}

/// Sends a `stream` request for the track `id` with the HTTP method
/// `method`, with `params` and `headers`.
async fn stream_as(
    server: &TestServer,
    method: Method,
    id: &str,
    params: Pairs<'_>,
    headers: Pairs<'_>,
) -> Response {
    server
        .request(request(method, "stream", id, params, headers))
        .await
}

/// Sends `GET /rest/stream` for the track `id` with `params` and `f=json`.
/// Checks that the response is the envelope of an error and no response of
/// a stream. Returns the code and the message.
async fn stream_error(server: &TestServer, id: &str, params: Pairs<'_>) -> (u64, String) {
    let mut all = params.to_vec();
    all.push(("f", "json"));
    let response = stream(server, id, &all).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    for name in [header::ACCEPT_RANGES, header::CACHE_CONTROL] {
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

/// Checks that `response` has the status and the headers of a transcode
/// with the content type `mime`: no ranges, no caching, and none of the
/// headers that describe a file.
fn assert_transcode_headers(response: &Response, mime: &str, case: &str) {
    assert_eq!(response.status(), StatusCode::OK, "{case}");
    assert_eq!(content_type(response), mime, "{case}");
    assert_eq!(
        header_of(response, &header::ACCEPT_RANGES),
        Some("none"),
        "{case}"
    );
    assert_eq!(
        header_of(response, &header::CACHE_CONTROL),
        Some("no-store"),
        "{case}"
    );
    for name in [
        header::CONTENT_RANGE,
        header::CONTENT_DISPOSITION,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        assert_eq!(header_of(response, &name), None, "{case} {name}");
    }
}

/// Reads the row of the track `id`.
async fn row(db: &Db, id: &str) -> Track {
    let id = id.to_owned();
    db.read(move |conn| tracks::get_track(conn, &id))
        .await
        .expect("read the track")
        .expect("the track exists")
}

/// The resolved path of the file of `track`, which is the path that the
/// transcoder gets.
fn resolved(server: &ScannedServer, track: &Track) -> PathBuf {
    fs::canonicalize(server.path(&track.rel_path)).expect("the path of the file")
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

/// `len` bytes that repeat after 251 bytes. The size of a chunk of the
/// transcoder is no multiple of 251, so two chunks start with other bytes,
/// and a body that leaves out or repeats a chunk differs from these bytes.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

/// The job of a transcode of the file at `path`, which is the file of the
/// track `track`.
fn job(
    track: &str,
    path: PathBuf,
    target: Target,
    bitrate_kbps: u32,
    offset_s: u32,
) -> TranscodeJob {
    TranscodeJob {
        track: track.to_owned(),
        path,
        target,
        bitrate_kbps,
        offset_s,
    }
}

#[tokio::test]
async fn request_for_a_format_gets_the_stream_of_the_transcoder() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let track = row(server.db(), id).await;
    let file = resolved(&server, &track);

    // `format=opus` with a limit: Opus at that bitrate, from the start.
    let params = [("format", "opus"), ("maxBitRate", "96")];
    let response = stream(&server, id, &params).await;
    assert_transcode_headers(&response, "audio/ogg", "opus");
    // The server does not know the length of the stream.
    assert_eq!(header_of(&response, &header::CONTENT_LENGTH), None);
    assert_eq!(
        header_of(&response, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some("*")
    );
    assert_eq!(body_bytes(response).await, FAKE_TRANSCODE);
    assert_eq!(
        server.transcoder().jobs(),
        [job(id, file.clone(), Target::Opus, 96, 0)]
    );

    // `timeOffset` reaches the job.
    let params = [
        ("format", "opus"),
        ("maxBitRate", "96"),
        ("timeOffset", "30"),
    ];
    let response = stream(&server, id, &params).await;
    assert_transcode_headers(&response, "audio/ogg", "timeOffset");
    assert_eq!(body_bytes(response).await, FAKE_TRANSCODE);
    assert_eq!(
        server.transcoder().jobs().last(),
        Some(&job(id, file.clone(), Target::Opus, 96, 30))
    );

    // `format=mp3` without a limit: MP3 at the bitrate of the config.
    let response = stream(&server, id, &[("format", "mp3")]).await;
    assert_transcode_headers(&response, "audio/mpeg", "mp3");
    assert_eq!(header_of(&response, &header::CONTENT_LENGTH), None);
    assert_eq!(body_bytes(response).await, FAKE_TRANSCODE);
    assert_eq!(
        server.transcoder().jobs().last(),
        Some(&job(id, file.clone(), Target::Mp3, 320, 0))
    );

    // `format=opus` without a limit, by POST and with the `.view` suffix.
    for (method, name) in [
        (Method::POST, "stream"),
        (Method::GET, "stream.view"),
        (Method::POST, "stream.view"),
    ] {
        let case = format!("{method} {name}");
        let request = request(method, name, id, &[("format", "Opus")], &[]);
        let response = server.request(request).await;
        assert_transcode_headers(&response, "audio/ogg", &case);
        assert_eq!(body_bytes(response).await, FAKE_TRANSCODE, "{case}");
        assert_eq!(
            server.transcoder().jobs().last(),
            Some(&job(id, file.clone(), Target::Opus, 192, 0)),
            "{case}"
        );
    }
    assert_eq!(server.transcoder().jobs().len(), 6);
    assert_eq!(server.transcoder().open_streams(), 0);
}

#[tokio::test]
async fn limit_below_the_bitrate_of_the_file_selects_opus_and_a_limit_at_it_the_file() {
    let server = TestServer::standard().await;
    for id in [&server.track.low_tide, &server.track.meridian] {
        let track = row(server.db(), id).await;
        let file = resolved(&server, &track);
        let original = fs::read(&file).expect("read the file");
        let bitrate = track.bitrate_kbps.expect("a bitrate");
        assert!(bitrate > 32, "the fixture has {bitrate} kbps");
        let jobs_before = server.transcoder().jobs().len();

        // A limit at the bitrate of the file, and one above it: the file,
        // for the lossless and for the lossy fixture.
        for limit in [bitrate, bitrate + 1, 100_000] {
            let limit = limit.to_string();
            let response = stream(&server, id, &[("maxBitRate", &limit)]).await;
            assert_eq!(content_type(&response), track.content_type, "{limit}");
            assert_eq!(
                header_of(&response, &header::ACCEPT_RANGES),
                Some("bytes"),
                "{limit}"
            );
            assert_eq!(body_bytes(response).await, original, "{limit}");
        }
        assert_eq!(server.transcoder().jobs().len(), jobs_before);

        // A limit below it: Opus at the limit. A limit below 32 kbps is
        // 32 kbps, and a format that names no target changes nothing.
        let below = (bitrate - 1).to_string();
        let cases: [(Pairs, u32); 4] = [
            (&[("maxBitRate", &below)], (bitrate - 1).min(320)),
            (&[("maxBitRate", "32")], 32),
            (&[("maxBitRate", "8")], 32),
            (&[("maxBitRate", "32"), ("format", "aac")], 32),
        ];
        for (params, kbps) in cases {
            let case = format!("{} {params:?}", track.suffix);
            let response = stream(&server, id, params).await;
            assert_transcode_headers(&response, "audio/ogg", &case);
            assert_eq!(body_bytes(response).await, FAKE_TRANSCODE, "{case}");
            assert_eq!(
                server.transcoder().jobs().last(),
                Some(&job(id, file.clone(), Target::Opus, kbps, 0)),
                "{case}"
            );
        }
        assert_eq!(server.transcoder().jobs().len(), jobs_before + 4);
    }
}

#[tokio::test]
async fn estimate_content_length_has_no_effect_on_a_transcode() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    // A stream of several chunks, whose last chunk is not whole.
    let bytes = pattern(3 * FAKE_CHUNK + 123);
    server.transcoder().set_bytes(bytes.clone());

    // With the parameter, with `false` and without it, the response has no
    // `Content-Length`, and the body is every byte of the transcoder.
    let cases: [Pairs; 4] = [
        &[("format", "opus"), ("estimateContentLength", "true")],
        &[
            ("format", "mp3"),
            ("maxBitRate", "96"),
            ("timeOffset", "30"),
            ("estimateContentLength", "true"),
        ],
        &[("format", "opus"), ("estimateContentLength", "false")],
        &[("format", "opus")],
    ];
    for params in cases {
        for method in [Method::GET, Method::POST] {
            let case = format!("{method} {params:?}");
            let response = stream_as(&server, method, id, params, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "{case}");
            assert_eq!(
                header_of(&response, &header::ACCEPT_RANGES),
                Some("none"),
                "{case}"
            );
            assert_eq!(
                header_of(&response, &header::CONTENT_LENGTH),
                None,
                "{case}"
            );
            let body = tokio::time::timeout(SECOND, body_bytes(response))
                .await
                .expect("the body ends with its stream");
            assert_eq!(body.len(), bytes.len(), "{case}");
            assert!(body == bytes, "{case}");
        }
    }
    assert_eq!(server.transcoder().jobs().len(), 8);
    assert_eq!(server.transcoder().open_streams(), 0);

    // A transcoder whose stream has a known length: the response still has
    // no `Content-Length`, with and without the parameter.
    server.transcoder().known_length();
    for params in cases {
        let response = stream(&server, id, params).await;
        assert_eq!(
            header_of(&response, &header::CONTENT_LENGTH),
            None,
            "{params:?}"
        );
        assert!(body_bytes(response).await == bytes, "{params:?}");
    }

    // A HEAD request has no length either.
    let params = [("format", "opus"), ("estimateContentLength", "true")];
    let response = stream_as(&server, Method::HEAD, id, &params, &[]).await;
    assert_transcode_headers(&response, "audio/ogg", "HEAD");
    assert_eq!(header_of(&response, &header::CONTENT_LENGTH), None);

    // The original file has its own length, whatever the parameter says.
    let track = row(server.db(), id).await;
    let params = [("timeOffset", "30"), ("estimateContentLength", "true")];
    let response = stream(&server, id, &params).await;
    assert_eq!(
        header_of(&response, &header::CONTENT_LENGTH),
        Some(track.size.to_string().as_str())
    );
    assert_eq!(header_of(&response, &header::ACCEPT_RANGES), Some("bytes"));
}

#[tokio::test]
async fn request_for_the_format_that_the_file_has_gets_the_original_file() {
    let server = TestServer::standard().await;
    // The MP3 fixture and the Opus fixture, with the format of each.
    for (id, format, other, other_mime) in [
        (&server.track.meridian, "mp3", "opus", "audio/ogg"),
        (&server.track.last_train, "opus", "mp3", "audio/mpeg"),
    ] {
        // The fixtures have about 30 kbps, which is below the lowest
        // bitrate of a transcode. The row gets a bitrate that a limit can
        // be below.
        let bitrate = 128;
        set_bitrate(server.db(), id, bitrate).await;
        let track = row(server.db(), id).await;
        assert_eq!(track.suffix, format);
        let file = resolved(&server, &track);
        let original = fs::read(&file).expect("read the file");
        let jobs_before = server.transcoder().jobs().len();

        // The format of the file, without a limit and with a limit that the
        // file is within: the file itself, with its ranges.
        let at = bitrate.to_string();
        let upper = format.to_uppercase();
        let cases: [Pairs; 5] = [
            &[("format", format)],
            &[("format", &upper)],
            &[("format", format), ("maxBitRate", "0")],
            &[("format", format), ("maxBitRate", &at)],
            &[("format", format), ("maxBitRate", "320")],
        ];
        for params in cases {
            let case = format!("{params:?}");
            let response = stream(&server, id, params).await;
            assert_eq!(content_type(&response), track.content_type, "{case}");
            assert_eq!(
                header_of(&response, &header::ACCEPT_RANGES),
                Some("bytes"),
                "{case}"
            );
            assert_eq!(body_bytes(response).await, original, "{case}");
        }
        assert_eq!(server.transcoder().jobs().len(), jobs_before);

        // A limit below the bitrate of the file: the same format at the
        // limit.
        let target = if format == "mp3" {
            Target::Mp3
        } else {
            Target::Opus
        };
        let below = (bitrate - 1).to_string();
        let params = [("format", format), ("maxBitRate", below.as_str())];
        let response = stream(&server, id, &params).await;
        assert_transcode_headers(&response, &track.content_type, format);
        assert_eq!(
            server.transcoder().jobs().last(),
            Some(&job(id, file.clone(), target, bitrate - 1, 0))
        );

        // The other format: a transcode, although the file is within the
        // limit.
        let params = [("format", other), ("maxBitRate", "320")];
        let response = stream(&server, id, &params).await;
        assert_transcode_headers(&response, other_mime, other);
        assert_eq!(server.transcoder().jobs().len(), jobs_before + 2);
    }
}

#[tokio::test]
async fn busy_transcoder_is_http_503_with_retry_after() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let track = row(server.db(), id).await;
    let original = fs::read(resolved(&server, &track)).expect("read the file");
    server.transcoder().fail_with(Some(FakeFailure::Busy));

    for method in [Method::GET, Method::POST] {
        let asked = Instant::now();
        let params = [("format", "opus"), ("f", "json")];
        let response = stream_as(&server, method.clone(), id, &params, &[]).await;
        assert!(asked.elapsed() < SECOND, "{method}");
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method}"
        );
        assert_eq!(
            header_of(&response, &header::RETRY_AFTER),
            Some("5"),
            "{method}"
        );
        for name in [header::CONTENT_TYPE, header::ACCEPT_RANGES] {
            assert_eq!(header_of(&response, &name), None, "{method} {name}");
        }
        // A browser-based client reads the status of a response of another
        // origin only with this header.
        assert_eq!(
            header_of(&response, &header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some("*"),
            "{method}"
        );
        assert_eq!(body_bytes(response).await, b"", "{method}");
    }

    // A request for the original file needs no transcode slot.
    let response = stream(&server, id, &[("format", "raw")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, original);

    // When a slot is free again, the request gets its stream.
    server.transcoder().fail_with(None);
    let response = stream(&server, id, &[("format", "opus")]).await;
    assert_transcode_headers(&response, "audio/ogg", "a free slot");
    assert_eq!(body_bytes(response).await, FAKE_TRANSCODE);
    assert_eq!(server.transcoder().jobs().len(), 1);
}

#[tokio::test]
async fn server_without_ffmpeg_answers_a_transcode_with_error_0_and_serves_the_file() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    let track = row(server.db(), id).await;
    let original = fs::read(resolved(&server, &track)).expect("read the file");
    let below = (track.bitrate_kbps.expect("a bitrate") - 1).to_string();
    server
        .transcoder()
        .fail_with(Some(FakeFailure::Unavailable));

    let cases: [Pairs; 3] = [
        &[("format", "opus")],
        &[("format", "mp3"), ("estimateContentLength", "true")],
        &[("maxBitRate", &below)],
    ];
    for params in cases {
        assert_eq!(
            stream_error(&server, id, params).await,
            (0, "ffmpeg not available".to_owned()),
            "{params:?}"
        );
    }
    let mut params = vec![("id", id), ("format", "opus")];
    let xml = server.get_xml("stream", &params).await;
    assert!(
        xml.contains(r#"<error code="0" message="ffmpeg not available"/>"#),
        "{xml}"
    );
    params.push(("f", "json"));
    let response = server.post_form("stream", &params).await;
    let envelope = subsonic_response(&body_string(response).await);
    assert_eq!(envelope["error"]["message"], "ffmpeg not available");

    // The requests that select the original file are served.
    let cases: [Pairs; 4] = [
        &[],
        &[("format", "raw")],
        &[("maxBitRate", "0")],
        &[("maxBitRate", "100000")],
    ];
    for params in cases {
        let response = stream(&server, id, params).await;
        assert_eq!(response.status(), StatusCode::OK, "{params:?}");
        assert_eq!(content_type(&response), "audio/flac", "{params:?}");
        assert_eq!(body_bytes(response).await, original, "{params:?}");
    }

    // A process that does not start is error 0 as well.
    server.transcoder().fail_with(Some(FakeFailure::Spawn));
    assert_eq!(
        stream_error(&server, id, &[("format", "opus")]).await,
        (0, "the server cannot start ffmpeg".to_owned())
    );
    assert_eq!(server.transcoder().jobs(), []);
}

#[tokio::test]
async fn request_headers_do_not_change_a_transcode() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();
    // The validators of the file, from a request for the original file.
    let first = stream(&server, id, &[]).await;
    let tag = header_of(&first, &header::ETAG).expect("a tag").to_owned();
    let date = header_of(&first, &header::LAST_MODIFIED)
        .expect("a date")
        .to_owned();

    let cases: [Pairs; 6] = [
        &[("range", "bytes=5-9")],
        &[("range", "bytes=-4")],
        &[("range", "bytes=1000000-")],
        &[("range", "bytes=5-9"), ("if-range", tag.as_str())],
        &[("if-none-match", tag.as_str())],
        &[("if-modified-since", date.as_str())],
    ];
    for headers in cases {
        for method in [Method::GET, Method::POST] {
            let case = format!("{method} {headers:?}");
            let params = [("format", "opus")];
            let response = stream_as(&server, method, id, &params, headers).await;
            assert_transcode_headers(&response, "audio/ogg", &case);
            assert_eq!(body_bytes(response).await, FAKE_TRANSCODE, "{case}");
        }
    }
    assert_eq!(server.transcoder().jobs().len(), 12);
}

#[tokio::test]
async fn head_request_for_a_transcode_gets_the_headers_and_starts_no_transcode() {
    let server = TestServer::standard().await;
    let id = server.track.low_tide.as_str();

    for (format, mime) in [("opus", "audio/ogg"), ("mp3", "audio/mpeg")] {
        let params = [("format", format), ("maxBitRate", "96")];
        let response = stream_as(&server, Method::HEAD, id, &params, &[]).await;
        assert_transcode_headers(&response, mime, format);
        assert_eq!(header_of(&response, &header::CONTENT_LENGTH), None);
        assert_eq!(body_bytes(response).await, b"");
    }

    // The request does not reach the transcoder, so it does not report a
    // transcoder that is busy.
    server.transcoder().fail_with(Some(FakeFailure::Busy));
    let response = stream_as(&server, Method::HEAD, id, &[("format", "opus")], &[]).await;
    assert_transcode_headers(&response, "audio/ogg", "HEAD of a busy transcoder");
    assert_eq!(server.transcoder().jobs(), []);

    // A HEAD request for a file that is not served is the error.
    let response = stream_as(
        &server,
        Method::HEAD,
        "tr-00000000000000000000000000000000",
        &[("format", "opus")],
        &[],
    )
    .await;
    assert_eq!(content_type(&response), "application/xml");
    assert_eq!(header_of(&response, &header::ACCEPT_RANGES), None);
}

/// The time `seconds` Unix seconds.
fn unix_time(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs())
}

/// Sets the modification time of the file at `path`.
fn set_modified(path: &std::path::Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(time))
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

/// Checks that `server` answers every request that selects a transcode of
/// the track `id` with error 70, by GET, by POST and by HEAD.
async fn assert_not_transcoded(server: &TestServer, id: &str, case: &str) {
    let below = "32";
    let cases: [Pairs; 3] = [
        &[("format", "opus")],
        &[("format", "mp3"), ("estimateContentLength", "true")],
        &[("maxBitRate", below)],
    ];
    for params in cases {
        assert_eq!(
            stream_error(server, id, params).await,
            (70, NOT_FOUND.to_owned()),
            "{case} {params:?}"
        );
        let mut form = params.to_vec();
        form.push(("f", "json"));
        let post = stream_as(server, Method::POST, id, &form, &[]).await;
        let envelope = subsonic_response(&body_string(post).await);
        assert_eq!(envelope["error"]["code"], 70, "{case} {params:?}");
        let head = stream_as(server, Method::HEAD, id, params, &[]).await;
        assert_eq!(content_type(&head), "application/xml", "{case}");
        assert_eq!(header_of(&head, &header::ACCEPT_RANGES), None, "{case}");
    }
}

#[tokio::test]
async fn file_that_is_not_served_is_not_given_to_the_transcoder() {
    let server = TestServer::standard_with_own_files().await;
    let id = server.track.low_tide.as_str();
    let before = row(server.db(), id).await;
    let path = server.path(&before.rel_path);
    let original = fs::read(&path).expect("read the file");
    assert!(before.bitrate_kbps.expect("a bitrate") > 32);

    // The file of another track takes the path, with the time of the file
    // that the scan saw.
    let other = row(server.db(), &server.track.meridian).await;
    let other = fs::read(server.path(&other.rel_path)).expect("read the other file");
    assert_ne!(other.len(), original.len());
    fs::write(&path, &other).expect("replace the file");
    set_modified(&path, unix_time(before.mtime));
    assert_not_transcoded(&server, id, "a replaced file").await;

    // The file is gone.
    fs::remove_file(&path).expect("delete the file");
    assert_not_transcoded(&server, id, "a deleted file").await;

    // A file outside the music folder with the size and the time of the
    // row, and a row whose relative path leads to it.
    let outside = tempfile::tempdir().expect("a directory outside the library");
    let secret = outside.path().join("secret.flac");
    fs::write(&secret, &original).expect("write the file outside");
    set_modified(&secret, unix_time(before.mtime));
    let directory = outside.path().file_name().and_then(|name| name.to_str());
    let rel_path = format!("../{}/secret.flac", directory.expect("a Unicode name"));
    let (relinked, leading_out) = (id.to_owned(), rel_path.clone());
    server
        .db()
        .write(move |conn| {
            tracks::relink_track(
                conn,
                &relinked,
                before.folder_id,
                &leading_out,
                before.mtime,
                before.size,
            )
        })
        .await
        .expect("change the path of the row");
    assert_eq!(fs::read(server.path(&rel_path)).unwrap(), original);
    assert_not_transcoded(&server, id, "a path out of the folder").await;

    // A track without a file.
    let placeholder = tracks::placeholder_path(id);
    let relinked = id.to_owned();
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
    assert_not_transcoded(&server, id, "a track without a file").await;

    // No request of this test reached the transcoder. The other tracks of
    // the album are transcoded.
    assert_eq!(server.transcoder().jobs(), []);
    let response = stream(&server, &server.track.breakwater, &[("format", "opus")]).await;
    assert_transcode_headers(&response, "audio/ogg", "another track");
    assert_eq!(server.transcoder().jobs().len(), 1);
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege that a test run does not have.
#[cfg(unix)]
#[tokio::test]
async fn link_out_of_the_music_folder_is_not_given_to_the_transcoder() {
    use std::os::unix::fs::symlink;

    let library = LibraryBuilder::standard().build();
    let root = library.root.path().to_path_buf();
    let outside = tempfile::tempdir().expect("a directory outside the library");
    let album = root.join("Aurora Field").join("Glass Harbour");

    // The file of one track is outside the folder, and a link is at its
    // path. The scan follows the link and gives the file its row.
    let link = album.join("01 Low Tide.flac");
    let target = outside.path().join("01 Low Tide.flac");
    fs::rename(&link, &target).unwrap();
    symlink(&target, &link).unwrap();
    // A link inside the folder to the file of another track.
    symlink("02 Breakwater.flac", album.join("04 Linked.flac")).unwrap();

    let server = TestServer::scanned(library).await;
    let catalogue = server.catalogue().await;
    let low_tide = catalogue.song("Low Tide").track.clone();
    assert!(server.path(&low_tide.rel_path).is_file());
    assert_not_transcoded(&server, &low_tide.id, "a link out of the folder").await;
    assert_eq!(server.transcoder().jobs(), []);

    // The transcoder gets the resolved path of a file behind a link that
    // stays in the folder: the path of the file itself.
    let id: String = server
        .db()
        .read(|conn| {
            let id = conn.query_row(
                "SELECT id FROM tracks WHERE rel_path LIKE '%04 Linked.flac'",
                [],
                |row| row.get(0),
            )?;
            Ok(id)
        })
        .await
        .expect("the track of the link");
    let response = stream(&server, &id, &[("format", "opus")]).await;
    assert_transcode_headers(&response, "audio/ogg", "a link in the folder");
    let breakwater = fs::canonicalize(album.join("02 Breakwater.flac")).unwrap();
    assert_eq!(
        server.transcoder().jobs(),
        [job(&id, breakwater, Target::Opus, 192, 0)]
    );
}

#[tokio::test]
async fn get_open_subsonic_extensions_lists_transcode_offset() {
    let server = TestServer::bare(Config::default()).await;
    let response = server
        .get_json_noauth("getOpenSubsonicExtensions", &[])
        .await;
    let extensions = response["openSubsonicExtensions"]
        .as_array()
        .expect("an array of extensions");
    let transcode_offset = serde_json::json!({"name": "transcodeOffset", "versions": [1]});
    assert!(extensions.contains(&transcode_offset), "{response}");

    let xml = server.get_xml("getOpenSubsonicExtensions", &[]).await;
    assert!(
        xml.contains(
            r#"<openSubsonicExtensions name="transcodeOffset"><versions>1</versions></openSubsonicExtensions>"#
        ),
        "{xml}"
    );
}

/// Serves the router of `server` on `127.0.0.1` with a port that the system
/// chooses, and returns the address. The task that serves ends with the
/// runtime of the test.
async fn listen(server: Arc<StandardServer>) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("the address of the listener");
    let router = Router::new().fallback(move |request: Request<Body>| {
        let server = Arc::clone(&server);
        async move { server.request(request).await }
    });
    tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve the router");
    });
    addr
}

/// Opens a connection to `addr` and sends a GET request for `path` with the
/// request headers `headers`, each a whole header line.
async fn send_get(addr: SocketAddr, path: &str, headers: &[&str]) -> TcpStream {
    let mut connection = tokio::time::timeout(LIMIT, TcpStream::connect(addr))
        .await
        .expect("the connection opens in time")
        .expect("the server accepts the connection");
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
    for header in headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    connection
        .write_all(request.as_bytes())
        .await
        .expect("send the request");
    connection
}

/// Reads from `connection` until the server ends it, for `limit` at most.
/// Returns the head of the response in lower case and the bytes after it.
async fn read_response(connection: &mut TcpStream, limit: Duration) -> (String, Vec<u8>) {
    let mut received = Vec::new();
    let ended = async {
        loop {
            match connection.read_buf(&mut received).await {
                // A connection that the server aborts ends with a reset on
                // some systems, which counts as the end.
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    };
    tokio::time::timeout(limit, ended)
        .await
        .expect("the server ends the connection");
    let end = received
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the end of the response head");
    let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
    (head, received[end + 4..].to_vec())
}

#[tokio::test]
async fn client_that_disconnects_ends_the_stream_of_the_transcoder() {
    let server = Arc::new(TestServer::standard().await);
    // The stream stays open after its bytes, as the output of ffmpeg does
    // while the process runs.
    server.transcoder().stay_open();
    let addr = listen(Arc::clone(&server)).await;

    let path = format!(
        "/rest/stream?{}",
        query(&[("id", &server.track.low_tide), ("format", "opus")])
    );
    let mut client = send_get(addr, &path, &[]).await;
    let mut received = Vec::new();
    let marker = String::from_utf8_lossy(FAKE_TRANSCODE).into_owned();
    let arrived = async {
        while !String::from_utf8_lossy(&received).contains(&marker) {
            let read = client
                .read_buf(&mut received)
                .await
                .expect("read the response");
            assert_ne!(read, 0, "the connection ended");
        }
    };
    tokio::time::timeout(LIMIT, arrived)
        .await
        .expect("the bytes of the transcode arrive");
    let head = String::from_utf8_lossy(&received).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200 ok\r\n"), "{head}");
    assert!(head.contains("content-type: audio/ogg\r\n"), "{head}");
    // The transcode runs while the client is connected.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.transcoder().open_streams(), 1);

    // The client disconnects. The server drops the body, and the body
    // drops the stream of the transcoder.
    drop(client);
    let ended = async {
        while server.transcoder().open_streams() > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(SECOND, ended)
        .await
        .expect("the stream of the transcoder is dropped within one second");
}

#[tokio::test]
async fn server_without_ffmpeg_starts_with_one_warning_and_serves_original_files() {
    let log = captured_log();
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .build();
    let root = library.root.path().to_path_buf();
    let original = fs::read(&library.files[0]).expect("read the file");

    // A data directory whose database holds the owner account and the
    // scanned library, as `init` and a scan leave it.
    let dir = tempfile::tempdir().expect("a temporary directory");
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
    let db = Db::open(data_dir.db_path()).expect("create the database");
    db.write(|conn| users::insert_owner(conn, OWNER, OWNER_PASSWORD))
        .await
        .expect("insert the owner")
        .expect("a new database has no owner");
    let summary = tokio::task::spawn_blocking({
        let (db, folders) = (db.clone(), vec![root.clone()]);
        move || {
            let cancel = CancellationToken::new();
            scanner::scan(&db, &folders, &cancel, false, ScanOptions::default())
        }
    })
    .await
    .expect("the scan does not panic")
    .expect("the scan reads and writes the database");
    assert_eq!(summary.errors, [] as [&str; 0]);
    let id: String = db
        .read(|conn| Ok(conn.query_row("SELECT id FROM tracks", [], |row| row.get(0))?))
        .await
        .expect("the track of the library");
    // The server opens the database itself.
    drop(db);

    // The config names a program that does not exist. The name is in the
    // directory of this test, so no other test logs it.
    let missing = dir.path().join("no ffmpeg in this directory");
    let mut config = Config::default();
    config.server.bind = "127.0.0.1".parse().expect("an address");
    config.server.port = 0;
    config.library.folders = vec![root];
    config.library.scan_on_start = false;
    config.transcoding.ffmpeg = missing.to_str().expect("a Unicode path").to_owned();

    let cancel = CancellationToken::new();
    let (listening, bound) = oneshot::channel();
    let options = ServerOptions {
        listening: Some(listening),
        ..ServerOptions::default()
    };
    let mut task = tokio::spawn({
        let (cancel, data_dir) = (cancel.clone(), data_dir.clone());
        async move { run_server(config, &data_dir, cancel, options).await }
    });
    // The server starts.
    let addr = tokio::time::timeout(LIMIT, bound)
        .await
        .expect("the listener is bound")
        .expect("the server sends its address");

    // The log has one warning that names the program, and no request adds
    // another one.
    let warnings = || -> Vec<String> {
        log.lines_with(missing.to_str().expect("a Unicode path"))
            .into_iter()
            .filter(|line| line.contains("WARN"))
            .collect()
    };
    let at_start = warnings();
    assert_eq!(at_start.len(), 1, "{at_start:?}");
    assert!(
        at_start[0].contains("ffmpeg is not available"),
        "{at_start:?}"
    );

    // The original file is served.
    let close = ["Connection: close"];
    for params in [
        vec![("id", id.as_str())],
        vec![("id", &id), ("format", "raw")],
    ] {
        let path = format!("/rest/stream?{}", query(&params));
        let mut client = send_get(addr, &path, &close).await;
        let (head, body) = read_response(&mut client, LIMIT).await;
        assert!(head.starts_with("http/1.1 200 ok\r\n"), "{head}");
        assert!(head.contains("content-type: audio/flac\r\n"), "{head}");
        assert!(body == original, "{params:?}");
    }

    // A request that needs a transcode gets error 0.
    for params in [
        vec![("id", id.as_str()), ("format", "opus"), ("f", "json")],
        vec![("id", &id), ("format", "mp3"), ("f", "json")],
        vec![("id", &id), ("maxBitRate", "32"), ("f", "json")],
    ] {
        let path = format!("/rest/stream?{}", query(&params));
        let mut client = send_get(addr, &path, &close).await;
        let (head, body) = read_response(&mut client, LIMIT).await;
        assert!(head.starts_with("http/1.1 200 ok\r\n"), "{head}");
        assert!(head.contains("content-type: application/json"), "{head}");
        let envelope = subsonic_response(&String::from_utf8_lossy(&body));
        assert_eq!(envelope["status"], "failed", "{params:?}");
        assert_eq!(envelope["error"]["code"], 0, "{params:?}");
        assert_eq!(
            envelope["error"]["message"], "ffmpeg not available",
            "{params:?}"
        );
    }
    assert_eq!(warnings(), at_start);

    cancel.cancel();
    tokio::time::timeout(LIMIT, &mut task)
        .await
        .expect("the server ends")
        .expect("the server does not panic")
        .expect("the server stops in order");
}
