//! Router tests of the user methods `getUser` and `getUsers`
//! (`docs/design.md` section 4).

mod common;

use std::path::PathBuf;

use serde_json::{Value, json};
use wn_server::db::queries::folders;

use common::{
    OWNER, OWNER_PASSWORD, TestServer, body_string, content_type, error, server,
    server_without_auth, subsonic_response, xml_document,
};

/// The protocol version that the requests without the auth defaults send.
const V: (&str, &str) = ("v", "1.16.1");

/// The client name that the requests without the auth defaults send.
const C: (&str, &str) = ("c", "users-tests");

/// The message of the error for the name of another user.
const OTHER_USER: &str = "username must be the name of the user of the request";

/// The attributes of the `user` element of the owner `owner`, in the order
/// the XML body holds them.
const USER_ATTRIBUTES: &str = concat!(
    r#"adminRole="true" commentRole="false" coverArtRole="false" downloadRole="false""#,
    r#" jukeboxRole="false" playlistRole="true" podcastRole="false" scrobblingEnabled="true""#,
    r#" settingsRole="false" shareRole="false" streamRole="true" uploadRole="false""#,
    r#" username="owner" videoConversionRole="false""#
);

/// Adds the music folders `/music/rock` and `/music/jazz` to the database of
/// `server`. Returns their IDs.
async fn add_folders(server: &TestServer) -> Vec<i64> {
    let paths = [PathBuf::from("/music/rock"), PathBuf::from("/music/jazz")];
    let rows = server
        .db()
        .write(move |conn| folders::sync_folders(conn, &paths))
        .await
        .expect("insert the folders");
    rows.into_iter().map(|folder| folder.id).collect()
}

/// The `user` object of the owner `owner` with the folder IDs `folder`.
fn owner_user(folder: &[i64]) -> Value {
    json!({
        "username": "owner",
        "scrobblingEnabled": true,
        "adminRole": true,
        "settingsRole": false,
        "downloadRole": false,
        "uploadRole": false,
        "playlistRole": true,
        "coverArtRole": false,
        "commentRole": false,
        "podcastRole": false,
        "streamRole": true,
        "jukeboxRole": false,
        "shareRole": false,
        "videoConversionRole": false,
        "folder": folder,
    })
}

#[tokio::test]
async fn get_user_returns_the_owner_with_its_roles_and_the_folder_ids() {
    let server = server().await;
    let ids = add_folders(&server).await;
    assert_eq!(ids, [1, 2]);

    let response = server.get_json("getUser", &[("username", "owner")]).await;
    assert_eq!(response["status"], "ok");
    // The comparison is of the whole object, so the user has these fields
    // and no other. It has no `email`.
    assert_eq!(response["user"], owner_user(&[1, 2]));
    assert_eq!(response["user"]["downloadRole"], false);
    assert_eq!(response["user"]["adminRole"], true);
    assert_eq!(response["user"]["folder"], json!([1, 2]));
    assert!(response["user"].get("email").is_none(), "{response}");
}

#[tokio::test]
async fn owner_has_no_download_role() {
    for server in [server().await, server_without_auth().await] {
        add_folders(&server).await;

        // The role is the JSON boolean `false`, and it is present.
        let response = server.get_json("getUser", &[("username", OWNER)]).await;
        assert_eq!(response["user"]["downloadRole"], Value::Bool(false));
        let response = server.get_json("getUsers", &[]).await;
        let users = response["users"]["user"].as_array().expect("a user list");
        assert!(!users.is_empty());
        for user in users {
            assert_eq!(user["downloadRole"], Value::Bool(false), "{user}");
        }

        for (method, params) in [("getUser", &[("username", OWNER)][..]), ("getUsers", &[])] {
            let xml = server.get_xml(method, params).await;
            assert!(xml.contains(r#" downloadRole="false""#), "{method}: {xml}");
            assert!(!xml.contains(r#"downloadRole="true""#), "{method}: {xml}");
        }
    }
}

#[tokio::test]
async fn get_user_xml_has_the_roles_as_attributes_and_one_folder_element_for_each_id() {
    let server = server().await;
    add_folders(&server).await;
    assert_eq!(
        server.get_xml("getUser", &[("username", "owner")]).await,
        xml_document(
            "ok",
            &format!(
                "><user {USER_ATTRIBUTES}><folder>1</folder><folder>2</folder></user>\
                 </subsonic-response>"
            )
        )
    );
}

#[tokio::test]
async fn user_of_a_server_without_folders_has_an_empty_folder_list() {
    let server = server().await;
    let response = server.get_json("getUser", &[("username", "owner")]).await;
    assert_eq!(response["user"], owner_user(&[]));
    assert_eq!(response["user"]["folder"], json!([]));

    assert_eq!(
        server.get_xml("getUser", &[("username", "owner")]).await,
        xml_document(
            "ok",
            &format!("><user {USER_ATTRIBUTES}/></subsonic-response>")
        )
    );
}

#[tokio::test]
async fn get_user_without_username_is_error_10() {
    for server in [server().await, server_without_auth().await] {
        add_folders(&server).await;
        for method in ["getUser", "getUser.view"] {
            let response = server.get_json(method, &[]).await;
            assert_eq!(response["status"], "failed", "{method}");
            assert_eq!(
                response["error"],
                error(10, "required parameter is missing: username"),
                "{method}"
            );
            assert!(response.get("user").is_none(), "{method}: {response}");
        }
    }

    // The name of the user of the request is the value of `u`, and it does
    // not stand in for `username`.
    let response = server()
        .await
        .get_json_noauth("getUser", &[("u", OWNER), ("p", OWNER_PASSWORD), V, C])
        .await;
    assert_eq!(
        response["error"],
        error(10, "required parameter is missing: username")
    );
}

#[tokio::test]
async fn get_user_for_another_name_is_error_50() {
    let server = server().await;
    add_folders(&server).await;
    // No account has one of these names. The answer does not tell whether an
    // account exists.
    for username in ["other", "admin", "owner2", "owne", " owner", ""] {
        for method in ["getUser", "getUser.view"] {
            let response = server.get_json(method, &[("username", username)]).await;
            assert_eq!(response["status"], "failed", "{method} {username:?}");
            assert_eq!(
                response["error"],
                error(50, OTHER_USER),
                "{method} {username:?}"
            );
            assert!(response.get("user").is_none(), "{method} {username:?}");
        }
    }

    assert_eq!(
        server.get_xml("getUser", &[("username", "other")]).await,
        xml_document(
            "failed",
            &format!(r#"><error code="50" message="{OTHER_USER}"/></subsonic-response>"#)
        )
    );
}

#[tokio::test]
async fn get_user_compares_the_name_without_regard_to_ascii_case() {
    let server = server().await;
    let ids = add_folders(&server).await;
    // The answer holds the stored name, not the spelling of the request.
    for username in ["OWNER", "Owner", "oWnEr"] {
        let response = server.get_json("getUser", &[("username", username)]).await;
        assert_eq!(response["status"], "ok", "{username}");
        assert_eq!(response["user"], owner_user(&ids), "{username}");
    }

    // A client sends the name that its user typed in `u` and in `username`.
    let response = server
        .get_json_noauth(
            "getUser",
            &[
                ("u", "OWNER"),
                ("p", OWNER_PASSWORD),
                V,
                C,
                ("username", "OWNER"),
            ],
        )
        .await;
    assert_eq!(response["user"], owner_user(&ids));
}

#[tokio::test]
async fn get_user_returns_the_owner_for_every_name_when_auth_is_disabled() {
    let server = server_without_auth().await;
    let ids = add_folders(&server).await;
    for username in ["other", "owner", "OWNER", "admin", ""] {
        // The request has `v` and `c` and names no user in `u`.
        let response = server
            .get_json_noauth("getUser", &[V, C, ("username", username)])
            .await;
        assert_eq!(response["status"], "ok", "{username:?}");
        assert_eq!(response["user"], owner_user(&ids), "{username:?}");
    }

    // A value of `u` changes nothing: no credential is read.
    let response = server
        .get_json_noauth("getUser", &[V, C, ("u", "nobody"), ("username", "other")])
        .await;
    assert_eq!(response["user"], owner_user(&ids));
}

#[tokio::test]
async fn get_users_returns_the_owner_in_a_list() {
    for server in [server().await, server_without_auth().await] {
        let ids = add_folders(&server).await;
        for method in ["getUsers", "getUsers.view"] {
            let response = server.get_json(method, &[]).await;
            assert_eq!(response["status"], "ok", "{method}");
            assert_eq!(
                response["users"],
                json!({ "user": [owner_user(&ids)] }),
                "{method}"
            );
        }

        assert_eq!(
            server.get_xml("getUsers", &[]).await,
            xml_document(
                "ok",
                &format!(
                    "><users><user {USER_ATTRIBUTES}><folder>1</folder><folder>2</folder></user>\
                     </users></subsonic-response>"
                )
            )
        );
    }
}

#[tokio::test]
async fn user_methods_answer_a_post_with_a_form_body() {
    let server = server().await;
    let ids = add_folders(&server).await;

    let response = server
        .post_form("getUser.view", &[("username", "owner"), ("f", "json")])
        .await;
    assert_eq!(content_type(&response), "application/json");
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["user"], owner_user(&ids));

    let response = server
        .post_form("getUser", &[("username", "other"), ("f", "json")])
        .await;
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["error"], error(50, OTHER_USER));

    let response = server.post_form("getUsers", &[("f", "json")]).await;
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["users"], json!({ "user": [owner_user(&ids)] }));
}

#[tokio::test]
async fn user_methods_need_a_user() {
    let server = server().await;
    add_folders(&server).await;
    for method in ["getUser", "getUsers"] {
        let response = server
            .get_json_noauth(method, &[V, C, ("username", OWNER)])
            .await;
        assert_eq!(
            response["error"],
            error(10, "required parameter is missing: u"),
            "{method}"
        );
        let response = server
            .get_json_noauth(
                method,
                &[V, C, ("u", OWNER), ("p", "wrong"), ("username", OWNER)],
            )
            .await;
        assert_eq!(
            response["error"],
            error(40, "wrong username or password"),
            "{method}"
        );
        assert!(response.get("user").is_none(), "{method}: {response}");
        assert!(response.get("users").is_none(), "{method}: {response}");
    }
}
