//! Router tests of the play queue methods `getPlayQueue` and `savePlayQueue`
//! (`docs/design.md` section 4).
//!
//! A test saves a queue through `savePlayQueue` and reads it back through
//! `getPlayQueue` and from the rows of `play_queue` and `play_queue_tracks`.
//! The songs of a queue are compared with the `song` element that `getSong`
//! returns for the same ID, which `tests/browse.rs` compares with the
//! library.

mod common;

use std::fs;

use axum::body::Body;
use axum::http::{Request, header};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use wn_server::api::model;
use wn_server::config::Config;
use wn_server::db::queries::play_queue::MAX_ENTRIES;

use common::{
    API_VERSION, CLIENT, OWNER, OWNER_PASSWORD, ScannedServer, TestServer, as_user, body_string,
    content_type, entry_xml, failed, get_song, guest, ok, ok_empty, post_body, remove_file,
    subsonic_response, unix_now, xml_failed, xml_ok, xml_ok_empty,
};

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of the error for a `current` that is none of the `id` values.
const NO_CURRENT: &str = "parameter current must be one of the values of id";

/// The message of the error for a `position` that is no position.
const NO_POSITION: &str = "parameter position must be an integer from 0 to 9223372036854775807";

/// The message of the error for a queue that would be too long.
const TOO_LONG: &str = "a play queue holds at most 100000 songs";

/// The ID of a track that no library has.
const UNKNOWN_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";

/// 2023-03-14T17:51:22Z.
const CHANGED_AT: i64 = 1_678_816_282;

/// The parameters of a `savePlayQueue` request with `songs` as its `id`
/// values, `current` and `extra`.
fn save_request<'a>(
    songs: &[&'a str],
    current: &'a str,
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut request: Vec<(&str, &str)> = songs.iter().map(|id| ("id", *id)).collect();
    request.push(("current", current));
    request.extend_from_slice(extra);
    request
}

/// Saves a queue of `songs` with the current song `current` and the
/// parameters `extra` through `savePlayQueue`, and checks that the request
/// succeeds.
async fn save(server: &TestServer, songs: &[&str], current: &str, extra: &[(&str, &str)]) {
    let request = save_request(songs, current, extra);
    let response = server.get_json("savePlayQueue", &request).await;
    assert_eq!(response, ok_empty(), "{request:?}");
}

/// Saves a queue of `songs` without a current song and with the parameters
/// `extra` through `savePlayQueue`, and checks that the request succeeds.
async fn save_without_current(server: &TestServer, songs: &[&str], extra: &[(&str, &str)]) {
    let mut request: Vec<(&str, &str)> = songs.iter().map(|id| ("id", *id)).collect();
    request.extend_from_slice(extra);
    let response = server.get_json("savePlayQueue", &request).await;
    assert_eq!(response, ok_empty(), "{request:?}");
}

/// The `playQueue` element that `getPlayQueue` returns.
async fn read_queue(server: &TestServer) -> Value {
    let mut response = server.get_json("getPlayQueue", &[]).await;
    assert_eq!(response["status"], "ok", "{response}");
    response["playQueue"].take()
}

/// The IDs of the songs of `queue`, a `playQueue` element, in the order of
/// its `entry` list.
fn entry_ids(queue: &Value) -> Vec<String> {
    queue["entry"]
        .as_array()
        .expect("an entry list")
        .iter()
        .map(|entry| entry["id"].as_str().expect("an ID").to_owned())
        .collect()
}

/// The `current` value that `getPlayQueue` returns.
async fn queue_current(server: &TestServer) -> String {
    let queue = read_queue(server).await;
    queue["current"]
        .as_str()
        .expect("a current song")
        .to_owned()
}

/// `ids` as owned texts.
fn list_of(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

/// The track ID of each row of `play_queue_tracks` of the user `name`, in
/// the order of the stored positions.
async fn stored(server: &TestServer, name: &'static str) -> Vec<String> {
    server
        .db()
        .read(move |conn| {
            let mut select = conn.prepare(
                "SELECT track_id FROM play_queue_tracks
                 JOIN users ON users.id = play_queue_tracks.user_id
                 WHERE users.name = ?1 ORDER BY position",
            )?;
            let rows = select
                .query_map([name], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await
        .expect("read the entries")
}

/// A `play_queue` row: the current song, the position in milliseconds, the
/// change time and the client.
type Head = (Option<String>, i64, i64, String);

/// The `play_queue` row of the owner. `None` when the owner has no row.
async fn head(server: &TestServer) -> Option<Head> {
    server
        .db()
        .read(|conn| {
            let head = conn
                .query_row(
                    "SELECT current_id, position_ms, changed_at, changed_by FROM play_queue
                     JOIN users ON users.id = play_queue.user_id WHERE users.name = ?1",
                    [OWNER],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            Ok(head)
        })
        .await
        .expect("read the queue row")
}

/// Everything that the server stores and answers for the queue of the
/// owner: the row, the entries and the `getPlayQueue` document. Two equal
/// values are one state of the queue.
async fn state(server: &TestServer) -> (Option<Head>, Vec<String>, Value) {
    (
        head(server).await,
        stored(server, OWNER).await,
        server.get_json("getPlayQueue", &[]).await,
    )
}

/// Sets the change time of the queue of the owner to [`CHANGED_AT`], which
/// is in the past.
async fn set_changed(server: &TestServer) {
    let changed = server
        .db()
        .write(|conn| {
            Ok(conn.execute(
                "UPDATE play_queue SET changed_at = ?1
                 WHERE user_id = (SELECT id FROM users WHERE name = ?2)",
                params![CHANGED_AT, OWNER],
            )?)
        })
        .await
        .expect("set the time");
    assert_eq!(changed, 1);
}

/// The absolute path of the file of the track `id`.
async fn file_of(server: &ScannedServer, id: &str) -> std::path::PathBuf {
    let catalogue = server.catalogue().await;
    let row = catalogue.songs.iter().find(|row| row.track.id == id);
    server.path(&row.expect("a track of the library").track.rel_path)
}

/// Deletes the file of the track `id` and scans. The scan deletes the track
/// row.
async fn delete_song(server: &ScannedServer, id: &str) {
    fs::remove_file(file_of(server, id).await).expect("delete the file");
    let summary = server.rescan().await;
    assert_eq!(summary.removed, 1);
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!(
        server.get_json("getSong", &[("id", id)]).await,
        failed(70, NOT_FOUND)
    );
}

/// Inserts a second account with the name `name` and the password
/// `password`. The server has no method that does so.
async fn add_user(server: &TestServer, name: &'static str, password: &'static str) {
    server
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO users (name, role, password, created_at, updated_at)
                 VALUES (?1, 'guest', ?2, ?3, ?3)",
                params![name, password, CHANGED_AT],
            )?;
            Ok(())
        })
        .await
        .expect("insert the user");
}

#[tokio::test]
async fn save_three_songs_then_get_returns_them_in_order_with_current_and_position() {
    let server = TestServer::standard().await;
    // The order of the request is no order of the library.
    let songs = [
        server.track.upstream.as_str(),
        server.track.low_tide.as_str(),
        server.track.meridian.as_str(),
    ];

    let before = unix_now();
    let request = save_request(&songs, songs[1], &[("position", "12345")]);
    let saved = server.get_json("savePlayQueue", &request).await;
    let after = unix_now();
    // `savePlayQueue` answers with the empty response.
    assert_eq!(saved, ok_empty());

    // The row holds the current song, the position in milliseconds, the time
    // of the request and the `c` value of the request.
    let (current, position_ms, changed_at, changed_by) =
        head(&server).await.expect("the owner has a queue row");
    assert_eq!(current.as_deref(), Some(songs[1]));
    assert_eq!(position_ms, 12_345);
    assert!((before..=after).contains(&changed_at), "{changed_at}");
    assert_eq!(changed_by, CLIENT);
    assert_eq!(stored(&server, OWNER).await, songs);

    let element = json!({
        "current": songs[1],
        "position": 12_345,
        "username": OWNER,
        "changed": model::iso8601(changed_at),
        "changedBy": CLIENT,
        "entry": [
            get_song(&server, songs[0]).await,
            get_song(&server, songs[1]).await,
            get_song(&server, songs[2]).await,
        ],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element.clone())
    );
    // The path with the `.view` suffix, and a parameter that the method does
    // not read.
    assert_eq!(
        server
            .get_json("getPlayQueue.view", &[("id", "x"), ("current", "x")])
            .await,
        ok("playQueue", element)
    );

    // In XML the values are attributes of the `playQueue` element and the
    // songs are its `entry` elements.
    let expected = format!(
        concat!(
            r#"<playQueue changed="{changed}" changedBy="{client}" current="{current}""#,
            r#" position="12345" username="owner">{first}{second}{third}</playQueue>"#
        ),
        changed = model::iso8601(changed_at),
        client = CLIENT,
        current = songs[1],
        first = entry_xml(&server, songs[0]).await,
        second = entry_xml(&server, songs[1]).await,
        third = entry_xml(&server, songs[2]).await,
    );
    assert_eq!(server.get_xml("getPlayQueue", &[]).await, xml_ok(&expected));
}

#[tokio::test]
async fn save_play_queue_answers_in_xml_with_the_empty_response() {
    let server = TestServer::standard().await;
    let track = server.track.contour.as_str();

    let request = save_request(&[track], track, &[]);
    assert_eq!(
        server.get_xml("savePlayQueue", &request).await,
        xml_ok_empty()
    );
    assert_eq!(entry_ids(&read_queue(&server).await), [track]);

    // A request without `current` answers with the empty response as well.
    assert_eq!(
        server.get_xml("savePlayQueue", &[("id", track)]).await,
        xml_ok_empty()
    );

    // An error is the error element.
    let request = save_request(&[track], UNKNOWN_TRACK, &[]);
    assert_eq!(
        server.get_xml("savePlayQueue", &request).await,
        xml_failed(0, NO_CURRENT)
    );
}

#[tokio::test]
async fn a_user_that_saved_no_queue_gets_the_play_queue_element_without_a_song() {
    let server = TestServer::standard().await;
    let created_at: i64 = server
        .db()
        .read(|conn| {
            let sql = "SELECT created_at FROM users WHERE name = ?1";
            Ok(conn.query_row(sql, [OWNER], |row| row.get(0))?)
        })
        .await
        .expect("read the account");
    let created = model::iso8601(created_at);

    // The three fields that the API requires are there. `changed` is the
    // creation time of the account, and no client saved the queue. The list
    // is `[]` in JSON and absent in XML.
    let element = json!({
        "position": 0,
        "username": OWNER,
        "changed": created,
        "changedBy": "",
        "entry": [],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    assert_eq!(
        server.get_xml("getPlayQueue", &[]).await,
        xml_ok(&format!(
            r#"<playQueue changed="{created}" changedBy="" position="0" username="owner"/>"#
        ))
    );
    // The read stores no row.
    assert_eq!(head(&server).await, None);
}

#[tokio::test]
async fn save_without_id_empties_the_queue() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;

    let before = unix_now();
    let cleared = server
        .get_json("savePlayQueue", &[("c", "Desk Player")])
        .await;
    let after = unix_now();
    assert_eq!(cleared, ok_empty());

    // The queue has no song, no current song and the position 0. The
    // request that emptied it is its last change.
    assert_eq!(stored(&server, OWNER).await, [] as [&str; 0]);
    let (current, position_ms, changed_at, changed_by) =
        head(&server).await.expect("the owner has a queue row");
    assert_eq!((current, position_ms), (None, 0));
    assert!((before..=after).contains(&changed_at), "{changed_at}");
    assert_eq!(changed_by, "Desk Player");
    let changed = model::iso8601(changed_at);
    let element = json!({
        "position": 0,
        "username": OWNER,
        "changed": changed,
        "changedBy": "Desk Player",
        "entry": [],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    assert_eq!(
        server.get_xml("getPlayQueue", &[]).await,
        xml_ok(&format!(
            concat!(
                r#"<playQueue changed="{changed}" changedBy="Desk Player""#,
                r#" position="0" username="owner"/>"#
            ),
            changed = changed
        ))
    );

    // An empty `current` counts as absent, and the position of a request
    // without a song is not stored.
    save(&server, &[a], a, &[]).await;
    let request = [("current", ""), ("position", "5000")];
    assert_eq!(server.get_json("savePlayQueue", &request).await, ok_empty());
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [] as [&str; 0]);
    assert_eq!(queue["position"], 0);
    assert_eq!(queue.get("current"), None);

    // A user that saved no queue empties it as well.
    let server = TestServer::standard().await;
    assert_eq!(server.get_json("savePlayQueue", &[]).await, ok_empty());
    let queue = read_queue(&server).await;
    assert_eq!(queue["changedBy"], CLIENT);
    assert_eq!(entry_ids(&queue), [] as [&str; 0]);
}

#[tokio::test]
async fn save_with_ids_and_without_current_is_read_with_the_first_song_as_current() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    // The queue before the request has `b` as its current song, and a
    // position.
    save(&server, &[b, a], b, &[("position", "9000")]).await;
    set_changed(&server).await;

    let before = unix_now();
    let request = [("id", a), ("id", b), ("c", "Supersonic")];
    let saved = server.get_json("savePlayQueue", &request).await;
    let after = unix_now();
    // The request answers with the empty response.
    assert_eq!(saved, ok_empty());

    // The row has no current song and the position 0, and it holds the time
    // and the client of the request. The entries have the order of the
    // request.
    let (current, position_ms, changed_at, changed_by) =
        head(&server).await.expect("the owner has a queue row");
    assert_eq!((current, position_ms), (None, 0));
    assert!((before..=after).contains(&changed_at), "{changed_at}");
    assert_eq!(changed_by, "Supersonic");
    assert_eq!(stored(&server, OWNER).await, [a, b]);

    // The document has the two songs in the order of the request, the first
    // of them as `current` and the position 0. The current song and the
    // position of the queue before the request are not kept.
    let changed = model::iso8601(changed_at);
    let element = json!({
        "current": a,
        "position": 0,
        "username": OWNER,
        "changed": changed,
        "changedBy": "Supersonic",
        "entry": [get_song(&server, a).await, get_song(&server, b).await],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    let expected = format!(
        concat!(
            r#"<playQueue changed="{changed}" changedBy="Supersonic" current="{current}""#,
            r#" position="0" username="owner">{first}{second}</playQueue>"#
        ),
        changed = changed,
        current = a,
        first = entry_xml(&server, a).await,
        second = entry_xml(&server, b).await,
    );
    assert_eq!(server.get_xml("getPlayQueue", &[]).await, xml_ok(&expected));
    // The read stores no current song.
    let (current, ..) = head(&server).await.expect("the owner has a queue row");
    assert_eq!(current, None);
}

#[tokio::test]
async fn save_without_current_checks_the_songs_and_the_position_and_stores_no_position() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();

    // An empty `current` counts as absent. The position is the position
    // within the current song, and a queue without one stores 0. A song that
    // is named twice is in the queue twice.
    let request = [
        ("id", a),
        ("id", b),
        ("id", a),
        ("current", ""),
        ("position", "5000"),
    ];
    assert_eq!(server.get_json("savePlayQueue", &request).await, ok_empty());
    let (current, position_ms, ..) = head(&server).await.expect("the owner has a queue row");
    assert_eq!((current, position_ms), (None, 0));
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [a, b, a]);
    assert_eq!(queue["position"], 0);
    assert_eq!(queue["current"], a);

    // The songs and the position are checked as in a request with
    // `current`, and a refused request leaves the queue.
    set_changed(&server).await;
    let before = state(&server).await;
    for (request, error) in [
        (vec![("id", UNKNOWN_TRACK)], failed(70, NOT_FOUND)),
        (vec![("id", b), ("id", "x")], failed(70, NOT_FOUND)),
        (vec![("id", ""), ("id", b)], failed(70, NOT_FOUND)),
        (
            vec![("id", b), ("position", "soon")],
            failed(0, NO_POSITION),
        ),
        (vec![("id", b), ("position", "-1")], failed(0, NO_POSITION)),
    ] {
        let response = server.get_json("savePlayQueue", &request).await;
        assert_eq!(response, error, "{request:?}");
    }
    assert_eq!(state(&server).await, before);

    // A form body saves the queue as a query string does.
    let form = [("id", b), ("id", a), ("f", "json")];
    let response = server.post_form("savePlayQueue", &form).await;
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(subsonic_response(&body_string(response).await), ok_empty());
    assert_eq!(stored(&server, OWNER).await, [b, a]);
    assert_eq!(queue_current(&server).await, b);
}

#[tokio::test]
async fn a_save_with_current_replaces_a_queue_that_was_saved_without_current() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    save_without_current(&server, &[a, b], &[]).await;
    assert_eq!(queue_current(&server).await, a);

    save(&server, &[b, c], c, &[("position", "40")]).await;

    // The row has a current song again. The document has the `current` and
    // the position of the request, and no song of the first queue that the
    // request did not name.
    let (current, position_ms, changed_at, _) =
        head(&server).await.expect("the owner has a queue row");
    assert_eq!((current.as_deref(), position_ms), (Some(c), 40));
    assert_eq!(stored(&server, OWNER).await, [b, c]);
    let changed = model::iso8601(changed_at);
    let element = json!({
        "current": c,
        "position": 40,
        "username": OWNER,
        "changed": changed,
        "changedBy": CLIENT,
        "entry": [get_song(&server, b).await, get_song(&server, c).await],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    let expected = format!(
        concat!(
            r#"<playQueue changed="{changed}" changedBy="{client}" current="{current}""#,
            r#" position="40" username="owner">{first}{second}</playQueue>"#
        ),
        changed = changed,
        client = CLIENT,
        current = c,
        first = entry_xml(&server, b).await,
        second = entry_xml(&server, c).await,
    );
    assert_eq!(server.get_xml("getPlayQueue", &[]).await, xml_ok(&expected));

    // A request without `current` replaces that queue in turn: the current
    // song and the position of the queue before it are not kept.
    save_without_current(&server, &[a, c], &[]).await;
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [a, c]);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 0);
    let (current, position_ms, ..) = head(&server).await.expect("the owner has a queue row");
    assert_eq!((current, position_ms), (None, 0));
}

#[tokio::test]
async fn save_without_id_empties_a_queue_that_was_saved_without_current() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    save_without_current(&server, &[a, b], &[]).await;
    set_changed(&server).await;

    let before = unix_now();
    let cleared = server
        .get_json("savePlayQueue", &[("c", "Desk Player")])
        .await;
    let after = unix_now();
    assert_eq!(cleared, ok_empty());

    // The queue has no song. The request that emptied it is its last change.
    assert_eq!(stored(&server, OWNER).await, [] as [&str; 0]);
    let (current, position_ms, changed_at, changed_by) =
        head(&server).await.expect("the owner has a queue row");
    assert_eq!((current, position_ms), (None, 0));
    assert!((before..=after).contains(&changed_at), "{changed_at}");
    assert_eq!(changed_by, "Desk Player");
    let changed = model::iso8601(changed_at);
    let element = json!({
        "position": 0,
        "username": OWNER,
        "changed": changed,
        "changedBy": "Desk Player",
        "entry": [],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    assert_eq!(
        server.get_xml("getPlayQueue", &[]).await,
        xml_ok(&format!(
            concat!(
                r#"<playQueue changed="{changed}" changedBy="Desk Player""#,
                r#" position="0" username="owner"/>"#
            ),
            changed = changed
        ))
    );
}

#[tokio::test]
async fn a_current_that_is_none_of_the_ids_leaves_a_queue_that_was_saved_without_current() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    save_without_current(&server, &[a, b], &[]).await;
    set_changed(&server).await;
    let before = state(&server).await;

    // A song of the library that the request does not queue, and the ID of
    // no song. The server does not take the first song in its place.
    for current in [c, UNKNOWN_TRACK] {
        let request = save_request(&[a, b], current, &[]);
        assert_eq!(
            server.get_json("savePlayQueue", &request).await,
            failed(0, NO_CURRENT),
            "{current}"
        );
        assert_eq!(
            server.get_xml("savePlayQueue", &request).await,
            xml_failed(0, NO_CURRENT),
            "{current}"
        );
        // A request without `id` queues no song that `current` can name.
        let request = [("current", current)];
        assert_eq!(
            server.get_json("savePlayQueue", &request).await,
            failed(0, NO_CURRENT),
            "{current}"
        );
        assert_eq!(
            server.get_xml("savePlayQueue", &request).await,
            xml_failed(0, NO_CURRENT),
            "{current}"
        );
    }

    // The queue is the one from before the requests: its row has no current
    // song, and its first song is read as `current`.
    assert_eq!(state(&server).await, before);
    let (current, ..) = head(&server).await.expect("the owner has a queue row");
    assert_eq!(current, None);
    assert_eq!(queue_current(&server).await, a);
}

#[tokio::test]
async fn a_current_that_is_not_in_the_queue_is_error_0() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let album = server.album.glass_harbour.as_str();
    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;
    let before = state(&server).await;

    // A song of the library that the request does not queue, the ID of no
    // song, and values that are no track ID.
    let upper = a.to_uppercase();
    for current in [c, UNKNOWN_TRACK, album, "tr-1", "1", upper.as_str()] {
        let request = save_request(&[a, b], current, &[]);
        let response = server.get_json("savePlayQueue", &request).await;
        assert_eq!(response, failed(0, NO_CURRENT), "{current}");

        // A request without `id` queues no song that `current` can name.
        let response = server
            .get_json("savePlayQueue", &[("current", current)])
            .await;
        assert_eq!(response, failed(0, NO_CURRENT), "{current}");
    }
    // The song that the stored queue has as its current song is none of the
    // songs of the request.
    let request = save_request(&[a, c], b, &[]);
    assert_eq!(
        server.get_json("savePlayQueue", &request).await,
        failed(0, NO_CURRENT)
    );

    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn a_queue_holds_a_song_as_often_as_the_request_names_it() {
    let server = TestServer::standard().await;
    let a = server.track.night_ferry.as_str();
    let b = server.track.delta.as_str();
    let c = server.track.open_road.as_str();
    let songs = [b, a, c, a, a];

    save(&server, &songs, a, &[("position", "250")]).await;

    assert_eq!(stored(&server, OWNER).await, songs);
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), songs);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 250);
    // The three entries of one song are the same element.
    assert_eq!(queue["entry"][1], get_song(&server, a).await);
    assert_eq!(queue["entry"][3], queue["entry"][1]);
    assert_eq!(queue["entry"][4], queue["entry"][1]);
    assert_eq!(queue["entry"][0], get_song(&server, b).await);
    assert_eq!(queue["entry"][2], get_song(&server, c).await);
    let body = server.get_xml("getPlayQueue", &[]).await;
    assert_eq!(body.matches("<entry ").count(), 5);
}

#[tokio::test]
async fn save_with_an_unknown_song_changes_nothing() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let album = server.album.glass_harbour.as_str();
    let artist = server.artist.aurora_field.as_str();
    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;
    let before = state(&server).await;

    // The ID of no track row, the ID of an item of another kind and values
    // without the shape of an ID, as the first, a middle and the last value.
    let upper = a.to_uppercase();
    for unknown in [
        UNKNOWN_TRACK,
        album,
        artist,
        "tr-1",
        "1",
        "",
        "' OR 1=1 --",
        upper.as_str(),
    ] {
        for songs in [[unknown, c, a], [c, unknown, a], [c, a, unknown]] {
            let request = save_request(&songs, c, &[("position", "1")]);
            let response = server.get_json("savePlayQueue", &request).await;
            assert_eq!(response, failed(70, NOT_FOUND), "{songs:?}");
        }
    }
    // The unknown song as the current song.
    let request = save_request(&[c, UNKNOWN_TRACK], UNKNOWN_TRACK, &[]);
    assert_eq!(
        server.get_json("savePlayQueue", &request).await,
        failed(70, NOT_FOUND)
    );

    // No song of a refused request is queued, and the current song, the
    // position, `changed` and `changedBy` are those from before.
    assert_eq!(state(&server).await, before);
    assert_eq!(stored(&server, OWNER).await, [a, b]);
}

#[tokio::test]
async fn position_is_in_milliseconds_and_0_without_a_value() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    /// The position that `getPlayQueue` returns.
    async fn position(server: &TestServer) -> Value {
        read_queue(server).await["position"].take()
    }

    // The value is stored and returned as it is, in milliseconds. It is not
    // compared with the duration of the song, which is half a second.
    save(&server, &[a], a, &[("position", "12345")]).await;
    assert_eq!(position(&server).await, 12_345);
    // An absent and an empty value are 0.
    save(&server, &[a], a, &[]).await;
    assert_eq!(position(&server).await, 0);
    save(&server, &[a], a, &[("position", "77")]).await;
    save(&server, &[a], a, &[("position", "")]).await;
    assert_eq!(position(&server).await, 0);
    // The largest value is the largest integer that the database stores.
    save(&server, &[a], a, &[("position", "9223372036854775807")]).await;
    assert_eq!(position(&server).await, 9_223_372_036_854_775_807_u64);
    let body = server.get_xml("getPlayQueue", &[]).await;
    assert!(
        body.contains(r#" position="9223372036854775807" "#),
        "{body}"
    );

    let before = state(&server).await;
    for value in ["-1", "1.5", "1e3", "soon", "9223372036854775808"] {
        let request = save_request(&[a], a, &[("position", value)]);
        let response = server.get_json("savePlayQueue", &request).await;
        assert_eq!(response, failed(0, NO_POSITION), "{value}");
        // A request without a song checks the value as well.
        let response = server
            .get_json("savePlayQueue", &[("position", value)])
            .await;
        assert_eq!(response, failed(0, NO_POSITION), "{value}");
    }
    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn changed_by_is_the_client_of_the_request_and_changed_is_its_time() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();

    save(&server, &[a, b], a, &[("c", "Symfonium")]).await;
    let first = read_queue(&server).await;
    assert_eq!(first["changedBy"], "Symfonium");
    assert_eq!(first["username"], OWNER);

    // The time is ISO 8601 in UTC with whole seconds.
    set_changed(&server).await;
    assert_eq!(read_queue(&server).await["changed"], "2023-03-14T17:51:22Z");
    let body = server.get_xml("getPlayQueue", &[]).await;
    assert!(
        body.contains(r#"<playQueue changed="2023-03-14T17:51:22Z" changedBy="Symfonium" "#),
        "{body}"
    );

    // Another client replaces the queue: the name and the time are those of
    // its request. The client that reads the queue does not change them.
    let before = unix_now();
    save(&server, &[b], b, &[("c", "Supersonic & <Friends>")]).await;
    let after = unix_now();
    let second = server.get_json("getPlayQueue", &[("c", "Reader")]).await;
    let second = &second["playQueue"];
    assert_eq!(second["changedBy"], "Supersonic & <Friends>");
    let (_, _, changed_at, _) = head(&server).await.expect("the owner has a queue row");
    assert!((before..=after).contains(&changed_at), "{changed_at}");
    assert_eq!(second["changed"], model::iso8601(changed_at));
    let body = server.get_xml("getPlayQueue", &[]).await;
    assert!(
        body.contains(r#" changedBy="Supersonic &amp; &lt;Friends&gt;" "#),
        "{body}"
    );

    // A name of more than 64 characters is cut to 64 characters.
    let long = "é".repeat(70);
    save(&server, &[a], a, &[("c", &long)]).await;
    assert_eq!(read_queue(&server).await["changedBy"], "é".repeat(64));
}

#[tokio::test]
async fn save_replaces_the_whole_queue() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let d = server.track.meridian.as_str();
    save(&server, &[a, b, c, d], d, &[("position", "9000")]).await;

    save(&server, &[c, b], c, &[("position", "20")]).await;

    // No song and no value of the first queue is left.
    assert_eq!(stored(&server, OWNER).await, [c, b]);
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [c, b]);
    assert_eq!(queue["current"], c);
    assert_eq!(queue["position"], 20);
    let rows: i64 = server
        .db()
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM play_queue", [], |row| row.get(0))?))
        .await
        .expect("count the rows");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_request_with_more_songs_than_a_queue_holds_is_error_0() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;
    let before = state(&server).await;
    let auth = format!("u={OWNER}&p={OWNER_PASSWORD}&v={API_VERSION}&c={CLIENT}&f=json");

    // One value more than a queue holds, in a form body of about 4.4 MB.
    let values = format!("id={a}&").repeat(MAX_ENTRIES + 1);
    let body = format!("{values}current={a}&{auth}");
    let response = post_body(&server, "savePlayQueue", body).await;
    assert_eq!(response, failed(0, TOO_LONG));

    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn play_queue_methods_answer_a_post_with_a_form_body() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let json_of = |response| async move {
        assert_eq!(content_type(&response), "application/json");
        subsonic_response(&body_string(response).await)
    };

    let form = [
        ("id", b),
        ("id", a),
        ("id", b),
        ("current", a),
        ("position", "4321"),
        ("f", "json"),
    ];
    let saved = json_of(server.post_form("savePlayQueue.view", &form).await).await;
    assert_eq!(saved, ok_empty());
    assert_eq!(stored(&server, OWNER).await, [b, a, b]);

    let read = json_of(server.post_form("getPlayQueue", &[("f", "json")]).await).await;
    assert_eq!(entry_ids(&read["playQueue"]), [b, a, b]);
    assert_eq!(read["playQueue"]["current"], a);
    assert_eq!(read["playQueue"]["position"], 4321);

    // The values of the query string come before the values of the body.
    let auth = format!("u={OWNER}&p={OWNER_PASSWORD}&v={API_VERSION}&c={CLIENT}&f=json");
    let request = Request::post(format!("/rest/savePlayQueue?id={c}&id={a}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("id={b}&current={b}&{auth}")))
        .expect("a request");
    assert_eq!(json_of(server.request(request).await).await, ok_empty());
    assert_eq!(stored(&server, OWNER).await, [c, a, b]);
}

#[tokio::test]
async fn the_songs_of_the_queue_carry_what_the_user_attached_to_them() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    save(&server, &[a, b, a], b, &[]).await;
    let starred = a.to_owned();
    server
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 SELECT id, ?1, 'track', ?2, 4, 7, ?2 FROM users WHERE name = ?3",
                params![starred, CHANGED_AT, OWNER],
            )?;
            Ok(())
        })
        .await
        .expect("write the annotation");

    let queue = read_queue(&server).await;
    for place in [0, 2] {
        let entry = &queue["entry"][place];
        assert_eq!(entry["starred"], "2023-03-14T17:51:22Z");
        assert_eq!(entry["userRating"], 4);
        assert_eq!(entry["playCount"], 7);
        assert_eq!(entry["played"], "2023-03-14T17:51:22Z");
        assert_eq!(*entry, get_song(&server, a).await);
    }
    let other = &queue["entry"][1];
    assert_eq!(other.get("starred"), None);
    assert_eq!(other["playCount"], 0);
}

#[tokio::test]
async fn each_user_has_a_queue_of_its_own() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    add_user(&server, "guest", "secret").await;
    /// The `playQueue` element that the guest reads.
    async fn queue_of_guest(server: &TestServer) -> Value {
        let mut response = server.get_json_noauth("getPlayQueue", &guest(&[])).await;
        assert_eq!(response["status"], "ok", "{response}");
        response["playQueue"].take()
    }

    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;
    let of_owner = state(&server).await;

    // The guest reads no song of the owner. The queue of the guest is the
    // one of a user that saved none.
    let unsaved = queue_of_guest(&server).await;
    assert_eq!(
        unsaved,
        json!({
            "position": 0,
            "username": "guest",
            "changed": "2023-03-14T17:51:22Z",
            "changedBy": "",
            "entry": [],
        })
    );

    // The guest saves a queue: the owner keeps the own one.
    let request = guest(&save_request(&[c, a], c, &[("position", "50")]));
    let saved = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(saved, ok_empty());
    let saved = queue_of_guest(&server).await;
    assert_eq!(saved["username"], "guest");
    assert_eq!(entry_ids(&saved), [c, a]);
    assert_eq!(saved["current"], c);
    assert_eq!(saved["position"], 50);
    assert_eq!(stored(&server, "guest").await, [c, a]);
    assert_eq!(state(&server).await, of_owner);

    // A request of the guest that is refused and one that empties the queue
    // leave the queue of the owner as well.
    let request = guest(&save_request(&[b, UNKNOWN_TRACK], b, &[]));
    let refused = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(refused, failed(70, NOT_FOUND));
    assert_eq!(entry_ids(&queue_of_guest(&server).await), [c, a]);
    let emptied = server.get_json_noauth("savePlayQueue", &guest(&[])).await;
    assert_eq!(emptied, ok_empty());
    assert_eq!(stored(&server, "guest").await, [] as [&str; 0]);
    assert_eq!(state(&server).await, of_owner);

    // The owner replaces the own queue and leaves the one of the guest.
    let request = guest(&save_request(&[b], b, &[]));
    let saved = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(saved, ok_empty());
    save(&server, &[c], c, &[]).await;
    assert_eq!(stored(&server, OWNER).await, [c]);
    assert_eq!(stored(&server, "guest").await, [b]);
    assert_eq!(entry_ids(&queue_of_guest(&server).await), [b]);
}

#[tokio::test]
async fn a_save_without_current_of_one_user_leaves_the_queue_of_another_user() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    add_user(&server, "guest", "secret").await;
    /// The answer of `getPlayQueue` to the guest.
    async fn document_of_guest(server: &TestServer) -> Value {
        server.get_json_noauth("getPlayQueue", &guest(&[])).await
    }

    // The queue of the owner has songs, a current song and a position.
    save(&server, &[a, b], b, &[("position", "9000")]).await;
    set_changed(&server).await;
    let of_owner = state(&server).await;

    // The guest saves songs without `current`: the queue of the guest holds
    // them, and the queue of the owner is the one from before.
    let request = guest(&[("id", c), ("id", a)]);
    let saved = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(saved, ok_empty());
    assert_eq!(stored(&server, "guest").await, [c, a]);
    let of_guest = document_of_guest(&server).await;
    assert_eq!(of_guest["playQueue"]["username"], "guest");
    assert_eq!(entry_ids(&of_guest["playQueue"]), [c, a]);
    assert_eq!(of_guest["playQueue"]["current"], c);
    assert_eq!(of_guest["playQueue"]["position"], 0);
    assert_eq!(state(&server).await, of_owner);

    // The owner saves songs without `current` while the queue of the guest
    // holds songs: the queue of the guest is the one from before.
    save_without_current(&server, &[b, c, a], &[]).await;
    assert_eq!(stored(&server, OWNER).await, [b, c, a]);
    assert_eq!(queue_current(&server).await, b);
    assert_eq!(stored(&server, "guest").await, [c, a]);
    assert_eq!(document_of_guest(&server).await, of_guest);
}

#[tokio::test]
async fn play_queue_with_authentication_off_is_the_queue_of_the_owner() {
    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    let server = TestServer::bare(config).await;

    // The server has no library, so a request stores a queue without a song.
    let request = [("v", API_VERSION), ("c", "Kitchen")];
    let saved = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(saved, ok_empty());
    let request = [("v", API_VERSION), ("c", CLIENT)];
    let read = server.get_json_noauth("getPlayQueue", &request).await;
    assert_eq!(read["playQueue"]["username"], OWNER);
    assert_eq!(read["playQueue"]["changedBy"], "Kitchen");
    assert_eq!(read["playQueue"]["entry"], json!([]));
}

#[tokio::test]
async fn the_queue_keeps_its_songs_through_a_rescan_and_a_moved_file() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let songs = [b, a, c, a];
    save(&server, &songs, a, &[("position", "4000")]).await;
    set_changed(&server).await;
    let before = state(&server).await;

    // A scan that finds nothing new writes no row of the queue.
    let summary = server.rescan().await;
    assert_eq!((summary.seen, summary.removed), (10, 0));
    assert_eq!(state(&server).await, before);

    // The file of `a`, the current song, moves to another directory under
    // another name. The track keeps its ID, so the queue keeps its two
    // entries of it, its current song and the position.
    let old = "Aurora Field/Glass Harbour/01 Low Tide.flac";
    let new = "Moved/High Tide.flac";
    fs::create_dir(server.path("Moved")).expect("create the directory");
    fs::rename(server.path(old), server.path(new)).expect("move the file");
    let summary = server.rescan().await;
    assert_eq!(
        (summary.seen, summary.moved, summary.added, summary.removed),
        (10, 1, 0, 0)
    );
    assert_eq!(summary.errors, [] as [&str; 0]);

    assert_eq!(stored(&server, OWNER).await, songs);
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), songs);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 4000);
    assert_eq!(queue["entry"][1]["path"], new);
    assert_eq!(queue["entry"][3]["path"], new);
    assert_eq!(queue["entry"][1], get_song(&server, a).await);
    // The scan is no change of the queue.
    assert_eq!(queue["changed"], "2023-03-14T17:51:22Z");
    assert_eq!(queue["changedBy"], CLIENT);
    assert_eq!(head(&server).await, before.0);
}

#[tokio::test]
async fn a_track_without_a_file_stays_in_the_queue_and_is_listed() {
    let server = TestServer::standard().await;
    let a = server.track.upstream.as_str();
    let b = server.track.delta.as_str();
    let songs = [a, b, a];
    save(&server, &songs, a, &[("position", "300")]).await;
    let with_file = read_queue(&server).await;
    assert_eq!(
        with_file["entry"][0]["path"],
        "Marlow Quartet/Slow Rivers/01 Upstream.m4a"
    );

    remove_file(&server, a).await;

    // The track keeps its entries, its place and its tags, and it stays the
    // current song. Its element has no `path`, as the `song` element of
    // `getSong` has none.
    assert_eq!(stored(&server, OWNER).await, songs);
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), songs);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 300);
    for place in [0, 2] {
        let entry = &queue["entry"][place];
        assert_eq!(entry["title"], "Upstream");
        assert_eq!(entry.get("path"), None);
        assert_eq!(*entry, get_song(&server, a).await);
    }
    assert_eq!(queue["entry"][1], with_file["entry"][1]);
    let body = server.get_xml("getPlayQueue", &[]).await;
    assert_eq!(body.matches("<entry ").count(), 3);
    assert!(!body.contains(&format!("/{a}")), "{body}");

    // The track is saved as every other track.
    save(&server, &[b, a], a, &[]).await;
    assert_eq!(stored(&server, OWNER).await, [b, a]);
    assert_eq!(queue_current(&server).await, a);
}

#[tokio::test]
async fn a_track_that_a_scan_deletes_leaves_the_queue_and_the_order_stays() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let d = server.track.contour.as_str();
    save(&server, &[d, b, a, c, b], a, &[("position", "9000")]).await;
    set_changed(&server).await;
    let row = head(&server).await;

    // The file of `b` is deleted, and the scan deletes the track row.
    delete_song(&server, b).await;

    // The two entries of `b` are gone. The other songs keep their order, and
    // the current song and the position are those from before.
    assert_eq!(stored(&server, OWNER).await, [d, a, c]);
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [d, a, c]);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 9000);
    assert_eq!(queue["entry"][1], get_song(&server, a).await);
    // The scan is no change of the queue.
    assert_eq!(queue["changed"], "2023-03-14T17:51:22Z");
    assert_eq!(queue["changedBy"], CLIENT);
    assert_eq!(head(&server).await, row);

    // A request that names the deleted song is refused, and the queue
    // stays.
    let before = state(&server).await;
    let request = save_request(&[d, b, a], a, &[]);
    assert_eq!(
        server.get_json("savePlayQueue", &request).await,
        failed(70, NOT_FOUND)
    );
    assert_eq!(state(&server).await, before);
}

#[tokio::test]
async fn when_a_scan_deletes_the_current_song_the_song_that_followed_is_current() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let d = server.track.contour.as_str();
    save(&server, &[a, b, c, d], b, &[("position", "9000")]).await;
    set_changed(&server).await;

    delete_song(&server, b).await;

    // `c` followed `b` in the queue. The stored position was one within
    // `b`, so the position is 0.
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [a, c, d]);
    assert_eq!(queue["current"], c);
    assert_eq!(queue["position"], 0);
    // The scan is no change of the queue.
    assert_eq!(queue["changed"], "2023-03-14T17:51:22Z");
    assert_eq!(queue["changedBy"], CLIENT);
    let body = server.get_xml("getPlayQueue", &[]).await;
    let attributes = format!(
        r#"<playQueue changed="2023-03-14T17:51:22Z" changedBy="{CLIENT}" current="{c}" position="0" username="owner">"#
    );
    assert!(body.contains(&attributes), "{body}");
    assert_eq!(body.matches("<entry ").count(), 3);

    // The current song is the last of the queue: no song followed it, and
    // the last song that is left is the current one.
    save(&server, &[a, c, d], d, &[("position", "700")]).await;
    delete_song(&server, d).await;
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [a, c]);
    assert_eq!(queue["current"], c);
    assert_eq!(queue["position"], 0);

    // The next request stores a queue as every other request does.
    save(&server, &[c, a], a, &[("position", "40")]).await;
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [c, a]);
    assert_eq!(queue["current"], a);
    assert_eq!(queue["position"], 40);
}

#[tokio::test]
async fn a_queue_whose_songs_a_scan_deleted_has_no_current_song() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    save(&server, &[a, a], a, &[("position", "9000")]).await;
    set_changed(&server).await;

    delete_song(&server, a).await;

    // The `playQueue` element of a queue without a song. The time and the
    // client are those of the request that saved the queue.
    let element = json!({
        "position": 0,
        "username": OWNER,
        "changed": "2023-03-14T17:51:22Z",
        "changedBy": CLIENT,
        "entry": [],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    assert_eq!(stored(&server, OWNER).await, [] as [&str; 0]);
}

#[tokio::test]
async fn a_scan_tells_a_queue_saved_without_current_from_one_whose_current_song_it_deleted() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let d = server.track.contour.as_str();
    add_user(&server, "guest", "secret").await;
    /// The `playQueue` element that the guest reads.
    async fn queue_of_guest(server: &TestServer) -> Value {
        let request = as_user("guest", "secret", &[]);
        let mut response = server.get_json_noauth("getPlayQueue", &request).await;
        assert_eq!(response["status"], "ok", "{response}");
        response["playQueue"].take()
    }

    // The owner saves four songs without `current`. The guest saves the same
    // songs with `c` as the current song.
    let songs = [a, b, c, d];
    save_without_current(&server, &songs, &[]).await;
    set_changed(&server).await;
    let row = head(&server).await;
    let request = as_user(
        "guest",
        "secret",
        &save_request(&songs, c, &[("position", "9000")]),
    );
    let saved = server.get_json_noauth("savePlayQueue", &request).await;
    assert_eq!(saved, ok_empty());

    // The file of `c` is deleted, and the scan deletes the track row.
    delete_song(&server, c).await;

    // Both queues hold the same three songs, and their current songs differ.
    // The queue of the owner was saved without `current`, and its first song
    // is the current one.
    assert_eq!(stored(&server, OWNER).await, [a, b, d]);
    let element = json!({
        "current": a,
        "position": 0,
        "username": OWNER,
        "changed": "2023-03-14T17:51:22Z",
        "changedBy": CLIENT,
        "entry": [
            get_song(&server, a).await,
            get_song(&server, b).await,
            get_song(&server, d).await,
        ],
    });
    assert_eq!(
        server.get_json("getPlayQueue", &[]).await,
        ok("playQueue", element)
    );
    let body = server.get_xml("getPlayQueue", &[]).await;
    let attributes = format!(
        r#"<playQueue changed="2023-03-14T17:51:22Z" changedBy="{CLIENT}" current="{a}" position="0" username="owner">"#
    );
    assert!(body.contains(&attributes), "{body}");
    assert_eq!(body.matches("<entry ").count(), 3);
    // The scan is no change of the queue, and it stores no current song.
    assert_eq!(head(&server).await, row);
    // The scan deleted the current song of the queue of the guest: the song
    // that followed it is the current one.
    let of_guest = queue_of_guest(&server).await;
    assert_eq!(entry_ids(&of_guest), [a, b, d]);
    assert_eq!(of_guest["current"], d);
    assert_eq!(of_guest["position"], 0);

    // The first song is deleted. The queue of the owner has the first song
    // that is left as the current one, and the queue of the guest keeps the
    // song that followed its deleted current song.
    delete_song(&server, a).await;
    let queue = read_queue(&server).await;
    assert_eq!(entry_ids(&queue), [b, d]);
    assert_eq!(queue["current"], b);
    assert_eq!(queue["position"], 0);
    let of_guest = queue_of_guest(&server).await;
    assert_eq!(entry_ids(&of_guest), [b, d]);
    assert_eq!(of_guest["current"], d);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_at_the_same_time_leave_one_queue_whole() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let d = server.track.meridian.as_str();
    // The queue of each client, with the values that only it sends.
    let phone = save_request(&[a, b, c, a], b, &[("position", "111"), ("c", "Phone")]);
    let desktop = save_request(&[d, c], d, &[("position", "222"), ("c", "Desktop")]);
    let wholes = [
        (vec![a, b, c, a], b, 111, "Phone"),
        (vec![d, c], d, 222, "Desktop"),
    ];

    for round in 0..20 {
        let responses = tokio::join!(
            server.get_json("savePlayQueue", &phone),
            server.get_json("savePlayQueue", &desktop),
            server.get_json("getPlayQueue", &[]),
            server.get_json("savePlayQueue", &desktop),
            server.get_json("savePlayQueue", &phone),
        );
        assert_eq!(responses.0, ok_empty());
        assert_eq!(responses.1, ok_empty());
        assert_eq!(responses.3, ok_empty());
        assert_eq!(responses.4, ok_empty());

        // The queue that is stored after the requests, and the queue that a
        // request read between them, is the queue of one client: its songs,
        // its current song, its position and its name.
        let stored_queue = read_queue(&server).await;
        let read_between = &responses.2["playQueue"];
        for queue in [&stored_queue, read_between] {
            // The first round reads the queue of a user that saved none.
            if round == 0 && queue["changedBy"] == "" {
                continue;
            }
            let found = (
                entry_ids(queue),
                queue["current"].as_str().expect("a current song"),
                queue["position"].as_u64().expect("a position"),
                queue["changedBy"].as_str().expect("a client"),
            );
            let whole = wholes.iter().any(|(songs, current, position, client)| {
                found == (list_of(songs), *current, *position, *client)
            });
            assert!(whole, "round {round}: {queue}");
        }
        let rows = stored(&server, OWNER).await;
        assert!(
            wholes.iter().any(|(songs, ..)| rows == *songs),
            "round {round}: {rows:?}"
        );
    }
}

#[tokio::test]
async fn two_calls_return_the_same_document() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.open_road.as_str();
    save(&server, &[b, a, b], a, &[("position", "1500")]).await;

    let first = server.get_xml("getPlayQueue", &[]).await;
    assert!(first.contains(r#" status="ok" "#), "{first}");
    for _ in 0..3 {
        assert_eq!(server.get_xml("getPlayQueue", &[]).await, first);
    }
    let first = server.get_json("getPlayQueue", &[]).await;
    assert_eq!(server.get_json("getPlayQueue", &[]).await, first);
}
