//! Router tests of the annotation methods `star`, `unstar`, `setRating`,
//! `getStarred` and `getStarred2` (`docs/design.md` section 4).
//!
//! A star and a rating are data of the owner. The tests check what the
//! requests store, what a failed request leaves untouched, and what a rescan
//! keeps.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, TagExt};
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::model;
use wn_server::db::queries::annotations::{self, Annotation};
use wn_server::db::queries::browse::{self, Album, Artist, Song};
use wn_server::db::queries::{tracks, users};

use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, StandardServer, TestServer,
    body_string, content_type, form_request, subsonic_response,
};

/// 2023-11-14T22:13:20Z, the time that the tests store where a document is
/// compared.
const STARRED_AT: i64 = 1_700_000_000;

/// [`STARRED_AT`] as the models write it.
const STARRED: &str = "2023-11-14T22:13:20Z";

/// IDs of the three kinds that no row has.
const NO_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";
const NO_ALBUM: &str = "al-ffffffffffffffffffffffffffffffff";
const NO_ARTIST: &str = "ar-ffffffffffffffffffffffffffffffff";

/// The ID of a playlist. No playlist has an annotation.
const PLAYLIST: &str = "pl-00000000000000000000000000000001";

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of the error for a rating that is an integer outside 0 to 5.
const RATING_RANGE: &str = "parameter rating must be an integer from 0 to 5";

/// The message of the error for a rating that is no integer.
const RATING_INTEGER: &str = "parameter rating must be an integer";

fn unix_now() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    i64::try_from(now.as_secs()).expect("the time fits")
}

fn to_json(model: impl serde::Serialize) -> Value {
    serde_json::to_value(model).expect("the model serialises")
}

/// The `error` object with `code` and `message`.
fn error(code: u32, message: &str) -> Value {
    json!({ "code": code, "message": message })
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

/// The ID of the owner account of the server.
async fn owner_id(server: &TestServer) -> i64 {
    server
        .db()
        .read(|conn| users::get_by_name(conn, OWNER))
        .await
        .expect("read the owner")
        .expect("the server has an owner")
        .user
        .id
}

/// One row of the `annotations` table: `user_id`, `item_id`, `item_type`,
/// `starred_at`, `rating`, `play_count` and `last_played_at`.
type Row = (
    i64,
    String,
    String,
    Option<i64>,
    Option<u8>,
    i64,
    Option<i64>,
);

/// Every row of the `annotations` table, in the order of the users and then
/// of the item IDs.
async fn rows(server: &TestServer) -> Vec<Row> {
    server
        .db()
        .read(|conn| {
            let mut select = conn.prepare(
                "SELECT user_id, item_id, item_type, starred_at, rating, play_count,
                   last_played_at
                 FROM annotations ORDER BY user_id, item_id",
            )?;
            let rows = select
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await
        .expect("read the annotations")
}

/// The annotation of the owner for the item `id`. The default annotation
/// when the item has no row.
async fn annotation(server: &TestServer, id: &str) -> Annotation {
    let user_id = owner_id(server).await;
    let ids = [id.to_owned()];
    let found = server
        .db()
        .read(move |conn| annotations::get_many(conn, user_id, &ids))
        .await
        .expect("read the annotations");
    found.get(id).copied().unwrap_or_default()
}

/// Sets `starred_at` of the row of the owner for the item `id`.
async fn set_starred_at(server: &TestServer, id: &str, at: i64) {
    let user_id = owner_id(server).await;
    let id = id.to_owned();
    let changed = server
        .db()
        .write(move |conn| {
            Ok(conn.execute(
                "UPDATE annotations SET starred_at = ?3 WHERE user_id = ?1 AND item_id = ?2",
                params![user_id, id, at],
            )?)
        })
        .await
        .expect("set the time of the star");
    assert_eq!(changed, 1, "the item has a row");
}

/// Sends `method` with `params` and checks that the answer is the empty
/// response: the status `ok` and the five fields of the envelope.
async fn ok(server: &TestServer, method: &str, params: &[(&str, &str)]) {
    let response = server.get_json(method, params).await;
    assert_eq!(response["status"], "ok", "{method} {params:?}: {response}");
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(5),
        "{method} {params:?}: {response}"
    );
}

/// Sends `method` with `params` and checks that the answer is the error
/// `code` with `message`, and that the envelope holds nothing else.
async fn fails(
    server: &TestServer,
    method: &str,
    params: &[(&str, &str)],
    code: u32,
    message: &str,
) {
    let response = server.get_json(method, params).await;
    assert_eq!(
        response["status"], "failed",
        "{method} {params:?}: {response}"
    );
    assert_eq!(
        response["error"],
        error(code, message),
        "{method} {params:?}"
    );
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(6),
        "{method} {params:?}: {response}"
    );
}

/// The `starred2` object of `getStarred2` with `params`.
async fn starred2(server: &TestServer, params: &[(&str, &str)]) -> Value {
    let mut response = server.get_json("getStarred2", params).await;
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(6),
        "{response}"
    );
    response["starred2"].take()
}

/// The `starred` object of `getStarred` with `params`.
async fn starred(server: &TestServer, params: &[(&str, &str)]) -> Value {
    let mut response = server.get_json("getStarred", params).await;
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(6),
        "{response}"
    );
    response["starred"].take()
}

/// The value of `id` of each element of the list `list`.
fn ids(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap_or_else(|| panic!("{list} is a list"))
        .iter()
        .map(|element| element["id"].as_str().expect("an element has an ID"))
        .collect()
}

/// The object of the lists of `getStarred` and `getStarred2` without a
/// starred item.
fn no_stars() -> Value {
    json!({ "artist": [], "album": [], "song": [] })
}

async fn song(server: &TestServer, id: &str) -> Song {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::track(conn, &id))
        .await
        .expect("read the track")
        .expect("the track exists")
}

async fn album(server: &TestServer, id: &str) -> Album {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::album(conn, &id))
        .await
        .expect("read the album")
        .expect("the album exists")
}

async fn artist(server: &TestServer, id: &str) -> Artist {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| browse::artist(conn, &id))
        .await
        .expect("read the artist")
        .expect("the artist exists")
}

/// Whether `text` is the ISO 8601 form of a time from `from` to `to`, both
/// in Unix seconds, as the models write a time.
fn is_time_between(text: &Value, from: i64, to: i64) -> bool {
    (from..=to).any(|time| *text == model::iso8601(time))
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

/// Sets the tag `key` of an audio file to `text`. The file gets a later
/// modification time than it had, so that the scanner sees the change.
fn retag(path: &Path, key: ItemKey, text: &str) {
    let before = fs::metadata(path).unwrap().modified().unwrap();
    let mut file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let tag = file
        .primary_tag_mut()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()));
    tag.insert_text(key, text.to_owned());
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(before + Duration::from_secs(5)))
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

#[tokio::test]
async fn star_with_id_and_album_id_stars_the_song_and_the_album_and_unstar_clears_them() {
    let server = TestServer::standard().await;
    let low_tide = server.track.low_tide.as_str();
    let glass_harbour = server.album.glass_harbour.as_str();
    assert_eq!(starred2(&server, &[]).await, no_stars());

    let before = unix_now();
    ok(
        &server,
        "star",
        &[("id", low_tide), ("albumId", glass_harbour)],
    )
    .await;
    let after = unix_now();

    let listed = starred2(&server, &[]).await;
    assert_eq!(ids(&listed["song"]), [low_tide]);
    assert_eq!(ids(&listed["album"]), [glass_harbour]);
    assert_eq!(listed["artist"], json!([]));
    for element in [&listed["song"][0], &listed["album"][0]] {
        assert!(
            is_time_between(&element["starred"], before, after),
            "{element}"
        );
    }

    // The elements are those of the model constructors, which `getSong` and
    // `getAlbum` build their answers from: the song and the album show the
    // star there.
    let of_song = annotation(&server, low_tide).await;
    let of_album = annotation(&server, glass_harbour).await;
    assert!((before..=after).contains(&of_song.starred_at.expect("the song has a star")));
    assert_eq!(of_song.starred_at, of_album.starred_at);
    let child = model::child(&song(&server, low_tide).await, Some(&of_song));
    assert_eq!(listed["song"][0], to_json(&child));
    assert_eq!(
        child.starred,
        of_song.starred_at.map(model::iso8601),
        "{child:?}"
    );
    let album_row = album(&server, glass_harbour).await;
    let element = model::album_with_songs(&album_row, Some(&of_album), vec![child]);
    assert_eq!(element.starred, of_album.starred_at.map(model::iso8601));
    assert_eq!(
        listed["album"][0],
        to_json(model::album(&album_row, Some(&of_album)))
    );
    // No other track of the album has a star.
    assert_eq!(
        annotation(&server, &server.track.breakwater).await,
        Annotation::default()
    );

    ok(
        &server,
        "unstar",
        &[("id", low_tide), ("albumId", glass_harbour)],
    )
    .await;
    assert_eq!(starred2(&server, &[]).await, no_stars());
    assert_eq!(annotation(&server, low_tide).await.starred_at, None);
    assert_eq!(annotation(&server, glass_harbour).await.starred_at, None);
    let child = model::child(&song(&server, low_tide).await, None);
    assert_eq!(child.starred, None);
}

#[tokio::test]
async fn id_takes_the_id_of_a_track_an_album_and_an_artist() {
    let server = TestServer::standard().await;
    let track = server.track.meridian.as_str();
    let album = server.album.paper_maps.as_str();
    let artist = server.artist.aurora_field.as_str();

    ok(
        &server,
        "star",
        &[("id", track), ("id", album), ("id", artist)],
    )
    .await;
    let listed = starred2(&server, &[]).await;
    assert_eq!(ids(&listed["song"]), [track]);
    assert_eq!(ids(&listed["album"]), [album]);
    assert_eq!(ids(&listed["artist"]), [artist]);
    let kinds: Vec<(String, String)> = rows(&server)
        .await
        .into_iter()
        .map(|row| (row.1, row.2))
        .collect();
    let mut expected = vec![
        (track.to_owned(), "track".to_owned()),
        (album.to_owned(), "album".to_owned()),
        (artist.to_owned(), "artist".to_owned()),
    ];
    expected.sort();
    assert_eq!(kinds, expected);

    // The star of an album is one star: `albumId` removes the star that
    // `id` set, and `id` removes the star that `artistId` set.
    ok(&server, "unstar", &[("albumId", album)]).await;
    ok(&server, "star", &[("artistId", artist)]).await;
    ok(&server, "unstar", &[("id", artist)]).await;
    let listed = starred2(&server, &[]).await;
    assert_eq!(ids(&listed["song"]), [track]);
    assert_eq!(listed["album"], json!([]));
    assert_eq!(listed["artist"], json!([]));
}

/// Stars the track "Low Tide", its album and its artist at [`STARRED_AT`],
/// and rates the three 5, 4 and 3.
async fn star_and_rate_low_tide(server: &StandardServer) {
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    let artist = server.artist.aurora_field.as_str();
    ok(
        server,
        "star",
        &[("id", track), ("albumId", album), ("artistId", artist)],
    )
    .await;
    for (id, rating) in [(track, "5"), (album, "4"), (artist, "3")] {
        ok(server, "setRating", &[("id", id), ("rating", rating)]).await;
        set_starred_at(server, id, STARRED_AT).await;
    }
}

#[tokio::test]
async fn get_starred2_lists_the_items_in_the_forms_of_the_id3_methods() {
    let server = TestServer::standard().await;
    star_and_rate_low_tide(&server).await;
    let track = server.track.low_tide.as_str();
    let album_id = server.album.glass_harbour.as_str();
    let artist_id = server.artist.aurora_field.as_str();

    let listed = starred2(&server, &[]).await;
    let annotation_of = |rating: u8| Annotation {
        starred_at: Some(STARRED_AT),
        rating: Some(rating),
        ..Annotation::default()
    };
    let expected = json!({
        "artist": [model::artist(&artist(&server, artist_id).await, Some(&annotation_of(3)))],
        "album": [model::album(&album(&server, album_id).await, Some(&annotation_of(4)))],
        "song": [model::child(&song(&server, track).await, Some(&annotation_of(5)))],
    });
    assert_eq!(listed, expected);

    // The values that the elements hold, spelled out.
    assert_eq!(
        listed["artist"][0],
        json!({ "id": artist_id, "name": "Aurora Field", "albumCount": 2, "starred": STARRED })
    );
    let element = &listed["album"][0];
    assert_eq!(
        (&element["id"], &element["name"], &element["artist"]),
        (
            &json!(album_id),
            &json!("Glass Harbour"),
            &json!("Aurora Field")
        )
    );
    assert_eq!(
        (
            &element["songCount"],
            &element["starred"],
            &element["userRating"]
        ),
        (&json!(3), &json!(STARRED), &json!(4))
    );
    assert!(element.get("isDir").is_none(), "{element}");
    let element = &listed["song"][0];
    assert_eq!(
        (&element["id"], &element["title"], &element["isDir"]),
        (&json!(track), &json!("Low Tide"), &json!(false))
    );
    assert_eq!(
        (
            &element["starred"],
            &element["userRating"],
            &element["playCount"]
        ),
        (&json!(STARRED), &json!(5), &json!(0))
    );
    assert_eq!(element["albumId"], album_id);

    // In XML the three lists are child elements of `starred2`, and a time
    // and a rating are attributes.
    let body = server.get_xml("getStarred2", &[]).await;
    let (head, rest) = body
        .split_once("<starred2>")
        .expect("the body holds the element");
    assert_eq!(head, xml_document("ok", ">"));
    let (content, tail) = rest.split_once("</starred2>").expect("the element ends");
    assert_eq!(tail, "</subsonic-response>");
    assert_eq!(content.matches("<album ").count(), 1, "{content}");
    assert_eq!(content.matches("<artist ").count(), 1, "{content}");
    assert_eq!(content.matches("<song ").count(), 1, "{content}");
    assert_eq!(
        content.matches(&format!(r#" starred="{STARRED}""#)).count(),
        3,
        "{content}"
    );
    assert!(
        content.contains(&format!(
            r#"<artist albumCount="2" id="{artist_id}" name="Aurora Field" starred="{STARRED}"/>"#
        )),
        "{content}"
    );
    assert!(content.contains(r#" userRating="4""#), "{content}");
    assert!(content.contains(r#" userRating="5""#), "{content}");
}

#[tokio::test]
async fn get_starred_lists_the_items_in_the_forms_of_the_folder_methods() {
    let server = TestServer::standard().await;
    star_and_rate_low_tide(&server).await;
    let track = server.track.low_tide.as_str();
    let album_id = server.album.glass_harbour.as_str();
    let artist_id = server.artist.aurora_field.as_str();

    let listed = starred(&server, &[]).await;
    let annotation_of = |rating: u8| Annotation {
        starred_at: Some(STARRED_AT),
        rating: Some(rating),
        ..Annotation::default()
    };
    let album_row = album(&server, album_id).await;
    let expected = json!({
        "artist": [model::artist_dir(&artist(&server, artist_id).await, Some(&annotation_of(3)))],
        "album": [model::child_dir_for_album(&album_row, Some(&annotation_of(4)))],
        "song": [model::child(&song(&server, track).await, Some(&annotation_of(5)))],
    });
    assert_eq!(listed, expected);

    // The artist is the `Artist` of the folder methods, which has the
    // rating, and the album is a directory below its artist.
    assert_eq!(
        listed["artist"][0],
        json!({ "id": artist_id, "name": "Aurora Field", "starred": STARRED, "userRating": 3 })
    );
    let element = &listed["album"][0];
    assert_eq!(
        (&element["id"], &element["isDir"], &element["parent"]),
        (&json!(album_id), &json!(true), &json!(artist_id))
    );
    assert_eq!(
        (
            &element["title"],
            &element["starred"],
            &element["userRating"]
        ),
        (&json!("Glass Harbour"), &json!(STARRED), &json!(4))
    );
    assert!(element.get("songCount").is_none(), "{element}");
    // The song is the element that `getStarred2` lists.
    assert_eq!(listed["song"], starred2(&server, &[]).await["song"]);

    let body = server.get_xml("getStarred", &[]).await;
    let (head, rest) = body
        .split_once("<starred>")
        .expect("the body holds the element");
    assert_eq!(head, xml_document("ok", ">"));
    let (content, tail) = rest.split_once("</starred>").expect("the element ends");
    assert_eq!(tail, "</subsonic-response>");
    assert!(
        content.contains(&format!(
            r#"<artist id="{artist_id}" name="Aurora Field" starred="{STARRED}" userRating="3"/>"#
        )),
        "{content}"
    );
    assert_eq!(content.matches(r#" isDir="true""#).count(), 1, "{content}");
    assert_eq!(content.matches(r#" isDir="false""#).count(), 1, "{content}");
}

#[tokio::test]
async fn the_lists_are_empty_without_a_star() {
    let server = TestServer::standard().await;
    // A rating alone is no star.
    ok(
        &server,
        "setRating",
        &[("id", &server.track.low_tide), ("rating", "5")],
    )
    .await;

    assert_eq!(starred2(&server, &[]).await, no_stars());
    assert_eq!(starred(&server, &[]).await, no_stars());
    // An empty list is absent in XML, so the element is empty.
    assert_eq!(
        server.get_xml("getStarred2", &[]).await,
        xml_document("ok", "><starred2/></subsonic-response>")
    );
    assert_eq!(
        server.get_xml("getStarred", &[]).await,
        xml_document("ok", "><starred/></subsonic-response>")
    );
}

#[tokio::test]
async fn the_write_methods_answer_with_the_empty_response_in_xml_and_for_the_view_suffix() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let empty = xml_document("ok", "/>");

    assert_eq!(server.get_xml("star", &[("id", track)]).await, empty);
    assert_eq!(
        server
            .get_xml("setRating.view", &[("id", track), ("rating", "2")])
            .await,
        empty
    );
    let stored = annotation(&server, track).await;
    assert!(stored.starred_at.is_some());
    assert_eq!(stored.rating, Some(2));
    assert_eq!(server.get_xml("unstar.view", &[("id", track)]).await, empty);
    assert_eq!(annotation(&server, track).await.starred_at, None);

    assert_eq!(
        server.get_xml("star", &[("id", NO_TRACK)]).await,
        xml_document(
            "failed",
            r#"><error code="70" message="not found"/></subsonic-response>"#
        )
    );
    assert_eq!(
        server
            .get_xml("setRating", &[("id", track), ("rating", "6")])
            .await,
        xml_document(
            "failed",
            &format!(r#"><error code="0" message="{RATING_RANGE}"/></subsonic-response>"#)
        )
    );
}

#[tokio::test]
async fn a_repeated_star_takes_the_time_of_the_last_request() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let user_id = owner_id(&server).await;

    ok(&server, "star", &[("id", track)]).await;
    // The row gets the star of an earlier time, a rating and a play.
    let id = track.to_owned();
    server
        .db()
        .write(move |conn| {
            conn.execute(
                "UPDATE annotations SET starred_at = 100, rating = 4, play_count = 7,
                   last_played_at = 50
                 WHERE user_id = ?1 AND item_id = ?2",
                params![user_id, id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let before = unix_now();
    ok(&server, "star", &[("id", track)]).await;
    let after = unix_now();

    let stored = annotation(&server, track).await;
    let starred_at = stored.starred_at.expect("the track has a star");
    assert!((before..=after).contains(&starred_at), "{starred_at}");
    assert_eq!(
        stored,
        Annotation {
            starred_at: Some(starred_at),
            rating: Some(4),
            play_count: 7,
            last_played_at: Some(50),
        }
    );
    assert_eq!(rows(&server).await.len(), 1);
    let listed = starred2(&server, &[]).await;
    assert!(is_time_between(
        &listed["song"][0]["starred"],
        before,
        after
    ));
}

#[tokio::test]
async fn unstar_of_an_item_without_a_star_is_no_error() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    let artist = server.artist.aurora_field.as_str();
    let everything = [("id", track), ("albumId", album), ("artistId", artist)];

    // No item has a row.
    ok(&server, "unstar", &everything).await;
    assert_eq!(rows(&server).await, []);

    // An item with a rating and no star keeps its row as it is.
    ok(&server, "setRating", &[("id", album), ("rating", "3")]).await;
    let rated = rows(&server).await;
    ok(&server, "unstar", &everything).await;
    assert_eq!(rows(&server).await, rated);

    // A second `unstar` after a star changes nothing.
    ok(&server, "star", &everything).await;
    ok(&server, "unstar", &everything).await;
    let unstarred = rows(&server).await;
    assert!(unstarred.iter().all(|row| row.3.is_none()), "{unstarred:?}");
    ok(&server, "unstar", &everything).await;
    assert_eq!(rows(&server).await, unstarred);
    assert_eq!(annotation(&server, album).await.rating, Some(3));
    assert_eq!(starred2(&server, &[]).await, no_stars());
}

#[tokio::test]
async fn set_rating_sets_replaces_and_removes_the_rating() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album_id = server.album.glass_harbour.as_str();
    let artist_id = server.artist.aurora_field.as_str();
    ok(
        &server,
        "star",
        &[
            ("id", track),
            ("albumId", album_id),
            ("artistId", artist_id),
        ],
    )
    .await;

    ok(&server, "setRating", &[("id", album_id), ("rating", "4")]).await;
    let listed = starred2(&server, &[]).await;
    assert_eq!(listed["album"][0]["userRating"], 4);
    assert!(listed["song"][0].get("userRating").is_none());
    // The album shows the rating in both of its forms, as `getAlbum` and
    // `getMusicDirectory` build them.
    let stored = annotation(&server, album_id).await;
    let album_row = album(&server, album_id).await;
    assert_eq!(model::album(&album_row, Some(&stored)).user_rating, Some(4));
    let directory = model::child_dir_for_album(&album_row, Some(&stored));
    assert_eq!(directory.user_rating, Some(4));
    assert_eq!(starred(&server, &[]).await["album"][0]["userRating"], 4);

    // Every value from 1 to 5 is stored, for each kind of item.
    for (id, list) in [(track, "song"), (album_id, "album"), (artist_id, "artist")] {
        for rating in 1..=5_u8 {
            let text = rating.to_string();
            ok(&server, "setRating", &[("id", id), ("rating", &text)]).await;
            assert_eq!(annotation(&server, id).await.rating, Some(rating));
            assert_eq!(starred(&server, &[]).await[list][0]["userRating"], rating);
        }
    }

    // 0 removes the rating and keeps the star.
    for (id, list) in [(track, "song"), (album_id, "album"), (artist_id, "artist")] {
        ok(&server, "setRating", &[("id", id), ("rating", "0")]).await;
        let stored = annotation(&server, id).await;
        assert_eq!(stored.rating, None);
        assert!(stored.starred_at.is_some());
        let element = &starred(&server, &[]).await[list][0];
        assert_eq!(element["id"], id);
        assert!(element.get("userRating").is_none(), "{element}");
        // A second removal is no error.
        ok(&server, "setRating", &[("id", id), ("rating", "0")]).await;
    }
    assert!(
        starred2(&server, &[]).await["album"][0]
            .get("userRating")
            .is_none()
    );

    // A removal for an item without a row writes none.
    let other = server.track.breakwater.as_str();
    ok(&server, "setRating", &[("id", other), ("rating", "0")]).await;
    assert_eq!(rows(&server).await.len(), 3);

    // `unstar` keeps a rating.
    ok(&server, "setRating", &[("id", track), ("rating", "2")]).await;
    ok(&server, "unstar", &[("id", track)]).await;
    assert_eq!(
        annotation(&server, track).await,
        Annotation {
            rating: Some(2),
            ..Annotation::default()
        }
    );
}

#[tokio::test]
async fn a_rating_outside_0_to_5_is_error_0_and_nothing_is_written() {
    let server = TestServer::standard().await;
    let album = server.album.glass_harbour.as_str();
    ok(&server, "setRating", &[("id", album), ("rating", "3")]).await;
    let before = rows(&server).await;

    for rating in ["6", "-1", "10", "255", "256", "261", "9223372036854775807"] {
        let params = [("id", album), ("rating", rating)];
        fails(&server, "setRating", &params, 0, RATING_RANGE).await;
        assert_eq!(rows(&server).await, before, "{rating}");
    }
    for rating in ["five", "4.5", "4 ", " 4", "0x4", "9223372036854775808", "٤"] {
        let params = [("id", album), ("rating", rating)];
        fails(&server, "setRating", &params, 0, RATING_INTEGER).await;
        assert_eq!(rows(&server).await, before, "{rating}");
    }
    // The rating is checked before the item, so the answer for an unknown
    // item with such a rating is the same.
    let params = [("id", NO_ALBUM), ("rating", "6")];
    fails(&server, "setRating", &params, 0, RATING_RANGE).await;
    assert_eq!(annotation(&server, album).await.rating, Some(3));
}

#[tokio::test]
async fn set_rating_needs_id_and_rating() {
    let server = TestServer::standard().await;
    let album = server.album.glass_harbour.as_str();
    let missing = |name: &str| format!("required parameter is missing: {name}");

    fails(&server, "setRating", &[], 10, &missing("id")).await;
    fails(&server, "setRating", &[("rating", "4")], 10, &missing("id")).await;
    fails(
        &server,
        "setRating",
        &[("id", album)],
        10,
        &missing("rating"),
    )
    .await;
    let empty = [("id", album), ("rating", "")];
    fails(&server, "setRating", &empty, 10, &missing("rating")).await;
    assert_eq!(rows(&server).await, []);

    // The first `id` is the item. `setRating` takes one.
    let other = server.album.paper_maps.as_str();
    let two = [("id", album), ("id", other), ("rating", "4")];
    ok(&server, "setRating", &two).await;
    assert_eq!(annotation(&server, album).await.rating, Some(4));
    assert_eq!(annotation(&server, other).await.rating, None);
}

#[tokio::test]
async fn set_rating_for_an_id_of_no_item_is_error_70() {
    let server = TestServer::standard().await;
    let album = server.album.glass_harbour.as_str();
    ok(&server, "setRating", &[("id", album), ("rating", "3")]).await;
    let before = rows(&server).await;

    let upper_case = album.to_uppercase();
    for id in [
        NO_TRACK,
        NO_ALBUM,
        NO_ARTIST,
        PLAYLIST,
        "",
        "1",
        "glass harbour",
        upper_case.as_str(),
    ] {
        for rating in ["0", "1", "5"] {
            let params = [("id", id), ("rating", rating)];
            fails(&server, "setRating", &params, 70, NOT_FOUND).await;
            assert_eq!(rows(&server).await, before, "{id:?} {rating}");
        }
    }
}

#[tokio::test]
async fn several_ids_of_one_request_are_written_together() {
    let server = TestServer::standard().await;
    let request = [
        ("id", server.track.low_tide.as_str()),
        ("albumId", server.album.glass_harbour.as_str()),
        ("artistId", server.artist.aurora_field.as_str()),
        ("id", server.track.upstream.as_str()),
        ("albumId", server.album.slow_rivers.as_str()),
        ("artistId", server.artist.marlow_quartet.as_str()),
        // An item that the request names twice is one row.
        ("id", server.track.low_tide.as_str()),
        ("id", server.album.glass_harbour.as_str()),
    ];

    let before = unix_now();
    ok(&server, "star", &request).await;
    let after = unix_now();

    let stored = rows(&server).await;
    assert_eq!(stored.len(), 6, "{stored:?}");
    // One write gave every item the same time.
    let starred_at = stored[0].3.expect("the first row has a star");
    assert!((before..=after).contains(&starred_at));
    assert!(
        stored.iter().all(|row| row.3 == Some(starred_at)),
        "{stored:?}"
    );
    let listed = starred2(&server, &[]).await;
    assert_eq!(
        (
            ids(&listed["artist"]).len(),
            ids(&listed["album"]).len(),
            ids(&listed["song"]).len()
        ),
        (2, 2, 2)
    );

    ok(&server, "unstar", &request).await;
    assert_eq!(starred2(&server, &[]).await, no_stars());
    assert_eq!(rows(&server).await.len(), 6);
}

#[tokio::test]
async fn an_id_of_no_item_is_error_70_and_no_item_of_the_request_is_written() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    let artist = server.artist.aurora_field.as_str();
    let other_track = server.track.upstream.as_str();
    let other_album = server.album.slow_rivers.as_str();

    // One item has a star and one a rating before the requests.
    ok(&server, "star", &[("id", other_track)]).await;
    ok(
        &server,
        "setRating",
        &[("id", other_album), ("rating", "4")],
    )
    .await;
    set_starred_at(&server, other_track, STARRED_AT).await;
    let before = rows(&server).await;
    assert_eq!(before.len(), 2);

    // A value that names no item of the kind its parameter takes.
    let bad: [(&str, &str); 14] = [
        ("id", NO_TRACK),
        ("id", NO_ALBUM),
        ("id", NO_ARTIST),
        ("id", PLAYLIST),
        ("id", ""),
        ("id", "low tide"),
        ("albumId", NO_ALBUM),
        ("albumId", track),
        ("albumId", artist),
        ("albumId", ""),
        ("artistId", NO_ARTIST),
        ("artistId", album),
        ("artistId", track),
        ("artistId", "1"),
    ];
    // The valid items of the request: two without a row, and the two that
    // have one.
    let good = [
        ("id", track),
        ("albumId", album),
        ("artistId", artist),
        ("id", other_track),
        ("albumId", other_album),
    ];
    for bad in bad {
        // The value is the first, a middle and the last of the request.
        for place in [0, 2, good.len()] {
            let mut request = good.to_vec();
            request.insert(place, bad);
            for method in ["star", "unstar"] {
                fails(&server, method, &request, 70, NOT_FOUND).await;
                assert_eq!(rows(&server).await, before, "{method} {bad:?} at {place}");
            }
        }
        // The value alone.
        for method in ["star", "unstar"] {
            fails(&server, method, &[bad], 70, NOT_FOUND).await;
        }
    }
    assert_eq!(rows(&server).await, before);
    assert_eq!(ids(&starred2(&server, &[]).await["song"]), [other_track]);

    // The same items without the value are written.
    ok(&server, "star", &good).await;
    assert_eq!(rows(&server).await.len(), 5);
}

#[tokio::test]
async fn star_and_unstar_without_an_id_succeed_and_write_nothing() {
    let server = TestServer::standard().await;
    ok(&server, "star", &[]).await;
    ok(&server, "unstar", &[]).await;
    // A parameter that the methods do not take names no item.
    ok(&server, "star", &[("songId", &server.track.low_tide)]).await;
    assert_eq!(rows(&server).await, []);
}

#[tokio::test]
async fn star_takes_the_ids_of_the_query_string_and_of_a_form_body() {
    let server = TestServer::standard().await;
    let low_tide = server.track.low_tide.as_str();
    let breakwater = server.track.breakwater.as_str();
    let album = server.album.glass_harbour.as_str();

    let path = format!("/rest/star?id={low_tide}&f=json");
    let form = [
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
        ("id", breakwater),
        ("albumId", album),
    ];
    let response = server.request(form_request(&path, &form)).await;
    assert_eq!(content_type(&response), "application/json");
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["status"], "ok", "{response}");

    let listed = starred2(&server, &[]).await;
    let mut songs = ids(&listed["song"]);
    songs.sort_unstable();
    let mut expected = [low_tide, breakwater];
    expected.sort_unstable();
    assert_eq!(songs, expected);
    assert_eq!(ids(&listed["album"]), [album]);

    // A form body with an unknown ID fails as a query string does, and the
    // ID of the query string is not written either.
    let path = format!("/rest/unstar?id={low_tide}&f=json");
    let form = [
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
        ("id", NO_TRACK),
    ];
    let response = server.request(form_request(&path, &form)).await;
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["error"], error(70, NOT_FOUND));
    assert_eq!(ids(&starred2(&server, &[]).await["song"]).len(), 2);
}

#[tokio::test]
async fn a_star_and_a_rating_belong_to_the_user_of_the_request() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    server
        .db()
        .write(|conn| {
            conn.execute(
                "INSERT INTO users (name, role, password, created_at, updated_at)
                 VALUES ('second', 'guest', 'walnut', 1, 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let second = [
        ("u", "second"),
        ("p", "walnut"),
        ("v", API_VERSION),
        ("c", CLIENT),
    ];
    let as_second = |extra: &[(&'static str, &str)]| {
        let mut params: Vec<(&str, String)> = second
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect();
        params.extend(
            extra
                .iter()
                .map(|(name, value)| (*name, (*value).to_owned())),
        );
        params
    };
    let send = |method: &'static str, params: Vec<(&'static str, String)>| {
        let server = &server;
        async move {
            let params: Vec<(&str, &str)> = params
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            server.get_json_noauth(method, &params).await
        }
    };

    // The second user stars the track and rates the album.
    let response = send("star", as_second(&[("id", track)])).await;
    assert_eq!(response["status"], "ok", "{response}");
    let response = send("setRating", as_second(&[("id", album), ("rating", "5")])).await;
    assert_eq!(response["status"], "ok", "{response}");

    // The owner has no star and no rating, and its requests do not change
    // those of the second user.
    assert_eq!(starred2(&server, &[]).await, no_stars());
    assert_eq!(annotation(&server, track).await, Annotation::default());
    assert_eq!(annotation(&server, album).await, Annotation::default());
    ok(&server, "unstar", &[("id", track)]).await;
    ok(&server, "setRating", &[("id", album), ("rating", "1")]).await;
    ok(&server, "setRating", &[("id", album), ("rating", "0")]).await;
    ok(&server, "star", &[("albumId", album)]).await;

    let response = send("getStarred2", as_second(&[])).await;
    assert_eq!(ids(&response["starred2"]["song"]), [track]);
    assert_eq!(response["starred2"]["album"], json!([]));
    let response = send("getStarred", as_second(&[])).await;
    assert_eq!(ids(&response["starred"]["song"]), [track]);

    let owner = owner_id(&server).await;
    let stored = rows(&server).await;
    let of_second: Vec<_> = stored.iter().filter(|row| row.0 != owner).collect();
    assert_eq!(of_second.len(), 2, "{stored:?}");
    assert!(
        of_second
            .iter()
            .any(|row| row.1 == track && row.3.is_some() && row.4.is_none()),
        "{stored:?}"
    );
    assert!(
        of_second
            .iter()
            .any(|row| row.1 == album && row.3.is_none() && row.4 == Some(5)),
        "{stored:?}"
    );
    let listed = starred2(&server, &[]).await;
    assert_eq!(ids(&listed["album"]), [album]);
    assert!(listed["album"][0].get("userRating").is_none());
}

#[tokio::test]
async fn a_request_without_a_user_writes_nothing() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let version = ("v", API_VERSION);
    let client = ("c", CLIENT);

    for method in ["star", "unstar", "setRating"] {
        let params = [version, client, ("id", track), ("rating", "5")];
        let response = server.get_json_noauth(method, &params).await;
        assert_eq!(response["error"]["code"], 10, "{method}: {response}");

        let params = [
            version,
            client,
            ("u", OWNER),
            ("p", "wrong"),
            ("id", track),
            ("rating", "5"),
        ];
        let response = server.get_json_noauth(method, &params).await;
        assert_eq!(response["error"]["code"], 40, "{method}: {response}");
    }
    assert_eq!(rows(&server).await, []);
    for method in ["getStarred", "getStarred2"] {
        let response = server.get_json_noauth(method, &[version, client]).await;
        assert_eq!(response["error"]["code"], 10, "{method}: {response}");
        assert!(response.get("starred").is_none() && response.get("starred2").is_none());
    }
}

#[tokio::test]
async fn stars_and_ratings_survive_a_rescan_a_renamed_file_and_a_moved_file() {
    let server = TestServer::standard_with_own_files().await;
    star_and_rate_low_tide(&server).await;
    let low_tide = server.track.low_tide.as_str();
    // A second track of the album has a star of an earlier time, so it is
    // the second song of the lists.
    let breakwater = server.track.breakwater.as_str();
    ok(&server, "star", &[("id", breakwater)]).await;
    set_starred_at(&server, breakwater, STARRED_AT - 1).await;

    let stored = rows(&server).await;
    assert_eq!(stored.len(), 4);
    let mut listed2 = starred2(&server, &[]).await;
    let mut listed = starred(&server, &[]).await;
    assert_eq!(ids(&listed2["song"]), [low_tide, breakwater]);
    assert_eq!(
        listed2["song"][0]["path"],
        "Aurora Field/Glass Harbour/01 Low Tide.flac"
    );
    assert_eq!(
        listed2["song"][1]["path"],
        "Aurora Field/Glass Harbour/02 Breakwater.flac"
    );

    // A scan that finds nothing changed.
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.moved, summary.changed), (10, 0, 0));
    assert_eq!(rows(&server).await, stored);
    assert_eq!(starred2(&server, &[]).await, listed2);
    assert_eq!(starred(&server, &[]).await, listed);

    // The file of one starred track is renamed, and the file of the other
    // moves to another directory.
    let moves = [
        (
            0,
            "Aurora Field/Glass Harbour/01 Low Tide.flac",
            "Aurora Field/Glass Harbour/01 High Tide.flac",
        ),
        (
            1,
            "Aurora Field/Glass Harbour/02 Breakwater.flac",
            "Moved/Elsewhere/Breakwater.flac",
        ),
    ];
    fs::create_dir_all(server.path("Moved/Elsewhere")).unwrap();
    for (place, from, to) in moves {
        fs::rename(server.path(from), server.path(to)).unwrap();
        let summary = server.rescan().await;
        assert_eq!(summary.errors, [] as [&str; 0], "{to}");
        assert_eq!(
            (summary.seen, summary.moved, summary.added, summary.removed),
            (10, 1, 0, 0),
            "{to}"
        );

        // The annotations are the rows they were, and the lists hold the
        // same elements. The song has its new path.
        assert_eq!(rows(&server).await, stored, "{to}");
        listed2["song"][place]["path"] = json!(to);
        listed["song"][place]["path"] = json!(to);
        assert_eq!(starred2(&server, &[]).await, listed2, "{to}");
        assert_eq!(starred(&server, &[]).await, listed, "{to}");
    }
    assert_eq!(
        annotation(&server, low_tide).await,
        Annotation {
            starred_at: Some(STARRED_AT),
            rating: Some(5),
            ..Annotation::default()
        }
    );
    let song = &listed2["song"][0];
    assert_eq!(
        (&song["id"], &song["starred"], &song["userRating"]),
        (&json!(low_tide), &json!(STARRED), &json!(5))
    );
    let album = &listed2["album"][0];
    assert_eq!(
        (&album["starred"], &album["userRating"]),
        (&json!(STARRED), &json!(4))
    );
    assert_eq!(listed["artist"][0]["userRating"], 3);

    // The moved track takes a new star at its new path.
    ok(&server, "unstar", &[("id", breakwater)]).await;
    assert_eq!(ids(&starred2(&server, &[]).await["song"]), [low_tide]);
    ok(&server, "star", &[("id", breakwater)]).await;
    assert_eq!(
        ids(&starred2(&server, &[]).await["song"]),
        [breakwater, low_tide]
    );
}

#[tokio::test]
async fn stars_and_ratings_survive_a_change_of_the_display_names() {
    let library = LibraryBuilder::new()
        .track("aurora field", "glass harbour", "Low Tide", 1, "flac")
        .track("aurora field", "glass harbour", "Breakwater", 2, "flac")
        .track("aurora field", "paper maps", "Meridian", 1, "mp3")
        .build();
    let server = TestServer::scanned(library).await;
    let catalogue = server.catalogue().await;
    let artist_id = catalogue.artist("aurora field").id.clone();
    let album_id = catalogue.album("glass harbour").id.clone();
    let track_id = catalogue.song("Low Tide").track.id.clone();

    ok(
        &server,
        "star",
        &[
            ("id", &track_id),
            ("albumId", &album_id),
            ("artistId", &artist_id),
        ],
    )
    .await;
    for (id, rating) in [(&track_id, "5"), (&album_id, "4"), (&artist_id, "3")] {
        ok(&server, "setRating", &[("id", id), ("rating", rating)]).await;
        set_starred_at(&server, id, STARRED_AT).await;
    }
    let stored = rows(&server).await;
    let listed = starred2(&server, &[]).await;
    assert_eq!(listed["album"][0]["name"], "glass harbour");
    assert_eq!(listed["artist"][0]["name"], "aurora field");

    // Every file gets another spelling of the artist, and the files of the
    // album another spelling of its title.
    let files = server.files().to_vec();
    for file in &files {
        retag(file, ItemKey::TrackArtist, "Aurora Field");
    }
    for file in &files[..2] {
        retag(file, ItemKey::AlbumTitle, "Glass Harbour");
    }
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.changed), (3, 3));

    // The rows of the album and of the artist have their IDs and the new
    // names, and the annotations are the rows they were.
    assert_eq!(rows(&server).await, stored);
    let after = starred2(&server, &[]).await;
    assert_eq!(
        after["artist"],
        json!([{ "id": artist_id, "name": "Aurora Field", "albumCount": 2, "starred": STARRED }])
    );
    let element = &after["album"][0];
    assert_eq!(
        (&element["id"], &element["name"], &element["artist"]),
        (
            &json!(album_id),
            &json!("Glass Harbour"),
            &json!("Aurora Field")
        )
    );
    assert_eq!(
        (&element["starred"], &element["userRating"]),
        (&json!(STARRED), &json!(4))
    );
    let element = &after["song"][0];
    assert_eq!(
        (&element["id"], &element["album"], &element["artist"]),
        (
            &json!(track_id),
            &json!("Glass Harbour"),
            &json!("Aurora Field")
        )
    );
    assert_eq!(
        (&element["starred"], &element["userRating"]),
        (&json!(STARRED), &json!(5))
    );
    assert_eq!(
        starred(&server, &[]).await["artist"],
        json!([{
            "id": artist_id, "name": "Aurora Field", "starred": STARRED, "userRating": 3
        }])
    );
}

#[tokio::test]
async fn a_track_without_a_file_is_starred_rated_and_listed() {
    let server = TestServer::standard().await;
    let starred_before = server.track.upstream.as_str();
    let starred_after = server.track.delta.as_str();
    let folder = server.folder_id.to_string();

    // One track has its star before it loses its file, and one gets it
    // afterwards.
    ok(&server, "star", &[("id", starred_before)]).await;
    remove_file(&server, starred_before).await;
    remove_file(&server, starred_after).await;
    ok(&server, "star", &[("id", starred_after)]).await;
    ok(
        &server,
        "setRating",
        &[("id", starred_after), ("rating", "4")],
    )
    .await;
    set_starred_at(&server, starred_before, STARRED_AT + 1).await;
    set_starred_at(&server, starred_after, STARRED_AT).await;

    for listed in [starred2(&server, &[]).await, starred(&server, &[]).await] {
        assert_eq!(ids(&listed["song"]), [starred_before, starred_after]);
        for element in listed["song"].as_array().unwrap() {
            // The element has no path, and the other values of the track.
            assert!(element.get("path").is_none(), "{element}");
            assert_eq!(element["albumId"], server.album.slow_rivers.as_str());
            assert_eq!(element["suffix"], "m4a");
        }
        assert_eq!(listed["song"][1]["userRating"], 4);
        assert_eq!(listed["song"][1]["starred"], STARRED);
    }
    let row = song(&server, starred_after).await;
    assert!(!row.track.has_file());
    let stored = annotation(&server, starred_after).await;
    assert_eq!(
        starred2(&server, &[]).await["song"][1],
        to_json(model::child(&row, Some(&stored)))
    );

    // A track without a file is in no music folder.
    let in_folder = starred2(&server, &[("musicFolderId", &folder)]).await;
    assert_eq!(in_folder, no_stars());

    ok(&server, "unstar", &[("id", starred_after)]).await;
    ok(
        &server,
        "setRating",
        &[("id", starred_after), ("rating", "0")],
    )
    .await;
    assert_eq!(
        annotation(&server, starred_after).await,
        Annotation::default()
    );
    assert_eq!(ids(&starred2(&server, &[]).await["song"]), [starred_before]);
}

#[tokio::test]
async fn music_folder_id_limits_the_lists_to_the_items_of_one_folder() {
    let server = TestServer::standard().await;
    // A second music folder holds the two tracks of "Slow Rivers".
    let slow_rivers = server.album.slow_rivers.clone();
    let second = server
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO folders (path, name) VALUES ('/music/second', 'second')",
                [],
            )?;
            let second = conn.last_insert_rowid();
            conn.execute(
                "UPDATE tracks SET folder_id = ?1 WHERE album_id = ?2",
                params![second, slow_rivers],
            )?;
            Ok(second)
        })
        .await
        .unwrap();
    let first = server.folder_id.to_string();
    let second = second.to_string();

    let aurora_field = server.artist.aurora_field.as_str();
    let marlow_quartet = server.artist.marlow_quartet.as_str();
    let glass_harbour = server.album.glass_harbour.as_str();
    let slow_rivers = server.album.slow_rivers.as_str();
    let low_tide = server.track.low_tide.as_str();
    let upstream = server.track.upstream.as_str();
    ok(
        &server,
        "star",
        &[
            ("artistId", aurora_field),
            ("artistId", marlow_quartet),
            ("albumId", glass_harbour),
            ("albumId", slow_rivers),
            ("id", low_tide),
            ("id", upstream),
        ],
    )
    .await;

    for (method, key) in [("getStarred2", "starred2"), ("getStarred", "starred")] {
        let lists = |folder: Option<&str>| {
            let server = &server;
            let params: Vec<(&str, String)> = folder
                .map(|folder| ("musicFolderId", folder.to_owned()))
                .into_iter()
                .collect();
            async move {
                let params: Vec<(&str, &str)> = params
                    .iter()
                    .map(|(name, value)| (*name, value.as_str()))
                    .collect();
                let response = server.get_json(method, &params).await;
                assert_eq!(response["status"], "ok", "{method}: {response}");
                let listed = &response[key];
                let sorted = |list: &Value| {
                    let mut list: Vec<String> = ids(list).into_iter().map(str::to_owned).collect();
                    list.sort();
                    list
                };
                (
                    sorted(&listed["artist"]),
                    sorted(&listed["album"]),
                    sorted(&listed["song"]),
                )
            }
        };
        let sorted = |ids: &[&str]| {
            let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
            ids.sort();
            ids
        };

        let everything = (
            sorted(&[aurora_field, marlow_quartet]),
            sorted(&[glass_harbour, slow_rivers]),
            sorted(&[low_tide, upstream]),
        );
        assert_eq!(lists(None).await, everything, "{method}");
        // An empty value is no filter.
        assert_eq!(lists(Some("")).await, everything, "{method}");
        assert_eq!(
            lists(Some(&first)).await,
            (
                sorted(&[aurora_field]),
                sorted(&[glass_harbour]),
                sorted(&[low_tide])
            ),
            "{method}"
        );
        assert_eq!(
            lists(Some(&second)).await,
            (
                sorted(&[marlow_quartet]),
                sorted(&[slow_rivers]),
                sorted(&[upstream])
            ),
            "{method}"
        );
        // No item is in a folder that does not exist.
        for folder in ["0", "-1", "999999"] {
            assert_eq!(
                lists(Some(folder)).await,
                (Vec::new(), Vec::new(), Vec::new()),
                "{method} {folder}"
            );
        }
        let not_an_integer = "parameter musicFolderId must be an integer";
        for folder in ["first", "1.5", &format!("{first} ")] {
            let params = [("musicFolderId", folder)];
            fails(&server, method, &params, 0, not_an_integer).await;
        }
    }
}

#[tokio::test]
async fn the_lists_have_the_item_that_was_starred_last_as_the_first() {
    let server = TestServer::standard().await;
    let tracks = [
        server.track.low_tide.as_str(),
        server.track.breakwater.as_str(),
        server.track.night_ferry.as_str(),
        server.track.meridian.as_str(),
    ];
    let albums = [
        server.album.glass_harbour.as_str(),
        server.album.paper_maps.as_str(),
        server.album.slow_rivers.as_str(),
    ];
    let artists = [
        server.artist.aurora_field.as_str(),
        server.artist.marlow_quartet.as_str(),
        server.artist.various_artists.as_str(),
    ];
    for id in tracks.iter().chain(&albums).chain(&artists) {
        ok(&server, "star", &[("id", id)]).await;
    }
    // The times are not in the order of the requests.
    for (id, at) in [
        (tracks[0], 300),
        (tracks[1], 100),
        (tracks[2], 400),
        (tracks[3], 200),
        (albums[0], 100),
        (albums[1], 300),
        (albums[2], 200),
        (artists[0], 200),
        (artists[1], 100),
        (artists[2], 300),
    ] {
        set_starred_at(&server, id, at).await;
    }

    for listed in [starred2(&server, &[]).await, starred(&server, &[]).await] {
        assert_eq!(
            ids(&listed["song"]),
            [tracks[2], tracks[0], tracks[3], tracks[1]]
        );
        assert_eq!(ids(&listed["album"]), [albums[1], albums[2], albums[0]]);
        assert_eq!(ids(&listed["artist"]), [artists[2], artists[0], artists[1]]);
    }

    // A new star puts an item at the front.
    ok(&server, "star", &[("id", tracks[1])]).await;
    assert_eq!(
        ids(&starred2(&server, &[]).await["song"]),
        [tracks[1], tracks[2], tracks[0], tracks[3]]
    );

    // Items with the same time are in the order of their IDs.
    for id in tracks {
        set_starred_at(&server, id, 500).await;
    }
    let mut by_id = tracks;
    by_id.sort_unstable();
    assert_eq!(ids(&starred2(&server, &[]).await["song"]), by_id);
}

#[tokio::test]
async fn a_star_of_an_item_that_is_gone_is_not_listed() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    ok(&server, "star", &[("id", track)]).await;
    let user_id = owner_id(&server).await;
    // The table holds a star for an ID of each kind that no row has, as it
    // does between the deletion of an item and the end of the scan.
    server
        .db()
        .write(move |conn| {
            for (item_id, item_type) in [
                (NO_TRACK, "track"),
                (NO_ALBUM, "album"),
                (NO_ARTIST, "artist"),
            ] {
                conn.execute(
                    "INSERT INTO annotations (user_id, item_id, item_type, starred_at)
                     VALUES (?1, ?2, ?3, 100)",
                    params![user_id, item_id, item_type],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    for listed in [starred2(&server, &[]).await, starred(&server, &[]).await] {
        assert_eq!(ids(&listed["song"]), [track]);
        assert_eq!(listed["album"], json!([]));
        assert_eq!(listed["artist"], json!([]));
    }
}
