//! The CORS headers of the API route (`docs/design.md` section 3.8).
//!
//! A browser-based client is a page of another origin than the server. The
//! browser lets such a page read a response only when the response allows
//! the origin of the page. Subsonic credentials travel in the parameters of
//! a request and not in cookies, so the server allows every origin and never
//! sends `Access-Control-Allow-Credentials`.

use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The value of `Access-Control-Allow-Origin`: every origin.
const ALLOW_ORIGIN: &str = "*";

/// The value of `Access-Control-Allow-Methods`: the methods the route
/// answers.
const ALLOW_METHODS: &str = "GET, POST, OPTIONS";

/// The value of `Access-Control-Allow-Headers`: the request headers that the
/// target clients set and that a browser may name in a preflight request.
/// Feishin sets `Content-Type: application/json`, and a player sets `Range`.
const ALLOW_HEADERS: &str = "Content-Type, Range";

/// The value of `Access-Control-Max-Age`: a browser may reuse the answer to
/// a preflight request for one day, in seconds.
const MAX_AGE: &str = "86400";

/// The value of `Access-Control-Expose-Headers`: the response headers a page
/// may read in addition to the ones a browser always shows it. A player
/// reads them from a media response.
const EXPOSE_HEADERS: &str = "Content-Range, Accept-Ranges, Content-Length";

/// The middleware of the API route. It answers an `OPTIONS` request itself
/// with HTTP 204 and the allowed methods and headers, so a preflight request
/// needs no user. It passes every other request on and adds
/// `Access-Control-Expose-Headers` to the response. Every response carries
/// `Access-Control-Allow-Origin: *`, whatever the route answers: an
/// envelope, an authentication error, a body of a handler or HTTP 405.
pub async fn add_headers(request: Request, next: Next) -> Response {
    let mut response = if request.method() == Method::OPTIONS {
        let preflight = [
            (header::ACCESS_CONTROL_ALLOW_METHODS, ALLOW_METHODS),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS),
            (header::ACCESS_CONTROL_MAX_AGE, MAX_AGE),
        ];
        (StatusCode::NO_CONTENT, preflight).into_response()
    } else {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(EXPOSE_HEADERS),
        );
        response
    };
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static(ALLOW_ORIGIN),
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{self, Body};
    use axum::http;
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    /// A handler that answers as a media method answers a range request:
    /// HTTP 206 with a body and headers of its own.
    async fn part() -> Response {
        let headers = [
            (header::CONTENT_TYPE, "audio/flac"),
            (header::CONTENT_RANGE, "bytes 0-3/8"),
            (header::ACCEPT_RANGES, "bytes"),
        ];
        (StatusCode::PARTIAL_CONTENT, headers, "fLaC").into_response()
    }

    #[tokio::test]
    async fn response_with_a_body_of_its_own_keeps_it_and_gains_the_headers() {
        let router = Router::new()
            .route("/part", get(part))
            .route_layer(middleware::from_fn(add_headers));
        let request = http::Request::get("/part")
            .body(Body::empty())
            .expect("a request");
        let response = match router.oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let headers = response.headers();
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        assert_eq!(
            headers[header::ACCESS_CONTROL_EXPOSE_HEADERS],
            "Content-Range, Accept-Ranges, Content-Length"
        );
        assert_eq!(headers[header::CONTENT_TYPE], "audio/flac");
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 0-3/8");
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        assert_eq!(&bytes[..], b"fLaC");
    }
}
