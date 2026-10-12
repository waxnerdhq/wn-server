//! Router tests of request authentication (`docs/design.md` section 3.7): the
//! credential forms, the errors, the form body, the public method, disabled
//! authentication, the protection of every method of the table, and the log
//! of a failed attempt.
//!
//! The tests send their parameters through the `noauth` helpers of
//! `TestServer`, so a request holds the credentials that the test names and
//! no other.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wn_server::VERSION;
use wn_server::api::{METHODS, PUBLIC_METHODS};
use wn_server::config::{Config, DataDir};
use wn_server::server::{ServerError, run_server};

use common::{
    OWNER, OWNER_PASSWORD, TestServer, body_string, captured_log, content_type, form_request,
    subsonic_response, token,
};

/// Parameters of a request: names and values.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The protocol version that the requests of these tests send.
const V: (&str, &str) = ("v", "1.16.1");

/// The client name that the requests of these tests send.
const C: (&str, &str) = ("c", "auth-tests");

/// The hex encoding of the owner password `sesame`.
const SESAME_HEX: &str = "736573616d65";

/// The example of the Subsonic API documentation: the token of the password
/// `sesame` for the salt `c19b2d`.
const SESAME_SALT: &str = "c19b2d";
const SESAME_TOKEN: &str = "26719a1196d2a940705a59634eb18eab";

/// A server that authenticates.
async fn server() -> TestServer {
    TestServer::bare(Config::default()).await
}

/// A server with `auth.disabled_unsafe = true`.
async fn server_without_auth() -> TestServer {
    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    TestServer::bare(config).await
}

/// Calls `ping` with `params` and `f=json`, and no other parameter.
async fn ping(server: &TestServer, params: Pairs<'_>) -> Value {
    server.get_json_noauth("ping", params).await
}

/// The `error` object with `code` and `message`.
fn error(code: u32, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

/// The `error` object of error 10 that names `parameter`.
fn error_10(parameter: &str) -> Value {
    error(10, &format!("required parameter is missing: {parameter}"))
}

/// The `error` object of error 40.
fn error_40() -> Value {
    error(40, "wrong username or password")
}

/// The `error` object of error 42.
fn error_42() -> Value {
    error(42, "authentication mechanism not supported")
}

/// The `error` object of error 43.
fn error_43() -> Value {
    error(43, "conflicting authentication mechanisms")
}

/// The JSON body of a failed response with `code` and `message`.
fn json_error_document(code: u32, message: &str) -> String {
    format!(
        concat!(
            r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","type":"wn-server","#,
            r#""serverVersion":"{}","openSubsonic":true,"#,
            r#""error":{{"code":{},"message":"{}"}}}}}}"#
        ),
        VERSION, code, message
    )
}

/// The XML body of a failed response with `code` and `message`.
fn xml_error_document(code: u32, message: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true">"#,
            r#"<error code="{}" message="{}"/></subsonic-response>"#
        ),
        VERSION, code, message
    )
}

/// Sends `request`. Checks that the response is HTTP 200 with the JSON
/// content type. Returns the value of `subsonic-response`.
async fn json_of(server: &TestServer, request: Request<Body>) -> Value {
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    subsonic_response(&body_string(response).await)
}

#[tokio::test]
async fn ping_with_a_plain_password_succeeds() {
    let server = server().await;
    let response = ping(&server, &[("u", OWNER), ("p", OWNER_PASSWORD), V, C]).await;
    assert_eq!(response["status"], "ok");
    assert!(response.get("error").is_none(), "{response}");
}

#[tokio::test]
async fn ping_with_an_enc_hex_password_succeeds() {
    let server = server().await;
    let upper = format!("enc:{}", SESAME_HEX.to_uppercase());
    for password in [format!("enc:{SESAME_HEX}"), upper] {
        let response = ping(&server, &[("u", OWNER), ("p", &password), V, C]).await;
        assert_eq!(response["status"], "ok", "{password}");
    }
}

#[tokio::test]
async fn ping_with_a_token_and_salt_succeeds() {
    let server = server().await;
    let response = ping(
        &server,
        &[("u", OWNER), ("t", SESAME_TOKEN), ("s", SESAME_SALT), V, C],
    )
    .await;
    assert_eq!(response["status"], "ok");

    // Another salt has another token.
    let other = token(OWNER_PASSWORD, "another salt & more");
    assert_ne!(other, SESAME_TOKEN);
    let response = ping(
        &server,
        &[
            ("u", OWNER),
            ("t", &other),
            ("s", "another salt & more"),
            V,
            C,
        ],
    )
    .await;
    assert_eq!(response["status"], "ok");
}

#[tokio::test]
async fn each_credential_form_succeeds_in_xml() {
    let server = server().await;
    let enc = format!("enc:{SESAME_HEX}");
    let forms: [Pairs; 3] = [
        &[("u", OWNER), ("p", OWNER_PASSWORD), V, C],
        &[("u", OWNER), ("p", &enc), V, C],
        &[("u", OWNER), ("t", SESAME_TOKEN), ("s", SESAME_SALT), V, C],
    ];
    for params in forms {
        let response = server.get_raw_noauth("ping.view", params).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(content_type(&response), "application/xml");
        let body = body_string(response).await;
        assert!(body.contains(r#"status="ok""#), "{body}");
        assert!(body.ends_with(r#"openSubsonic="true"/>"#), "{body}");
    }
}

#[tokio::test]
async fn user_name_is_found_without_regard_to_ascii_case() {
    let server = server().await;
    for name in ["OWNER", "Owner"] {
        let response = ping(&server, &[("u", name), ("p", OWNER_PASSWORD), V, C]).await;
        assert_eq!(response["status"], "ok", "{name}");
    }
}

#[tokio::test]
async fn wrong_credential_is_error_40() {
    let server = server().await;
    let upper_token = SESAME_TOKEN.to_uppercase();
    let cases: [Pairs; 11] = [
        &[("p", "wrong")],
        &[("p", "")],
        &[("p", "Sesame")],
        &[("p", "sesame ")],
        // The hex encoding of `sesamf`.
        &[("p", "enc:736573616d66")],
        // Not hex, and hex with an odd number of digits.
        &[("p", "enc:sesame")],
        &[("p", "enc:736573616d6")],
        // The token of another salt, in upper case, and of no password.
        &[("t", SESAME_TOKEN), ("s", "c19b2e")],
        &[("t", &upper_token), ("s", SESAME_SALT)],
        &[("t", ""), ("s", "")],
        // The password is not a token.
        &[("t", OWNER_PASSWORD), ("s", SESAME_SALT)],
    ];
    for credential in cases {
        let mut params = vec![("u", OWNER), V, C];
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "failed", "{credential:?}");
        assert_eq!(response["error"], error_40(), "{credential:?}");
    }
}

#[tokio::test]
async fn unknown_user_is_error_40_like_a_wrong_password() {
    let server = server().await;
    // The token of an empty password for the salt.
    let empty_password_token = token("", SESAME_SALT);
    let cases: [Pairs; 7] = [
        &[("u", "nobody"), ("p", OWNER_PASSWORD)],
        &[("u", "nobody"), ("t", SESAME_TOKEN), ("s", SESAME_SALT)],
        &[("u", ""), ("p", OWNER_PASSWORD)],
        &[("u", "owner "), ("p", OWNER_PASSWORD)],
        // The credentials that match an empty password.
        &[("u", "nobody"), ("p", "")],
        &[("u", "nobody"), ("p", "enc:")],
        &[
            ("u", "nobody"),
            ("t", &empty_password_token),
            ("s", SESAME_SALT),
        ],
    ];
    for credential in cases {
        let mut params = vec![V, C];
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "failed", "{credential:?}");
        // The response does not tell an unknown name from a wrong password.
        assert_eq!(response["error"], error_40(), "{credential:?}");
    }
}

#[tokio::test]
async fn missing_v_or_c_is_error_10_naming_the_parameter() {
    let server = server().await;
    let credentials = [("u", OWNER), ("p", OWNER_PASSWORD)];

    let response = ping(&server, &[credentials[0], credentials[1], C]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"], error_10("v"));

    let response = ping(&server, &[credentials[0], credentials[1], V]).await;
    assert_eq!(response["error"], error_10("c"));

    // `v` is named first when both are absent.
    let response = ping(&server, &credentials).await;
    assert_eq!(response["error"], error_10("v"));

    // The answer is the same for a wrong password and for a token.
    let response = ping(&server, &[("u", OWNER), ("p", "wrong"), C]).await;
    assert_eq!(response["error"], error_10("v"));
    let response = ping(
        &server,
        &[("u", OWNER), ("t", SESAME_TOKEN), ("s", SESAME_SALT), V],
    )
    .await;
    assert_eq!(response["error"], error_10("c"));
}

#[tokio::test]
async fn value_of_v_is_not_compared() {
    let server = server().await;
    for version in ["", "0.0.1", "1.13.0", "99", "not a version"] {
        let response = ping(
            &server,
            &[("u", OWNER), ("p", OWNER_PASSWORD), ("v", version), C],
        )
        .await;
        assert_eq!(response["status"], "ok", "{version}");
    }
}

#[tokio::test]
async fn request_without_a_user_name_or_a_credential_is_error_10() {
    let server = server().await;
    let cases: [(Pairs, &str); 7] = [
        (&[], "u"),
        (&[("p", OWNER_PASSWORD)], "u"),
        (&[("t", SESAME_TOKEN), ("s", SESAME_SALT)], "u"),
        (&[("u", OWNER)], "p, or t and s"),
        (&[("u", "nobody")], "p, or t and s"),
        (&[("u", OWNER), ("t", SESAME_TOKEN)], "s"),
        (&[("u", OWNER), ("s", SESAME_SALT)], "t"),
    ];
    for (credential, parameter) in cases {
        let mut params = vec![V, C];
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "failed", "{credential:?}");
        assert_eq!(response["error"], error_10(parameter), "{credential:?}");
    }
}

#[tokio::test]
async fn api_key_is_error_42() {
    let server = server().await;
    for key in ["some-key", ""] {
        let response = ping(&server, &[("apiKey", key), V, C]).await;
        assert_eq!(response["status"], "failed", "{key:?}");
        assert_eq!(response["error"], error_42(), "{key:?}");
    }
}

#[tokio::test]
async fn api_key_with_a_user_name_is_error_43() {
    let server = server().await;
    let cases: [Pairs; 6] = [
        &[("u", OWNER)],
        &[("u", "nobody")],
        &[("u", OWNER), ("p", OWNER_PASSWORD)],
        &[("u", OWNER), ("t", SESAME_TOKEN), ("s", SESAME_SALT)],
        // A key next to a credential without a name.
        &[("p", OWNER_PASSWORD)],
        &[("t", SESAME_TOKEN), ("s", SESAME_SALT)],
    ];
    for other in cases {
        let mut params = vec![("apiKey", "some-key"), V, C];
        params.extend_from_slice(other);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "failed", "{other:?}");
        assert_eq!(response["error"], error_43(), "{other:?}");
    }
}

#[tokio::test]
async fn password_with_a_token_is_error_43() {
    let server = server().await;
    let cases: [Pairs; 5] = [
        // Both credentials are right.
        &[
            ("u", OWNER),
            ("p", OWNER_PASSWORD),
            ("t", SESAME_TOKEN),
            ("s", SESAME_SALT),
        ],
        &[("u", OWNER), ("p", OWNER_PASSWORD), ("t", SESAME_TOKEN)],
        &[("u", OWNER), ("p", OWNER_PASSWORD), ("s", SESAME_SALT)],
        &[("u", OWNER), ("p", "wrong"), ("t", "wrong")],
        &[("u", "nobody"), ("p", OWNER_PASSWORD), ("t", SESAME_TOKEN)],
    ];
    for credential in cases {
        let mut params = vec![V, C];
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "failed", "{credential:?}");
        assert_eq!(response["error"], error_43(), "{credential:?}");
    }
}

#[tokio::test]
async fn auth_error_is_the_documented_envelope_in_json_and_xml() {
    let server = server().await;
    let wrong = [("u", OWNER), ("p", "wrong"), V, C];

    let mut params = wrong.to_vec();
    params.push(("f", "json"));
    let response = server.get_raw_noauth("ping", &params).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(
        body_string(response).await,
        json_error_document(40, "wrong username or password")
    );

    for method in ["ping", "ping.view"] {
        let response = server.get_raw_noauth(method, &wrong).await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        assert_eq!(content_type(&response), "application/xml", "{method}");
        assert_eq!(
            body_string(response).await,
            xml_error_document(40, "wrong username or password"),
            "{method}"
        );
    }

    let cases: [(Pairs, u32, &str); 4] = [
        (&[], 10, "required parameter is missing: v"),
        (&[V, C], 10, "required parameter is missing: u"),
        (
            &[V, C, ("apiKey", "some-key")],
            42,
            "authentication mechanism not supported",
        ),
        (
            &[V, C, ("apiKey", "some-key"), ("u", OWNER)],
            43,
            "conflicting authentication mechanisms",
        ),
    ];
    for (params, code, message) in cases {
        let response = server.get_raw_noauth("getLicense", params).await;
        assert_eq!(response.status(), StatusCode::OK, "{code}");
        assert_eq!(content_type(&response), "application/xml", "{code}");
        assert_eq!(
            body_string(response).await,
            xml_error_document(code, message),
            "{code}"
        );
    }
}

#[tokio::test]
async fn post_with_every_parameter_in_the_form_body_authenticates() {
    let server = server().await;
    let enc = format!("enc:{SESAME_HEX}");
    let forms: [Pairs; 3] = [
        &[("u", OWNER), ("p", OWNER_PASSWORD), V, C, ("f", "json")],
        &[("u", OWNER), ("p", &enc), V, C, ("f", "json")],
        &[
            ("u", OWNER),
            ("t", SESAME_TOKEN),
            ("s", SESAME_SALT),
            V,
            C,
            ("f", "json"),
        ],
    ];
    for params in forms {
        for path in ["/rest/ping", "/rest/ping.view", "/rest/getLicense"] {
            let response = json_of(&server, form_request(path, params)).await;
            assert_eq!(response["status"], "ok", "{path} {params:?}");
        }
    }
}

#[tokio::test]
async fn post_with_a_wrong_credential_in_the_form_body_is_rejected() {
    let server = server().await;
    let cases: [(Pairs, Value); 5] = [
        (&[("u", OWNER), ("p", "wrong"), V, C], error_40()),
        (&[("u", "nobody"), ("p", OWNER_PASSWORD), V, C], error_40()),
        (&[("u", OWNER), ("p", OWNER_PASSWORD), C], error_10("v")),
        (&[("apiKey", "some-key"), V, C], error_42()),
        (
            &[
                ("u", OWNER),
                ("p", OWNER_PASSWORD),
                ("t", SESAME_TOKEN),
                V,
                C,
            ],
            error_43(),
        ),
    ];
    for (params, expected) in cases {
        let mut params = params.to_vec();
        params.push(("f", "json"));
        let response = json_of(&server, form_request("/rest/ping", &params)).await;
        assert_eq!(response["status"], "failed", "{params:?}");
        assert_eq!(response["error"], expected, "{params:?}");
    }
}

#[tokio::test]
async fn query_string_and_form_body_are_one_set_of_parameters() {
    let server = server().await;

    // The name is in the query string and the password is in the body.
    let request = form_request(
        "/rest/ping?u=owner&v=1.16.1&c=auth-tests&f=json",
        &[("p", OWNER_PASSWORD)],
    );
    assert_eq!(json_of(&server, request).await["status"], "ok");

    // The query string supplies the first value of `p`.
    let request = form_request(
        "/rest/ping?u=owner&p=wrong&v=1.16.1&c=auth-tests&f=json",
        &[("p", OWNER_PASSWORD)],
    );
    assert_eq!(json_of(&server, request).await["error"], error_40());

    // A password in the query string and a token in the body conflict.
    let request = form_request(
        "/rest/ping?u=owner&p=sesame&v=1.16.1&c=auth-tests&f=json",
        &[("t", SESAME_TOKEN), ("s", SESAME_SALT)],
    );
    assert_eq!(json_of(&server, request).await["error"], error_43());

    // The form body of a GET request is not read, so it authenticates
    // nothing.
    let request = Request::get("/rest/ping?f=json")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("u=owner&p=sesame&v=1.16.1&c=auth-tests"))
        .expect("a request");
    assert_eq!(json_of(&server, request).await["error"], error_10("v"));

    // A POST body of another content type is not read either.
    let request = Request::post("/rest/ping?f=json")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("u=owner&p=sesame&v=1.16.1&c=auth-tests"))
        .expect("a request");
    assert_eq!(json_of(&server, request).await["error"], error_10("v"));
}

#[tokio::test]
async fn get_open_subsonic_extensions_needs_no_credentials_and_lists_form_post() {
    let form_post = json!({ "name": "formPost", "versions": [1] });
    for server in [server().await, server_without_auth().await] {
        // No parameter at all, not even `v` and `c`.
        let response = server
            .get_json_noauth("getOpenSubsonicExtensions", &[])
            .await;
        assert_eq!(response["status"], "ok");
        let extensions = response["openSubsonicExtensions"]
            .as_array()
            .expect("an array of extensions");
        assert!(extensions.contains(&form_post), "{response}");

        // Credentials that authenticate nothing do not change the answer.
        let cases: [Pairs; 4] = [
            &[("u", OWNER), ("p", "wrong"), V, C],
            &[("u", "nobody"), ("t", "abc"), ("s", "def")],
            &[("apiKey", "some-key"), ("u", OWNER)],
            &[("u", OWNER), ("p", OWNER_PASSWORD), ("t", SESAME_TOKEN)],
        ];
        for params in cases {
            for method in [
                "getOpenSubsonicExtensions",
                "getOpenSubsonicExtensions.view",
            ] {
                let response = server.get_json_noauth(method, params).await;
                assert_eq!(response["status"], "ok", "{method} {params:?}");
                assert_eq!(
                    response["openSubsonicExtensions"][0], form_post,
                    "{method} {params:?}"
                );
            }
        }

        let response = server
            .get_raw_noauth("getOpenSubsonicExtensions", &[])
            .await;
        assert_eq!(content_type(&response), "application/xml");
        let xml = body_string(response).await;
        assert!(
            xml.contains(
                r#"<openSubsonicExtensions name="formPost"><versions>1</versions></openSubsonicExtensions>"#
            ),
            "{xml}"
        );

        let request = form_request("/rest/getOpenSubsonicExtensions", &[("f", "json")]);
        let response = json_of(&server, request).await;
        assert_eq!(response["openSubsonicExtensions"][0], form_post);
    }
}

#[tokio::test]
async fn every_method_that_is_not_public_needs_a_user() {
    let server = server().await;
    let wrong_token = token("wrong", SESAME_SALT);
    let rejected: [(Pairs, Value); 9] = [
        (&[], error_10("v")),
        (&[V], error_10("c")),
        (&[V, C], error_10("u")),
        (&[V, C, ("u", OWNER)], error_10("p, or t and s")),
        (&[V, C, ("u", OWNER), ("p", "wrong")], error_40()),
        (&[V, C, ("u", "nobody"), ("p", OWNER_PASSWORD)], error_40()),
        (
            &[V, C, ("u", OWNER), ("t", &wrong_token), ("s", "other")],
            error_40(),
        ),
        (&[V, C, ("apiKey", "some-key")], error_42()),
        (
            &[
                V,
                C,
                ("u", OWNER),
                ("p", OWNER_PASSWORD),
                ("t", SESAME_TOKEN),
            ],
            error_43(),
        ),
    ];

    let mut protected = 0;
    for (method, _) in METHODS {
        if PUBLIC_METHODS.contains(method) {
            continue;
        }
        protected += 1;
        for name in [(*method).to_owned(), format!("{method}.view")] {
            // The error is the one of the missing or wrong credential, in the
            // envelope. A handler that ran would answer something else: a
            // method without a handler answers `not supported`.
            for (params, expected) in &rejected {
                let response = server.get_json_noauth(&name, params).await;
                assert_eq!(response["status"], "failed", "{name} {params:?}");
                assert_eq!(response["error"], *expected, "{name} {params:?}");
                assert_eq!(
                    response.as_object().map(serde_json::Map::len),
                    Some(6),
                    "{name} {params:?}: {response}"
                );
            }

            // The same holds for a form body.
            let path = format!("/rest/{name}");
            let form = [("u", OWNER), ("p", "wrong"), V, C, ("f", "json")];
            let response = json_of(&server, form_request(&path, &form)).await;
            assert_eq!(response["error"], error_40(), "{name}");

            // A request with a user reaches the handler.
            for credential in [
                &[("u", OWNER), ("p", OWNER_PASSWORD)][..],
                &[("u", OWNER), ("t", SESAME_TOKEN), ("s", SESAME_SALT)],
            ] {
                let mut params = vec![V, C];
                params.extend_from_slice(credential);
                let response = server.get_json_noauth(&name, &params).await;
                let code = response["error"]["code"].as_u64();
                assert!(
                    !matches!(code, Some(40..=44)),
                    "{name} {credential:?}: {response}"
                );
                assert_ne!(response["error"], error_10("u"), "{name}");
            }
        }
    }
    assert_eq!(protected, METHODS.len() - PUBLIC_METHODS.len());
    assert_eq!(PUBLIC_METHODS, ["getOpenSubsonicExtensions"]);
}

#[tokio::test]
async fn method_without_a_handler_answers_not_supported_only_to_a_user() {
    let server = server().await;
    let response = server.get_json_noauth("getVideos", &[V, C]).await;
    assert_eq!(response["error"], error_10("u"));

    let response = server
        .get_json_noauth("getVideos", &[("u", OWNER), ("p", OWNER_PASSWORD), V, C])
        .await;
    assert_eq!(response["error"], error(0, "not supported"));

    // A name that is not in the table has no handler to protect.
    let response = server.get_json_noauth("nope", &[]).await;
    assert_eq!(response["error"], error(0, "unknown endpoint"));
}

#[tokio::test]
async fn ping_with_only_v_and_c_succeeds_when_auth_is_disabled() {
    let server = server_without_auth().await;
    let response = ping(&server, &[V, C]).await;
    assert_eq!(response["status"], "ok");
    assert!(response.get("error").is_none(), "{response}");

    let response = server.get_raw_noauth("ping.view", &[V, C]).await;
    assert_eq!(content_type(&response), "application/xml");
    let body = body_string(response).await;
    assert!(body.contains(r#"status="ok""#), "{body}");

    let request = form_request("/rest/getLicense", &[V, C, ("f", "json")]);
    let response = json_of(&server, request).await;
    assert_eq!(response["license"], json!({ "valid": true }));
}

#[tokio::test]
async fn disabled_auth_reads_no_credential() {
    let server = server_without_auth().await;
    let cases: [Pairs; 7] = [
        &[("u", OWNER), ("p", OWNER_PASSWORD)],
        &[("u", OWNER), ("p", "wrong")],
        &[("u", "nobody"), ("p", "wrong")],
        &[("u", OWNER)],
        &[("apiKey", "some-key")],
        &[("apiKey", "some-key"), ("u", OWNER)],
        &[("u", OWNER), ("p", OWNER_PASSWORD), ("t", SESAME_TOKEN)],
    ];
    for credential in cases {
        let mut params = vec![V, C];
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["status"], "ok", "{credential:?}");
    }
}

#[tokio::test]
async fn disabled_auth_still_requires_v_and_c() {
    let server = server_without_auth().await;
    let response = ping(&server, &[]).await;
    assert_eq!(response["error"], error_10("v"));
    let response = ping(&server, &[C]).await;
    assert_eq!(response["error"], error_10("v"));
    let response = ping(&server, &[V]).await;
    assert_eq!(response["error"], error_10("c"));
    let response = ping(&server, &[("u", OWNER), ("p", OWNER_PASSWORD)]).await;
    assert_eq!(response["error"], error_10("v"));
}

#[tokio::test]
async fn disabled_auth_opens_every_method_of_the_table() {
    let server = server_without_auth().await;
    for (method, _) in METHODS {
        let response = server.get_json_noauth(method, &[V, C]).await;
        let code = response["error"]["code"].as_u64();
        assert!(!matches!(code, Some(40..=44)), "{method}: {response}");
        assert_ne!(response["error"], error_10("u"), "{method}");
    }
}

#[tokio::test]
async fn auth_defaults_of_the_test_server_authenticate_each_request_with_a_new_salt() {
    let server = server().await;
    for method in ["ping", "getLicense"] {
        assert_eq!(server.get_json(method, &[]).await["status"], "ok");
        assert!(server.get_xml(method, &[]).await.contains(r#"status="ok""#));
        let response = server.post_form(method, &[("f", "json")]).await;
        let response = subsonic_response(&body_string(response).await);
        assert_eq!(response["status"], "ok", "{method}");
    }

    // A parameter of the caller replaces the default of that name.
    let response = server.get_json("ping", &[("u", "nobody")]).await;
    assert_eq!(response["error"], error_40());

    // The `noauth` helpers add nothing.
    let response = server.get_json_noauth("ping", &[]).await;
    assert_eq!(response["error"], error_10("v"));
}

/// The lines of the captured log that report a failed attempt of the client
/// `client`. Every test of this file logs to the same buffer, so a test
/// finds its lines by a client name that only it sends.
fn failed_lines(client: &str) -> Vec<String> {
    captured_log()
        .lines_with(&format!(r#"client="{client}""#))
        .into_iter()
        .filter(|line| line.contains("authentication failed"))
        .collect()
}

#[tokio::test]
async fn failed_attempt_is_logged_with_user_and_client_and_without_the_credential() {
    let log = captured_log();
    let server = server().await;

    let secret_hex = hex::encode("log-secret-encoded");
    let enc = format!("enc:{secret_hex}");
    // The right token of the owner for a salt, sent with another salt.
    let right_token = token(OWNER_PASSWORD, "log-secret-right-salt");
    let attempts: [(&str, Pairs, u32); 9] = [
        ("log-fail-plain", &[("p", "log-secret-password")], 40),
        ("log-fail-enc", &[("p", &enc)], 40),
        (
            "log-fail-token",
            &[("t", "log-secret-token"), ("s", "log-secret-salt")],
            40,
        ),
        (
            "log-fail-salt",
            &[("t", &right_token), ("s", "log-secret-other-salt")],
            40,
        ),
        ("log-fail-key", &[("apiKey", "log-secret-key")], 43),
        (
            "log-fail-both",
            &[("p", "log-secret-both"), ("t", "log-secret-token-2")],
            43,
        ),
        ("log-fail-half", &[("t", "log-secret-half")], 10),
        ("log-fail-none", &[], 10),
        // The right password of the owner in a request that lacks `v`.
        ("log-fail-version", &[("p", OWNER_PASSWORD)], 10),
    ];
    for (client, credential, code) in attempts {
        let mut params = vec![("u", "log-fail-user"), ("c", client)];
        if client != "log-fail-version" {
            params.push(V);
        }
        params.extend_from_slice(credential);
        let response = ping(&server, &params).await;
        assert_eq!(response["error"]["code"], code, "{client}");

        let lines = failed_lines(client);
        assert_eq!(lines.len(), 1, "{client}: {lines:?}");
        let line = &lines[0];
        for field in [
            " INFO ".to_owned(),
            r#"user="log-fail-user""#.to_owned(),
            format!("code={code}"),
        ] {
            assert!(line.contains(&field), "{field}: {line}");
        }
    }

    // The same attempts in a form body.
    let form = [
        ("u", "log-fail-user"),
        ("c", "log-fail-form"),
        V,
        ("p", "log-secret-form-password"),
        ("f", "json"),
    ];
    let response = json_of(&server, form_request("/rest/ping", &form)).await;
    assert_eq!(response["error"], error_40());
    assert_eq!(failed_lines("log-fail-form").len(), 1);

    // An attempt with the name of the owner and a wrong token.
    let params = [
        ("u", OWNER),
        ("c", "log-fail-owner"),
        V,
        ("t", "log-secret-owner-token"),
        ("s", "log-secret-owner-salt"),
    ];
    assert_eq!(ping(&server, &params).await["error"], error_40());
    let lines = failed_lines("log-fail-owner");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(r#"user="owner""#), "{}", lines[0]);

    // The log holds every line of this test binary, down to the `trace`
    // level. No line holds a password, a token, a salt or a key, in any
    // encoding, and none holds the stored password of the owner.
    let text = log.text();
    for secret in [
        "log-secret-",
        secret_hex.as_str(),
        right_token.as_str(),
        OWNER_PASSWORD,
        SESAME_HEX,
        SESAME_TOKEN,
    ] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
}

#[tokio::test]
async fn failed_attempt_log_cuts_the_user_and_the_client_to_64_characters() {
    captured_log();
    let server = server().await;
    let user = format!("log-cut-user-{}", "u".repeat(100));
    let client = format!("log-cut-client-{}", "c".repeat(100));
    let response = ping(&server, &[("u", &user), ("c", &client), V, ("p", "wrong")]).await;
    assert_eq!(response["error"], error_40());

    // The closing quote follows the 64th character of each value.
    let lines = failed_lines(&client[..64]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    let field = format!(r#"user="{}""#, &user[..64]);
    assert!(lines[0].contains(&field), "{field}: {}", lines[0]);
}

#[tokio::test]
async fn failed_attempt_log_keeps_a_line_break_of_a_value_inside_its_line() {
    let log = captured_log();
    let server = server().await;
    let user = "log-break-user\n2026-01-01T00:00:00Z  INFO forged line";
    let response = ping(
        &server,
        &[("u", user), ("c", "log-break-client"), V, ("p", "wrong")],
    )
    .await;
    assert_eq!(response["error"], error_40());

    let lines = failed_lines("log-break-client");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains(r"log-break-user\n2026-01-01"),
        "{}",
        lines[0]
    );
    assert!(
        !log.text()
            .lines()
            .any(|line| line.starts_with("2026-01-01T00:00:00Z  INFO forged")),
        "{}",
        log.text()
    );
}

#[tokio::test]
async fn successful_request_logs_no_failed_attempt() {
    captured_log();
    let server = server().await;
    let response = ping(
        &server,
        &[
            ("u", OWNER),
            ("p", OWNER_PASSWORD),
            V,
            ("c", "log-ok-client"),
        ],
    )
    .await;
    assert_eq!(response["status"], "ok");
    assert!(failed_lines("log-ok-client").is_empty());

    // A public method logs none either, whatever the credentials are.
    let response = server
        .get_json_noauth(
            "getOpenSubsonicExtensions",
            &[("u", OWNER), ("p", "wrong"), V, ("c", "log-public-client")],
        )
        .await;
    assert_eq!(response["status"], "ok");
    assert!(failed_lines("log-public-client").is_empty());
}

#[tokio::test]
async fn run_warns_at_startup_that_disabled_auth_is_unsafe() {
    let log = captured_log();
    let dir = tempfile::tempdir().expect("a temporary directory");
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
    let warning = "authentication is disabled (auth.disabled_unsafe = true)";

    // The data directory has no database, so the server stops before it
    // binds an address. No other test of this file starts the server, so
    // every line with the warning is a line of this test.
    let error = run_server(Config::default(), &data_dir, CancellationToken::new())
        .await
        .expect_err("the server does not start without a database");
    assert!(matches!(error, ServerError::NoDatabase(_)), "{error:?}");
    assert_eq!(log.lines_with(warning), Vec::<String>::new());

    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    let error = run_server(config, &data_dir, CancellationToken::new())
        .await
        .expect_err("the server does not start without a database");
    assert!(matches!(error, ServerError::NoDatabase(_)), "{error:?}");
    let lines = log.lines_with(warning);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(" WARN "), "{}", lines[0]);
    assert!(lines[0].contains("This is unsafe"), "{}", lines[0]);
}
