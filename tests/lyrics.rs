//! Router tests of `getLyrics` and `getLyricsBySongId` (`docs/design.md`
//! section 4). The server has no lyrics, and both methods answer with a
//! result that holds none.
//!
//! The tests compare the whole response body with the document that the
//! design gives, in JSON and in XML.

mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use wn_server::config::Config;

use common::{
    API_VERSION, CLIENT, OWNER, OWNER_PASSWORD, StandardServer, TestServer, body_string,
    config_without_auth, content_type, json_document, remove_file, xml_document,
};

/// Parameters of a request: names and values.
type Pairs<'a> = &'a [(&'a str, &'a str)];

/// The two path forms of `getLyrics`.
const GET_LYRICS: [&str; 2] = ["getLyrics", "getLyrics.view"];

/// The two path forms of `getLyricsBySongId`.
const GET_LYRICS_BY_SONG_ID: [&str; 2] = ["getLyricsBySongId", "getLyricsBySongId.view"];

/// The ID of a track that no library has.
const NO_TRACK: &str = "tr-00000000000000000000000000000000";

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of error 10 for a request without `id`.
const MISSING_ID: &str = "required parameter is missing: id";

/// The status that the table in `docs/design.md` section 4 gives the two
/// methods.
const DESIGN_STATUS: &str = "empty result: no lyrics in version 1";

/// The JSON body of `getLyrics`: a `lyrics` element whose text is empty.
fn lyrics_json() -> String {
    json_document("ok", r#","lyrics":{"value":""}"#)
}

/// The XML body of `getLyrics`: a `lyrics` element without an attribute and
/// without text.
fn lyrics_xml() -> String {
    xml_document("ok", "><lyrics></lyrics></subsonic-response>")
}

/// The JSON body of `getLyricsBySongId` for a song: a `lyricsList` without a
/// field.
fn lyrics_list_json() -> String {
    json_document("ok", r#","lyricsList":{}"#)
}

/// The XML body of `getLyricsBySongId` for a song: an empty `lyricsList`
/// element.
fn lyrics_list_xml() -> String {
    xml_document("ok", "><lyricsList/></subsonic-response>")
}

/// The JSON body of the error `code` with `message`.
fn error_json(code: u32, message: &str) -> String {
    json_document(
        "failed",
        &format!(r#","error":{{"code":{code},"message":"{message}"}}"#),
    )
}

/// The XML body of the error `code` with `message`.
fn error_xml(code: u32, message: &str) -> String {
    xml_document(
        "failed",
        &format!(r#"><error code="{code}" message="{message}"/></subsonic-response>"#),
    )
}

/// Sends `GET /rest/<name>` with `params`, the auth defaults and `f=json`.
/// Checks the status and the content type and returns the body.
async fn json_body(server: &TestServer, name: &str, params: Pairs<'_>) -> String {
    let mut params = params.to_vec();
    params.push(("f", "json"));
    let response = server.get_raw(name, &params).await;
    assert_eq!(response.status(), StatusCode::OK, "{name}");
    assert_eq!(content_type(&response), "application/json", "{name}");
    body_string(response).await
}

/// Sends `name` with `params` and the auth defaults by GET in JSON, by GET
/// in XML and by POST with a form body in JSON. Checks that the body is
/// `json` or `xml`, byte for byte.
async fn expect(server: &TestServer, name: &str, params: Pairs<'_>, json: &str, xml: &str) {
    let case = format!("{name} {params:?}");
    assert_eq!(json_body(server, name, params).await, json, "{case}");
    assert_eq!(server.get_xml(name, params).await, xml, "{case}");

    let mut form = params.to_vec();
    form.push(("f", "json"));
    let response = server.post_form(name, &form).await;
    assert_eq!(response.status(), StatusCode::OK, "{case}");
    assert_eq!(content_type(&response), "application/json", "{case}");
    assert_eq!(body_string(response).await, json, "{case}");
}

/// The IDs of the ten tracks of the standard library.
async fn track_ids(server: &StandardServer) -> Vec<String> {
    let catalogue = server.catalogue().await;
    let ids: Vec<String> = catalogue
        .songs
        .iter()
        .map(|song| song.track.id.clone())
        .collect();
    assert_eq!(ids.len(), 10);
    ids
}

/// The methods of `docs/design.md` section 4 whose status is `status`, in
/// the order of the table, and the status of the row `Extensions advertised`.
fn design_rows(status: &str) -> (Vec<&'static str>, &'static str) {
    const DESIGN: &str = include_str!("../docs/design.md");
    let (_, rest) = DESIGN
        .split_once("\n## 4. OpenSubsonic coverage\n")
        .expect("the design has section 4");
    let (section, _) = rest
        .split_once("\n## 5. ")
        .expect("section 5 follows section 4");
    let rows: Vec<(&str, &str)> = section
        .lines()
        .filter_map(|line| line.strip_prefix("| "))
        .filter_map(|row| row.strip_suffix(" |"))
        .filter_map(|row| row.split_once(" | "))
        .collect();
    let methods = rows
        .iter()
        .filter(|(_, found)| *found == status)
        .flat_map(|(names, _)| names.split(", "))
        .collect();
    let extensions = rows
        .iter()
        .find(|(names, _)| *names == "Extensions advertised")
        .map(|(_, extensions)| *extensions)
        .expect("the table has the row of the extensions");
    (methods, extensions)
}

#[tokio::test]
async fn get_lyrics_returns_an_empty_lyrics_element_for_every_artist_and_title() {
    let server = TestServer::standard().await;
    let open_road = server.track.open_road.as_str();
    // The artist and the title of a song of the library, each of them alone,
    // names that the library does not have, empty values, and parameters
    // that the method does not have.
    let requests: [Pairs; 8] = [
        &[],
        &[("artist", "Tessa Lund"), ("title", "Open Road")],
        &[("artist", "Aurora Field")],
        &[("title", "Low Tide")],
        &[("artist", "Nobody"), ("title", "Nothing")],
        &[("artist", ""), ("title", "")],
        &[("artist", "Édith Piaf"), ("title", "<b>\"&'")],
        &[("id", open_road), ("enhanced", "true")],
    ];
    for name in GET_LYRICS {
        for params in requests {
            expect(&server, name, params, &lyrics_json(), &lyrics_xml()).await;
        }
    }

    // The element holds the text of the lyrics, which is empty, and no
    // artist and no title.
    let response = server.get_json("getLyrics", requests[1]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["lyrics"], json!({ "value": "" }));
    // The five envelope fields and `lyrics`.
    assert_eq!(response.as_object().map(serde_json::Map::len), Some(6));
}

#[tokio::test]
async fn get_lyrics_answers_a_server_without_a_library() {
    let server = TestServer::bare(Config::default()).await;
    for name in GET_LYRICS {
        let params = [("artist", "Tessa Lund"), ("title", "Open Road")];
        expect(&server, name, &params, &lyrics_json(), &lyrics_xml()).await;
    }
}

#[tokio::test]
async fn get_lyrics_by_song_id_returns_an_empty_lyrics_list_for_every_song() {
    let server = TestServer::standard().await;
    let (json, xml) = (lyrics_list_json(), lyrics_list_xml());
    for id in track_ids(&server).await {
        let id = id.as_str();
        for name in GET_LYRICS_BY_SONG_ID {
            expect(&server, name, &[("id", id)], &json, &xml).await;
            // `enhanced` is the parameter of version 2 of the `songLyrics`
            // extension. It changes nothing.
            for enhanced in ["true", "false", "yes"] {
                let params = [("id", id), ("enhanced", enhanced)];
                expect(&server, name, &params, &json, &xml).await;
            }
        }
    }

    let id = server.track.open_road.as_str();
    let response = server.get_json("getLyricsBySongId", &[("id", id)]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["lyricsList"], json!({}));
    // The five envelope fields and `lyricsList`.
    assert_eq!(response.as_object().map(serde_json::Map::len), Some(6));
}

#[tokio::test]
async fn get_lyrics_by_song_id_answers_a_track_without_a_file() {
    let server = TestServer::standard().await;
    let id = server.track.open_road.as_str();
    remove_file(&server, id).await;
    // The track has a row, so its ID is the ID of a song: `getSong` returns
    // it without `path`.
    let song = server.get_json("getSong", &[("id", id)]).await;
    assert_eq!(song["song"]["id"], id);
    assert_eq!(song["song"].get("path"), None);

    for name in GET_LYRICS_BY_SONG_ID {
        let (json, xml) = (lyrics_list_json(), lyrics_list_xml());
        expect(&server, name, &[("id", id)], &json, &xml).await;
    }
}

#[tokio::test]
async fn get_lyrics_by_song_id_without_an_id_is_error_10() {
    let server = TestServer::standard().await;
    let open_road = server.track.open_road.as_str();
    // No parameter, and the ID of a song under names that are not `id`.
    let requests: [Pairs; 4] = [
        &[],
        &[("ID", open_road)],
        &[("songId", open_road)],
        &[("enhanced", "true")],
    ];
    let (json, xml) = (error_json(10, MISSING_ID), error_xml(10, MISSING_ID));
    for name in GET_LYRICS_BY_SONG_ID {
        for params in requests {
            expect(&server, name, params, &json, &xml).await;
        }
    }
}

#[tokio::test]
async fn get_lyrics_by_song_id_with_the_id_of_no_song_is_error_70() {
    let server = TestServer::standard().await;
    let open_road = server.track.open_road.clone();
    let upper = open_road.to_uppercase();
    let long = format!("{open_road}0");
    let padded = format!(" {open_road}");
    let values = [
        // A track ID that no row has.
        NO_TRACK,
        // The IDs of an album and of an artist of the library, and an ID of
        // the kind of a playlist.
        server.album.summer_sampler.as_str(),
        server.artist.aurora_field.as_str(),
        "pl-00000000000000000000000000000000",
        // Values without the shape of an ID.
        "",
        "1",
        "tr-",
        "Open Road",
        "' OR 1=1 --",
        upper.as_str(),
        long.as_str(),
        padded.as_str(),
    ];
    let (json, xml) = (error_json(70, NOT_FOUND), error_xml(70, NOT_FOUND));
    for name in GET_LYRICS_BY_SONG_ID {
        for value in values {
            expect(&server, name, &[("id", value)], &json, &xml).await;
        }
        // The first value of `id` is the one that counts.
        let params = [("id", NO_TRACK), ("id", open_road.as_str())];
        expect(&server, name, &params, &json, &xml).await;
    }

    // A server without a library has no song.
    let server = TestServer::bare(Config::default()).await;
    for name in GET_LYRICS_BY_SONG_ID {
        let params = [("id", open_road.as_str())];
        expect(&server, name, &params, &json, &xml).await;
    }
}

#[tokio::test]
async fn lyrics_methods_answer_the_authentication_error_to_a_request_without_a_user() {
    let server = TestServer::standard().await;
    let open_road = server.track.open_road.as_str();
    let (v, c) = (("v", API_VERSION), ("c", CLIENT));
    let rejected: [(Pairs, u32, &str); 4] = [
        (&[], 10, "required parameter is missing: v"),
        (&[v, c], 10, "required parameter is missing: u"),
        (
            &[v, c, ("u", OWNER), ("p", "wrong")],
            40,
            "wrong username or password",
        ),
        (
            &[v, c, ("u", "nobody"), ("p", OWNER_PASSWORD)],
            40,
            "wrong username or password",
        ),
    ];
    // The ID of a song, the ID of no song and no ID: the error is the one
    // of the credentials in each case, so the check of the user comes
    // before the handler.
    let arguments: [Pairs; 4] = [
        &[("id", open_road)],
        &[("id", NO_TRACK)],
        &[],
        &[("artist", "Tessa Lund"), ("title", "Open Road")],
    ];
    for name in GET_LYRICS.into_iter().chain(GET_LYRICS_BY_SONG_ID) {
        for (credentials, code, message) in rejected {
            for argument in arguments {
                let mut params = argument.to_vec();
                params.extend_from_slice(credentials);
                let case = format!("{name} {params:?}");

                let response = server.get_raw_noauth(name, &params).await;
                assert_eq!(response.status(), StatusCode::OK, "{case}");
                assert_eq!(content_type(&response), "application/xml", "{case}");
                assert_eq!(
                    body_string(response).await,
                    error_xml(code, message),
                    "{case}"
                );

                params.push(("f", "json"));
                let response = server.get_raw_noauth(name, &params).await;
                assert_eq!(response.status(), StatusCode::OK, "{case}");
                assert_eq!(content_type(&response), "application/json", "{case}");
                assert_eq!(
                    body_string(response).await,
                    error_json(code, message),
                    "{case}"
                );
            }
        }
    }

    // The password of the owner is a user, as the token of the auth
    // defaults is.
    let credentials = [v, c, ("u", OWNER), ("p", OWNER_PASSWORD), ("f", "json")];
    let response = server.get_raw_noauth("getLyrics", &credentials).await;
    assert_eq!(body_string(response).await, lyrics_json());
    let mut params = vec![("id", open_road)];
    params.extend_from_slice(&credentials);
    let response = server.get_raw_noauth("getLyricsBySongId", &params).await;
    assert_eq!(body_string(response).await, lyrics_list_json());
}

#[tokio::test]
async fn lyrics_methods_answer_without_credentials_when_auth_is_disabled() {
    let server = TestServer::bare(config_without_auth()).await;
    let (v, c) = (("v", API_VERSION), ("c", CLIENT));
    for name in GET_LYRICS {
        let response = server.get_raw_noauth(name, &[v, c, ("f", "json")]).await;
        assert_eq!(body_string(response).await, lyrics_json(), "{name}");
        let response = server.get_raw_noauth(name, &[v, c]).await;
        assert_eq!(body_string(response).await, lyrics_xml(), "{name}");
    }
    // The server has no library, so the ID is the ID of no song.
    for name in GET_LYRICS_BY_SONG_ID {
        let params = [("id", NO_TRACK), v, c, ("f", "json")];
        let response = server.get_raw_noauth(name, &params).await;
        assert_eq!(
            body_string(response).await,
            error_json(70, NOT_FOUND),
            "{name}"
        );
        let response = server.get_raw_noauth(name, &[v, c, ("f", "json")]).await;
        assert_eq!(
            body_string(response).await,
            error_json(10, MISSING_ID),
            "{name}"
        );
    }
}

#[tokio::test]
async fn get_lyrics_by_song_id_reports_a_database_error_as_error_0() {
    let server = TestServer::bare(Config::default()).await;
    server
        .db()
        .write(|conn| Ok(conn.execute_batch("DROP TABLE tracks")?))
        .await
        .expect("drop the tracks table");

    // The message holds nothing of the statement that failed.
    let message = "the server cannot read its library";
    let (json, xml) = (error_json(0, message), error_xml(0, message));
    for name in GET_LYRICS_BY_SONG_ID {
        expect(&server, name, &[("id", NO_TRACK)], &json, &xml).await;
        // A value without the shape of a track ID does not reach the
        // database.
        let (json, xml) = (error_json(70, NOT_FOUND), error_xml(70, NOT_FOUND));
        expect(&server, name, &[("id", "1")], &json, &xml).await;
    }
    // `getLyrics` does not read the database.
    for name in GET_LYRICS {
        expect(&server, name, &[], &lyrics_json(), &lyrics_xml()).await;
    }
}

#[tokio::test]
async fn get_open_subsonic_extensions_does_not_list_song_lyrics() {
    let server = TestServer::bare(Config::default()).await;
    for name in [
        "getOpenSubsonicExtensions",
        "getOpenSubsonicExtensions.view",
    ] {
        let response = server.get_json(name, &[]).await;
        assert_eq!(response["status"], "ok", "{name}");
        let names: Vec<&str> = response["openSubsonicExtensions"]
            .as_array()
            .expect("an array of extensions")
            .iter()
            .map(|extension| extension["name"].as_str().expect("a name"))
            .collect();
        // The list is not empty, so the next check reads the names of the
        // extensions that the server has.
        assert!(names.contains(&"formPost"), "{name}: {names:?}");
        assert!(!names.contains(&"songLyrics"), "{name}: {names:?}");

        let xml = server.get_xml(name, &[]).await;
        assert!(xml.contains(r#"name="formPost""#), "{name}: {xml}");
        assert!(!xml.to_lowercase().contains("lyrics"), "{name}: {xml}");
    }

    // The method is public, and the answer without a user is the same.
    let response: Value = server
        .get_json_noauth("getOpenSubsonicExtensions", &[])
        .await;
    let listed = response["openSubsonicExtensions"].to_string();
    assert!(listed.contains("formPost"), "{listed}");
    assert!(!listed.to_lowercase().contains("lyrics"), "{listed}");

    assert!(
        wn_server::api::EXTENSIONS
            .iter()
            .all(|(extension, _)| *extension != "songLyrics")
    );
}

#[test]
fn design_section_4_gives_both_methods_the_empty_result_and_no_extension() {
    let (methods, extensions) = design_rows(DESIGN_STATUS);
    assert_eq!(methods, ["getLyrics", "getLyricsBySongId"]);
    assert!(extensions.contains("`formPost`"), "{extensions}");
    assert!(
        !extensions.to_lowercase().contains("lyrics"),
        "{extensions}"
    );
}
