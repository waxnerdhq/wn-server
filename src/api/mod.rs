//! The OpenSubsonic API: the router, the method table and the state that the
//! handlers share (`docs/design.md` section 3.8).
//!
//! # Lists that hold one line per entry
//!
//! The list of handler modules, [`AppState`], [`METHODS`] and [`EXTENSIONS`]
//! hold one line for each entry that the design names, and a blank line
//! follows every entry. An entry that the server does not have yet is a
//! comment line with its name, or a `METHODS` line that maps the name to
//! `not_implemented`.
//!
//! A change that adds an entry replaces the line of that entry and keeps the
//! blank lines. Two changes then never touch neighbouring lines, and git
//! merges them without a conflict. rustfmt removes blank lines inside a list,
//! so these items carry `#[rustfmt::skip]`. The two places that build an
//! `AppState` follow the same rule.

pub mod envelope;
pub mod model;
pub mod params;
pub mod xml;

// The handler modules, in the order of `docs/design.md` section 3.1.

pub mod system;

// browse

// folders

// lists

// search

// annotate

// playlists

// queue

// lyrics

// users

// stubs

// media

// covers

// scan

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{self, Body};
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{self, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing::field::Empty;

use crate::config::Config;
use envelope::{ApiError, Format, SubsonicResponse};
use params::Params;

/// The largest form body the router reads, in bytes. The body is read before
/// the request is authenticated, so the limit bounds the memory that one
/// request can take. A play queue of 100 000 track IDs is a body of about
/// 4.4 MB.
const FORM_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// The content type of a form body.
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// The state that the handlers share. `docs/design.md` section 3.8 names the
/// fields, and they are in that order here.
#[derive(Clone)]
#[rustfmt::skip]
pub struct AppState {
    /// The configuration the server runs with.
    pub config: Arc<Config>,

    // db

    // covers

    // transcoder

    // scan

    // cancel

    /// The moment the server started.
    pub started_at: Instant,
}

/// What a handler returns when its method succeeds: nothing, or one value
/// under one key of the response envelope.
#[derive(Debug, Clone, PartialEq)]
pub struct Payload(SubsonicResponse);

impl Payload {
    /// A success that carries nothing.
    pub fn empty() -> Self {
        Self(SubsonicResponse::empty())
    }

    /// A success that carries `value` under `key`. `value` is a response
    /// model (see [`model`]).
    pub fn new(key: &str, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).expect("a response model serialises to JSON");
        Self(SubsonicResponse::ok(key, value))
    }
}

/// The result of a handler that answers with the response envelope.
pub type ApiResult = Result<Payload, ApiError>;

/// A handler can return a `Payload` or an `ApiError`. Neither knows the
/// format of the request, so the response carries the envelope as an
/// extension and has no body. The router renders the envelope.
impl IntoResponse for Payload {
    fn into_response(self) -> Response {
        unrendered(self.0)
    }
}

/// See the implementation for [`Payload`].
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        unrendered(failure(&self))
    }
}

/// A response without a body that carries `envelope` for the router.
fn unrendered(envelope: SubsonicResponse) -> Response {
    let mut response = Response::new(Body::empty());
    response.extensions_mut().insert(envelope);
    response
}

/// The envelope of `error`.
fn failure(error: &ApiError) -> SubsonicResponse {
    SubsonicResponse::error(error.code(), &error.message())
}

/// Renders `envelope` in `format` as an HTTP 200 response.
fn render(envelope: &SubsonicResponse, format: Format) -> Response {
    let (content_type, body) = envelope.render(format);
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// The future of one call of a [`Handler`].
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// The handler of one API method, as [`METHODS`] holds it. The request
/// carries the [`Params`] of the call as an extension. The state is
/// [`AppState`]; the tests of this module use handlers without a state.
pub type Handler<S = AppState> = fn(Request, S) -> HandlerFuture;

/// Makes a [`Handler`] from an axum handler function: an `async fn` whose
/// arguments are extractors and whose result implements `IntoResponse`.
///
/// The extractors a handler uses are [`Params`] and `State<AppState>`. A
/// handler that answers with the envelope returns [`ApiResult`]. A handler
/// that answers with a body of its own returns `Result<Response, ApiError>`.
macro_rules! handler {
    ($function:path) => {
        |request, state| ::axum::handler::Handler::call($function, request, state)
    };
}

/// The handler of a method that the table lists and the server does not
/// serve. The response is error 0 with the message `not supported`.
fn not_implemented(_request: Request, _state: AppState) -> HandlerFuture {
    Box::pin(async { ApiError::Other("not supported".to_owned()).into_response() })
}

/// Every API method name of `docs/design.md` section 4, in the order of that
/// table, with its handler. A request for a name that is not here gets
/// error 0 with the message `unknown endpoint`.
#[rustfmt::skip]
pub const METHODS: &[(&str, Handler)] = &[
    ("ping", handler!(system::ping)),

    ("getLicense", handler!(system::get_license)),

    ("getOpenSubsonicExtensions", handler!(system::get_open_subsonic_extensions)),

    ("tokenInfo", not_implemented),

    ("getMusicFolders", not_implemented),

    ("getArtists", not_implemented),

    ("getArtist", not_implemented),

    ("getAlbum", not_implemented),

    ("getSong", not_implemented),

    ("getGenres", not_implemented),

    ("getIndexes", not_implemented),

    ("getMusicDirectory", not_implemented),

    ("getVideos", not_implemented),

    ("getVideoInfo", not_implemented),

    ("getArtistInfo", not_implemented),

    ("getArtistInfo2", not_implemented),

    ("getAlbumInfo", not_implemented),

    ("getAlbumInfo2", not_implemented),

    ("getSimilarSongs", not_implemented),

    ("getSimilarSongs2", not_implemented),

    ("getTopSongs", not_implemented),

    ("getAlbumList", not_implemented),

    ("getAlbumList2", not_implemented),

    ("getRandomSongs", not_implemented),

    ("getSongsByGenre", not_implemented),

    ("getNowPlaying", not_implemented),

    ("getStarred", not_implemented),

    ("getStarred2", not_implemented),

    ("search", not_implemented),

    ("search2", not_implemented),

    ("search3", not_implemented),

    ("getPlaylists", not_implemented),

    ("getPlaylist", not_implemented),

    ("createPlaylist", not_implemented),

    ("updatePlaylist", not_implemented),

    ("deletePlaylist", not_implemented),

    ("stream", not_implemented),

    ("download", not_implemented),

    ("hls", not_implemented),

    ("getCaptions", not_implemented),

    ("getCoverArt", not_implemented),

    ("getLyrics", not_implemented),

    ("getLyricsBySongId", not_implemented),

    ("getAvatar", not_implemented),

    ("star", not_implemented),

    ("unstar", not_implemented),

    ("setRating", not_implemented),

    ("scrobble", not_implemented),

    ("getShares", not_implemented),

    ("getPodcasts", not_implemented),

    ("getNewestPodcasts", not_implemented),

    ("getInternetRadioStations", not_implemented),

    ("getBookmarks", not_implemented),

    ("createShare", not_implemented),

    ("updateShare", not_implemented),

    ("deleteShare", not_implemented),

    ("refreshPodcasts", not_implemented),

    ("createPodcastChannel", not_implemented),

    ("deletePodcastChannel", not_implemented),

    ("deletePodcastEpisode", not_implemented),

    ("downloadPodcastEpisode", not_implemented),

    ("jukeboxControl", not_implemented),

    ("createInternetRadioStation", not_implemented),

    ("updateInternetRadioStation", not_implemented),

    ("deleteInternetRadioStation", not_implemented),

    ("getChatMessages", not_implemented),

    ("addChatMessage", not_implemented),

    ("createBookmark", not_implemented),

    ("deleteBookmark", not_implemented),

    ("getUser", not_implemented),

    ("getUsers", not_implemented),

    ("createUser", not_implemented),

    ("updateUser", not_implemented),

    ("deleteUser", not_implemented),

    ("changePassword", not_implemented),

    ("getPlayQueue", not_implemented),

    ("savePlayQueue", not_implemented),

    ("getScanStatus", not_implemented),

    ("startScan", not_implemented),

    ("getTranscodeDecision", not_implemented),

    ("getTranscodeStream", not_implemented),
];

/// The OpenSubsonic extensions the server advertises: the name and the
/// versions of each. `docs/design.md` section 4 names them, and they are in
/// that order here.
#[rustfmt::skip]
pub const EXTENSIONS: &[(&str, &[u32])] = &[
    // formPost

    // transcodeOffset

    // songLyrics
];

/// Builds the router of the server: one route for `/rest/{method}` that
/// answers GET and POST. A path outside `/rest/` gets HTTP 404.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/rest/{method}", get(dispatch).post(dispatch))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_response(record_response),
        )
        .with_state(state)
}

/// The span of one request. [`respond`] records the method name and the
/// client and user parameters when it has read them, and [`record_response`]
/// records the status. The span has no field for the URI, so the query string
/// with the credentials is not logged.
fn request_span<B>(_request: &http::Request<B>) -> Span {
    tracing::info_span!(
        "request",
        method = Empty,
        c = Empty,
        u = Empty,
        status = Empty
    )
}

/// Records the HTTP status of the response in the span of the request.
fn record_response<B>(response: &http::Response<B>, latency: Duration, span: &Span) {
    span.record("status", response.status().as_u16());
    tracing::debug!(
        latency_ms = latency.as_millis(),
        "finished processing request"
    );
}

/// The handler of the one route. `name` is the last path segment. A segment
/// that is not UTF-8 text names no method, so it is answered as an unknown
/// method.
async fn dispatch(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
    request: Request,
) -> Response {
    let name = name.map(|Path(name)| name).unwrap_or_default();
    respond(METHODS, state, &name, request).await
}

/// Answers one API request with the handler that `methods` holds for `name`.
/// A `.view` suffix of `name` is not part of the method name.
///
/// The parameters are the fields of the query string, followed by the fields
/// of the body when the request is a POST with a form body. The `f`
/// parameter selects the format before the handler runs, so an error of the
/// handler has the format of the request. A response of the handler that
/// carries no envelope is returned as it is.
async fn respond<S>(
    methods: &[(&str, Handler<S>)],
    state: S,
    name: &str,
    request: Request,
) -> Response {
    let name = name.strip_suffix(".view").unwrap_or(name);
    let (mut parts, body) = request.into_parts();

    let mut params = Params::from_query(parts.uri.query().unwrap_or_default());
    let mut body_error = None;
    let body = if parts.method == Method::POST && has_form_body(&parts.headers) {
        match body::to_bytes(body, FORM_BODY_LIMIT).await {
            Ok(bytes) => params.append_form(&bytes),
            Err(error) => {
                tracing::debug!(%error, "cannot read the form body");
                body_error = Some(ApiError::Other("cannot read the form body".to_owned()));
            }
        }
        Body::empty()
    } else {
        body
    };

    let span = Span::current();
    span.record("method", name);
    if let Some(client) = params.opt_str("c") {
        span.record("c", client);
    }
    if let Some(user) = params.opt_str("u") {
        span.record("u", user);
    }

    let format = match Format::parse(params.opt_str("f")) {
        Ok(format) => format,
        Err(error) => return render(&failure(&error), Format::Xml),
    };
    if let Some(error) = body_error {
        return render(&failure(&error), format);
    }
    let Some((_, handler)) = methods.iter().find(|(method, _)| *method == name) else {
        let error = ApiError::Other("unknown endpoint".to_owned());
        return render(&failure(&error), format);
    };

    parts.extensions.insert(params);
    let mut response = handler(Request::from_parts(parts, body), state).await;
    match response.extensions_mut().remove::<SubsonicResponse>() {
        Some(envelope) => render(&envelope, format),
        None => response,
    }
}

/// Whether the `Content-Type` header names a form body. Parameters of the
/// media type, such as `charset`, are allowed.
fn has_form_body(headers: &http::HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(FORM_CONTENT_TYPE))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::*;

    /// The method names of the table in `docs/design.md` section 4, in the
    /// order of the table.
    fn design_methods() -> Vec<&'static str> {
        const DESIGN: &str = include_str!("../../docs/design.md");
        let (_, rest) = DESIGN
            .split_once("\n## 4. OpenSubsonic coverage\n")
            .expect("the design has section 4");
        let (section, _) = rest
            .split_once("\n## 5. ")
            .expect("section 5 follows section 4");
        section
            .lines()
            .filter_map(|line| line.strip_prefix("| "))
            .filter_map(|row| row.split(" | ").next())
            .filter(|cell| *cell != "Endpoint" && *cell != "Extensions advertised")
            .flat_map(|cell| cell.split(", "))
            .collect()
    }

    /// A handler that returns every value of `id` and the first value of
    /// `name`.
    async fn echo(params: Params) -> ApiResult {
        Ok(Payload::new(
            "echo",
            json!({ "id": params.all("id"), "name": params.opt_str("name") }),
        ))
    }

    /// A handler that fails with error 70.
    async fn missing() -> ApiResult {
        Err(ApiError::NotFound)
    }

    /// A handler that answers with a body of its own.
    async fn bytes() -> Result<Response, ApiError> {
        Ok((
            StatusCode::PARTIAL_CONTENT,
            [(header::CONTENT_TYPE, "audio/flac")],
            "fLaC",
        )
            .into_response())
    }

    /// Handlers that need no state.
    const TEST_METHODS: &[(&str, Handler<()>)] = &[
        ("echo", handler!(echo)),
        ("missing", handler!(missing)),
        ("bytes", handler!(bytes)),
    ];

    fn get(uri: &str) -> Request {
        http::Request::get(uri)
            .body(Body::empty())
            .expect("a request")
    }

    fn post(uri: &str, content_type: &str, body: impl Into<Body>) -> Request {
        http::Request::post(uri)
            .header(header::CONTENT_TYPE, content_type)
            .body(body.into())
            .expect("a request")
    }

    /// Calls `respond` with the test methods. Returns the status, the content
    /// type and the body.
    async fn call(name: &str, request: Request) -> (StatusCode, String, String) {
        let response = respond(TEST_METHODS, (), name, request).await;
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().expect("an ASCII content type").to_owned())
            .unwrap_or_default();
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        let text = String::from_utf8(bytes.to_vec()).expect("a UTF-8 body");
        (status, content_type, text)
    }

    /// The `subsonic-response` object of a JSON body.
    fn json_response(body: &str) -> Value {
        let mut document: Value = serde_json::from_str(body).expect("a JSON body");
        document["subsonic-response"].take()
    }

    #[test]
    fn methods_are_the_names_of_design_section_4_in_table_order() {
        let methods: Vec<&str> = METHODS.iter().map(|(name, _)| *name).collect();
        assert_eq!(methods, design_methods());
    }

    #[test]
    fn method_names_are_unique() {
        let mut seen = HashSet::new();
        for (name, _) in METHODS {
            assert!(seen.insert(*name), "{name} is listed twice");
        }
    }

    #[tokio::test]
    async fn handler_receives_the_query_values_and_then_the_form_values() {
        let request = post(
            "/rest/echo?id=1&f=json",
            FORM_CONTENT_TYPE,
            "id=2&name=Mix+1",
        );
        let (status, content_type, body) = call("echo", request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        let response = json_response(&body);
        assert_eq!(response["status"], "ok");
        assert_eq!(
            response["echo"],
            json!({ "id": ["1", "2"], "name": "Mix 1" })
        );
    }

    #[tokio::test]
    async fn form_body_selects_the_format() {
        let request = post("/rest/echo", FORM_CONTENT_TYPE, "f=json&id=7");
        let (_, content_type, body) = call("echo", request).await;
        assert_eq!(content_type, "application/json");
        assert_eq!(json_response(&body)["echo"]["id"], json!(["7"]));
    }

    #[tokio::test]
    async fn form_content_type_may_have_parameters_and_any_letter_case() {
        for content_type in [
            "application/x-www-form-urlencoded; charset=UTF-8",
            "Application/X-WWW-Form-Urlencoded",
            "application/x-www-form-urlencoded ;charset=utf-8",
        ] {
            let request = post("/rest/echo?f=json", content_type, "id=2");
            let (_, _, body) = call("echo", request).await;
            assert_eq!(
                json_response(&body)["echo"]["id"],
                json!(["2"]),
                "{content_type}"
            );
        }
    }

    #[tokio::test]
    async fn body_of_another_content_type_is_not_read() {
        for content_type in ["text/plain", "application/json", "multipart/form-data"] {
            let request = post("/rest/echo?f=json&id=1", content_type, "id=2");
            let (_, _, body) = call("echo", request).await;
            assert_eq!(
                json_response(&body)["echo"]["id"],
                json!(["1"]),
                "{content_type}"
            );
        }
    }

    #[tokio::test]
    async fn form_body_of_a_get_request_is_not_read() {
        let request = http::Request::get("/rest/echo?f=json&id=1")
            .header(header::CONTENT_TYPE, FORM_CONTENT_TYPE)
            .body(Body::from("id=2"))
            .expect("a request");
        let (_, _, body) = call("echo", request).await;
        assert_eq!(json_response(&body)["echo"]["id"], json!(["1"]));
    }

    #[tokio::test]
    async fn form_body_over_the_limit_is_error_0() {
        let request = post(
            "/rest/echo?f=json",
            FORM_CONTENT_TYPE,
            vec![b'a'; FORM_BODY_LIMIT + 1],
        );
        let (status, content_type, body) = call("echo", request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        let response = json_response(&body);
        assert_eq!(response["status"], "failed");
        assert_eq!(
            response["error"],
            json!({ "code": 0, "message": "cannot read the form body" })
        );
    }

    #[tokio::test]
    async fn view_suffix_is_not_part_of_the_method_name() {
        let (_, _, body) = call("echo.view", get("/rest/echo.view?f=json&id=1")).await;
        assert_eq!(json_response(&body)["echo"]["id"], json!(["1"]));
    }

    #[tokio::test]
    async fn handler_error_has_the_format_of_the_request() {
        let (status, content_type, body) = call("missing", get("/rest/missing?f=json")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        let response = json_response(&body);
        assert_eq!(response["status"], "failed");
        assert_eq!(
            response["error"],
            json!({ "code": 70, "message": "not found" })
        );

        let (status, content_type, body) = call("missing", get("/rest/missing")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/xml");
        assert!(
            body.contains(r#"status="failed""#) && body.contains(r#"<error code="70""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn response_without_an_envelope_is_returned_as_it_is() {
        let (status, content_type, body) = call("bytes", get("/rest/bytes?f=json")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(content_type, "audio/flac");
        assert_eq!(body, "fLaC");
    }

    #[tokio::test]
    async fn unknown_method_is_error_0_in_the_format_of_the_request() {
        let (status, content_type, body) = call("nope", get("/rest/nope?f=json")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        assert_eq!(
            json_response(&body)["error"],
            json!({ "code": 0, "message": "unknown endpoint" })
        );
    }

    #[tokio::test]
    async fn unsupported_format_is_error_0_in_xml_before_the_method_is_looked_up() {
        for name in ["echo", "nope"] {
            let uri = format!("/rest/{name}?f=jsonp");
            let (status, content_type, body) = call(name, get(&uri)).await;
            assert_eq!(status, StatusCode::OK, "{name}");
            assert_eq!(content_type, "application/xml", "{name}");
            assert!(
                body.contains(r#"<error code="0" message="unsupported format"/>"#),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn params_extractor_fails_outside_the_router() {
        let (mut parts, _) = get("/rest/echo?id=1").into_parts();
        let error =
            <Params as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts, &())
                .await
                .expect_err("the request has no parameters");
        assert_eq!(error.code(), 0);
    }

    #[tokio::test]
    async fn payload_and_error_carry_the_envelope_until_the_router_renders_it() {
        let response = Payload::new("license", json!({ "valid": true })).into_response();
        assert_eq!(
            response.extensions().get::<SubsonicResponse>(),
            Some(&SubsonicResponse::ok("license", json!({ "valid": true })))
        );

        let response = ApiError::NotFound.into_response();
        assert_eq!(
            response.extensions().get::<SubsonicResponse>(),
            Some(&SubsonicResponse::error(70, "not found"))
        );
    }

    #[test]
    fn form_body_is_detected_by_the_media_type() {
        let headers = |value: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, value.parse().expect("a header value"));
            headers
        };
        assert!(has_form_body(&headers("application/x-www-form-urlencoded")));
        assert!(!has_form_body(&headers(
            "application/x-www-form-urlencoded-not"
        )));
        assert!(!has_form_body(&headers("text/plain")));
        assert!(!has_form_body(&http::HeaderMap::new()));
    }
}
