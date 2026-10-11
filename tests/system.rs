//! Router tests of the request pipeline and of the system methods `ping`,
//! `getLicense` and `getOpenSubsonicExtensions`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::METHODS;
use wn_server::config::Config;

use common::{TestServer, body_string, captured_log, content_type, subsonic_response, urlencode};

/// The methods that return `not supported` in local mode for good
/// (`docs/design.md` section 4, status "deferred: `not supported`").
const NOT_SUPPORTED: [&str; 23] = [
    "tokenInfo",
    "getVideos",
    "getVideoInfo",
    "hls",
    "getCaptions",
    "createShare",
    "updateShare",
    "deleteShare",
    "refreshPodcasts",
    "createPodcastChannel",
    "deletePodcastChannel",
    "deletePodcastEpisode",
    "downloadPodcastEpisode",
    "jukeboxControl",
    "createInternetRadioStation",
    "updateInternetRadioStation",
    "deleteInternetRadioStation",
    "getChatMessages",
    "addChatMessage",
    "createBookmark",
    "deleteBookmark",
    "getTranscodeDecision",
    "getTranscodeStream",
];

fn server() -> TestServer {
    TestServer::bare(Config::default())
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

/// The XML body of error 0 with `message`.
fn xml_error_0(message: &str) -> String {
    xml_document(
        "failed",
        &format!(r#"><error code="0" message="{message}"/></subsonic-response>"#),
    )
}

/// The `error` object of error 0 with `message`.
fn error_0(message: &str) -> Value {
    json!({ "code": 0, "message": message })
}

#[tokio::test]
async fn ping_json_is_the_documented_envelope() {
    let response = server().get_raw("ping", &[("f", "json")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(body_string(response).await, json_document("ok", ""));
}

#[tokio::test]
async fn ping_view_is_the_xml_envelope_with_xmlns() {
    let response = server().get_raw("ping.view", &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
    let body = body_string(response).await;
    assert!(
        body.contains(r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#),
        "{body}"
    );
    assert_eq!(body, xml_document("ok", "/>"));
}

#[tokio::test]
async fn method_answers_with_and_without_the_view_suffix_in_both_formats() {
    let server = server();
    for method in ["ping", "ping.view"] {
        assert_eq!(
            server.get_xml(method, &[]).await,
            xml_document("ok", "/>"),
            "{method}"
        );
        assert_eq!(
            server.get_xml(method, &[("f", "xml")]).await,
            xml_document("ok", "/>"),
            "{method}"
        );
        let response = server.get_json(method, &[]).await;
        assert_eq!(response["status"], "ok", "{method}");
        assert_eq!(response["version"], "1.16.1", "{method}");
        assert_eq!(response["type"], "wn-server", "{method}");
        assert_eq!(response["serverVersion"], VERSION, "{method}");
        assert_eq!(response["openSubsonic"], true, "{method}");
    }
}

#[tokio::test]
async fn get_license_returns_a_valid_license() {
    let server = server();
    let response = server.get_json("getLicense", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["license"], json!({ "valid": true }));

    assert_eq!(
        server.get_xml("getLicense", &[]).await,
        xml_document("ok", r#"><license valid="true"/></subsonic-response>"#)
    );
}

#[tokio::test]
async fn get_open_subsonic_extensions_returns_an_empty_list() {
    let server = server();
    let response = server.get_json("getOpenSubsonicExtensions", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["openSubsonicExtensions"], json!([]));

    let response = server
        .get_raw("getOpenSubsonicExtensions", &[("f", "json")])
        .await;
    assert_eq!(
        body_string(response).await,
        json_document("ok", r#","openSubsonicExtensions":[]"#)
    );

    // An empty array has no element in XML.
    assert_eq!(
        server.get_xml("getOpenSubsonicExtensions", &[]).await,
        xml_document("ok", "/>")
    );
}

#[tokio::test]
async fn unknown_method_is_error_0_unknown_endpoint() {
    let server = server();
    let response = server.get_json("nope", &[]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"], error_0("unknown endpoint"));

    assert_eq!(
        server.get_xml("nope.view", &[]).await,
        xml_error_0("unknown endpoint")
    );

    // A segment that is not UTF-8 text after percent-decoding.
    let response = server.get_path("/rest/%FF?f=json").await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["error"], error_0("unknown endpoint"));
}

#[tokio::test]
async fn method_names_are_case_sensitive() {
    let response = server().get_json("Ping", &[]).await;
    assert_eq!(response["error"], error_0("unknown endpoint"));
}

#[tokio::test]
async fn listed_method_without_a_handler_is_error_0_not_supported() {
    let server = server();
    let response = server.get_json("getPodcasts", &[]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"], error_0("not supported"));

    assert_eq!(
        server.get_xml("getPodcasts", &[]).await,
        xml_error_0("not supported")
    );
}

#[tokio::test]
async fn deferred_methods_are_not_supported() {
    let server = server();
    for method in NOT_SUPPORTED {
        let response = server.get_json(method, &[]).await;
        assert_eq!(response["status"], "failed", "{method}");
        assert_eq!(response["error"], error_0("not supported"), "{method}");
    }
}

#[tokio::test]
async fn every_listed_method_is_routed() {
    let server = server();
    for (method, _) in METHODS {
        for name in [(*method).to_owned(), format!("{method}.view")] {
            let response = server.get_json(&name, &[]).await;
            match response["status"].as_str() {
                Some("ok") => {}
                Some("failed") => {
                    assert_eq!(response["error"], error_0("not supported"), "{name}");
                }
                other => panic!("{name}: status {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn system_methods_are_the_methods_with_a_handler() {
    let server = server();
    let mut served = Vec::new();
    for (method, _) in METHODS {
        if server.get_json(method, &[]).await["status"] == "ok" {
            served.push(*method);
        }
    }
    assert_eq!(served, ["ping", "getLicense", "getOpenSubsonicExtensions"]);
}

#[tokio::test]
async fn path_outside_rest_is_http_404() {
    let server = server();
    for path in [
        "/",
        "/?f=json",
        "/ping",
        "/rest",
        "/rest/",
        "/rest/ping/extra",
        "/api/ping",
        "/index.html",
    ] {
        let response = server.get_path(path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(body_string(response).await.is_empty(), "{path}");
    }
}

#[tokio::test]
async fn jsonp_format_is_error_0_in_xml() {
    let server = server();
    let response = server
        .get_raw("ping", &[("f", "jsonp"), ("callback", "show")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
    assert_eq!(
        body_string(response).await,
        xml_error_0("unsupported format")
    );

    // The format is checked before the method name.
    assert_eq!(
        server.get_xml("nope", &[("f", "jsonp")]).await,
        xml_error_0("unsupported format")
    );
}

#[tokio::test]
async fn api_error_is_http_200() {
    let server = server();
    for (method, params) in [
        ("nope", &[("f", "json")][..]),
        ("getPodcasts", &[]),
        ("ping", &[("f", "yaml")]),
    ] {
        let response = server.get_raw(method, params).await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
    }
}

#[tokio::test]
async fn post_with_a_form_body_selects_json() {
    let response = server().post_form("ping", &[("f", "json")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(body_string(response).await, json_document("ok", ""));
}

#[tokio::test]
async fn post_without_a_format_answers_in_xml() {
    let server = server();
    let response = server.post_form("ping.view", &[("c", "app")]).await;
    assert_eq!(content_type(&response), "application/xml");
    assert_eq!(body_string(response).await, xml_document("ok", "/>"));

    let response = server.post_form("nope", &[("f", "json")]).await;
    assert_eq!(content_type(&response), "application/json");
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["error"], error_0("unknown endpoint"));
}

#[tokio::test]
async fn query_string_of_a_post_comes_before_the_form_body() {
    let request = Request::post("/rest/ping?f=json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("f=xml"))
        .expect("a request");
    let response = server().request(request).await;
    assert_eq!(content_type(&response), "application/json");
}

#[tokio::test]
async fn post_body_that_is_not_a_form_is_not_read() {
    let request = Request::post("/rest/ping")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("f=json"))
        .expect("a request");
    let response = server().request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
}

#[tokio::test]
async fn other_http_methods_are_http_405() {
    let server = server();
    for method in ["PUT", "DELETE", "PATCH"] {
        let request = Request::builder()
            .method(method)
            .uri("/rest/ping?f=json")
            .body(Body::empty())
            .expect("a request");
        let response = server.request(request).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
}

/// The line of the captured log that reports the end of the request of the
/// client `client`. Every test of this file logs to the same buffer, so a
/// test finds its request by a client name that only it sends.
fn finished_line(client: &str) -> String {
    let field = format!(r#"c="{client}""#);
    let lines = captured_log().lines_with(&field);
    lines
        .into_iter()
        .find(|line| line.contains("finished processing request"))
        .unwrap_or_else(|| panic!("no line for {field}: {}", captured_log().text()))
}

#[tokio::test]
async fn request_log_names_method_client_user_and_status_without_the_query_string() {
    let log = captured_log();
    let query = urlencode(&[
        ("u", "log-get-user"),
        ("c", "log-get-client"),
        ("v", "1.16.1"),
        ("f", "json"),
        ("p", "get-secret-password"),
        ("t", "get-secret-token"),
        ("s", "get-secret-salt"),
        ("apiKey", "get-secret-key"),
    ]);
    let response = server().get_path(&format!("/rest/ping.view?{query}")).await;
    assert_eq!(response.status(), StatusCode::OK);

    let line = finished_line("log-get-client");
    for field in [r#"method="ping""#, r#"u="log-get-user""#, "status=200"] {
        assert!(line.contains(field), "{field}: {line}");
    }
    let text = log.text();
    assert!(!text.contains("get-secret-"), "{text}");
    assert!(!text.contains(&query), "{text}");
}

#[tokio::test]
async fn request_log_names_client_and_user_of_a_form_body_without_the_body() {
    let log = captured_log();
    let response = server()
        .post_form(
            "getLicense",
            &[
                ("u", "log-form-user"),
                ("c", "log-form-client"),
                ("f", "json"),
                ("p", "form-secret-password"),
                ("t", "form-secret-token"),
                ("s", "form-secret-salt"),
                ("apiKey", "form-secret-key"),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let line = finished_line("log-form-client");
    for field in [
        r#"method="getLicense""#,
        r#"u="log-form-user""#,
        "status=200",
    ] {
        assert!(line.contains(field), "{field}: {line}");
    }
    let text = log.text();
    assert!(!text.contains("form-secret-"), "{text}");
}

#[tokio::test]
async fn request_log_of_a_path_outside_rest_has_the_status_and_no_method() {
    let log = captured_log();
    let response = server()
        .get_path("/log-outside-rest?p=outside-secret")
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let text = log.text();
    assert!(
        text.lines()
            .any(|line| line.contains("request{status=404}")),
        "{text}"
    );
    assert!(!text.contains("outside-secret"), "{text}");
    assert!(!text.contains("log-outside-rest"), "{text}");
}
