//! Router tests of the folder methods `getIndexes` and `getMusicDirectory`
//! (`docs/design.md` section 4).
//!
//! The tests of the standard library compare the whole response with the
//! document that the library gives, in JSON and in XML. The names, the
//! indexes and the order of the albums and of the songs are written here.
//! The IDs, the creation times and the end time of the scan differ from run
//! to run, so the tests read them from the rows.
//!
//! A song of a directory is compared with the answer of `getSong` for its
//! ID, and the songs of an album directory with the songs of `getAlbum`.
//! `tests/browse.rs` compares those with the files of the library.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::model;
use wn_server::config::Config;
use wn_server::db::queries::browse::Album;
use wn_server::db::queries::scans::{self, Scan, ScanCounts};
use wn_server::db::queries::tracks::{self, TrackContent};
use wn_server::db::queries::{albums, artists, folders, users};
use wn_server::ids::{self, Kind};
use wn_server::library::tags::TrackMeta;

use common::{
    LibraryBuilder, PICTURE_PNG, StandardServer, TestLibrary, TestServer, body_bytes, body_string,
    content_type, subsonic_response,
};

/// The value of `ignoredArticles`.
const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// An artist ID that no row has.
const UNKNOWN_ARTIST: &str = "ar-ffffffffffffffffffffffffffffffff";

/// An album ID that no row has.
const UNKNOWN_ALBUM: &str = "al-ffffffffffffffffffffffffffffffff";

/// The `subsonic-response` object of a success that carries `value` under
/// `key`.
fn ok(key: &str, value: Value) -> Value {
    let mut response = json!({
        "status": "ok",
        "version": "1.16.1",
        "type": "wn-server",
        "serverVersion": VERSION,
        "openSubsonic": true,
    });
    response[key] = value;
    response
}

/// The `subsonic-response` object of the error `code` with `message`.
fn failed(code: u32, message: &str) -> Value {
    json!({
        "status": "failed",
        "version": "1.16.1",
        "type": "wn-server",
        "serverVersion": VERSION,
        "openSubsonic": true,
        "error": { "code": code, "message": message },
    })
}

/// The XML body that has `status` and then `content` in the root element.
fn xml_document(status: &str, content: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="{}""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true">"#,
            "{}</subsonic-response>"
        ),
        status, VERSION, content
    )
}

/// The XML body of a success whose root element holds `content`.
fn xml_ok(content: &str) -> String {
    xml_document("ok", content)
}

/// The XML body of the error `code` with `message`.
fn xml_failed(code: u32, message: &str) -> String {
    xml_document(
        "failed",
        &format!(r#"<error code="{code}" message="{message}"/>"#),
    )
}

/// The attributes that the XML body holds for the JSON object `element`:
/// each field with a scalar value, in the byte order of the names. The
/// values of the tests need no escape.
fn xml_attributes(element: &Value) -> String {
    let fields = element.as_object().expect("an element is an object");
    fields
        .iter()
        .filter(|(_, value)| !value.is_array() && !value.is_object())
        .map(|(name, value)| match value {
            Value::String(text) => format!(r#" {name}="{text}""#),
            scalar => format!(r#" {name}="{scalar}""#),
        })
        .collect()
}

/// The `indexes` element of the answer to `getIndexes` with `params`.
async fn indexes(server: &TestServer, params: &[(&str, &str)]) -> Value {
    let response = server.get_json("getIndexes", params).await;
    assert_eq!(response["status"], "ok", "{params:?}: {response}");
    response["indexes"].clone()
}

/// The `directory` element of the answer to `getMusicDirectory` for `id`.
async fn directory(server: &TestServer, id: &str) -> Value {
    let response = server.get_json("getMusicDirectory", &[("id", id)]).await;
    assert_eq!(response["status"], "ok", "{id}: {response}");
    response["directory"].clone()
}

/// The `lastModified` of `getIndexes`.
async fn last_modified(server: &TestServer) -> i64 {
    let indexes = indexes(server, &[]).await;
    indexes["lastModified"]
        .as_i64()
        .unwrap_or_else(|| panic!("lastModified is no integer: {indexes}"))
}

/// The rows of the `scans` table in the order of the scans.
async fn scan_rows(server: &TestServer) -> Vec<Scan> {
    server.db().read(scans::list).await.expect("read the scans")
}

/// The names of the indexes of an `indexes` or `artists` element, each with
/// the names of its artists.
fn index_names(element: &Value) -> Vec<(String, Vec<String>)> {
    let text = |value: &Value| value.as_str().expect("a string").to_owned();
    element["index"]
        .as_array()
        .unwrap_or_else(|| panic!("no index list: {element}"))
        .iter()
        .map(|index| {
            let names = index["artist"]
                .as_array()
                .expect("an artist list")
                .iter()
                .map(|artist| text(&artist["name"]))
                .collect();
            (text(&index["name"]), names)
        })
        .collect()
}

/// The values of `field` of the artists of an `indexes` or `artists`
/// element, in the order of the document.
fn artist_column(element: &Value, field: &str) -> Vec<String> {
    element["index"]
        .as_array()
        .unwrap_or_else(|| panic!("no index list: {element}"))
        .iter()
        .flat_map(|index| index["artist"].as_array().expect("an artist list"))
        .map(|artist| artist[field].as_str().expect("a string").to_owned())
        .collect()
}

/// The values of `field` of the items of the list `list`.
fn column<'a>(list: &'a Value, field: &str) -> Vec<&'a Value> {
    list.as_array()
        .unwrap_or_else(|| panic!("no list: {list}"))
        .iter()
        .map(|item| &item[field])
        .collect()
}

/// The texts of `field` of the items of the list `list`.
fn texts(list: &Value, field: &str) -> Vec<String> {
    column(list, field)
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("{field} is no string: {list}"))
                .to_owned()
        })
        .collect()
}

/// The `Child` element that lists the album `album` as a directory, for a
/// user who attached nothing to it.
fn album_dir_json(album: &Album) -> Value {
    let mut child = json!({
        "id": album.id,
        "parent": album.artist_id,
        "isDir": true,
        "title": album.name,
        "album": album.name,
        "artist": album.artist_name,
        "duration": model::seconds(album.duration_ms),
        "created": model::iso8601(album.created_at),
        "artistId": album.artist_id,
        "mediaType": "album",
        "playCount": 0,
    });
    if let Some(year) = album.year {
        child["year"] = json!(year);
    }
    if let Some(genre) = &album.genre {
        child["genre"] = json!(genre);
    }
    if album.cover_source.is_some() {
        child["coverArt"] = json!(album.id);
    }
    child
}

/// The ID of the owner account of `server`.
async fn owner_id(server: &TestServer) -> i64 {
    server
        .db()
        .read(users::get_owner)
        .await
        .expect("read the owner")
        .expect("the server has an owner")
        .user
        .id
}

/// Gives the track `id` its placeholder path, as the scanner does for a
/// track that it keeps without a file.
async fn remove_file(server: &TestServer, id: &str) {
    let id = id.to_owned();
    server
        .db()
        .write(move |conn| {
            let track = tracks::get_track(conn, &id)?.expect("the track exists");
            let placeholder = tracks::placeholder_path(&id);
            tracks::relink_track(
                conn,
                &id,
                track.folder_id,
                &placeholder,
                track.mtime,
                track.size,
            )
        })
        .await
        .expect("give the track a placeholder path");
}

/// The time of the clock in Unix seconds.
fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    i64::try_from(elapsed.as_secs()).expect("the time fits")
}

/// The three artists of the standard library with the names of their
/// indexes, in the order of `getIndexes`.
fn standard_artists(server: &StandardServer) -> [(&'static str, &'static str, &str); 3] {
    [
        ("A", "Aurora Field", server.artist.aurora_field.as_str()),
        ("M", "Marlow Quartet", server.artist.marlow_quartet.as_str()),
        (
            "V",
            "Various Artists",
            server.artist.various_artists.as_str(),
        ),
    ]
}

#[tokio::test]
async fn get_indexes_returns_the_artists_of_the_library_in_their_indexes() {
    let server = TestServer::standard().await;
    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let last_modified = rows[0].finished_at.expect("the scan has ended") * 1000;

    let index: Vec<Value> = standard_artists(&server)
        .iter()
        .map(|(index, name, id)| json!({ "name": index, "artist": [{ "id": id, "name": name }] }))
        .collect();
    let expected = json!({
        "ignoredArticles": IGNORED_ARTICLES,
        "lastModified": last_modified,
        "index": index,
    });
    for method in ["getIndexes", "getIndexes.view"] {
        let response = server.get_json(method, &[]).await;
        assert_eq!(response, ok("indexes", expected.clone()), "{method}");
        // The time is a JSON number.
        assert!(
            response["indexes"]["lastModified"].is_i64(),
            "{method}: {response}"
        );
    }

    let index: String = standard_artists(&server)
        .iter()
        .map(|(index, name, id)| {
            format!(r#"<index name="{index}"><artist id="{id}" name="{name}"/></index>"#)
        })
        .collect();
    assert_eq!(
        server.get_xml("getIndexes", &[]).await,
        xml_ok(&format!(
            r#"<indexes ignoredArticles="{IGNORED_ARTICLES}" lastModified="{last_modified}">{index}</indexes>"#
        ))
    );
}

#[tokio::test]
async fn get_indexes_groups_the_artists_as_get_artists_does() {
    let library = LibraryBuilder::new()
        .track("The Beatles", "First", "One", 1, "mp3")
        .track("beck", "Second", "Two", 1, "mp3")
        .track("Them", "Third", "Three", 1, "mp3")
        .track("Édith Piaf", "Fourth", "Four", 1, "mp3")
        .track("2Pac", "Fifth", "Five", 1, "mp3")
        .track("Los Lobos", "Sixth", "Six", 1, "mp3")
        .track("The", "Seventh", "Seven", 1, "mp3")
        .track("!!!", "Eighth", "Eight", 1, "mp3")
        .build();
    let server = TestServer::scanned(library).await;

    let indexes = indexes(&server, &[]).await;
    assert_eq!(indexes["ignoredArticles"], IGNORED_ARTICLES);
    let names = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    let expected: [(String, Vec<String>); 5] = [
        // The names that start with a digit or a sign.
        ("#".to_owned(), names(&["!!!", "2Pac"])),
        // "The Beatles" is ordered as "beatles", before "beck".
        ("B".to_owned(), names(&["The Beatles", "beck"])),
        // A letter with a diacritic counts as its base letter.
        ("E".to_owned(), names(&["Édith Piaf"])),
        ("L".to_owned(), names(&["Los Lobos"])),
        // An article is ignored only when another word follows it.
        ("T".to_owned(), names(&["The", "Them"])),
    ];
    assert_eq!(index_names(&indexes), expected);

    // `getArtists` has the same articles, indexes, artists and order.
    let response = server.get_json("getArtists", &[]).await;
    let artists = &response["artists"];
    assert_eq!(artists["ignoredArticles"], indexes["ignoredArticles"]);
    assert_eq!(index_names(artists), index_names(&indexes));
    assert_eq!(artist_column(artists, "id"), artist_column(&indexes, "id"));

    let xml = server.get_xml("getIndexes", &[]).await;
    assert!(
        xml.contains(r##"<index name="#"><artist id="ar-"##),
        "{xml}"
    );
    assert!(xml.contains(r#"<index name="E"><artist id="ar-"#), "{xml}");
    assert!(xml.contains(r#" name="Édith Piaf"/>"#), "{xml}");
}

#[tokio::test]
async fn get_indexes_of_a_server_without_a_library_has_no_index_and_the_time_0() {
    let server = TestServer::bare(Config::default()).await;
    let expected = json!({
        "ignoredArticles": IGNORED_ARTICLES,
        "lastModified": 0,
        "index": [],
    });
    assert_eq!(
        server.get_json("getIndexes", &[]).await,
        ok("indexes", expected.clone())
    );
    assert_eq!(
        server.get_xml("getIndexes", &[]).await,
        xml_ok(&format!(
            r#"<indexes ignoredArticles="{IGNORED_ARTICLES}" lastModified="0"/>"#
        ))
    );

    // No scan has ended, so no time is at or after the last change: the
    // answer holds the list, which is empty.
    for since in ["0", "1", "9223372036854775807"] {
        assert_eq!(
            server
                .get_json("getIndexes", &[("ifModifiedSince", since)])
                .await,
            ok("indexes", expected.clone()),
            "{since}"
        );
    }
}

/// A server with two music folders, scanned, and the libraries that are the
/// folders. The first folder holds "Aurora Field" and one album of "Shared
/// Artist". The second holds "Marlow Quartet" and another album of "Shared
/// Artist".
async fn two_folder_server() -> (TestServer, TestLibrary, TestLibrary) {
    let first = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .track("Shared Artist", "First", "One", 1, "mp3")
        .build();
    let second = LibraryBuilder::new()
        .track("Marlow Quartet", "Slow Rivers", "Upstream", 1, "m4a")
        .track("Shared Artist", "Second", "Two", 1, "ogg")
        .build();
    let mut config = Config::default();
    config.library.folders = vec![
        first.root.path().to_path_buf(),
        second.root.path().to_path_buf(),
    ];
    config.library.scan_poll_ms = 10;
    let server = TestServer::bare(config).await;
    server.scan_and_wait().await;
    (server, first, second)
}

#[tokio::test]
async fn get_indexes_with_music_folder_id_returns_the_artists_of_that_folder() {
    let (server, _first, _second) = two_folder_server().await;
    let last_modified = last_modified(&server).await;

    // The folders have the IDs 1 and 2, in the order of the config.
    let folders = server.get_json("getMusicFolders", &[]).await;
    assert_eq!(
        column(&folders["musicFolders"]["musicFolder"], "id"),
        [1, 2]
    );

    let all = indexes(&server, &[]).await;
    assert_eq!(
        artist_column(&all, "name"),
        ["Aurora Field", "Marlow Quartet", "Shared Artist"]
    );
    let in_first = indexes(&server, &[("musicFolderId", "1")]).await;
    assert_eq!(
        artist_column(&in_first, "name"),
        ["Aurora Field", "Shared Artist"]
    );
    let in_second = indexes(&server, &[("musicFolderId", "2")]).await;
    assert_eq!(
        artist_column(&in_second, "name"),
        ["Marlow Quartet", "Shared Artist"]
    );
    // An artist of a folder is the element that the whole list holds, and
    // the time is the one of the library.
    assert_eq!(in_first["index"][0], all["index"][0]);
    for document in [&all, &in_first, &in_second] {
        assert_eq!(document["lastModified"], last_modified, "{document}");
        assert_eq!(document["ignoredArticles"], IGNORED_ARTICLES);
    }

    // The artists of a folder are those of `getArtists` for it.
    for folder in ["1", "2"] {
        let response = server
            .get_json("getArtists", &[("musicFolderId", folder)])
            .await;
        let indexes = indexes(&server, &[("musicFolderId", folder)]).await;
        assert_eq!(
            artist_column(&response["artists"], "id"),
            artist_column(&indexes, "id"),
            "{folder}"
        );
    }

    // The directory of an artist holds its albums of every folder.
    let shared = all["index"][2]["artist"][0]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();
    let albums = directory(&server, &shared).await;
    assert_eq!(texts(&albums["child"], "title"), ["First", "Second"]);

    // An empty value counts as an absent parameter.
    assert_eq!(indexes(&server, &[("musicFolderId", "")]).await, all);

    // An ID that no folder has: no artist is in that folder.
    for id in ["3", "0", "-1", "9223372036854775807"] {
        assert_eq!(
            server
                .get_json("getIndexes", &[("musicFolderId", id)])
                .await,
            ok(
                "indexes",
                json!({
                    "ignoredArticles": IGNORED_ARTICLES,
                    "lastModified": last_modified,
                    "index": [],
                })
            ),
            "{id}"
        );
    }
    assert_eq!(
        server
            .get_xml("getIndexes", &[("musicFolderId", "3")])
            .await,
        xml_ok(&format!(
            r#"<indexes ignoredArticles="{IGNORED_ARTICLES}" lastModified="{last_modified}"/>"#
        ))
    );
}

#[tokio::test]
async fn get_indexes_with_a_parameter_that_is_no_integer_is_error_0() {
    let server = TestServer::standard().await;
    let values = [
        "abc",
        "1.5",
        "1 ",
        "0x1",
        "9223372036854775808",
        "2026-10-11T08:15:30Z",
    ];
    for parameter in ["musicFolderId", "ifModifiedSince"] {
        let message = format!("parameter {parameter} must be an integer");
        for value in values {
            for method in ["getIndexes", "getIndexes.view"] {
                assert_eq!(
                    server.get_json(method, &[(parameter, value)]).await,
                    failed(0, &message),
                    "{method} {parameter} {value:?}"
                );
            }
        }
        assert_eq!(
            server.get_xml("getIndexes", &[(parameter, "abc")]).await,
            xml_failed(0, &message),
            "{parameter}"
        );
    }
}

#[tokio::test]
async fn last_modified_is_the_end_time_of_the_scan_in_milliseconds() {
    let before = unix_now();
    let server = TestServer::standard().await;
    let after = unix_now();

    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let finished_at = rows[0].finished_at.expect("the scan has ended");
    assert!((before..=after).contains(&finished_at), "{finished_at}");

    let value = last_modified(&server).await;
    assert_eq!(value, finished_at * 1000);
    // The value is a time in milliseconds: it has thirteen digits.
    assert_eq!(value.to_string().len(), 13, "{value}");

    // Requests do not change it.
    for _ in 0..3 {
        assert_eq!(last_modified(&server).await, value);
    }
    let xml = server.get_xml("getIndexes", &[]).await;
    assert!(
        xml.contains(&format!(r#" lastModified="{value}">"#)),
        "{xml}"
    );
}

#[tokio::test]
async fn if_modified_since_at_or_after_last_modified_returns_no_index() {
    let server = TestServer::standard().await;
    let all = indexes(&server, &[]).await;
    let last = all["lastModified"].as_i64().expect("a time");
    assert_eq!(index_names(&all).len(), 3);

    // A time before the last change: the answer holds the artists.
    let earlier = (last - 1).to_string();
    for since in ["", "0", "-1", "1", "999", earlier.as_str()] {
        assert_eq!(
            indexes(&server, &[("ifModifiedSince", since)]).await,
            all,
            "{since:?}"
        );
    }

    // The time of the last change and every later time: the answer has the
    // articles and the time, and no `index`.
    let unchanged = json!({ "ignoredArticles": IGNORED_ARTICLES, "lastModified": last });
    let at = last.to_string();
    let later = (last + 1).to_string();
    let folder_id = server.folder_id.to_string();
    for since in [at.as_str(), later.as_str(), "9223372036854775807"] {
        for method in ["getIndexes", "getIndexes.view"] {
            let response = server.get_json(method, &[("ifModifiedSince", since)]).await;
            assert_eq!(
                response,
                ok("indexes", unchanged.clone()),
                "{method} {since}"
            );
            assert!(
                response["indexes"].get("index").is_none(),
                "{method} {since}: {response}"
            );
        }
        assert_eq!(
            server
                .get_xml("getIndexes", &[("ifModifiedSince", since)])
                .await,
            xml_ok(&format!(
                r#"<indexes ignoredArticles="{IGNORED_ARTICLES}" lastModified="{last}"/>"#
            )),
            "{since}"
        );
        // The time is the one of the library, with and without a folder.
        let params = [
            ("ifModifiedSince", since),
            ("musicFolderId", folder_id.as_str()),
        ];
        assert_eq!(indexes(&server, &params).await, unchanged, "{since}");
    }

    // The first value of the parameter is the one that counts.
    let params = [
        ("ifModifiedSince", "0"),
        ("ifModifiedSince", later.as_str()),
    ];
    assert_eq!(indexes(&server, &params).await, all);
}

/// The four counts of the changes of a scan.
fn changes(summary: &wn_server::library::scanner::ScanSummary) -> (u64, u64, u64, u64) {
    (
        summary.added,
        summary.changed,
        summary.moved,
        summary.removed,
    )
}

#[tokio::test]
async fn last_modified_changes_with_a_scan_that_changes_the_library_and_with_no_other() {
    let server = TestServer::standard_with_own_files().await;
    let first = last_modified(&server).await;
    let unchanged =
        |last: i64| json!({ "ignoredArticles": IGNORED_ARTICLES, "lastModified": last });
    let since = |time: i64| time.to_string();

    // A scan that finds nothing changed keeps the time. A client that sends
    // the time back gets no index.
    for _ in 0..2 {
        let summary = server.rescan().await;
        assert_eq!(changes(&summary), (0, 0, 0, 0));
        assert_eq!(summary.errors, [] as [&str; 0]);
        assert_eq!(last_modified(&server).await, first);
        assert_eq!(
            indexes(&server, &[("ifModifiedSince", &since(first))]).await,
            unchanged(first)
        );
    }
    assert_eq!(scan_rows(&server).await.len(), 3);

    // A file of a new artist. The scans run in the same second in most runs
    // of the test, and the time is greater all the same.
    let extra = LibraryBuilder::new()
        .track("Zeta Line", "Extra", "One", 1, "mp3")
        .build();
    let target = server.path("Zeta Line/Extra/01 One.mp3");
    fs::create_dir_all(target.parent().expect("a directory")).expect("create the directory");
    fs::copy(&extra.files[0], &target).expect("copy the file");
    let summary = server.rescan().await;
    assert_eq!(changes(&summary), (1, 0, 0, 0));
    let second = last_modified(&server).await;
    assert!(second > first, "{second} {first}");
    let rows = scan_rows(&server).await;
    let finished_at = rows.last().and_then(|scan| scan.finished_at);
    assert_eq!(Some(second / 1000), finished_at, "{rows:?}");

    // The client that holds the old time gets the artists with the new one.
    let answer = indexes(&server, &[("ifModifiedSince", &since(first))]).await;
    assert_eq!(answer["lastModified"], second);
    assert_eq!(
        artist_column(&answer, "name"),
        [
            "Aurora Field",
            "Marlow Quartet",
            "Various Artists",
            "Zeta Line"
        ]
    );
    assert_eq!(answer, indexes(&server, &[]).await);
    // The client that holds the new time gets none.
    assert_eq!(
        indexes(&server, &[("ifModifiedSince", &since(second))]).await,
        unchanged(second)
    );

    // The next scan without a change keeps the new time.
    assert_eq!(changes(&server.rescan().await), (0, 0, 0, 0));
    assert_eq!(last_modified(&server).await, second);

    // A cover image for an album that had none. The scan sets the cover and
    // stores no change of a track. The artists are the same, and so is the
    // time.
    let paper_maps = server.album.paper_maps.as_str();
    let aurora_field = server.artist.aurora_field.as_str();
    let album = directory(&server, aurora_field).await["child"][1].clone();
    assert_eq!(album["id"], paper_maps);
    assert!(album.get("coverArt").is_none(), "{album}");
    fs::copy(
        server.path("Aurora Field/Glass Harbour/cover.jpg"),
        server.path("Aurora Field/Paper Maps/cover.jpg"),
    )
    .expect("copy the cover");
    assert_eq!(changes(&server.rescan().await), (0, 0, 0, 0));
    let album = directory(&server, aurora_field).await["child"][1].clone();
    assert_eq!(album["coverArt"], paper_maps);
    assert_eq!(last_modified(&server).await, second);

    // A file that moved.
    fs::rename(
        server.path("Aurora Field/Glass Harbour/01 Low Tide.flac"),
        server.path("Aurora Field/Glass Harbour/Low Tide.flac"),
    )
    .expect("move the file");
    assert_eq!(changes(&server.rescan().await), (0, 0, 1, 0));
    let third = last_modified(&server).await;
    assert!(third > second, "{third} {second}");

    // The files of an artist are deleted.
    fs::remove_dir_all(server.path("Marlow Quartet")).expect("delete the directory");
    assert_eq!(changes(&server.rescan().await), (0, 0, 0, 2));
    let fourth = last_modified(&server).await;
    assert!(fourth > third, "{fourth} {third}");
    for old in [first, second, third] {
        let answer = indexes(&server, &[("ifModifiedSince", &since(old))]).await;
        assert_eq!(answer["lastModified"], fourth, "{old}");
        assert_eq!(
            artist_column(&answer, "name"),
            ["Aurora Field", "Various Artists", "Zeta Line"],
            "{old}"
        );
    }
    assert_eq!(
        indexes(&server, &[("ifModifiedSince", &since(fourth))]).await,
        unchanged(fourth)
    );
}

#[tokio::test]
async fn a_scan_without_an_end_time_and_a_scan_with_an_error_return_the_artists() {
    let server = TestServer::standard().await;
    let all = indexes(&server, &[]).await;
    let last = all["lastModified"].as_i64().expect("a time");
    let at_last = last.to_string();
    let since = [("ifModifiedSince", at_last.as_str())];
    let unchanged = json!({ "ignoredArticles": IGNORED_ARTICLES, "lastModified": last });
    assert_eq!(indexes(&server, &since).await, unchanged);

    // A scan runs. It can have stored changes, so the answer holds the
    // artists. The time is still the one of the scan that ended.
    let running = server
        .db()
        .write(|conn| scans::insert(conn))
        .await
        .expect("start a scan");
    assert_eq!(indexes(&server, &since).await, all);

    // The scan ends without a change and without an error: the time stays.
    let end = |id: i64, error: Option<&'static str>| {
        let db = server.db().clone();
        async move {
            db.write(move |conn| scans::finish(conn, id, &ScanCounts::default(), error))
                .await
                .expect("end the scan");
        }
    };
    end(running, None).await;
    assert_eq!(indexes(&server, &since).await, unchanged);

    // A cancelled scan keeps what it stored, so its end is a new time. It
    // is greater than the old one, also in the same second.
    let cancelled = server
        .db()
        .write(|conn| scans::insert(conn))
        .await
        .expect("start a scan");
    end(cancelled, Some("cancelled")).await;
    let after_cancel = last_modified(&server).await;
    assert!(after_cancel > last, "{after_cancel} {last}");
    let answer = indexes(&server, &since).await;
    assert_eq!(answer["index"], all["index"]);
    assert_eq!(answer["lastModified"], after_cancel);

    // The scan after it completes what the cancelled scan left, so it
    // counts without a change of a track.
    let next = server
        .db()
        .write(|conn| scans::insert(conn))
        .await
        .expect("start a scan");
    end(next, None).await;
    let after_next = last_modified(&server).await;
    assert!(after_next > after_cancel, "{after_next} {after_cancel}");
}

#[tokio::test]
async fn get_music_directory_of_an_artist_lists_its_albums_as_directories() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;

    // The albums of each artist in the order of `getArtist`: by year.
    let artists: [(&str, &str, &[&str]); 3] = [
        (
            "Aurora Field",
            &server.artist.aurora_field,
            &["Glass Harbour", "Paper Maps"],
        ),
        (
            "Marlow Quartet",
            &server.artist.marlow_quartet,
            &["Slow Rivers"],
        ),
        (
            "Various Artists",
            &server.artist.various_artists,
            &["Summer Sampler"],
        ),
    ];
    for (name, id, titles) in artists {
        let children: Vec<Value> = titles
            .iter()
            .map(|title| album_dir_json(catalogue.album(title)))
            .collect();
        // An artist has no parent, and the element has no play count.
        let expected = json!({ "id": id, "name": name, "child": children });
        for method in ["getMusicDirectory", "getMusicDirectory.view"] {
            assert_eq!(
                server.get_json(method, &[("id", id)]).await,
                ok("directory", expected.clone()),
                "{method} {name}"
            );
        }

        let elements: String = children
            .iter()
            .map(|child| format!("<child{}/>", xml_attributes(child)))
            .collect();
        assert_eq!(
            server.get_xml("getMusicDirectory", &[("id", id)]).await,
            xml_ok(&format!(
                r#"<directory id="{id}" name="{name}">{elements}</directory>"#
            )),
            "{name}"
        );

        // The albums and their order are those of `getArtist`.
        let response = server.get_json("getArtist", &[("id", id)]).await;
        assert_eq!(
            column(&response["artist"]["album"], "id"),
            column(&expected["child"], "id"),
            "{name}"
        );
    }

    // What the builder of the library writes for the two albums of "Aurora
    // Field". "Glass Harbour" has the file `cover.jpg`, and "Paper Maps" has
    // no cover.
    let aurora_field = server.artist.aurora_field.as_str();
    let albums = directory(&server, aurora_field).await["child"].clone();
    assert_eq!(
        albums,
        json!([
            {
                "id": server.album.glass_harbour,
                "parent": aurora_field,
                "isDir": true,
                "title": "Glass Harbour",
                "album": "Glass Harbour",
                "artist": "Aurora Field",
                "year": 2019,
                "genre": "Electronic",
                "coverArt": server.album.glass_harbour,
                "duration": 2,
                "created": model::iso8601(catalogue.album("Glass Harbour").created_at),
                "artistId": aurora_field,
                "mediaType": "album",
                "playCount": 0,
            },
            {
                "id": server.album.paper_maps,
                "parent": aurora_field,
                "isDir": true,
                "title": "Paper Maps",
                "album": "Paper Maps",
                "artist": "Aurora Field",
                "year": 2021,
                "genre": "Electronic",
                "duration": 2,
                "created": model::iso8601(catalogue.album("Paper Maps").created_at),
                "artistId": aurora_field,
                "mediaType": "album",
                "playCount": 0,
            },
        ])
    );

    // The whole document of one artist, written out.
    let slow_rivers = server.album.slow_rivers.as_str();
    let marlow_quartet = server.artist.marlow_quartet.as_str();
    assert_eq!(
        server
            .get_xml("getMusicDirectory", &[("id", marlow_quartet)])
            .await,
        xml_ok(&format!(
            concat!(
                r#"<directory id="{artist}" name="Marlow Quartet">"#,
                r#"<child album="Slow Rivers" artist="Marlow Quartet" artistId="{artist}""#,
                r#" coverArt="{album}" created="{created}" duration="1" genre="Jazz""#,
                r#" id="{album}" isDir="true" mediaType="album" parent="{artist}""#,
                r#" playCount="0" title="Slow Rivers" year="2015"/></directory>"#
            ),
            artist = marlow_quartet,
            album = slow_rivers,
            created = model::iso8601(catalogue.album("Slow Rivers").created_at),
        ))
    );
}

/// The songs of an album as `getAlbum` returns them.
async fn songs_of_album(server: &TestServer, id: &str) -> Value {
    let response = server.get_json("getAlbum", &[("id", id)]).await;
    assert_eq!(response["status"], "ok", "{id}: {response}");
    response["album"]["song"].clone()
}

#[tokio::test]
async fn get_music_directory_of_an_album_lists_its_songs_by_disc_and_track() {
    let server = TestServer::standard().await;

    // Each album with its artist and its songs in the order of `getAlbum`.
    let albums: [(&str, &str, &str, &[&str]); 4] = [
        (
            "Glass Harbour",
            &server.album.glass_harbour,
            &server.artist.aurora_field,
            &["Low Tide", "Breakwater", "Night Ferry"],
        ),
        (
            "Paper Maps",
            &server.album.paper_maps,
            &server.artist.aurora_field,
            &["Meridian", "Contour", "Legend"],
        ),
        (
            "Slow Rivers",
            &server.album.slow_rivers,
            &server.artist.marlow_quartet,
            &["Upstream", "Delta"],
        ),
        (
            "Summer Sampler",
            &server.album.summer_sampler,
            &server.artist.various_artists,
            &["Open Road", "Last Train"],
        ),
    ];
    for (name, id, artist_id, titles) in albums {
        // The songs are the elements that `getAlbum` returns.
        let songs = songs_of_album(&server, id).await;
        assert_eq!(texts(&songs, "title"), titles, "{name}");
        let expected = json!({
            "id": id,
            "parent": artist_id,
            "name": name,
            "playCount": 0,
            "child": songs,
        });
        for method in ["getMusicDirectory", "getMusicDirectory.view"] {
            assert_eq!(
                server.get_json(method, &[("id", id)]).await,
                ok("directory", expected.clone()),
                "{method} {name}"
            );
        }

        // Every song is a file of the directory.
        for song in songs.as_array().expect("a song list") {
            assert_eq!(song["isDir"], false, "{song}");
            assert_eq!(song["parent"], id, "{song}");
            assert_eq!(song["albumId"], id, "{song}");
            assert_eq!(song["artistId"], artist_id, "{song}");
            assert_eq!(song["mediaType"], "song", "{song}");
            assert!(song["path"].is_string(), "{song}");
        }

        let elements: String = songs
            .as_array()
            .expect("a song list")
            .iter()
            .map(|song| format!("<child{}/>", xml_attributes(song)))
            .collect();
        assert_eq!(
            server.get_xml("getMusicDirectory", &[("id", id)]).await,
            xml_ok(&format!(
                concat!(
                    r#"<directory id="{id}" name="{name}" parent="{artist_id}" playCount="0">"#,
                    "{elements}</directory>"
                ),
                id = id,
                name = name,
                artist_id = artist_id,
                elements = elements,
            )),
            "{name}"
        );
    }

    // "Paper Maps" has two tracks on disc 1 and one on disc 2. The order of
    // the paths is another one: `01 Legend.mp3` is the first path.
    let songs = directory(&server, &server.album.paper_maps).await["child"].clone();
    assert_eq!(texts(&songs, "title"), ["Meridian", "Contour", "Legend"]);
    assert_eq!(column(&songs, "discNumber"), [1, 1, 2]);
    assert_eq!(column(&songs, "track"), [1, 2, 1]);
    assert_eq!(
        texts(&songs, "path"),
        [
            "Aurora Field/Paper Maps/01 Meridian.mp3",
            "Aurora Field/Paper Maps/02 Contour.mp3",
            "Aurora Field/Paper Maps/01 Legend.mp3",
        ]
    );
    assert_eq!(
        column(&songs, "id"),
        [
            &server.track.meridian,
            &server.track.contour,
            &server.track.legend
        ]
    );

    // A compilation is a directory of "Various Artists", and each song
    // names its track artist.
    let sampler = directory(&server, &server.album.summer_sampler).await;
    assert_eq!(sampler["parent"], server.artist.various_artists.as_str());
    assert_eq!(
        texts(&sampler["child"], "artist"),
        ["Tessa Lund", "Aurora Field"]
    );

    // The cover of a song is the cover of its album. "Slow Rivers" has the
    // picture of its tracks, and "Summer Sampler" has none.
    let slow_rivers = directory(&server, &server.album.slow_rivers).await;
    assert_eq!(
        column(&slow_rivers["child"], "coverArt"),
        [&server.album.slow_rivers, &server.album.slow_rivers]
    );
    assert!(sampler["child"][0].get("coverArt").is_none(), "{sampler}");
}

/// The IDs of the standard library: its artists, its albums and its tracks.
fn standard_ids(server: &StandardServer) -> [BTreeSet<String>; 3] {
    let set = |ids: &[&String]| ids.iter().map(|id| (*id).clone()).collect();
    let (artist, album, track) = (&server.artist, &server.album, &server.track);
    [
        set(&[
            &artist.aurora_field,
            &artist.marlow_quartet,
            &artist.various_artists,
        ]),
        set(&[
            &album.glass_harbour,
            &album.paper_maps,
            &album.slow_rivers,
            &album.summer_sampler,
        ]),
        set(&[
            &track.low_tide,
            &track.breakwater,
            &track.night_ferry,
            &track.meridian,
            &track.contour,
            &track.legend,
            &track.upstream,
            &track.delta,
            &track.open_road,
            &track.last_train,
        ]),
    ]
}

#[tokio::test]
async fn the_tree_is_walked_from_the_indexes_to_every_song_and_back() {
    let server = TestServer::standard().await;
    let [artist_ids, album_ids, track_ids] = standard_ids(&server);
    let text = |value: &Value| value.as_str().expect("a string").to_owned();

    // Down: the artists of the indexes, the albums of each artist and the
    // songs of each album.
    let indexes = indexes(&server, &[]).await;
    let mut artists = BTreeSet::new();
    let mut albums = BTreeSet::new();
    let mut songs = Vec::new();
    for artist in artist_column(&indexes, "id") {
        assert!(artists.insert(artist.clone()), "{artist} is listed twice");
        let artist_dir = directory(&server, &artist).await;
        assert_eq!(artist_dir["id"], artist.as_str());
        // An artist is at the first level and has no parent.
        assert!(artist_dir.get("parent").is_none(), "{artist_dir}");
        // The ID is the artist ID of the ID3 methods.
        let response = server.get_json("getArtist", &[("id", &artist)]).await;
        assert_eq!(response["artist"]["name"], artist_dir["name"]);

        for album in artist_dir["child"].as_array().expect("a child list") {
            let album_id = text(&album["id"]);
            assert!(
                albums.insert(album_id.clone()),
                "{album_id} is listed twice"
            );
            assert_eq!(album["isDir"], true, "{album}");
            assert_eq!(album["parent"], artist.as_str(), "{album}");
            assert_eq!(album["artistId"], artist.as_str(), "{album}");

            let album_dir = directory(&server, &album_id).await;
            assert_eq!(album_dir["id"], album_id.as_str());
            assert_eq!(album_dir["name"], album["title"]);
            assert_eq!(album_dir["parent"], artist.as_str(), "{album_dir}");
            // The ID is the album ID of the ID3 methods, and the songs are
            // those of `getAlbum`.
            assert_eq!(
                songs_of_album(&server, &album_id).await,
                album_dir["child"],
                "{album_id}"
            );

            for song in album_dir["child"].as_array().expect("a child list") {
                assert_eq!(song["isDir"], false, "{song}");
                assert_eq!(song["parent"], album_id.as_str(), "{song}");
                // The ID is the track ID that `getSong` takes, and the
                // element is the song of that method.
                let id = text(&song["id"]);
                let response = server.get_json("getSong", &[("id", &id)]).await;
                assert_eq!(response["status"], "ok", "{id}: {response}");
                assert_eq!(response["song"], *song, "{id}");
                songs.push((id, album_id.clone(), artist.clone()));
            }
        }
    }

    // The walk reached every row of the library once.
    assert_eq!(artists, artist_ids);
    assert_eq!(albums, album_ids);
    let reached: BTreeSet<String> = songs.iter().map(|(id, _, _)| id.clone()).collect();
    assert_eq!(reached, track_ids);
    assert_eq!(songs.len(), 10);

    // Up: from each song, `parent` names the directory that lists it, and
    // `parent` of that directory names the directory that lists the album.
    let listed = artist_column(&indexes, "id");
    for (song, album, artist) in &songs {
        let response = server.get_json("getSong", &[("id", song)]).await;
        let parent = text(&response["song"]["parent"]);
        assert_eq!(&parent, album);

        let album_dir = directory(&server, &parent).await;
        assert!(
            texts(&album_dir["child"], "id").contains(song),
            "{song}: {album_dir}"
        );
        let grandparent = text(&album_dir["parent"]);
        assert_eq!(&grandparent, artist);

        let artist_dir = directory(&server, &grandparent).await;
        assert!(
            texts(&artist_dir["child"], "id").contains(&parent),
            "{parent}: {artist_dir}"
        );
        assert!(artist_dir.get("parent").is_none(), "{artist_dir}");
        assert!(listed.contains(&grandparent), "{grandparent}");
    }
}

/// The body and the content type of the answer of `getCoverArt` to `params`.
/// The answer must be HTTP 200.
async fn cover(server: &TestServer, params: &[(&str, &str)]) -> (Vec<u8>, String) {
    let response = server.get_raw("getCoverArt", params).await;
    assert_eq!(response.status(), StatusCode::OK, "{params:?}");
    let content_type = content_type(&response);
    (body_bytes(response).await, content_type)
}

#[tokio::test]
async fn the_cover_art_ids_of_the_tree_are_ids_that_get_cover_art_serves() {
    let server = TestServer::standard().await;
    let glass_harbour = server.album.glass_harbour.as_str();
    let slow_rivers = server.album.slow_rivers.as_str();

    // An artist has no image, so no artist of the indexes has `coverArt`.
    let indexes = indexes(&server, &[]).await;
    for index in indexes["index"].as_array().expect("an index list") {
        for artist in index["artist"].as_array().expect("an artist list") {
            assert!(artist.get("coverArt").is_none(), "{artist}");
        }
    }

    // Every album and every song of the tree, each with the ID of its own
    // row and its `coverArt`.
    let mut items: Vec<(String, Option<String>)> = Vec::new();
    let mut read = |element: &Value| {
        let text = |value: &Value| value.as_str().expect("a string").to_owned();
        items.push((text(&element["id"]), element.get("coverArt").map(text)));
    };
    for artist in artist_column(&indexes, "id") {
        let artist_dir = directory(&server, &artist).await;
        assert!(artist_dir.get("coverArt").is_none(), "{artist_dir}");
        for album in artist_dir["child"].as_array().expect("a child list") {
            read(album);
            let id = album["id"].as_str().expect("an ID");
            let album_dir = directory(&server, id).await;
            album_dir["child"]
                .as_array()
                .expect("a child list")
                .iter()
                .for_each(&mut read);
        }
    }
    assert_eq!(items.len(), 4 + 10);

    // The values are the IDs of the two albums that have a cover. "Glass
    // Harbour" and its three songs name the one, "Slow Rivers" and its two
    // songs the other.
    let named = |id: &str| {
        items
            .iter()
            .filter(|(_, cover_art)| cover_art.as_deref() == Some(id))
            .count()
    };
    assert_eq!(named(glass_harbour), 1 + 3);
    assert_eq!(named(slow_rivers), 1 + 2);
    let values: BTreeSet<&str> = items
        .iter()
        .filter_map(|(_, cover_art)| cover_art.as_deref())
        .collect();
    assert_eq!(values, BTreeSet::from([glass_harbour, slow_rivers]));

    // `getCoverArt` serves each value: the image as its source stores it,
    // and a JPEG for a size.
    let cover_file = server.path("Aurora Field/Glass Harbour/cover.jpg");
    let sources: [(&str, Vec<u8>, &str); 2] = [
        (
            glass_harbour,
            fs::read(&cover_file).expect("read the cover file"),
            "image/jpeg",
        ),
        // The picture in the first track of the album.
        (slow_rivers, PICTURE_PNG.to_vec(), "image/png"),
    ];
    for (id, bytes, mime) in sources {
        for method in ["getCoverArt", "getCoverArt.view"] {
            let response = server.get_raw(method, &[("id", id)]).await;
            assert_eq!(response.status(), StatusCode::OK, "{method} {id}");
            assert_eq!(content_type(&response), mime, "{method} {id}");
            assert_eq!(body_bytes(response).await, bytes, "{method} {id}");
        }
        let (body, content_type) = cover(&server, &[("id", id), ("size", "32")]).await;
        assert_eq!(content_type, "image/jpeg", "{id}");
        let format = image::guess_format(&body).expect("an image");
        assert_eq!(format.to_mime_type(), "image/jpeg", "{id}");
        image::load_from_memory(&body).expect("decode the cover");
    }

    // An item without `coverArt` has no cover: `getCoverArt` answers error
    // 70 for the ID of its row. These are the two albums without a cover
    // and their five songs.
    let without: Vec<&str> = items
        .iter()
        .filter(|(_, cover_art)| cover_art.is_none())
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(without.len(), 2 + 5);
    for id in without {
        assert_eq!(
            server.get_json("getCoverArt", &[("id", id)]).await,
            failed(70, NOT_FOUND),
            "{id}"
        );
    }
}

/// 2023-03-14T17:51:22Z.
const STARRED_AT: i64 = 1_678_816_282;

/// 2023-03-15T00:00:00Z.
const PLAYED_AT: i64 = 1_678_838_400;

#[tokio::test]
async fn folder_methods_report_what_the_user_attached_to_an_item() {
    let server = TestServer::standard().await;
    let user_id = owner_id(&server).await;
    let artist = server.artist.aurora_field.clone();
    let album = server.album.glass_harbour.clone();
    let track = server.track.breakwater.clone();
    server
        .db()
        .write(move |conn| {
            let mut insert = conn.prepare(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            insert.execute(params![
                user_id,
                artist,
                "artist",
                STARRED_AT,
                3,
                0,
                None::<i64>
            ])?;
            // A play is the play of a track: the row of the album holds a
            // star and a rating, and the row of the track holds the plays.
            insert.execute(params![
                user_id,
                album,
                "album",
                STARRED_AT,
                4,
                0,
                None::<i64>
            ])?;
            insert.execute(params![
                user_id, track, "track", STARRED_AT, 5, 3, PLAYED_AT
            ])?;
            Ok(())
        })
        .await
        .expect("write the annotations");
    let starred = "2023-03-14T17:51:22Z";
    let played = "2023-03-15T00:00:00Z";
    let artist = server.artist.aurora_field.as_str();
    let album = server.album.glass_harbour.as_str();

    // The artist in the indexes: the star and the rating.
    let indexes = indexes(&server, &[]).await;
    assert_eq!(
        indexes["index"][0]["artist"],
        json!([{
            "id": artist,
            "name": "Aurora Field",
            "starred": starred,
            "userRating": 3,
        }])
    );
    // An artist without a star and a rating has neither field.
    assert_eq!(
        indexes["index"][1]["artist"],
        json!([{ "id": server.artist.marlow_quartet, "name": "Marlow Quartet" }])
    );

    // The plays of the album as `getAlbum` reports them. Both forms of an
    // album report the same plays.
    let response = server.get_json("getAlbum", &[("id", album)]).await;
    let album_id3 = &response["album"];
    assert_eq!(album_id3["starred"], starred);
    assert!(album_id3["playCount"].is_u64(), "{album_id3}");

    // The directory of the artist: the star and the rating, and no play
    // count. The album in it has the star, the rating and the plays.
    let artist_dir = directory(&server, artist).await;
    assert_eq!(artist_dir["starred"], starred);
    assert_eq!(artist_dir["userRating"], 3);
    assert!(artist_dir.get("playCount").is_none(), "{artist_dir}");
    let child = &artist_dir["child"][0];
    assert_eq!(child["id"], album);
    assert_eq!(child["starred"], starred);
    assert_eq!(child["userRating"], 4);
    assert_eq!(child["playCount"], album_id3["playCount"]);
    assert_eq!(child.get("played"), album_id3.get("played"));
    // The other album has no value of the user.
    let other = &artist_dir["child"][1];
    assert!(other.get("starred").is_none(), "{other}");
    assert!(other.get("userRating").is_none(), "{other}");
    assert_eq!(other["playCount"], 0);

    // The directory of the album: the star, the rating and the play count.
    let album_dir = directory(&server, album).await;
    assert_eq!(album_dir["starred"], starred);
    assert_eq!(album_dir["userRating"], 4);
    assert_eq!(album_dir["playCount"], album_id3["playCount"]);
    // The song in it is the song of `getSong`, with the four values.
    let song = &album_dir["child"][1];
    assert_eq!(song["id"], server.track.breakwater.as_str());
    assert_eq!(song["starred"], starred);
    assert_eq!(song["userRating"], 5);
    assert_eq!(song["playCount"], 3);
    assert_eq!(song["played"], played);
    assert_eq!(album_dir["child"], songs_of_album(&server, album).await);

    // In XML the values are attributes.
    let xml = server.get_xml("getIndexes", &[]).await;
    assert!(
        xml.contains(&format!(
            r#"<artist id="{artist}" name="Aurora Field" starred="{starred}" userRating="3"/>"#
        )),
        "{xml}"
    );
    let xml = server.get_xml("getMusicDirectory", &[("id", artist)]).await;
    assert!(
        xml.contains(&format!(
            r#"<directory id="{artist}" name="Aurora Field" starred="{starred}" userRating="3">"#
        )),
        "{xml}"
    );
    let xml = server.get_xml("getMusicDirectory", &[("id", album)]).await;
    assert!(
        xml.contains(&format!(
            concat!(
                r#"<directory id="{album}" name="Glass Harbour" parent="{artist}""#,
                r#" playCount="{plays}" starred="{starred}" userRating="4">"#
            ),
            album = album,
            artist = artist,
            plays = album_dir["playCount"],
            starred = starred,
        )),
        "{xml}"
    );
    // The song has its plays as attributes.
    assert!(
        xml.contains(&format!(r#" playCount="3" played="{played}" "#)),
        "{xml}"
    );
}

#[tokio::test]
async fn the_ids_of_the_tree_are_the_ids_that_star_and_set_rating_take() {
    let server = TestServer::standard().await;
    let artist = server.artist.marlow_quartet.as_str();
    let album = server.album.slow_rivers.as_str();

    // The IDs come from the tree.
    let artist_dir = directory(&server, artist).await;
    let album_id = artist_dir["child"][0]["id"].as_str().expect("an ID");
    assert_eq!(album_id, album);
    let album_dir = directory(&server, album_id).await;
    let song_id = album_dir["child"][0]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();

    for params in [
        vec![("artistId", artist)],
        vec![("albumId", album_id)],
        vec![("id", song_id.as_str())],
    ] {
        let response = server.get_json("star", &params).await;
        assert_eq!(response["status"], "ok", "{params:?}: {response}");
    }
    for (id, rating) in [(artist, "2"), (album_id, "3"), (song_id.as_str(), "4")] {
        let response = server
            .get_json("setRating", &[("id", id), ("rating", rating)])
            .await;
        assert_eq!(response["status"], "ok", "{id}: {response}");
    }

    // The tree shows the star and the rating of each item, and the star is
    // the one that the ID3 methods show.
    let indexes = indexes(&server, &[]).await;
    let entry = &indexes["index"][1]["artist"][0];
    assert_eq!(entry["id"], artist);
    assert_eq!(entry["userRating"], 2);
    let response = server.get_json("getArtist", &[("id", artist)]).await;
    assert!(entry["starred"].is_string(), "{entry}");
    assert_eq!(entry["starred"], response["artist"]["starred"]);

    let artist_dir = directory(&server, artist).await;
    assert_eq!(artist_dir["starred"], entry["starred"]);
    assert_eq!(artist_dir["userRating"], 2);
    let child = &artist_dir["child"][0];
    let response = server.get_json("getAlbum", &[("id", album)]).await;
    assert!(child["starred"].is_string(), "{child}");
    assert_eq!(child["starred"], response["album"]["starred"]);
    assert_eq!(child["userRating"], 3);

    let album_dir = directory(&server, album).await;
    assert_eq!(album_dir["starred"], child["starred"]);
    assert_eq!(album_dir["userRating"], 3);
    let song = &album_dir["child"][0];
    assert!(song["starred"].is_string(), "{song}");
    assert_eq!(song["userRating"], 4);

    // The starred lists of the folder methods hold the same elements.
    let response = server.get_json("getStarred", &[]).await;
    assert_eq!(response["starred"]["artist"], json!([entry]));
    assert_eq!(response["starred"]["album"], json!([child]));
    assert_eq!(response["starred"]["song"], json!([song]));
}

#[tokio::test]
async fn get_music_directory_answers_error_70_for_an_id_that_names_no_directory() {
    let server = TestServer::standard().await;
    let artist = server.artist.aurora_field.as_str();
    let album = server.album.glass_harbour.as_str();
    // A song is no directory, also one that exists.
    let track = server.track.low_tide.as_str();

    let upper = artist.to_uppercase();
    let long = format!("{album}0");
    let short = &album[..album.len() - 1];
    let leading_space = format!(" {artist}");
    let trailing_space = format!("{album} ");
    let wildcard = format!("{}%", &album[..3]);
    let ids = [
        UNKNOWN_ARTIST,
        UNKNOWN_ALBUM,
        track,
        "tr-ffffffffffffffffffffffffffffffff",
        "pl-ffffffffffffffffffffffffffffffff",
        "",
        "1",
        "0",
        "-1",
        &artist[..3],
        &album[3..],
        "' OR 1=1 --",
        "%",
        "/",
        "Aurora Field",
        "Aurora Field/Glass Harbour",
        upper.as_str(),
        long.as_str(),
        short,
        leading_space.as_str(),
        trailing_space.as_str(),
        wildcard.as_str(),
    ];
    for id in ids {
        for method in ["getMusicDirectory", "getMusicDirectory.view"] {
            assert_eq!(
                server.get_json(method, &[("id", id)]).await,
                failed(70, NOT_FOUND),
                "{method} {id:?}"
            );
        }
        assert_eq!(
            server.get_xml("getMusicDirectory", &[("id", id)]).await,
            xml_failed(70, NOT_FOUND),
            "{id:?}"
        );
    }

    // The IDs of the rows are found.
    for id in [artist, album] {
        let response = server.get_json("getMusicDirectory", &[("id", id)]).await;
        assert_eq!(response["status"], "ok", "{id}");
    }
}

#[tokio::test]
async fn get_music_directory_without_an_id_is_error_10() {
    let server = TestServer::standard().await;
    let message = "required parameter is missing: id";
    for method in ["getMusicDirectory", "getMusicDirectory.view"] {
        assert_eq!(
            server.get_json(method, &[]).await,
            failed(10, message),
            "{method}"
        );
        // The name of a parameter is compared as it is written.
        assert_eq!(
            server
                .get_json(method, &[("ID", &server.album.glass_harbour)])
                .await,
            failed(10, message),
            "{method}"
        );
        // `getIndexes` has no directory ID, and its parameters are none of
        // this method.
        assert_eq!(
            server.get_json(method, &[("musicFolderId", "1")]).await,
            failed(10, message),
            "{method}"
        );
    }
    assert_eq!(
        server.get_xml("getMusicDirectory", &[]).await,
        xml_failed(10, message)
    );
}

#[tokio::test]
async fn a_track_without_a_file_is_in_no_directory() {
    let server = TestServer::standard().await;
    let folder_id = server.folder_id.to_string();
    let in_folder = [("musicFolderId", folder_id.as_str())];
    let artist = server.artist.marlow_quartet.as_str();
    let album = server.album.slow_rivers.as_str();
    let upstream = server.track.upstream.as_str();
    let delta = server.track.delta.as_str();
    remove_file(&server, upstream).await;

    // The directory of the album holds the track that has a file. `getAlbum`
    // lists both tracks.
    let album_dir = directory(&server, album).await;
    assert_eq!(texts(&album_dir["child"], "id"), [delta]);
    assert_eq!(texts(&album_dir["child"], "title"), ["Delta"]);
    let songs = songs_of_album(&server, album).await;
    assert_eq!(texts(&songs, "id"), [upstream, delta]);
    assert_eq!(album_dir["child"][0], songs[1]);
    let xml = server.get_xml("getMusicDirectory", &[("id", album)]).await;
    assert_eq!(xml.matches("<child ").count(), 1, "{xml}");
    // The placeholder path of the row is in no document.
    assert!(!xml.contains(&format!("/{upstream}")), "{xml}");

    // The track keeps its ID: `getSong` returns it, without a path.
    let response = server.get_json("getSong", &[("id", upstream)]).await;
    assert_eq!(response["song"]["parent"], album);
    assert!(response["song"].get("path").is_none(), "{response}");

    // No track of the album has a file. The album and the artist keep their
    // rows, so the directory of the album is listed and holds nothing.
    remove_file(&server, delta).await;
    let expected = json!({ "id": album, "parent": artist, "name": "Slow Rivers", "playCount": 0, "child": [] });
    assert_eq!(
        server.get_json("getMusicDirectory", &[("id", album)]).await,
        ok("directory", expected)
    );
    assert_eq!(
        server.get_xml("getMusicDirectory", &[("id", album)]).await,
        xml_ok(&format!(
            r#"<directory id="{album}" name="Slow Rivers" parent="{artist}" playCount="0"/>"#
        ))
    );
    let artist_dir = directory(&server, artist).await;
    assert_eq!(texts(&artist_dir["child"], "id"), [album]);

    // The indexes list the artist, as `getArtists` does. With a music folder
    // they leave it out: no track of it has a file in the folder.
    assert_eq!(
        artist_column(&indexes(&server, &[]).await, "name"),
        ["Aurora Field", "Marlow Quartet", "Various Artists"]
    );
    assert_eq!(
        artist_column(&indexes(&server, &in_folder).await, "name"),
        ["Aurora Field", "Various Artists"]
    );
    for params in [&[][..], &in_folder[..]] {
        let response = server.get_json("getArtists", params).await;
        assert_eq!(
            artist_column(&response["artists"], "id"),
            artist_column(&indexes(&server, params).await, "id"),
            "{params:?}"
        );
    }

    // A walk of the tree reaches the eight tracks that have a file.
    let mut reached = BTreeSet::new();
    for artist in artist_column(&indexes(&server, &[]).await, "id") {
        for album in texts(&directory(&server, &artist).await["child"], "id") {
            reached.extend(texts(&directory(&server, &album).await["child"], "id"));
        }
    }
    let [_, _, mut with_file] = standard_ids(&server);
    with_file.remove(upstream);
    with_file.remove(delta);
    assert_eq!(reached, with_file);
}

/// One track that [`written`] stores.
struct Row {
    artist: &'static str,
    album: String,
    title: String,
    disc_no: Option<u32>,
    track_no: Option<u32>,
}

/// A server whose database holds `rows` as the scanner stores tracks, with
/// the counts of the albums and of the artists. No file exists for a track.
/// The path of a track is its place in `rows`, so the order of the paths is
/// the order of `rows`.
async fn written(rows: Vec<Row>) -> TestServer {
    let server = TestServer::bare(Config::default()).await;
    server
        .db()
        .write(move |conn| {
            let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?;
            for (number, row) in rows.iter().enumerate() {
                let artist_id = artists::upsert_by_key(conn, row.artist)?;
                let album_id =
                    albums::upsert_by_key(conn, &artist_id, &row.album, row.artist, false)?;
                let meta = TrackMeta {
                    title: row.title.clone(),
                    artist: row.artist.to_owned(),
                    album_artist: row.artist.to_owned(),
                    album: row.album.clone(),
                    track_no: row.track_no,
                    disc_no: row.disc_no,
                    year: None,
                    genre: None,
                    compilation: false,
                    duration_ms: 1000,
                    bitrate_kbps: Some(900),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    has_picture: false,
                    suffix: "flac".to_owned(),
                    content_type: "audio/flac".to_owned(),
                    lossless: true,
                };
                let content = TrackContent {
                    fingerprint: "100:aa",
                    mtime: 1000,
                    size: 100,
                    meta: &meta,
                    artist_id: &artist_id,
                    album_id: &album_id,
                };
                let id = ids::new_id(Kind::Track);
                let rel_path = format!("{number:05}.flac");
                tracks::insert_track(conn, &id, folder[0].id, &rel_path, &content)?;
            }
            albums::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .await
        .expect("write the library");
    server
}

#[tokio::test]
async fn a_directory_with_many_children_is_listed_in_full_and_in_order() {
    const ARTIST: &str = "Aurora Field";
    const ALBUMS: usize = 300;
    const DISCS: u32 = 3;
    const TRACKS_PER_DISC: u32 = 200;

    // 300 albums of one track, written from the last title to the first.
    let mut rows: Vec<Row> = (0..ALBUMS)
        .rev()
        .map(|number| Row {
            artist: ARTIST,
            album: format!("Album {number:03}"),
            title: "One".to_owned(),
            disc_no: None,
            track_no: None,
        })
        .collect();
    // One album of 600 tracks on three discs, written from the last track
    // of the last disc to the first, so that the paths have that order.
    for disc in (1..=DISCS).rev() {
        for track in (1..=TRACKS_PER_DISC).rev() {
            rows.push(Row {
                artist: ARTIST,
                album: "Long Play".to_owned(),
                title: format!("Disc {disc} Track {track:03}"),
                disc_no: Some(disc),
                track_no: Some(track),
            });
        }
    }
    let server = written(rows).await;
    let artist = artist_column(&indexes(&server, &[]).await, "id");
    assert_eq!(artist.len(), 1);

    // The directory of the artist: every album, by title.
    let artist_dir = directory(&server, &artist[0]).await;
    let mut titles: Vec<String> = (0..ALBUMS)
        .map(|number| format!("Album {number:03}"))
        .collect();
    titles.push("Long Play".to_owned());
    assert_eq!(texts(&artist_dir["child"], "title"), titles);
    let album_ids = texts(&artist_dir["child"], "id");
    let distinct: BTreeSet<&String> = album_ids.iter().collect();
    assert_eq!(distinct.len(), ALBUMS + 1);
    assert!(
        column(&artist_dir["child"], "parent")
            .iter()
            .all(|parent| **parent == artist[0].as_str()),
        "{artist_dir}"
    );

    // The directory of the long album: every track, by disc and track.
    let long_play = album_ids.last().expect("the last album");
    let album_dir = directory(&server, long_play).await;
    let titles: Vec<String> = (1..=DISCS)
        .flat_map(|disc| {
            (1..=TRACKS_PER_DISC).map(move |track| format!("Disc {disc} Track {track:03}"))
        })
        .collect();
    assert_eq!(titles.len(), 600);
    assert_eq!(texts(&album_dir["child"], "title"), titles);
    assert!(
        column(&album_dir["child"], "parent")
            .iter()
            .all(|parent| **parent == long_play.as_str()),
        "{album_dir}"
    );

    // The annotations of all children are read at once: a star on the first
    // and on the last child shows on both.
    let song_ids = texts(&album_dir["child"], "id");
    let stars = [
        ("albumId", album_ids[0].as_str()),
        ("albumId", long_play.as_str()),
        ("id", song_ids[0].as_str()),
        ("id", song_ids[599].as_str()),
    ];
    let response = server.get_json("star", &stars).await;
    assert_eq!(response["status"], "ok", "{response}");
    let artist_dir = directory(&server, &artist[0]).await;
    let starred: Vec<usize> = (0..=ALBUMS)
        .filter(|&place| artist_dir["child"][place]["starred"].is_string())
        .collect();
    assert_eq!(starred, [0, ALBUMS]);
    let album_dir = directory(&server, long_play).await;
    assert!(album_dir["starred"].is_string(), "{album_dir}");
    let starred: Vec<usize> = (0..600)
        .filter(|&place| album_dir["child"][place]["starred"].is_string())
        .collect();
    assert_eq!(starred, [0, 599]);
}

#[tokio::test]
async fn folder_methods_answer_a_post_with_a_form_body() {
    let server = TestServer::standard().await;
    let json_of = |response| async move {
        assert_eq!(content_type(&response), "application/json");
        subsonic_response(&body_string(response).await)
    };
    let last = last_modified(&server).await.to_string();

    let form = [("f", "json")];
    let response = json_of(server.post_form("getIndexes.view", &form).await).await;
    assert_eq!(response, server.get_json("getIndexes", &[]).await);
    assert_eq!(index_names(&response["indexes"]).len(), 3);

    let folder_id = server.folder_id.to_string();
    let form = [
        ("musicFolderId", folder_id.as_str()),
        ("ifModifiedSince", last.as_str()),
        ("f", "json"),
    ];
    let response = json_of(server.post_form("getIndexes", &form).await).await;
    assert!(response["indexes"].get("index").is_none(), "{response}");
    assert_eq!(response["indexes"]["lastModified"].to_string(), last);

    for id in [&server.artist.aurora_field, &server.album.paper_maps] {
        let form = [("id", id.as_str()), ("f", "json")];
        let response = json_of(server.post_form("getMusicDirectory.view", &form).await).await;
        assert_eq!(
            response,
            server.get_json("getMusicDirectory", &[("id", id)]).await
        );
        assert_eq!(response["directory"]["id"], id.as_str());
    }

    let form = [("id", server.track.low_tide.as_str()), ("f", "json")];
    let response = json_of(server.post_form("getMusicDirectory", &form).await).await;
    assert_eq!(response, failed(70, NOT_FOUND));
}

#[tokio::test]
async fn two_calls_return_the_same_document() {
    let server = TestServer::standard().await;
    let folder_id = server.folder_id.to_string();
    let last = last_modified(&server).await.to_string();
    let calls: [(&str, Vec<(&str, &str)>); 6] = [
        ("getIndexes", vec![]),
        ("getIndexes", vec![("musicFolderId", &folder_id)]),
        ("getIndexes", vec![("ifModifiedSince", &last)]),
        (
            "getMusicDirectory",
            vec![("id", &server.artist.aurora_field)],
        ),
        ("getMusicDirectory", vec![("id", &server.album.paper_maps)]),
        (
            "getMusicDirectory",
            vec![("id", &server.album.summer_sampler)],
        ),
    ];
    for (method, params) in &calls {
        let first = server.get_xml(method, params).await;
        assert!(first.contains(r#" status="ok" "#), "{method}: {first}");
        for _ in 0..3 {
            assert_eq!(server.get_xml(method, params).await, first, "{method}");
        }
        let first = server.get_json(method, params).await;
        assert_eq!(server.get_json(method, params).await, first, "{method}");
    }
}
