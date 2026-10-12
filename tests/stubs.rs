//! Router tests of the methods that answer without the library
//! (`docs/design.md` section 4): the empty results of deferred features, the
//! error stubs and the disabled `download`.

mod common;

use std::fs;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::config::Config;
use wn_server::db::queries::folders;
use wn_server::ids::{self, Kind};

use common::{
    LibraryBuilder, OWNER, OWNER_PASSWORD, TestLibrary, TestServer, body_string, content_type,
    form_request, token, urlencode,
};

/// Parameters of a request: names and values.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The protocol version that the requests without the auth defaults send.
const V: (&str, &str) = ("v", "1.16.1");

/// The client name that the requests without the auth defaults send.
const C: (&str, &str) = ("c", "stubs-tests");

/// The salt of the tokens that these tests send.
const SALT: &str = "c19b2d";

/// The hex encoding of the owner password `sesame`.
const SESAME_HEX: &str = "enc:736573616d65";

/// The message of the error that `download` returns.
const DOWNLOAD_DISABLED: &str = "download is disabled on this server";

/// The message of the error that the user management methods return.
const USER_MANAGEMENT: &str = "user management is not available on this server";

/// What a stub answers to a request that has a user.
#[derive(Debug, Clone, Copy)]
enum Answer {
    /// The status `ok` and one entry: this key with the value of this JSON
    /// text.
    Result(&'static str, &'static str),
    /// The status `failed` and the error with this code and this message.
    Error(u32, &'static str),
}

/// Every stub with its answer, in the order of the table in
/// `docs/design.md` section 4.
const STUBS: [(&str, Answer); 19] = [
    ("getArtistInfo", Answer::Result("artistInfo", "{}")),
    ("getArtistInfo2", Answer::Result("artistInfo2", "{}")),
    ("getAlbumInfo", Answer::Result("albumInfo", "{}")),
    ("getAlbumInfo2", Answer::Result("albumInfo", "{}")),
    ("getSimilarSongs", Answer::Result("similarSongs", "{}")),
    ("getSimilarSongs2", Answer::Result("similarSongs2", "{}")),
    ("getTopSongs", Answer::Result("topSongs", "{}")),
    (
        "getNowPlaying",
        Answer::Result("nowPlaying", r#"{"entry":[]}"#),
    ),
    ("download", Answer::Error(50, DOWNLOAD_DISABLED)),
    ("getAvatar", Answer::Error(70, "not found")),
    ("getShares", Answer::Result("shares", r#"{"share":[]}"#)),
    (
        "getPodcasts",
        Answer::Result("podcasts", r#"{"channel":[]}"#),
    ),
    (
        "getNewestPodcasts",
        Answer::Result("newestPodcasts", r#"{"episode":[]}"#),
    ),
    (
        "getInternetRadioStations",
        Answer::Result("internetRadioStations", r#"{"internetRadioStation":[]}"#),
    ),
    (
        "getBookmarks",
        Answer::Result("bookmarks", r#"{"bookmark":[]}"#),
    ),
    ("createUser", Answer::Error(50, USER_MANAGEMENT)),
    ("updateUser", Answer::Error(50, USER_MANAGEMENT)),
    ("deleteUser", Answer::Error(50, USER_MANAGEMENT)),
    ("changePassword", Answer::Error(50, USER_MANAGEMENT)),
];

/// The statuses that the table in `docs/design.md` section 4 gives a stub.
const STUB_STATUSES: [&str; 6] = [
    "empty result stubs",
    "empty list stub",
    "empty list stubs",
    "disabled: error 50",
    "error 70",
    "deferred: error 50",
];

impl Answer {
    /// The JSON body of the answer.
    fn json(self) -> String {
        match self {
            Answer::Result(key, value) => json_document("ok", &format!(r#","{key}":{value}"#)),
            Answer::Error(code, message) => Format::Json.error(code, message),
        }
    }

    /// The XML body of the answer. An object without fields is an empty
    /// element, and so is an object whose one field is an empty list.
    fn xml(self) -> String {
        match self {
            Answer::Result(key, _) => xml_document("ok", &format!("><{key}/></subsonic-response>")),
            Answer::Error(code, message) => Format::Xml.error(code, message),
        }
    }
}

/// The format of a response.
#[derive(Debug, Clone, Copy)]
enum Format {
    /// The request has `f=json`.
    Json,
    /// The request has no `f`.
    Xml,
}

const FORMATS: [Format; 2] = [Format::Json, Format::Xml];

impl Format {
    /// The parameter that selects the format.
    fn params(self) -> Pairs<'static> {
        match self {
            Format::Json => &[("f", "json")],
            Format::Xml => &[],
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Xml => "application/xml",
        }
    }

    /// The body of a failed response with `code` and `message`.
    fn error(self, code: u32, message: &str) -> String {
        match self {
            Format::Json => json_document(
                "failed",
                &format!(r#","error":{{"code":{code},"message":"{message}"}}"#),
            ),
            Format::Xml => xml_document(
                "failed",
                &format!(r#"><error code="{code}" message="{message}"/></subsonic-response>"#),
            ),
        }
    }
}

/// How a request carries its parameters.
#[derive(Debug, Clone, Copy)]
enum Transport {
    /// GET with the parameters in the query string.
    Get,
    /// POST with the parameters in a form body.
    PostForm,
    /// POST with the parameters in the query string and no body.
    PostQuery,
}

const TRANSPORTS: [Transport; 3] = [Transport::Get, Transport::PostForm, Transport::PostQuery];

/// The two path forms of `download`.
const DOWNLOAD_NAMES: [&str; 2] = ["download", "download.view"];

/// The request for the method `name` that carries `params` by `transport`.
fn request(transport: Transport, name: &str, params: Pairs<'_>) -> Request<Body> {
    let path = format!("/rest/{name}");
    let with_query = format!("{path}?{}", urlencode(params));
    let request = match transport {
        Transport::Get => Request::get(with_query).body(Body::empty()),
        Transport::PostQuery => Request::post(with_query).body(Body::empty()),
        Transport::PostForm => return form_request(&path, params),
    };
    request.expect("a request")
}

/// The JSON body that has `status` and then `rest`. `rest` is the text that
/// follows the envelope fields inside the response object.
fn json_document(status: &str, rest: &str) -> String {
    format!(
        concat!(
            r#"{{"subsonic-response":{{"status":"{}","version":"1.16.1","type":"wn-server","#,
            r#""serverVersion":"{}","openSubsonic":true{}}}}}"#
        ),
        status, VERSION, rest
    )
}

/// The XML body that has `status` and then `rest`. `rest` is the text that
/// follows the last attribute of the root element.
fn xml_document(status: &str, rest: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="{}""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true"{}"#
        ),
        status, VERSION, rest
    )
}

/// A server that authenticates.
async fn server() -> TestServer {
    TestServer::bare(Config::default()).await
}

/// The config with `auth.disabled_unsafe = true`.
fn config_without_auth() -> Config {
    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    config
}

/// The methods of `docs/design.md` section 4 whose status is one of
/// `statuses`, in the order of the table.
fn design_methods(statuses: &[&str]) -> Vec<&'static str> {
    const DESIGN: &str = include_str!("../docs/design.md");
    let (_, rest) = DESIGN
        .split_once("\n## 4. OpenSubsonic coverage\n")
        .expect("the design has section 4");
    let (section, _) = rest
        .split_once("\n## 5. ")
        .expect("section 5 follows section 4");
    section
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|row| row.strip_suffix(" |"))
        .filter_map(|row| row.split_once(" | "))
        .filter(|(_, status)| statuses.contains(status))
        .flat_map(|(names, _)| names.split(", "))
        .collect()
}

#[test]
fn stub_table_holds_every_stub_of_design_section_4() {
    let stubs: Vec<&str> = STUBS.iter().map(|(method, _)| *method).collect();
    assert_eq!(stubs, design_methods(&STUB_STATUSES));
}

#[tokio::test]
async fn every_stub_answers_with_its_key_or_its_error() {
    for server in [
        server().await,
        TestServer::bare(config_without_auth()).await,
    ] {
        for (method, answer) in STUBS {
            for name in [method.to_owned(), format!("{method}.view")] {
                let response = server.get_json(&name, &[]).await;
                match answer {
                    Answer::Result(key, value) => {
                        let value: Value = serde_json::from_str(value).expect("JSON text");
                        assert_eq!(response["status"], "ok", "{name}");
                        assert_eq!(response[key], value, "{name}");
                    }
                    Answer::Error(code, message) => {
                        assert_eq!(response["status"], "failed", "{name}");
                        assert_eq!(
                            response["error"],
                            json!({ "code": code, "message": message }),
                            "{name}"
                        );
                    }
                }
                // The five envelope fields and the one entry of the answer.
                assert_eq!(
                    response.as_object().map(serde_json::Map::len),
                    Some(6),
                    "{name}: {response}"
                );

                let response = server.get_raw(&name, &[("f", "json")]).await;
                assert_eq!(response.status(), StatusCode::OK, "{name}");
                assert_eq!(body_string(response).await, answer.json(), "{name}");
                assert_eq!(server.get_xml(&name, &[]).await, answer.xml(), "{name}");

                // A stub reads no parameter, so the parameters that a client
                // sends to the method do not change the answer.
                let sent = [
                    ("f", "json"),
                    ("id", "ar-0123456789abcdef0123456789abcdef"),
                    ("count", "5"),
                    ("username", OWNER),
                    ("password", "new-password"),
                ];
                let response = server.post_form(&name, &sent).await;
                assert_eq!(response.status(), StatusCode::OK, "{name}");
                assert_eq!(content_type(&response), "application/json", "{name}");
                assert_eq!(body_string(response).await, answer.json(), "{name}");
            }
        }
    }
}

/// A server whose database holds one track, and the file of that track.
struct Library {
    server: TestServer,
    /// The ID of the track.
    track_id: String,
    /// The bytes of the file of the track.
    file: Vec<u8>,
    /// The directory that holds the file of the track. It keeps the file on
    /// disk for the life of the test.
    _files: TestLibrary,
}

/// A server with `config` whose library holds one FLAC track. The file of
/// the track is on disk, and the `folders` and `tracks` rows name it, so the
/// ID of the track is the ID of a file that exists.
async fn library(config: Config) -> Library {
    let files = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .build();
    let path = files.files[0].clone();
    let rel_path = "Aurora Field/Glass Harbour/01 Low Tide.flac";
    assert_eq!(path, files.root.path().join(rel_path));
    let file = fs::read(&path).expect("read the file of the track");
    assert!(file.starts_with(b"fLaC"), "the file is not a FLAC file");
    let size = i64::try_from(file.len()).expect("the size of a small file");
    let fingerprint = ids::fingerprint(&path).expect("the fingerprint of the file");

    let root = files.root.path().to_path_buf();
    let track_id = ids::new_id(Kind::Track);
    let album_id = ids::new_id(Kind::Album);
    let artist_id = ids::new_id(Kind::Artist);
    let server = TestServer::bare(config).await;
    let id = track_id.clone();
    server
        .db()
        .write(move |conn| {
            let folder_id = folders::sync_folders(conn, &[root])?[0].id;
            conn.execute(
                "INSERT INTO artists (id, key, name, sort_name)
                 VALUES (?1, 'aurora field', 'Aurora Field', 'Aurora Field')",
                params![artist_id],
            )?;
            conn.execute(
                "INSERT INTO albums (id, artist_id, key, name, artist_name, created_at)
                 VALUES (?1, ?2, 'glass harbour', 'Glass Harbour', 'Aurora Field', 0)",
                params![album_id, artist_id],
            )?;
            conn.execute(
                "INSERT INTO tracks (
                     id, folder_id, rel_path, fingerprint, title, title_lc, album_id, artist_id,
                     artist_name, album_name, duration_ms, size, suffix, content_type, lossless,
                     mtime, created_at, updated_at
                 )
                 VALUES (
                     ?1, ?2, ?3, ?4, 'Low Tide', 'low tide', ?5, ?6,
                     'Aurora Field', 'Glass Harbour', 500, ?7, 'flac', 'audio/flac', 1,
                     0, 0, 0
                 )",
                params![
                    id,
                    folder_id,
                    rel_path,
                    fingerprint,
                    album_id,
                    artist_id,
                    size
                ],
            )?;
            Ok(())
        })
        .await
        .expect("insert the track");
    Library {
        server,
        track_id,
        file,
        _files: files,
    }
}

/// Whether `part` is `whole` or a run of bytes of `whole`. The body of a
/// response that serves a file, in full or as a range, is such a part of the
/// file.
fn is_part_of(part: &[u8], whole: &[u8]) -> bool {
    !part.is_empty() && whole.windows(part.len()).any(|window| window == part)
}

impl Library {
    /// Sends `request` and checks that the response is the error `code` with
    /// `message` in `format` and nothing else:
    ///
    /// - HTTP 200 with the content type of the format.
    /// - None of the headers that a response with the bytes of a file
    ///   carries (`docs/design.md` section 3.9).
    /// - A body that equals the error document byte for byte, and that is no
    ///   part of the file of the track.
    async fn expect_error(
        &self,
        request: Request<Body>,
        format: Format,
        code: u32,
        message: &str,
        case: &str,
    ) {
        let response = self.server.request(request).await;
        assert_eq!(response.status(), StatusCode::OK, "{case}");
        assert_eq!(content_type(&response), format.content_type(), "{case}");
        for name in [
            header::ACCEPT_RANGES,
            header::CONTENT_RANGE,
            header::CONTENT_DISPOSITION,
            header::ETAG,
            header::LAST_MODIFIED,
        ] {
            assert!(!response.headers().contains_key(&name), "{case}: {name}");
        }
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the response body");
        assert_eq!(
            String::from_utf8_lossy(&body),
            format.error(code, message),
            "{case}: the body is not the error document"
        );
        assert!(
            !is_part_of(&body, &self.file),
            "{case}: the body is a part of the file"
        );
    }
}

#[test]
fn part_of_a_file_is_recognised() {
    let file = b"fLaC\x00\x00\x00\x22audio";
    assert!(is_part_of(file, file));
    assert!(is_part_of(b"fLaC", file));
    assert!(is_part_of(b"\x00\x22au", file));
    assert!(is_part_of(b"audio", file));
    assert!(!is_part_of(b"fLaC\x01", file));
    assert!(!is_part_of(b"<error/>", file));
    assert!(!is_part_of(b"", file));
}

#[tokio::test]
async fn download_is_error_50_and_no_byte_of_the_file_in_every_request_form() {
    let library = library(Config::default()).await;
    let id = library.track_id.as_str();
    let sesame_token = token(OWNER_PASSWORD, SALT);
    let credentials: [Pairs; 3] = [
        &[("u", OWNER), ("p", OWNER_PASSWORD)],
        &[("u", OWNER), ("p", SESAME_HEX)],
        &[("u", OWNER), ("t", &sesame_token), ("s", SALT)],
    ];
    // The ID of the track whose file exists, alone and with the parameters
    // of `stream` that select the original file. Then an ID that names no
    // track, and no ID.
    let arguments: [Pairs; 5] = [
        &[("id", id)],
        &[("id", id), ("format", "raw")],
        &[
            ("id", id),
            ("maxBitRate", "0"),
            ("estimateContentLength", "true"),
        ],
        &[("id", "tr-00000000000000000000000000000000")],
        &[],
    ];

    let mut requests = 0;
    for name in DOWNLOAD_NAMES {
        for transport in TRANSPORTS {
            for credential in credentials {
                for argument in arguments {
                    for format in FORMATS {
                        let mut params = vec![V, C];
                        params.extend_from_slice(credential);
                        params.extend_from_slice(argument);
                        params.extend_from_slice(format.params());
                        let case = format!("{transport:?} {name} {argument:?} {format:?}");
                        let request = request(transport, name, &params);
                        library
                            .expect_error(request, format, 50, DOWNLOAD_DISABLED, &case)
                            .await;
                        requests += 1;
                    }
                }
            }
        }
    }
    assert_eq!(requests, 2 * 3 * 3 * 5 * 2);
}

#[tokio::test]
async fn download_without_a_user_is_the_authentication_error_and_no_byte_of_the_file() {
    let library = library(Config::default()).await;
    let id = library.track_id.as_str();
    let wrong_token = token("wrong", SALT);
    let rejected: [(Pairs, u32, &str); 9] = [
        (&[], 10, "required parameter is missing: v"),
        (&[V], 10, "required parameter is missing: c"),
        (&[V, C], 10, "required parameter is missing: u"),
        (
            &[V, C, ("u", OWNER)],
            10,
            "required parameter is missing: p, or t and s",
        ),
        (
            &[V, C, ("u", OWNER), ("p", "wrong")],
            40,
            "wrong username or password",
        ),
        (
            &[V, C, ("u", "nobody"), ("p", OWNER_PASSWORD)],
            40,
            "wrong username or password",
        ),
        (
            &[V, C, ("u", OWNER), ("t", &wrong_token), ("s", SALT)],
            40,
            "wrong username or password",
        ),
        (
            &[V, C, ("apiKey", "some-key")],
            42,
            "authentication mechanism not supported",
        ),
        (
            &[V, C, ("u", OWNER), ("p", OWNER_PASSWORD), ("t", "abc")],
            43,
            "conflicting authentication mechanisms",
        ),
    ];

    for name in DOWNLOAD_NAMES {
        for transport in TRANSPORTS {
            for (credential, code, message) in rejected {
                for format in FORMATS {
                    let mut params = vec![("id", id)];
                    params.extend_from_slice(credential);
                    params.extend_from_slice(format.params());
                    let case = format!("{transport:?} {name} error {code} {format:?}");
                    let request = request(transport, name, &params);
                    library
                        .expect_error(request, format, code, message, &case)
                        .await;
                }
            }
        }
    }
}

#[tokio::test]
async fn download_is_error_50_when_auth_is_disabled() {
    let library = library(config_without_auth()).await;
    let id = library.track_id.as_str();
    // No credential is read, so each of these requests acts as the owner.
    let credentials: [Pairs; 3] = [
        &[],
        &[("u", OWNER), ("p", OWNER_PASSWORD)],
        &[("u", "nobody"), ("p", "wrong")],
    ];
    for name in DOWNLOAD_NAMES {
        for transport in TRANSPORTS {
            for credential in credentials {
                for format in FORMATS {
                    let mut params = vec![V, C, ("id", id)];
                    params.extend_from_slice(credential);
                    params.extend_from_slice(format.params());
                    let case = format!("{transport:?} {name} {credential:?} {format:?}");
                    let request = request(transport, name, &params);
                    library
                        .expect_error(request, format, 50, DOWNLOAD_DISABLED, &case)
                        .await;
                }
            }
        }
    }
}

#[tokio::test]
async fn download_does_not_answer_the_headers_of_a_file_request() {
    let library = library(Config::default()).await;
    let query = urlencode(&[
        ("id", library.track_id.as_str()),
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        V,
        C,
        ("f", "json"),
    ]);
    for name in DOWNLOAD_NAMES {
        // A range request and the conditions of a cached copy get the error
        // with HTTP 200, not HTTP 206 or 304.
        let headers = [
            (header::RANGE, "bytes=0-99"),
            (header::IF_RANGE, "\"some-tag\""),
            (header::IF_NONE_MATCH, "*"),
            (header::IF_MODIFIED_SINCE, "Thu, 01 Jan 1970 00:00:00 GMT"),
        ];
        for (header_name, value) in headers {
            let request = Request::get(format!("/rest/{name}?{query}"))
                .header(&header_name, value)
                .body(Body::empty())
                .expect("a request");
            let case = format!("{name} {header_name}");
            library
                .expect_error(request, Format::Json, 50, DOWNLOAD_DISABLED, &case)
                .await;
        }

        // The router answers HEAD as it answers GET, without the body.
        let request = Request::head(format!("/rest/{name}?{query}"))
            .body(Body::empty())
            .expect("a request");
        let response = library.server.request(request).await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(content_type(&response), "application/json", "{name}");
        assert!(body_string(response).await.is_empty(), "{name}");
    }
}
