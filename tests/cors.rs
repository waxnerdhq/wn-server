//! Router tests of the CORS headers of the API route (`docs/design.md`
//! section 3.8): the allowed origin on every response of the route, the
//! answer to a preflight request, the exposed headers, and the absence of
//! `Access-Control-Allow-Credentials`.

mod common;

use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode, header};
use axum::response::Response;
use serde_json::Value;
use wn_server::config::Config;

use common::{
    API_VERSION, OWNER, OWNER_PASSWORD, TestServer, body_string, captured_log, content_type,
    form_request, subsonic_response, urlencode,
};

/// The origin of the page of a browser-based client.
const ORIGIN: &str = "http://client.example";

/// The parameters that authenticate a request as the owner.
const AS_OWNER: [(&str, &str); 4] = [
    ("u", OWNER),
    ("p", OWNER_PASSWORD),
    ("v", API_VERSION),
    ("c", "cors-tests"),
];

/// The value of `Access-Control-Expose-Headers`.
const EXPOSED: &str = "Content-Range, Accept-Ranges, Content-Length";

/// A server that authenticates.
async fn server() -> TestServer {
    TestServer::bare(Config::default()).await
}

/// A request as the page at [`ORIGIN`] sends it: `method` for
/// `/rest/<name>` with `params` in the query string, the `Origin` header and
/// no body.
fn page_request(method: Method, name: &str, params: &[(&str, &str)]) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/rest/{name}?{}", urlencode(params)))
        .header(header::ORIGIN, ORIGIN)
        .body(Body::empty())
        .expect("a request")
}

/// A preflight request of the page at [`ORIGIN`] for a POST to
/// `/rest/<name>`. It has no parameter and no credential.
fn preflight(name: &str) -> Request<Body> {
    Request::builder()
        .method(Method::OPTIONS)
        .uri(format!("/rest/{name}"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .body(Body::empty())
        .expect("a request")
}

/// The value of the response header `name`. `None` when the response does
/// not carry the header. A header that is present twice fails the test.
fn header_of(response: &Response, name: HeaderName) -> Option<&str> {
    let mut values = response.headers().get_all(&name).iter();
    let value = values.next()?;
    assert!(values.next().is_none(), "{name} is present twice");
    Some(value.to_str().expect("an ASCII header value"))
}

/// The value of `Access-Control-Allow-Origin` of `response`.
fn allow_origin(response: &Response) -> Option<&str> {
    header_of(response, header::ACCESS_CONTROL_ALLOW_ORIGIN)
}

/// The `subsonic-response` object of the JSON body of `response`.
async fn envelope(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    subsonic_response(&body_string(response).await)
}

/// The lines of the captured log that report a failed attempt of the client
/// `client`.
fn failed_lines(client: &str) -> Vec<String> {
    captured_log()
        .lines_with(&format!(r#"client="{client}""#))
        .into_iter()
        .filter(|line| line.contains("authentication failed"))
        .collect()
}

#[tokio::test]
async fn ping_response_allows_every_origin() {
    let server = server().await;

    let response = server.get_raw("ping", &[("f", "json")]).await;
    assert_eq!(allow_origin(&response), Some("*"));
    assert_eq!(envelope(response).await["status"], "ok");

    let response = server.get_raw("ping.view", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
    assert_eq!(allow_origin(&response), Some("*"));

    let response = server.post_form("ping", &[("f", "json")]).await;
    assert_eq!(allow_origin(&response), Some("*"));
    assert_eq!(envelope(response).await["status"], "ok");

    // The header does not depend on the `Origin` header of the request.
    let mut params = AS_OWNER.to_vec();
    params.push(("f", "json"));
    let request = page_request(Method::GET, "ping", &params);
    let response = server.request(request).await;
    assert_eq!(allow_origin(&response), Some("*"));
    assert_eq!(envelope(response).await["status"], "ok");
}

#[tokio::test]
async fn error_envelope_allows_every_origin() {
    let server = server().await;
    for (name, message) in [("nope", "unknown endpoint"), ("getVideos", "not supported")] {
        let response = server.get_raw(name, &[("f", "json")]).await;
        assert_eq!(allow_origin(&response), Some("*"), "{name}");
        let response = envelope(response).await;
        assert_eq!(response["status"], "failed", "{name}");
        assert_eq!(response["error"]["message"], message, "{name}");

        let response = server.get_raw(name, &[]).await;
        assert_eq!(content_type(&response), "application/xml", "{name}");
        assert_eq!(allow_origin(&response), Some("*"), "{name}");
        assert!(body_string(response).await.contains(message), "{name}");
    }

    let response = server.get_raw("ping", &[("f", "jsonp")]).await;
    assert_eq!(allow_origin(&response), Some("*"));
    let body = body_string(response).await;
    assert!(body.contains("unsupported format"), "{body}");
}

#[tokio::test]
async fn authentication_error_allows_every_origin() {
    let server = server().await;
    let rejected: [(&[(&str, &str)], u64); 4] = [
        (&[], 10),
        (&[("v", API_VERSION), ("c", "cors-tests")], 10),
        (
            &[
                ("u", OWNER),
                ("p", "wrong"),
                ("v", API_VERSION),
                ("c", "cors-tests"),
            ],
            40,
        ),
        (
            &[("apiKey", "key"), ("v", API_VERSION), ("c", "cors-tests")],
            42,
        ),
    ];
    for (params, code) in rejected {
        let mut params = params.to_vec();
        params.push(("f", "json"));

        let request = page_request(Method::GET, "ping", &params);
        let response = server.request(request).await;
        assert_eq!(allow_origin(&response), Some("*"), "{params:?}");
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_EXPOSE_HEADERS),
            Some(EXPOSED),
            "{params:?}"
        );
        let response = envelope(response).await;
        assert_eq!(response["status"], "failed", "{params:?}");
        assert_eq!(response["error"]["code"], code, "{params:?}");

        let response = server.request(form_request("/rest/ping", &params)).await;
        assert_eq!(allow_origin(&response), Some("*"), "{params:?}");
        assert_eq!(
            envelope(response).await["error"]["code"],
            code,
            "{params:?}"
        );
    }
}

#[tokio::test]
async fn preflight_request_is_answered_with_http_204_and_the_allow_headers() {
    let server = server().await;

    let mut with_headers = preflight("ping.view");
    with_headers.headers_mut().insert(
        header::ACCESS_CONTROL_REQUEST_HEADERS,
        "content-type, range".parse().expect("a header value"),
    );
    for request in [preflight("ping.view"), with_headers] {
        let response = server.request(request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(allow_origin(&response), Some("*"));
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_METHODS),
            Some("GET, POST, OPTIONS")
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_HEADERS),
            Some("Content-Type, Range")
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_MAX_AGE),
            Some("86400")
        );
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_EXPOSE_HEADERS),
            None
        );
        assert_eq!(header_of(&response, header::ALLOW), None);
        assert_eq!(content_type(&response), "");
        assert!(body_string(response).await.is_empty());
    }
}

#[tokio::test]
async fn preflight_request_is_answered_before_authentication() {
    let log = captured_log();
    let server = server().await;

    // A method that needs a user, the public method, a method without a
    // handler and a name that is not a method. None of the requests has a
    // credential, and none gets the envelope of an error.
    for name in [
        "ping",
        "getOpenSubsonicExtensions",
        "stream.view",
        "getVideos",
        "nope",
    ] {
        let response = server.request(preflight(name)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{name}");
        assert_eq!(allow_origin(&response), Some("*"), "{name}");
        assert!(body_string(response).await.is_empty(), "{name}");
    }

    // The same wrong password is a failed attempt in a GET request and is
    // not read in an OPTIONS request.
    let wrong = |client: &'static str| {
        [
            ("u", OWNER),
            ("p", "wrong"),
            ("v", API_VERSION),
            ("c", client),
            ("f", "json"),
        ]
    };
    let request = page_request(Method::GET, "ping", &wrong("cors-get-client"));
    let response = envelope(server.request(request).await).await;
    assert_eq!(response["error"]["code"], 40);
    assert_eq!(failed_lines("cors-get-client").len(), 1, "{}", log.text());

    let request = page_request(Method::OPTIONS, "ping", &wrong("cors-options-client"));
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(body_string(response).await.is_empty());
    assert_eq!(
        failed_lines("cors-options-client"),
        Vec::<String>::new(),
        "{}",
        log.text()
    );
}

#[tokio::test]
async fn no_response_carries_allow_credentials() {
    let server = server().await;
    let mut as_owner = AS_OWNER.to_vec();
    as_owner.push(("f", "json"));
    let requests = [
        ("ok", page_request(Method::GET, "ping", &as_owner)),
        ("error 0", page_request(Method::GET, "nope", &as_owner)),
        ("error 10", page_request(Method::GET, "ping", &[])),
        ("form", form_request("/rest/ping", &as_owner)),
        ("preflight", preflight("ping.view")),
        ("HTTP 405", page_request(Method::PUT, "ping", &as_owner)),
        ("HTTP 404", page_request(Method::GET, "ping/extra", &[])),
    ];
    for (case, request) in requests {
        let response = server.request(request).await;
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
            None,
            "{case}"
        );
    }
}

#[tokio::test]
async fn response_exposes_the_headers_a_player_reads() {
    let server = server().await;
    // `stream` and `getCoverArt` answer an error envelope to these requests.
    // The test covers the envelope of a media method and not a media body.
    // The unit test in `src/api/cors.rs` covers a response with a body of
    // its own.
    for name in ["ping", "stream", "getCoverArt", "getOpenSubsonicExtensions"] {
        let response = server.get_raw(name, &[]).await;
        assert_eq!(allow_origin(&response), Some("*"), "{name}");
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_EXPOSE_HEADERS),
            Some(EXPOSED),
            "{name}"
        );

        let response = server.post_form(name, &[]).await;
        assert_eq!(allow_origin(&response), Some("*"), "{name}");
        assert_eq!(
            header_of(&response, header::ACCESS_CONTROL_EXPOSE_HEADERS),
            Some(EXPOSED),
            "{name}"
        );
    }
}

#[tokio::test]
async fn other_http_method_is_http_405_that_allows_every_origin() {
    let server = server().await;
    for method in [Method::PUT, Method::DELETE, Method::PATCH] {
        let request = page_request(method.clone(), "ping", &AS_OWNER);
        let response = server.request(request).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        assert_eq!(allow_origin(&response), Some("*"), "{method}");
        assert_eq!(
            header_of(&response, header::ALLOW),
            Some("GET,HEAD,POST,OPTIONS"),
            "{method}"
        );
    }
}

#[tokio::test]
async fn path_outside_the_route_has_no_cors_header() {
    let server = server().await;
    for path in ["/", "/rest", "/rest/", "/rest/ping/extra", "/api/ping"] {
        for method in [Method::GET, Method::OPTIONS] {
            let request = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(header::ORIGIN, ORIGIN)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .body(Body::empty())
                .expect("a request");
            let response = server.request(request).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
            for name in [
                header::ACCESS_CONTROL_ALLOW_ORIGIN,
                header::ACCESS_CONTROL_ALLOW_METHODS,
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                header::ACCESS_CONTROL_MAX_AGE,
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
            ] {
                assert_eq!(header_of(&response, name), None, "{method} {path}");
            }
        }
    }
}

#[tokio::test]
async fn disabled_auth_answers_a_page_of_any_origin_without_a_password() {
    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    let server = TestServer::bare(config).await;

    let params = [("v", API_VERSION), ("c", "cors-tests"), ("f", "json")];
    let request = page_request(Method::GET, "getLicense", &params);
    let response = server.request(request).await;
    assert_eq!(allow_origin(&response), Some("*"));
    let response = envelope(response).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["license"]["valid"], true);
}
