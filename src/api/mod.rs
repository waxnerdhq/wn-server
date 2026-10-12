//! The OpenSubsonic API: the router, the method table and the state that the
//! handlers share (`docs/design.md` section 3.8).
//!
//! # Authentication
//!
//! The router authenticates a request before it calls the handler of the
//! method. The methods of [`PUBLIC_METHODS`] are the exception. A method that
//! a change adds to [`METHODS`] therefore needs a user without any step in
//! its handler (`docs/design.md` section 3.7).
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
//! merges them without a conflict. The two places that build an `AppState`,
//! `server::run_server` and the test server, follow the same rule.
//!
//! rustfmt removes the blank lines inside an array, and in a struct it
//! removes the blank line between a comment and the field after it. The
//! arrays, the struct and the statements that build the struct therefore
//! carry `#[rustfmt::skip]`.

pub mod cors;
pub mod envelope;
pub mod model;
pub mod params;
pub mod xml;

// The handler modules, in the order of `docs/design.md` section 3.1.

pub mod system;

pub mod browse;

// folders

// lists

// search

pub mod annotate;

// playlists

// queue

// lyrics

pub mod users;

pub mod stubs;

// media

// covers

pub mod scan;

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

use crate::auth::extract::{Authed, Authenticate};
use crate::config::Config;
use crate::db::Db;
use crate::library::scan_controller::ScanController;
use envelope::{ApiError, Format, SubsonicResponse};
use params::Params;

/// The largest form body the router reads, in bytes. The body is read before
/// the request is authenticated, so the limit bounds the memory that one
/// request can take. A play queue of 100 000 track IDs is a body of about
/// 4.4 MB.
const FORM_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// The content type of a form body.
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// The longest value that the span of a request holds for the method name,
/// `c` or `u`, in characters. The client chooses these values, and every log
/// line of the request repeats them.
const LOG_FIELD_CHARS: usize = 64;

/// The state that the handlers share. `docs/design.md` section 3.8 names the
/// fields, and they are in that order here.
#[derive(Clone)]
#[rustfmt::skip]
pub struct AppState {
    /// The configuration the server runs with.
    pub config: Arc<Config>,

    /// The library database. It holds the user accounts.
    pub db: Db,

    /// The cover art service: it resolves, resizes and caches covers.
    pub covers: crate::library::covers::Covers,

    // transcoder

    /// The scans of the server. It runs at most one at a time.
    pub scan: ScanController,

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
/// The extractors a handler uses are [`Params`], `State<AppState>` and
/// [`Authed`], which is the user of the request. A handler that answers with
/// the envelope returns [`ApiResult`]. A handler that answers with a body of
/// its own returns `Result<Response, ApiError>`.
///
/// The router authenticates the request before it calls the handler, whether
/// the handler takes [`Authed`] or not.
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

    ("getMusicFolders", handler!(browse::get_music_folders)),

    ("getArtists", handler!(browse::get_artists)),

    ("getArtist", handler!(browse::get_artist)),

    ("getAlbum", handler!(browse::get_album)),

    ("getSong", handler!(browse::get_song)),

    ("getGenres", handler!(browse::get_genres)),

    ("getIndexes", not_implemented),

    ("getMusicDirectory", not_implemented),

    ("getVideos", not_implemented),

    ("getVideoInfo", not_implemented),

    ("getArtistInfo", handler!(stubs::get_artist_info)),

    ("getArtistInfo2", handler!(stubs::get_artist_info2)),

    ("getAlbumInfo", handler!(stubs::get_album_info)),

    ("getAlbumInfo2", handler!(stubs::get_album_info2)),

    ("getSimilarSongs", handler!(stubs::get_similar_songs)),

    ("getSimilarSongs2", handler!(stubs::get_similar_songs2)),

    ("getTopSongs", handler!(stubs::get_top_songs)),

    ("getAlbumList", not_implemented),

    ("getAlbumList2", not_implemented),

    ("getRandomSongs", not_implemented),

    ("getSongsByGenre", not_implemented),

    ("getNowPlaying", handler!(stubs::get_now_playing)),

    ("getStarred", handler!(annotate::get_starred)),

    ("getStarred2", handler!(annotate::get_starred2)),

    ("search", not_implemented),

    ("search2", not_implemented),

    ("search3", not_implemented),

    ("getPlaylists", not_implemented),

    ("getPlaylist", not_implemented),

    ("createPlaylist", not_implemented),

    ("updatePlaylist", not_implemented),

    ("deletePlaylist", not_implemented),

    ("stream", not_implemented),

    ("download", handler!(stubs::download)),

    ("hls", not_implemented),

    ("getCaptions", not_implemented),

    ("getCoverArt", not_implemented),

    ("getLyrics", not_implemented),

    ("getLyricsBySongId", not_implemented),

    ("getAvatar", handler!(stubs::get_avatar)),

    ("star", handler!(annotate::star)),

    ("unstar", handler!(annotate::unstar)),

    ("setRating", handler!(annotate::set_rating)),

    ("scrobble", not_implemented),

    ("getShares", handler!(stubs::get_shares)),

    ("getPodcasts", handler!(stubs::get_podcasts)),

    ("getNewestPodcasts", handler!(stubs::get_newest_podcasts)),

    ("getInternetRadioStations", handler!(stubs::get_internet_radio_stations)),

    ("getBookmarks", handler!(stubs::get_bookmarks)),

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

    ("getUser", handler!(users::get_user)),

    ("getUsers", handler!(users::get_users)),

    ("createUser", handler!(stubs::user_management)),

    ("updateUser", handler!(stubs::user_management)),

    ("deleteUser", handler!(stubs::user_management)),

    ("changePassword", handler!(stubs::user_management)),

    ("getPlayQueue", not_implemented),

    ("savePlayQueue", not_implemented),

    ("getScanStatus", handler!(scan::get_scan_status)),

    ("startScan", handler!(scan::start_scan)),

    ("getTranscodeDecision", not_implemented),

    ("getTranscodeStream", not_implemented),
];

/// The methods that answer a request without a user. `docs/design.md`
/// section 4 marks them `public`.
///
/// Every other method of [`METHODS`] is answered only for a request that the
/// router has authenticated. A method needs no entry anywhere to be
/// protected: a name that is absent from this list is not public.
pub const PUBLIC_METHODS: &[&str] = &["getOpenSubsonicExtensions"];

/// The OpenSubsonic extensions the server advertises: the name and the
/// versions of each. `docs/design.md` section 4 names them, and they are in
/// that order here.
#[rustfmt::skip]
pub const EXTENSIONS: &[(&str, &[u32])] = &[
    ("formPost", &[1]),

    // transcodeOffset

    // songLyrics
];

/// Builds the router of the server: one route for `/rest/{method}` that
/// answers GET and POST. A path outside `/rest/` gets HTTP 404.
///
/// The route answers `OPTIONS` with [`cors::preflight`], which is not an API
/// method, so that request is not authenticated. [`cors::add_headers`] wraps
/// the route and nothing else, and adds its headers to every response of the
/// route.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/rest/{method}",
            get(dispatch).post(dispatch).options(cors::preflight),
        )
        .route_layer(axum::middleware::from_fn(cors::add_headers))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_response(record_response),
        )
        .with_state(state)
}

/// The span of one request. [`respond`] records the method name and the
/// client and user parameters when it has read them, each cut to
/// [`LOG_FIELD_CHARS`] characters, and [`record_response`] records the
/// status. The span has no field for the URI, so the query string with the
/// credentials is not logged.
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
    respond(METHODS, PUBLIC_METHODS, state, &name, request).await
}

/// Answers one API request with the handler that `methods` holds for `name`.
/// A `.view` suffix of `name` is not part of the method name.
///
/// The parameters are the fields of the query string, followed by the fields
/// of the body when the request is a POST with a form body. The `f`
/// parameter selects the format before the handler runs, so an error of the
/// handler has the format of the request. A response of the handler that
/// carries no envelope is returned as it is.
///
/// `state` authenticates the request of every method that `public` does not
/// name. The handler of such a method is called only when the request has a
/// user, and it finds the user in the request as [`Authed`]. The check does
/// not depend on the handler, so a handler cannot leave it out.
async fn respond<S: Authenticate>(
    methods: &[(&str, Handler<S>)],
    public: &[&str],
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
    span.record("method", log_field(name));
    if let Some(client) = params.opt_str("c") {
        span.record("c", log_field(client));
    }
    if let Some(user) = params.opt_str("u") {
        span.record("u", log_field(user));
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

    if !public.contains(&name) {
        match state.authenticate(&params).await {
            Ok(user) => {
                parts.extensions.insert(Authed(user));
            }
            Err(error) => return render(&failure(&error), format),
        }
    }

    parts.extensions.insert(params);
    let mut response = handler(Request::from_parts(parts, body), state).await;
    match response.extensions_mut().remove::<SubsonicResponse>() {
        Some(envelope) => render(&envelope, format),
        None => response,
    }
}

/// The first [`LOG_FIELD_CHARS`] characters of `value`. A log line holds a
/// value that the client chooses in this form.
pub(crate) fn log_field(value: &str) -> &str {
    match value.char_indices().nth(LOG_FIELD_CHARS) {
        Some((end, _)) => &value[..end],
        None => value,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::extract;
    use crate::config;
    use crate::db::queries::users::{self, User};

    /// The rows of the table in `docs/design.md` section 4, in the order of
    /// the table: the method names of a row and its status.
    fn design_rows() -> Vec<(Vec<&'static str>, &'static str)> {
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
            .filter_map(|row| row.strip_suffix(" |"))
            .filter_map(|row| row.split_once(" | "))
            .filter(|(names, _)| *names != "Endpoint" && *names != "Extensions advertised")
            .map(|(names, status)| (names.split(", ").collect(), status))
            .collect()
    }

    /// The method names of the table in `docs/design.md` section 4, in the
    /// order of the table.
    fn design_methods() -> Vec<&'static str> {
        design_rows()
            .into_iter()
            .flat_map(|(names, _)| names)
            .collect()
    }

    /// The state of the handlers that need no state. It authenticates no
    /// request, so a test method runs with it only when the test names the
    /// method as public.
    impl Authenticate for () {
        async fn authenticate(&self, _params: &Params) -> Result<User, ApiError> {
            Err(ApiError::BadCredentials)
        }
    }

    /// A state that holds the accounts of a database and authenticates as
    /// the server does. It has no other part of [`AppState`].
    #[derive(Clone)]
    struct Accounts(Db);

    impl Authenticate for Accounts {
        async fn authenticate(&self, params: &Params) -> Result<User, ApiError> {
            extract::authenticate(&config::Auth::default(), &self.0, params).await
        }
    }

    /// The accounts of a memory database with the owner `owner` and the
    /// password `sesame`.
    async fn accounts() -> Accounts {
        let db = Db::open_memory().expect("a memory database");
        db.write(|conn| users::insert_owner(conn, "owner", "sesame"))
            .await
            .expect("insert the owner")
            .expect("the database had no owner");
        Accounts(db)
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

    /// The names of [`TEST_METHODS`]. The tests of the request pipeline call
    /// them as public methods.
    const TEST_PUBLIC: &[&str] = &["echo", "missing", "bytes"];

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
        let response = respond(TEST_METHODS, TEST_PUBLIC, (), name, request).await;
        read(response).await
    }

    /// The status, the content type and the body of `response`.
    async fn read(response: Response) -> (StatusCode, String, String) {
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

    #[test]
    fn public_methods_are_the_methods_that_the_design_marks_public() {
        let public: Vec<&str> = design_rows()
            .into_iter()
            .filter(|(_, status)| status.split(", ").any(|word| word == "public"))
            .flat_map(|(names, _)| names)
            .collect();
        assert_eq!(PUBLIC_METHODS, public);
        assert_eq!(PUBLIC_METHODS, ["getOpenSubsonicExtensions"]);
    }

    #[test]
    fn public_methods_are_in_the_method_table() {
        for name in PUBLIC_METHODS {
            assert!(
                METHODS.iter().any(|(method, _)| method == name),
                "{name} is not a method"
            );
        }
    }

    /// The number of calls of [`unguarded`].
    static UNGUARDED_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// A handler that takes no [`Authed`] and counts its calls.
    async fn unguarded() -> ApiResult {
        UNGUARDED_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(Payload::empty())
    }

    #[tokio::test]
    async fn method_that_is_not_public_does_not_run_without_a_user() {
        const UNGUARDED: &[(&str, Handler<()>)] = &[("unguarded", handler!(unguarded))];

        // The state `()` authenticates no request.
        for (name, uri, content_type) in [
            ("unguarded", "/rest/unguarded?f=json", "application/json"),
            ("unguarded", "/rest/unguarded", "application/xml"),
            (
                "unguarded.view",
                "/rest/unguarded.view?f=json",
                "application/json",
            ),
        ] {
            let response = respond(UNGUARDED, &[], (), name, get(uri)).await;
            let (status, found, body) = read(response).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(found, content_type, "{uri}");
            assert!(body.contains("wrong username or password"), "{body}");
        }
        assert_eq!(UNGUARDED_CALLS.load(Ordering::SeqCst), 0);

        // The same handler runs when the method is public.
        let uri = "/rest/unguarded?f=json";
        let response = respond(UNGUARDED, &["unguarded"], (), "unguarded", get(uri)).await;
        let (_, _, body) = read(response).await;
        assert_eq!(json_response(&body)["status"], "ok");
        assert_eq!(UNGUARDED_CALLS.load(Ordering::SeqCst), 1);
    }

    /// The number of calls of [`added_later`].
    static ADDED_LATER_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// The handler of the method `addedLater`. It takes no [`Authed`] and
    /// counts its calls.
    async fn added_later(params: Params) -> ApiResult {
        ADDED_LATER_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(Payload::new("added", json!({ "id": params.opt_str("id") })))
    }

    /// The table of a server to which a change added the method `addedLater`.
    /// The change wrote the handler and this one line, and nothing else.
    const LATER_METHODS: &[(&str, Handler<Accounts>)] = &[
        (
            "getOpenSubsonicExtensions",
            handler!(system::get_open_subsonic_extensions),
        ),
        ("addedLater", handler!(added_later)),
    ];

    /// Calls `method` of [`LATER_METHODS`] with the public methods of the
    /// server. Returns the response object.
    async fn call_later(state: &Accounts, method: &str, query: &str) -> Value {
        let uri = format!("/rest/{method}?f=json&id=7&{query}");
        let request = get(&uri);
        let response = respond(
            LATER_METHODS,
            PUBLIC_METHODS,
            state.clone(),
            method,
            request,
        )
        .await;
        let (status, _, body) = read(response).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        json_response(&body)
    }

    #[tokio::test]
    async fn method_added_to_the_table_is_protected_without_a_step_in_its_handler() {
        let state = accounts().await;
        for (query, code) in [
            ("", 10),
            ("v=1.16.1&c=test", 10),
            ("v=1.16.1&c=test&u=owner", 10),
            ("v=1.16.1&c=test&u=owner&p=wrong", 40),
            ("v=1.16.1&c=test&u=nobody&p=sesame", 40),
            ("v=1.16.1&c=test&u=owner&t=abc&s=def", 40),
            ("v=1.16.1&c=test&apiKey=k", 42),
            ("v=1.16.1&c=test&apiKey=k&u=owner", 43),
            ("v=1.16.1&c=test&u=owner&p=sesame&t=abc", 43),
        ] {
            for method in ["addedLater", "addedLater.view"] {
                let response = call_later(&state, method, query).await;
                assert_eq!(response["status"], "failed", "{method} {query}");
                assert_eq!(response["error"]["code"], code, "{method} {query}");
                assert!(response.get("added").is_none(), "{method} {query}");
            }
        }
        assert_eq!(ADDED_LATER_CALLS.load(Ordering::SeqCst), 0);

        let response = call_later(&state, "addedLater", "v=1.16.1&c=test&u=owner&p=sesame").await;
        assert_eq!(response["status"], "ok");
        assert_eq!(response["added"], json!({ "id": "7" }));
        assert_eq!(ADDED_LATER_CALLS.load(Ordering::SeqCst), 1);

        // The public method of the same table needs no user.
        let response = call_later(&state, "getOpenSubsonicExtensions", "").await;
        assert_eq!(response["status"], "ok");
    }

    /// A handler that returns the name of the user of the request.
    async fn who_am_i(Authed(user): Authed) -> ApiResult {
        Ok(Payload::new("user", json!({ "name": user.name })))
    }

    #[tokio::test]
    async fn handler_receives_the_user_that_the_router_authenticated() {
        const WHO_AM_I: &[(&str, Handler<Accounts>)] = &[("whoAmI", handler!(who_am_i))];
        let state = accounts().await;
        let call = |public: &'static [&'static str], query: &'static str| {
            let state = state.clone();
            async move {
                let uri = format!("/rest/whoAmI?f=json&v=1.16.1&c=test&{query}");
                let response = respond(WHO_AM_I, public, state, "whoAmI", get(&uri)).await;
                let (_, _, body) = read(response).await;
                json_response(&body)
            }
        };

        // The stored name is returned, not the spelling of the request.
        let response = call(&[], "u=OWNER&p=sesame").await;
        assert_eq!(response["user"], json!({ "name": "owner" }));

        // A handler that takes `Authed` needs a user when its method is
        // public as well.
        let response = call(&["whoAmI"], "u=owner&p=wrong").await;
        assert_eq!(response["error"]["code"], 40);
        let response = call(&["whoAmI"], "u=owner&p=sesame").await;
        assert_eq!(response["user"], json!({ "name": "owner" }));
    }

    #[tokio::test]
    async fn format_body_and_method_name_are_checked_before_the_user() {
        // The state `()` authenticates no request, and no method is public.
        let call = |name: &'static str, request: Request| async move {
            read(respond(TEST_METHODS, &[], (), name, request).await).await
        };

        let (_, content_type, body) = call("echo", get("/rest/echo?f=jsonp")).await;
        assert_eq!(content_type, "application/xml");
        assert!(body.contains("unsupported format"), "{body}");

        let request = post(
            "/rest/echo?f=json",
            FORM_CONTENT_TYPE,
            vec![b'a'; FORM_BODY_LIMIT + 1],
        );
        let (_, _, body) = call("echo", request).await;
        assert_eq!(
            json_response(&body)["error"]["message"],
            "cannot read the form body"
        );

        let (_, _, body) = call("nope", get("/rest/nope?f=json")).await;
        assert_eq!(json_response(&body)["error"]["message"], "unknown endpoint");

        let (_, _, body) = call("echo", get("/rest/echo?f=json")).await;
        assert_eq!(json_response(&body)["error"]["code"], 40);
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
    fn log_field_is_at_most_64_characters() {
        assert_eq!(log_field(""), "");
        assert_eq!(log_field("Symfonium"), "Symfonium");
        let exact = "a".repeat(64);
        assert_eq!(log_field(&exact), exact);
        assert_eq!(log_field(&"a".repeat(65)), exact);
        assert_eq!(log_field(&"a".repeat(400_000)), exact);

        // The cut is between characters, not inside the bytes of one.
        let wide = "é".repeat(70);
        assert_eq!(log_field(&wide), "é".repeat(64));
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
