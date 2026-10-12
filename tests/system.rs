//! Router tests of the request pipeline and of the system methods `ping`,
//! `getLicense` and `getOpenSubsonicExtensions`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::{EXTENSIONS, METHODS};
use wn_server::config::Config;
use wn_server::db::queries::users;

use common::{
    OWNER, TestServer, body_string, captured_log, content_type, form_request, subsonic_response,
    token, urlencode,
};

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

async fn server() -> TestServer {
    TestServer::bare(Config::default()).await
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
    let response = server().await.get_raw("ping", &[("f", "json")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(body_string(response).await, json_document("ok", ""));
}

#[tokio::test]
async fn ping_view_is_the_xml_envelope_with_xmlns() {
    let response = server().await.get_raw("ping.view", &[]).await;
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
    let server = server().await;
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
    let server = server().await;
    let response = server.get_json("getLicense", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["license"], json!({ "valid": true }));

    assert_eq!(
        server.get_xml("getLicense", &[]).await,
        xml_document("ok", r#"><license valid="true"/></subsonic-response>"#)
    );
}

#[tokio::test]
async fn get_open_subsonic_extensions_returns_the_entries_of_extensions() {
    // The expected list follows `EXTENSIONS`, so the test holds for every
    // set of extensions. A server that advertises none returns `[]`.
    let expected = Value::Array(
        EXTENSIONS
            .iter()
            .map(|(name, versions)| json!({ "name": name, "versions": versions }))
            .collect(),
    );

    let server = server().await;
    let response = server.get_json("getOpenSubsonicExtensions", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["openSubsonicExtensions"], expected);

    let response = server
        .get_raw("getOpenSubsonicExtensions", &[("f", "json")])
        .await;
    assert_eq!(
        body_string(response).await,
        json_document("ok", &format!(r#","openSubsonicExtensions":{expected}"#))
    );

    // XML has one element for each extension, and none for an empty list.
    let xml = server.get_xml("getOpenSubsonicExtensions", &[]).await;
    assert_eq!(
        xml.matches("<openSubsonicExtensions ").count(),
        EXTENSIONS.len(),
        "{xml}"
    );
    for (name, _) in EXTENSIONS {
        let element = format!(r#"<openSubsonicExtensions name="{name}">"#);
        assert!(xml.contains(&element), "{element}: {xml}");
    }
    if EXTENSIONS.is_empty() {
        assert_eq!(xml, xml_document("ok", "/>"));
    }
}

#[tokio::test]
async fn unknown_method_is_error_0_unknown_endpoint() {
    let server = server().await;
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
    let response = server().await.get_json("Ping", &[]).await;
    assert_eq!(response["error"], error_0("unknown endpoint"));
}

#[tokio::test]
async fn listed_method_without_a_handler_is_error_0_not_supported() {
    let server = server().await;
    let response = server.get_json("getVideos", &[]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"], error_0("not supported"));

    assert_eq!(
        server.get_xml("getVideos", &[]).await,
        xml_error_0("not supported")
    );
}

#[tokio::test]
async fn deferred_methods_are_not_supported() {
    let server = server().await;
    for method in NOT_SUPPORTED {
        let response = server.get_json(method, &[]).await;
        assert_eq!(response["status"], "failed", "{method}");
        assert_eq!(response["error"], error_0("not supported"), "{method}");
    }
}

#[tokio::test]
async fn every_listed_method_is_routed() {
    let server = server().await;
    for (method, _) in METHODS {
        for name in [(*method).to_owned(), format!("{method}.view")] {
            // A routed method answers `ok`, or it fails for a reason of its
            // own, such as a missing parameter. Only a name that is not
            // routed fails with `unknown endpoint`.
            let response = server.get_json(&name, &[]).await;
            let status = response["status"].as_str();
            assert!(
                matches!(status, Some("ok" | "failed")),
                "{name}: {status:?}"
            );
            assert_ne!(response["error"]["message"], "unknown endpoint", "{name}");
        }
    }
}

#[tokio::test]
async fn system_methods_answer_ok() {
    let server = server().await;
    for method in ["ping", "getLicense", "getOpenSubsonicExtensions"] {
        let response = server.get_json(method, &[]).await;
        assert_eq!(response["status"], "ok", "{method}");
    }
}

#[tokio::test]
async fn path_outside_rest_is_http_404() {
    let server = server().await;
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
    let server = server().await;
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
    let server = server().await;
    for (method, params) in [
        ("nope", &[("f", "json")][..]),
        ("tokenInfo", &[]),
        ("ping", &[("f", "yaml")]),
    ] {
        let response = server.get_raw(method, params).await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
    }
}

#[tokio::test]
async fn post_with_a_form_body_selects_json() {
    let response = server().await.post_form("ping", &[("f", "json")]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(body_string(response).await, json_document("ok", ""));
}

#[tokio::test]
async fn post_without_a_format_answers_in_xml() {
    let server = server().await;
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
    let response = server().await.request(request).await;
    assert_eq!(content_type(&response), "application/json");
}

#[tokio::test]
async fn post_body_that_is_not_a_form_is_not_read() {
    let request = Request::post("/rest/ping")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("f=json"))
        .expect("a request");
    let response = server().await.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
}

#[tokio::test]
async fn other_http_methods_are_http_405() {
    let server = server().await;
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

/// The lines of the captured log that carry the span of a request of the
/// client `client`. Every test of this file logs to the same buffer, so a
/// test finds its request by a client name that only it sends.
fn request_lines(client: &str) -> Vec<String> {
    captured_log().lines_with(&format!(r#"c="{client}""#))
}

/// The one line of the captured log that reports the request of the client
/// `client`.
fn finished_line(client: &str) -> String {
    let lines: Vec<String> = request_lines(client)
        .into_iter()
        .filter(|line| line.contains("finished processing request"))
        .collect();
    assert_eq!(lines.len(), 1, "{client}: {}", captured_log().text());
    lines.into_iter().next().expect("one line")
}

/// Request parameters: pairs of a name and a value.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The names of the fields of a log line, in the order of the line: the
/// fields of the span and then the fields of the event. The tests send no
/// value that holds `=`, a space or a brace.
fn field_names(line: &str) -> Vec<&str> {
    line.split([' ', '{', '}'])
        .filter_map(|piece| piece.split_once('='))
        .map(|(name, _)| name)
        .collect()
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
    let response = server()
        .await
        .get_path(&format!("/rest/ping.view?{query}"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let line = finished_line("log-get-client");
    for field in [
        " INFO ",
        r#"method="ping""#,
        r#"u="log-get-user""#,
        "status=200",
        "duration_ms=",
    ] {
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
        .await
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
        " INFO ",
        r#"method="getLicense""#,
        r#"u="log-form-user""#,
        "status=200",
        "duration_ms=",
    ] {
        assert!(line.contains(field), "{field}: {line}");
    }
    let text = log.text();
    assert!(!text.contains("form-secret-"), "{text}");
}

#[tokio::test]
async fn request_log_is_one_line_at_info_with_method_client_user_status_and_duration() {
    captured_log();
    let response = server()
        .await
        .get_raw("ping.view", &[("c", "log-one-line-client"), ("f", "json")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    // A request that is answered logs this line and no other. The lines
    // that the trace layer writes by default at the start of a request and
    // at the end of a body are off for every request.
    let lines = request_lines("log-one-line-client");
    assert_eq!(lines.len(), 1, "{lines:?}");
    let log = captured_log();
    for message in ["started processing request", "end of stream"] {
        assert_eq!(log.lines_with(message), Vec::<String>::new());
    }
    let line = &lines[0];
    assert!(line.contains(" INFO "), "{line}");
    assert!(line.contains("finished processing request"), "{line}");
    assert_eq!(
        field_names(line),
        ["method", "c", "u", "status", "duration_ms"],
        "{line}"
    );
    for field in [r#"method="ping""#, r#"u="owner""#, "status=200"] {
        assert!(line.contains(field), "{field}: {line}");
    }
    // The duration is the last field: a whole number of milliseconds.
    let (_, duration) = line.rsplit_once("duration_ms=").expect("the duration");
    assert!(duration.parse::<u64>().is_ok(), "{line}");
}

#[tokio::test]
async fn request_log_of_stream_and_get_cover_art_is_at_debug() {
    captured_log();
    let server = server().await;
    let id = ("id", "tr-00000000000000000000000000000001");
    for (path_name, method, client) in [
        ("stream", "stream", "log-debug-stream"),
        ("stream.view", "stream", "log-debug-stream-view"),
        ("getCoverArt", "getCoverArt", "log-debug-cover"),
        ("getCoverArt.view", "getCoverArt", "log-debug-cover-view"),
    ] {
        server
            .get_raw(path_name, &[("c", client), id, ("f", "json")])
            .await;

        let lines = request_lines(client);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.contains(" DEBUG "), "{line}");
        assert!(line.contains("finished processing request"), "{line}");
        let field = format!(r#"method="{method}""#);
        assert!(line.contains(&field), "{field}: {line}");
    }

    // The level follows the method name. `download` names a track as
    // `stream` does and is logged at `info`.
    server
        .get_raw("download", &[("c", "log-info-download"), id, ("f", "json")])
        .await;
    let line = finished_line("log-info-download");
    assert!(line.contains(" INFO "), "{line}");
}

#[tokio::test]
async fn request_log_of_each_credential_form_has_the_request_and_not_the_credential() {
    let log = captured_log();
    let server = server().await;
    // The owner gets a password that no other test sends, so a line of the
    // log that holds it is a line of this test.
    const PASSWORD: &str = "log-cred-secret-password";
    server
        .db()
        .write(|conn| {
            let owner = users::get_owner(conn)?.expect("the owner");
            users::set_password(conn, owner.user.id, PASSWORD)
        })
        .await
        .expect("set the password");
    let hex_password = hex::encode(PASSWORD);
    let encoded = format!("enc:{hex_password}");
    let salt = "log-cred-secret-salt";
    let token = token(PASSWORD, salt);
    let api_key = "log-cred-secret-key";

    // Each form of a credential: its name, its parameters and whether the
    // server accepts it.
    let plain = [("u", OWNER), ("p", PASSWORD)];
    let encoded = [("u", OWNER), ("p", encoded.as_str())];
    let with_token = [("u", OWNER), ("t", token.as_str()), ("s", salt)];
    let key = [("apiKey", api_key)];
    let forms: [(&str, Pairs, &str); 4] = [
        ("plain", &plain, "ok"),
        ("encoded", &encoded, "ok"),
        ("token", &with_token, "ok"),
        ("key", &key, "failed"),
    ];
    for (form, credential, status) in forms {
        for body in [false, true] {
            // The credential is in the query string, or in a form body.
            let client = format!("log-cred-{form}-{}", if body { "post" } else { "get" });
            let mut params = credential.to_vec();
            params.extend([("v", "1.16.1"), ("c", client.as_str()), ("f", "json")]);
            let response = if body {
                server.request(form_request("/rest/ping", &params)).await
            } else {
                server.get_raw_noauth("ping", &params).await
            };
            assert_eq!(response.status(), StatusCode::OK, "{client}");
            let answer = subsonic_response(&body_string(response).await);
            assert_eq!(answer["status"], status, "{client}");

            // The request has its line, with the fields of the design and
            // no field of a credential.
            let line = finished_line(&client);
            assert!(line.contains(" INFO "), "{line}");
            let mut expected = vec!["method", "c", "u", "status", "duration_ms"];
            if !credential.iter().any(|(name, _)| *name == "u") {
                expected.retain(|name| *name != "u");
            }
            assert_eq!(field_names(&line), expected, "{line}");
        }
    }

    // No line of the log holds a credential: not these lines, and not the
    // line of the attempt that failed.
    let text = log.text();
    for secret in [
        PASSWORD,
        hex_password.as_str(),
        token.as_str(),
        salt,
        api_key,
    ] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
}

#[tokio::test]
async fn request_log_cuts_method_client_and_user_to_64_characters() {
    captured_log();
    let method = format!("log-cut-method-{}", "m".repeat(100));
    let client = format!("log-cut-client-{}", "c".repeat(100));
    let user = format!("log-cut-user-{}", "u".repeat(100));
    let response = server()
        .await
        .get_raw(
            &method,
            &[("c", client.as_str()), ("u", user.as_str()), ("f", "json")],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    // The closing quote follows the 64th character of each value.
    let line = finished_line(&client[..64]);
    for field in [
        format!(r#"method="{}""#, &method[..64]),
        format!(r#"u="{}""#, &user[..64]),
    ] {
        assert!(line.contains(&field), "{field}: {line}");
    }
}

#[tokio::test]
async fn request_log_of_a_path_outside_rest_has_the_status_and_no_method() {
    let log = captured_log();
    let response = server()
        .await
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
