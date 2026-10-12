//! Router tests of the playlist methods `getPlaylists`, `getPlaylist`,
//! `createPlaylist`, `updatePlaylist` and `deletePlaylist` (`docs/design.md`
//! section 4).
//!
//! A test writes playlists through the methods and reads them back through
//! the methods and from the rows of `playlists` and `playlist_tracks`. The
//! songs of a playlist are compared with the `song` element that `getSong`
//! returns for the same ID, which `tests/browse.rs` compares with the
//! library.

mod common;

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, header};
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::model;
use wn_server::config::Config;
use wn_server::db::queries::playlists::MAX_ENTRIES;
use wn_server::db::queries::tracks;
use wn_server::ids::{self, Kind};

use common::{
    API_VERSION, CLIENT, OWNER, OWNER_PASSWORD, ScannedServer, TestServer, body_string,
    content_type, subsonic_response,
};

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of the error for a `songIndexToRemove` that is the position
/// of no song.
const NO_POSITION: &str =
    "parameter songIndexToRemove must be the position of a song in the playlist, counted from 0";

/// The message of the error for a playlist that would be too long.
const TOO_LONG: &str = "a playlist holds at most 100000 songs";

/// The ID of a track that no library has.
const UNKNOWN_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";

/// The ID of a playlist that no server has.
const UNKNOWN_PLAYLIST: &str = "pl-ffffffffffffffffffffffffffffffff";

/// 2023-03-14T17:51:22Z.
const CREATED_AT: i64 = 1_678_816_282;

/// 2023-03-15T00:00:00Z.
const CHANGED_AT: i64 = 1_678_838_400;

/// The `subsonic-response` object of a success without a payload.
fn ok_empty() -> Value {
    json!({
        "status": "ok",
        "version": "1.16.1",
        "type": "wn-server",
        "serverVersion": VERSION,
        "openSubsonic": true,
    })
}

/// The `subsonic-response` object of a success that carries `value` under
/// `key`.
fn ok(key: &str, value: Value) -> Value {
    let mut response = ok_empty();
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

/// The XML body of a success whose root element holds `content`.
fn xml_ok(content: &str) -> String {
    xml_document("ok", &format!(">{content}</subsonic-response>"))
}

/// The XML body of a success without a payload.
fn xml_ok_empty() -> String {
    xml_document("ok", "/>")
}

/// The XML body of the error `code` with `message`.
fn xml_failed(code: u32, message: &str) -> String {
    let error = format!(r#"<error code="{code}" message="{message}"/>"#);
    xml_document("failed", &format!(">{error}</subsonic-response>"))
}

/// The time now in whole Unix seconds.
fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    i64::try_from(elapsed.as_secs()).expect("the time fits")
}

/// Creates a playlist named `name` with `songs` through `createPlaylist` and
/// returns its ID.
async fn create(server: &TestServer, name: &str, songs: &[&str]) -> String {
    let mut request = vec![("name", name)];
    request.extend(songs.iter().map(|id| ("songId", *id)));
    let response = server.get_json("createPlaylist", &request).await;
    assert_eq!(response["status"], "ok", "{response}");
    let id = response["playlist"]["id"]
        .as_str()
        .expect("the answer holds the ID of the playlist");
    assert!(
        matches!(ids::parse(id), Some((Kind::Playlist, _))),
        "{response}"
    );
    id.to_owned()
}

/// Sends `updatePlaylist` for the playlist `id` with `changes` and returns
/// the response.
async fn update(server: &TestServer, id: &str, changes: &[(&str, &str)]) -> Value {
    let mut request = vec![("playlistId", id)];
    request.extend_from_slice(changes);
    server.get_json("updatePlaylist", &request).await
}

/// The `playlist` element that `getPlaylist` returns for `id`.
async fn playlist(server: &TestServer, id: &str) -> Value {
    let mut response = server.get_json("getPlaylist", &[("id", id)]).await;
    assert_eq!(response["status"], "ok", "{response}");
    response["playlist"].take()
}

/// The IDs of the songs that `getPlaylist` lists for `id`, in the order of
/// the `entry` list.
async fn entry_ids(server: &TestServer, id: &str) -> Vec<String> {
    let playlist = playlist(server, id).await;
    playlist["entry"]
        .as_array()
        .expect("an entry list")
        .iter()
        .map(|entry| entry["id"].as_str().expect("an ID").to_owned())
        .collect()
}

/// The rows of `playlist_tracks` of the playlist `id`: the stored position
/// and the track ID of each, in the order of the positions.
async fn stored(server: &TestServer, id: &str) -> Vec<(i64, String)> {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| {
            let mut select = conn.prepare(
                "SELECT position, track_id FROM playlist_tracks
                 WHERE playlist_id = ?1 ORDER BY position",
            )?;
            let rows = select
                .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .await
        .expect("read the entries")
}

/// Checks that the playlist `id` holds `songs` in that order: in the `entry`
/// list of `getPlaylist`, and in the rows, whose positions are 0 to n - 1.
async fn assert_songs(server: &TestServer, id: &str, songs: &[&str]) {
    assert_eq!(entry_ids(server, id).await, songs);
    let rows: Vec<(i64, String)> = (0..).zip(songs.iter().map(|id| (*id).to_owned())).collect();
    assert_eq!(stored(server, id).await, rows);
}

/// The number of rows of `table`.
async fn count_rows(server: &TestServer, table: &'static str) -> i64 {
    server
        .db()
        .read(move |conn| {
            let sql = format!("SELECT count(*) FROM {table}");
            Ok(conn.query_row(&sql, [], |row| row.get(0))?)
        })
        .await
        .expect("count the rows")
}

/// Sets the creation time and the change time of the playlist `id` to
/// [`CREATED_AT`] and [`CHANGED_AT`], which are in the past.
async fn set_times(server: &TestServer, id: &str) {
    let id = id.to_owned();
    let changed = server
        .db()
        .write(move |conn| {
            Ok(conn.execute(
                "UPDATE playlists SET created_at = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, CREATED_AT, CHANGED_AT],
            )?)
        })
        .await
        .expect("set the times");
    assert_eq!(changed, 1);
}

/// The creation time and the change time of the playlist `id`, as the row
/// holds them.
async fn times(server: &TestServer, id: &str) -> (i64, i64) {
    let id = id.to_owned();
    server
        .db()
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT created_at, updated_at FROM playlists WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .expect("read the times")
}

/// The `song` element that `getSong` returns for `id`.
async fn song(server: &TestServer, id: &str) -> Value {
    let mut response = server.get_json("getSong", &[("id", id)]).await;
    assert_eq!(response["status"], "ok", "{response}");
    response["song"].take()
}

/// The element of the song `id` in XML under the name `entry`: the `song`
/// element of `getSong` with the other name.
async fn entry_xml(server: &TestServer, id: &str) -> String {
    let body = server.get_xml("getSong", &[("id", id)]).await;
    let start = body.find("<song ").expect("a song element");
    let end = body
        .rfind("</subsonic-response>")
        .expect("the end of the root element");
    body[start..end].replacen("<song ", "<entry ", 1)
}

/// The duration of a playlist that holds `songs`: the sum of the durations
/// of the tracks in milliseconds, rounded once to seconds.
async fn duration(server: &ScannedServer, songs: &[&str]) -> u64 {
    let catalogue = server.catalogue().await;
    let milliseconds: u64 = songs
        .iter()
        .map(|id| {
            let row = catalogue.songs.iter().find(|row| row.track.id == *id);
            row.expect("a track of the library").track.duration_ms
        })
        .sum();
    model::seconds(milliseconds)
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

/// Inserts a second account with the name `name` and the password
/// `password`. The server has no method that does so.
async fn add_user(server: &TestServer, name: &'static str, password: &'static str) {
    server
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO users (name, role, password, created_at, updated_at)
                 VALUES (?1, 'guest', ?2, 0, 0)",
                params![name, password],
            )?;
            Ok(())
        })
        .await
        .expect("insert the user");
}

/// `request` with the parameters that authenticate the user `name` with the
/// password `password`.
fn as_user<'a>(
    name: &'a str,
    password: &'a str,
    request: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut merged = request.to_vec();
    merged.extend([
        ("u", name),
        ("p", password),
        ("v", API_VERSION),
        ("c", CLIENT),
    ]);
    merged
}

/// Sends `POST /rest/<method>` with `body` as the form body and returns the
/// `subsonic-response` object. `body` holds every parameter of the request.
async fn post_body(server: &TestServer, method: &str, body: String) -> Value {
    let request = Request::post(format!("/rest/{method}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("a request");
    let response = server.request(request).await;
    assert_eq!(content_type(&response), "application/json", "{method}");
    subsonic_response(&body_string(response).await)
}

#[tokio::test]
async fn create_playlist_with_three_songs_returns_them_in_order_with_the_summed_duration() {
    let server = TestServer::standard().await;
    // The order of the request is no order of the library.
    let songs = [
        server.track.upstream.as_str(),
        server.track.low_tide.as_str(),
        server.track.meridian.as_str(),
    ];

    let before = unix_now();
    let request = [
        ("name", "Road Trip"),
        ("songId", songs[0]),
        ("songId", songs[1]),
        ("songId", songs[2]),
    ];
    let created = server.get_json("createPlaylist", &request).await;
    let after = unix_now();

    let id = created["playlist"]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();
    assert!(matches!(ids::parse(&id), Some((Kind::Playlist, _))), "{id}");
    // The creation time is the time of the request, and nothing changed the
    // playlist since.
    let (created_at, changed_at) = times(&server, &id).await;
    assert!((before..=after).contains(&created_at), "{created_at}");
    assert_eq!(changed_at, created_at);

    // The duration is the sum of the three durations in milliseconds,
    // rounded once. Each song lasts about half a second and reports one
    // second, so the sum of the rounded durations is another number.
    let seconds = duration(&server, &songs).await;
    assert_eq!(seconds, 2);
    let mut rounded_one_by_one = 0;
    for id in songs {
        rounded_one_by_one += song(&server, id).await["duration"]
            .as_u64()
            .expect("a duration");
    }
    assert_eq!(rounded_one_by_one, 3);

    let element = json!({
        "id": id,
        "name": "Road Trip",
        "owner": OWNER,
        "public": false,
        "songCount": 3,
        "duration": seconds,
        "created": model::iso8601(created_at),
        "changed": model::iso8601(created_at),
        // The cover of the first song, which is the cover of its album.
        "coverArt": server.album.slow_rivers,
        "entry": [
            song(&server, songs[0]).await,
            song(&server, songs[1]).await,
            song(&server, songs[2]).await,
        ],
    });
    // `createPlaylist` returns the playlist as `getPlaylist` does.
    assert_eq!(created, ok("playlist", element.clone()));
    assert_eq!(
        server.get_json("getPlaylist", &[("id", &id)]).await,
        ok("playlist", element)
    );
    assert_songs(&server, &id, &songs).await;

    // In XML the songs are `entry` elements of the `playlist` element.
    let time = model::iso8601(created_at);
    let expected = format!(
        concat!(
            r#"<playlist changed="{time}" coverArt="{cover_art}" created="{time}""#,
            r#" duration="{seconds}" id="{id}" name="Road Trip" owner="owner""#,
            r#" public="false" songCount="3">{first}{second}{third}</playlist>"#
        ),
        time = time,
        cover_art = server.album.slow_rivers,
        seconds = seconds,
        id = id,
        first = entry_xml(&server, songs[0]).await,
        second = entry_xml(&server, songs[1]).await,
        third = entry_xml(&server, songs[2]).await,
    );
    assert_eq!(
        server.get_xml("getPlaylist", &[("id", &id)]).await,
        xml_ok(&expected)
    );
}

#[tokio::test]
async fn create_playlist_answers_in_xml_with_the_playlist_and_its_songs() {
    let server = TestServer::standard().await;
    let track = server.track.contour.as_str();

    let body = server
        .get_xml("createPlaylist", &[("name", "One"), ("songId", track)])
        .await;
    let listed = server.get_json("getPlaylists", &[]).await;
    let id = listed["playlists"]["playlist"][0]["id"]
        .as_str()
        .expect("an ID");
    let (created_at, _) = times(&server, id).await;
    let time = model::iso8601(created_at);
    // The first song has no cover, so the playlist has none.
    let expected = format!(
        concat!(
            r#"<playlist changed="{time}" created="{time}" duration="1" id="{id}""#,
            r#" name="One" owner="owner" public="false" songCount="1">{entry}</playlist>"#
        ),
        time = time,
        id = id,
        entry = entry_xml(&server, track).await,
    );
    assert_eq!(body, xml_ok(&expected));
}

#[tokio::test]
async fn a_playlist_holds_a_song_as_often_as_the_request_names_it() {
    let server = TestServer::standard().await;
    let a = server.track.night_ferry.as_str();
    let b = server.track.delta.as_str();
    let c = server.track.open_road.as_str();
    let songs = [a, b, a, c, a];

    let id = create(&server, "Loop", &songs).await;
    assert_songs(&server, &id, &songs).await;

    // Every entry counts for the number and for the duration.
    let playlist = playlist(&server, &id).await;
    assert_eq!(playlist["songCount"], 5);
    assert_eq!(playlist["duration"], duration(&server, &songs).await);
    // The three entries of one song are the same element.
    assert_eq!(playlist["entry"][0], song(&server, a).await);
    assert_eq!(playlist["entry"][2], playlist["entry"][0]);
    assert_eq!(playlist["entry"][4], playlist["entry"][0]);
    assert_eq!(playlist["entry"][1], song(&server, b).await);
    assert_eq!(playlist["entry"][3], song(&server, c).await);
}

#[tokio::test]
async fn create_playlist_without_a_song_gives_a_playlist_without_an_entry() {
    let server = TestServer::standard().await;

    let created = server
        .get_json("createPlaylist", &[("name", "Empty")])
        .await;
    let id = created["playlist"]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();
    let (created_at, _) = times(&server, &id).await;
    let time = model::iso8601(created_at);
    // No comment and no cover. The list is `[]` in JSON and absent in XML.
    let element = json!({
        "id": id,
        "name": "Empty",
        "owner": OWNER,
        "public": false,
        "songCount": 0,
        "duration": 0,
        "created": time,
        "changed": time,
        "entry": [],
    });
    assert_eq!(created, ok("playlist", element.clone()));
    assert_eq!(
        server.get_json("getPlaylist", &[("id", &id)]).await,
        ok("playlist", element)
    );
    assert_eq!(
        server.get_xml("getPlaylist", &[("id", &id)]).await,
        xml_ok(&format!(
            concat!(
                r#"<playlist changed="{time}" created="{time}" duration="0" id="{id}""#,
                r#" name="Empty" owner="owner" public="false" songCount="0"/>"#
            ),
            time = time,
            id = id,
        ))
    );
    assert_eq!(stored(&server, &id).await, []);
}

#[tokio::test]
async fn get_playlists_lists_the_playlists_by_name_without_their_songs() {
    let server = TestServer::standard().await;
    let low_tide = server.track.low_tide.as_str();
    let meridian = server.track.meridian.as_str();
    let legend = server.track.legend.as_str();

    // A server without a playlist: `[]` in JSON and no element in XML.
    assert_eq!(
        server.get_json("getPlaylists", &[]).await,
        ok("playlists", json!({ "playlist": [] }))
    );
    assert_eq!(
        server.get_xml("getPlaylists", &[]).await,
        xml_ok("<playlists/>")
    );

    let b_side = create(&server, "b-side", &[meridian, low_tide]).await;
    let lower = create(&server, "alpha", &[]).await;
    let upper = create(&server, "Alpha", &[low_tide, legend, low_tide]).await;
    let changes = [("comment", "For the road"), ("public", "true")];
    assert_eq!(update(&server, &upper, &changes).await, ok_empty());
    for id in [&b_side, &lower, &upper] {
        set_times(&server, id).await;
    }
    let created = "2023-03-14T17:51:22Z";
    let changed = "2023-03-15T00:00:00Z";

    // The order is the order of the names without regard to case. Names
    // that differ by case only are in the order of their bytes.
    let upper_duration = duration(&server, &[low_tide, legend, low_tide]).await;
    let b_side_duration = duration(&server, &[meridian, low_tide]).await;
    let expected = json!({ "playlist": [
        {
            "id": upper,
            "name": "Alpha",
            "comment": "For the road",
            "owner": OWNER,
            "public": true,
            "songCount": 3,
            "duration": upper_duration,
            "created": created,
            "changed": changed,
            "coverArt": server.album.glass_harbour,
        },
        {
            "id": lower,
            "name": "alpha",
            "owner": OWNER,
            "public": false,
            "songCount": 0,
            "duration": 0,
            "created": created,
            "changed": changed,
        },
        {
            // The album of the first song has no cover.
            "id": b_side,
            "name": "b-side",
            "owner": OWNER,
            "public": false,
            "songCount": 2,
            "duration": b_side_duration,
            "created": created,
            "changed": changed,
        },
    ]});
    let response = server.get_json("getPlaylists", &[]).await;
    assert_eq!(response, ok("playlists", expected));

    // Each element is the element of `getPlaylist` without `entry`.
    for element in response["playlists"]["playlist"]
        .as_array()
        .expect("a list")
    {
        let id = element["id"].as_str().expect("an ID");
        let mut with_songs = playlist(&server, id).await;
        let fields = with_songs.as_object_mut().expect("an object");
        assert!(fields.remove("entry").is_some(), "{id}");
        assert_eq!(*element, with_songs);
    }

    let times = format!(r#"changed="{changed}" comment="For the road""#);
    let expected = format!(
        concat!(
            r#"<playlists>"#,
            r#"<playlist {times} coverArt="{cover_art}" created="{created}""#,
            r#" duration="{upper_duration}" id="{upper}" name="Alpha" owner="owner""#,
            r#" public="true" songCount="3"/>"#,
            r#"<playlist changed="{changed}" created="{created}" duration="0" id="{lower}""#,
            r#" name="alpha" owner="owner" public="false" songCount="0"/>"#,
            r#"<playlist changed="{changed}" created="{created}""#,
            r#" duration="{b_side_duration}" id="{b_side}" name="b-side" owner="owner""#,
            r#" public="false" songCount="2"/>"#,
            r#"</playlists>"#
        ),
        times = times,
        cover_art = server.album.glass_harbour,
        created = created,
        changed = changed,
        upper_duration = upper_duration,
        upper = upper,
        lower = lower,
        b_side_duration = b_side_duration,
        b_side = b_side,
    );
    assert_eq!(server.get_xml("getPlaylists", &[]).await, xml_ok(&expected));
}

#[tokio::test]
async fn get_playlists_takes_the_name_of_the_user_of_the_request_as_username() {
    let server = TestServer::standard().await;
    create(&server, "Mine", &[]).await;
    let own = server.get_json("getPlaylists", &[]).await;
    assert_eq!(own["playlists"]["playlist"][0]["name"], "Mine");

    // The name is compared without regard to case. An empty value counts as
    // absent.
    for username in [OWNER, "OWNER", ""] {
        let response = server
            .get_json("getPlaylists", &[("username", username)])
            .await;
        assert_eq!(response, own, "{username:?}");
    }
    // The playlists of another user are not served, whether the account
    // exists or not.
    add_user(&server, "guest", "secret").await;
    let message = "username must be the name of the user of the request";
    for username in ["guest", "nobody"] {
        let response = server
            .get_json("getPlaylists", &[("username", username)])
            .await;
        assert_eq!(response, failed(50, message), "{username}");
    }
}

#[tokio::test]
async fn get_playlists_with_authentication_off_returns_the_playlists_of_the_owner() {
    let mut config = Config::default();
    config.auth.disabled_unsafe = true;
    let server = TestServer::bare(config).await;
    let request = [("name", "Mix"), ("v", API_VERSION), ("c", CLIENT)];
    let created = server.get_json_noauth("createPlaylist", &request).await;
    assert_eq!(created["playlist"]["owner"], OWNER);

    // Every request acts as the owner, so every name is the name of the
    // user of the request.
    for username in [OWNER, "anyone"] {
        let request = [("username", username), ("v", API_VERSION), ("c", CLIENT)];
        let listed = server.get_json_noauth("getPlaylists", &request).await;
        assert_eq!(listed["playlists"]["playlist"][0]["name"], "Mix");
    }
}

#[tokio::test]
async fn update_playlist_removes_songs_by_position_and_appends_songs() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let d = server.track.meridian.as_str();
    let e = server.track.contour.as_str();
    let f = server.track.legend.as_str();

    // The positions 0 and 2 of three songs leave the middle one.
    let id = create(&server, "Mix", &[a, b, c]).await;
    let changes = [("songIndexToRemove", "0"), ("songIndexToRemove", "2")];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[b]).await;

    // A new song is the last one.
    assert_eq!(
        update(&server, &id, &[("songIdToAdd", d)]).await,
        ok_empty()
    );
    assert_songs(&server, &id, &[b, d]).await;

    // Several of each in one request. The songs are added in the order of
    // the values, and a song can be added again.
    let changes = [("songIdToAdd", a), ("songIdToAdd", e), ("songIdToAdd", a)];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[b, d, a, e, a]).await;

    // Every position names a song of the playlist as it is before the
    // request: the positions 3 and 1 remove `e` and `d` in either order. The
    // songs of the request follow the songs that are kept.
    let changes = [
        ("songIdToAdd", f),
        ("songIndexToRemove", "1"),
        ("songIdToAdd", c),
        ("songIndexToRemove", "3"),
    ];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[b, a, a, f, c]).await;
    let ascending = [("songIndexToRemove", "1"), ("songIndexToRemove", "3")];
    assert_eq!(update(&server, &id, &ascending).await, ok_empty());
    assert_songs(&server, &id, &[b, a, c]).await;

    // A position that is named twice removes one song.
    let twice = [("songIndexToRemove", "1"), ("songIndexToRemove", "1")];
    assert_eq!(update(&server, &id, &twice).await, ok_empty());
    assert_songs(&server, &id, &[b, c]).await;

    // An empty value counts as absent.
    let empty = [("songIndexToRemove", ""), ("songIndexToRemove", "0")];
    assert_eq!(update(&server, &id, &empty).await, ok_empty());
    assert_songs(&server, &id, &[c]).await;

    // The counts follow the songs.
    let playlist = playlist(&server, &id).await;
    assert_eq!(playlist["songCount"], 1);
    assert_eq!(playlist["duration"], duration(&server, &[c]).await);
}

#[tokio::test]
async fn a_position_never_names_a_song_that_the_same_request_adds() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let id = create(&server, "Mix", &[a, b]).await;

    // Position 1 is `b`, also in a request that adds `c`.
    let changes = [("songIdToAdd", c), ("songIndexToRemove", "1")];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[a, c]).await;

    // The playlist has two songs before the request, so it has no position
    // 2, although the request adds a third song.
    let changes = [("songIdToAdd", b), ("songIndexToRemove", "2")];
    assert_eq!(update(&server, &id, &changes).await, failed(0, NO_POSITION));
    assert_songs(&server, &id, &[a, c]).await;
}

#[tokio::test]
async fn update_playlist_with_a_position_outside_the_playlist_changes_nothing() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let id = create(&server, "Mix", &[a, b, c]).await;
    assert_eq!(
        update(&server, &id, &[("comment", "kept")]).await,
        ok_empty()
    );
    set_times(&server, &id).await;
    let before = server.get_json("getPlaylist", &[("id", &id)]).await;

    // The request names a new name, a new comment, a new `public` value, a
    // song to add and positions to remove. One position is no position of
    // the playlist, so nothing of the request is stored.
    for position in [
        "3",
        "4",
        "100000",
        "-1",
        "1.5",
        "two",
        "99999999999999999999",
    ] {
        let changes = [
            ("name", "Renamed"),
            ("comment", "changed"),
            ("public", "true"),
            ("songIdToAdd", a),
            ("songIndexToRemove", "0"),
            ("songIndexToRemove", position),
            ("songIndexToRemove", "1"),
        ];
        let response = update(&server, &id, &changes).await;
        assert_eq!(response, failed(0, NO_POSITION), "{position}");
        assert_eq!(
            server.get_json("getPlaylist", &[("id", &id)]).await,
            before,
            "{position}"
        );
        assert_songs(&server, &id, &[a, b, c]).await;
    }
    assert_eq!(times(&server, &id).await, (CREATED_AT, CHANGED_AT));

    // A playlist without a song has no position.
    let empty = create(&server, "Empty", &[]).await;
    let response = update(&server, &empty, &[("songIndexToRemove", "0")]).await;
    assert_eq!(response, failed(0, NO_POSITION));

    let request = [("playlistId", id.as_str()), ("songIndexToRemove", "3")];
    assert_eq!(
        server.get_xml("updatePlaylist", &request).await,
        xml_failed(0, NO_POSITION)
    );
}

#[tokio::test]
async fn update_playlist_with_an_unknown_song_changes_nothing() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let id = create(&server, "Mix", &[a, b]).await;
    set_times(&server, &id).await;
    let before = server.get_json("getPlaylist", &[("id", &id)]).await;

    // The unknown ID is the first, a middle or the last value. An ID of
    // another kind, a text that is no ID and an empty value name no song.
    let album = server.album.glass_harbour.as_str();
    let unknown = [
        [UNKNOWN_TRACK, c, a],
        [c, UNKNOWN_TRACK, a],
        [c, a, UNKNOWN_TRACK],
        [c, album, a],
        [c, id.as_str(), a],
        [c, "tr-1", a],
        [c, "' OR 1=1 --", a],
        [c, "", a],
    ];
    for songs in unknown {
        let changes = [
            ("name", "Renamed"),
            ("comment", "changed"),
            ("public", "true"),
            ("songIndexToRemove", "0"),
            ("songIdToAdd", songs[0]),
            ("songIdToAdd", songs[1]),
            ("songIdToAdd", songs[2]),
        ];
        let response = update(&server, &id, &changes).await;
        assert_eq!(response, failed(70, NOT_FOUND), "{songs:?}");
        assert_eq!(
            server.get_json("getPlaylist", &[("id", &id)]).await,
            before,
            "{songs:?}"
        );
        assert_songs(&server, &id, &[a, b]).await;
    }
    assert_eq!(times(&server, &id).await, (CREATED_AT, CHANGED_AT));

    let request = [("playlistId", id.as_str()), ("songIdToAdd", UNKNOWN_TRACK)];
    assert_eq!(
        server.get_xml("updatePlaylist", &request).await,
        xml_failed(70, NOT_FOUND)
    );
}

#[tokio::test]
async fn update_playlist_sets_the_name_the_comment_and_public() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let id = create(&server, "Mix", &[a]).await;
    set_times(&server, &id).await;
    let field = |playlist: &Value, name: &str| playlist.get(name).cloned();

    // A new playlist has no comment and is not public.
    let fresh = playlist(&server, &id).await;
    assert_eq!(field(&fresh, "comment"), None);
    assert_eq!(field(&fresh, "public"), Some(json!(false)));
    assert_eq!(fresh["changed"], "2023-03-15T00:00:00Z");

    // Each value alone. The others stay, and so do the songs.
    let before = unix_now();
    assert_eq!(
        update(&server, &id, &[("name", "Rock & \"Roll\" <live>")]).await,
        ok_empty()
    );
    let renamed = playlist(&server, &id).await;
    assert_eq!(renamed["name"], "Rock & \"Roll\" <live>");
    assert_eq!(field(&renamed, "comment"), None);
    assert_eq!(renamed["public"], false);
    // The change time is the time of the request. The creation time stays.
    let (created_at, changed_at) = times(&server, &id).await;
    assert_eq!(created_at, CREATED_AT);
    assert!((before..=unix_now()).contains(&changed_at), "{changed_at}");
    assert_eq!(renamed["created"], "2023-03-14T17:51:22Z");
    assert_eq!(renamed["changed"], model::iso8601(changed_at));

    assert_eq!(
        update(&server, &id, &[("comment", "Summer 2023")]).await,
        ok_empty()
    );
    let commented = playlist(&server, &id).await;
    assert_eq!(commented["comment"], "Summer 2023");
    assert_eq!(commented["name"], "Rock & \"Roll\" <live>");
    assert_eq!(commented["public"], false);

    for (value, public) in [("true", true), ("FALSE", false), ("True", true)] {
        assert_eq!(update(&server, &id, &[("public", value)]).await, ok_empty());
        let playlist = playlist(&server, &id).await;
        assert_eq!(playlist["public"], public, "{value}");
        assert_eq!(playlist["comment"], "Summer 2023");
    }

    // In XML the name is escaped.
    let body = server.get_xml("getPlaylist", &[("id", &id)]).await;
    let name = r#" name="Rock &amp; &quot;Roll&quot; &lt;live&gt;" "#;
    assert!(body.contains(name), "{body}");
    assert!(body.contains(r#" comment="Summer 2023" "#), "{body}");
    assert!(body.contains(r#" public="true" "#), "{body}");

    // The three values in one request.
    let changes = [
        ("name", "Mix 2"),
        ("comment", "Autumn"),
        ("public", "false"),
    ];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    let all = playlist(&server, &id).await;
    assert_eq!(
        (&all["name"], &all["comment"], &all["public"]),
        (&json!("Mix 2"), &json!("Autumn"), &json!(false))
    );

    // An empty name keeps the name. An empty comment removes the comment.
    // An empty `public` counts as absent.
    let changes = [("name", ""), ("comment", ""), ("public", "")];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    let cleared = playlist(&server, &id).await;
    assert_eq!(cleared["name"], "Mix 2");
    assert_eq!(field(&cleared, "comment"), None);
    assert_eq!(cleared["public"], false);
    assert_songs(&server, &id, &[a]).await;

    // A `public` value that is no boolean is error 0 and changes nothing.
    let changes = [("name", "Mix 3"), ("public", "yes")];
    assert_eq!(
        update(&server, &id, &changes).await,
        failed(0, "parameter public must be true or false")
    );
    assert_eq!(playlist(&server, &id).await, cleared);
}

#[tokio::test]
async fn update_playlist_without_a_change_keeps_the_change_time() {
    let server = TestServer::standard().await;
    let id = create(&server, "Mix", &[&server.track.low_tide]).await;
    set_times(&server, &id).await;
    let before = playlist(&server, &id).await;

    // The request names nothing to change. An empty name names no name.
    assert_eq!(update(&server, &id, &[]).await, ok_empty());
    assert_eq!(update(&server, &id, &[("name", "")]).await, ok_empty());
    assert_eq!(playlist(&server, &id).await, before);
    assert_eq!(times(&server, &id).await, (CREATED_AT, CHANGED_AT));

    // The answer of a change is the empty response, in XML too.
    let request = [("playlistId", id.as_str()), ("name", "Mix 2")];
    assert_eq!(
        server.get_xml("updatePlaylist", &request).await,
        xml_ok_empty()
    );
    assert_ne!(times(&server, &id).await.1, CHANGED_AT);
}

#[tokio::test]
async fn create_playlist_with_a_playlist_id_replaces_the_songs() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let id = create(&server, "Mix", &[a, b]).await;
    let changes = [("comment", "kept"), ("public", "true")];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    set_times(&server, &id).await;

    // The new list replaces the old one, in the order of the request. The
    // request names a name, which is not read.
    let before = unix_now();
    let request = [
        ("playlistId", id.as_str()),
        ("name", "Renamed"),
        ("songId", c),
        ("songId", a),
        ("songId", a),
    ];
    let replaced = server.get_json("createPlaylist", &request).await;
    let (created_at, changed_at) = times(&server, &id).await;
    assert_eq!(created_at, CREATED_AT);
    assert!((before..=unix_now()).contains(&changed_at), "{changed_at}");

    // The answer is the playlist with its new songs. The name, the comment,
    // `public` and the creation time stay.
    let element = json!({
        "id": id,
        "name": "Mix",
        "comment": "kept",
        "owner": OWNER,
        "public": true,
        "songCount": 3,
        "duration": duration(&server, &[c, a, a]).await,
        "created": "2023-03-14T17:51:22Z",
        "changed": model::iso8601(changed_at),
        "entry": [
            song(&server, c).await,
            song(&server, a).await,
            song(&server, a).await,
        ],
    });
    assert_eq!(replaced, ok("playlist", element.clone()));
    assert_eq!(
        server.get_json("getPlaylist", &[("id", &id)]).await,
        ok("playlist", element)
    );
    assert_songs(&server, &id, &[c, a, a]).await;
    // The request made no second playlist.
    assert_eq!(count_rows(&server, "playlists").await, 1);

    // A request without a song leaves no song.
    let request = [("playlistId", id.as_str())];
    let emptied = server.get_json("createPlaylist", &request).await;
    assert_eq!(emptied["playlist"]["songCount"], 0);
    assert_eq!(emptied["playlist"]["entry"], json!([]));
    assert_eq!(emptied["playlist"]["name"], "Mix");
    assert_songs(&server, &id, &[]).await;
}

#[tokio::test]
async fn create_playlist_that_is_refused_writes_nothing() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let id = create(&server, "Mix", &[a, b]).await;
    set_times(&server, &id).await;
    let before = server.get_json("getPlaylist", &[("id", &id)]).await;

    // An unknown song: no new playlist, and the old one keeps its songs.
    let album = server.album.glass_harbour.as_str();
    for unknown in [UNKNOWN_TRACK, album, "tr-1", ""] {
        let request = [("name", "New"), ("songId", a), ("songId", unknown)];
        let response = server.get_json("createPlaylist", &request).await;
        assert_eq!(response, failed(70, NOT_FOUND), "{unknown:?}");

        let request = [
            ("playlistId", id.as_str()),
            ("songId", b),
            ("songId", unknown),
            ("songId", a),
        ];
        let response = server.get_json("createPlaylist", &request).await;
        assert_eq!(response, failed(70, NOT_FOUND), "{unknown:?}");
    }
    // The ID of no playlist: an unknown one and a value of another shape.
    for unknown in [UNKNOWN_PLAYLIST, a, "pl-1", "1"] {
        let request = [("playlistId", unknown), ("name", "New"), ("songId", a)];
        let response = server.get_json("createPlaylist", &request).await;
        assert_eq!(response, failed(70, NOT_FOUND), "{unknown}");
    }
    // Neither a playlist nor a name. An empty value counts as absent.
    let missing = failed(10, "required parameter is missing: name");
    for request in [
        vec![("songId", a)],
        vec![("name", ""), ("songId", a)],
        vec![("playlistId", ""), ("songId", a)],
        vec![("playlistId", ""), ("name", "")],
    ] {
        let response = server.get_json("createPlaylist", &request).await;
        assert_eq!(response, missing, "{request:?}");
    }

    assert_eq!(count_rows(&server, "playlists").await, 1);
    assert_eq!(server.get_json("getPlaylist", &[("id", &id)]).await, before);
    assert_songs(&server, &id, &[a, b]).await;

    // An empty `playlistId` with a name creates a playlist.
    let request = [("playlistId", ""), ("name", "New"), ("songId", b)];
    let response = server.get_json("createPlaylist", &request).await;
    assert_eq!(response["playlist"]["name"], "New");
    assert_eq!(count_rows(&server, "playlists").await, 2);
}

#[tokio::test]
async fn delete_playlist_then_get_playlist_is_error_70() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let id = create(&server, "Mix", &[a, b, a]).await;
    let other = create(&server, "Other", &[b]).await;

    assert_eq!(
        server.get_json("deletePlaylist", &[("id", &id)]).await,
        ok_empty()
    );
    assert_eq!(
        server.get_json("getPlaylist", &[("id", &id)]).await,
        failed(70, NOT_FOUND)
    );
    assert_eq!(
        server.get_xml("getPlaylist", &[("id", &id)]).await,
        xml_failed(70, NOT_FOUND)
    );
    // The entries are gone with the playlist. The other playlist and the
    // songs are as they were.
    assert_eq!(stored(&server, &id).await, []);
    assert_songs(&server, &other, &[b]).await;
    let listed = server.get_json("getPlaylists", &[]).await;
    assert_eq!(listed["playlists"]["playlist"][0]["id"], other.as_str());
    assert_eq!(
        listed["playlists"]["playlist"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(song(&server, a).await["id"], a);
    assert_eq!(count_rows(&server, "tracks").await, 10);

    // The ID names no playlist any more.
    assert_eq!(
        server.get_json("deletePlaylist", &[("id", &id)]).await,
        failed(70, NOT_FOUND)
    );
    assert_eq!(
        update(&server, &id, &[("name", "Back")]).await,
        failed(70, NOT_FOUND)
    );
    let request = [("playlistId", id.as_str()), ("songId", a)];
    assert_eq!(
        server.get_json("createPlaylist", &request).await,
        failed(70, NOT_FOUND)
    );
    assert_eq!(count_rows(&server, "playlists").await, 1);

    // The answer of `deletePlaylist` in XML is the empty response.
    assert_eq!(
        server.get_xml("deletePlaylist", &[("id", &other)]).await,
        xml_ok_empty()
    );
    assert_eq!(count_rows(&server, "playlists").await, 0);
    assert_eq!(count_rows(&server, "playlist_tracks").await, 0);
}

#[tokio::test]
async fn a_required_id_that_is_absent_is_error_10_and_one_of_no_playlist_is_error_70() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let id = create(&server, "Mix", &[track]).await;

    for (method, parameter) in [
        ("getPlaylist", "id"),
        ("updatePlaylist", "playlistId"),
        ("deletePlaylist", "id"),
    ] {
        let message = format!("required parameter is missing: {parameter}");
        assert_eq!(
            server.get_json(method, &[("name", "x")]).await,
            failed(10, &message),
            "{method}"
        );
        assert_eq!(
            server.get_xml(method, &[]).await,
            xml_failed(10, &message),
            "{method}"
        );

        // An unknown ID, the ID of an item of another kind, and values that
        // do not have the shape of an ID.
        let upper = id.to_uppercase();
        let values = [
            UNKNOWN_PLAYLIST,
            track,
            server.album.glass_harbour.as_str(),
            "",
            "1",
            "pl-",
            "pl-1",
            "' OR 1=1 --",
            upper.as_str(),
        ];
        for value in values {
            let request = [(parameter, value), ("name", "x")];
            assert_eq!(
                server.get_json(method, &request).await,
                failed(70, NOT_FOUND),
                "{method} {value:?}"
            );
            assert_eq!(
                server.get_xml(method, &request).await,
                xml_failed(70, NOT_FOUND),
                "{method} {value:?}"
            );
        }
    }
    // No request changed or deleted the playlist.
    assert_eq!(playlist(&server, &id).await["name"], "Mix");
    assert_songs(&server, &id, &[track]).await;
}

#[tokio::test]
async fn playlist_methods_answer_a_post_with_a_form_body() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.night_ferry.as_str();
    let json_of = |response| async move {
        assert_eq!(content_type(&response), "application/json");
        subsonic_response(&body_string(response).await)
    };

    let form = [
        ("name", "Posted"),
        ("songId", b),
        ("songId", a),
        ("songId", b),
        ("f", "json"),
    ];
    let created = json_of(server.post_form("createPlaylist.view", &form).await).await;
    let id = created["playlist"]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();
    assert_songs(&server, &id, &[b, a, b]).await;

    let form = [
        ("playlistId", id.as_str()),
        ("songIndexToRemove", "2"),
        ("songIndexToRemove", "0"),
        ("songIdToAdd", c),
        ("comment", "From a form"),
        ("f", "json"),
    ];
    let updated = json_of(server.post_form("updatePlaylist", &form).await).await;
    assert_eq!(updated, ok_empty());
    assert_songs(&server, &id, &[a, c]).await;

    let form = [("id", id.as_str()), ("f", "json")];
    let read = json_of(server.post_form("getPlaylist", &form).await).await;
    assert_eq!(read["playlist"]["comment"], "From a form");
    let listed = json_of(server.post_form("getPlaylists", &[("f", "json")]).await).await;
    assert_eq!(listed["playlists"]["playlist"][0]["id"], id.as_str());
    let deleted = json_of(server.post_form("deletePlaylist", &form).await).await;
    assert_eq!(deleted, ok_empty());
    assert_eq!(count_rows(&server, "playlists").await, 0);
}

#[tokio::test]
async fn a_request_with_more_songs_than_a_playlist_holds_is_error_0() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let id = create(&server, "Mix", &[track]).await;
    let auth = format!("u={OWNER}&p={OWNER_PASSWORD}&v={API_VERSION}&c={CLIENT}&f=json");
    // One value more than a playlist holds, in a form body of about 4.4 MB.
    let values = |name: &str| format!("{name}={track}&").repeat(MAX_ENTRIES + 1);

    let body = format!("{}name=Big&{auth}", values("songId"));
    let response = post_body(&server, "createPlaylist", body).await;
    assert_eq!(response, failed(0, TOO_LONG));

    let body = format!("{}playlistId={id}&name=Big&{auth}", values("songIdToAdd"));
    let response = post_body(&server, "updatePlaylist", body).await;
    assert_eq!(response, failed(0, TOO_LONG));

    // No request wrote a row.
    assert_eq!(count_rows(&server, "playlists").await, 1);
    assert_eq!(playlist(&server, &id).await["name"], "Mix");
    assert_songs(&server, &id, &[track]).await;
}

#[tokio::test]
async fn the_songs_of_a_playlist_carry_what_the_user_attached_to_them() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let id = create(&server, "Mix", &[a, b, a]).await;
    let starred = a.to_owned();
    server
        .db()
        .write(move |conn| {
            conn.execute(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 SELECT id, ?1, 'track', ?2, 4, 7, ?3 FROM users WHERE name = ?4",
                params![starred, CREATED_AT, CHANGED_AT, OWNER],
            )?;
            Ok(())
        })
        .await
        .expect("write the annotation");

    let playlist = playlist(&server, &id).await;
    for place in [0, 2] {
        let entry = &playlist["entry"][place];
        assert_eq!(entry["starred"], "2023-03-14T17:51:22Z");
        assert_eq!(entry["userRating"], 4);
        assert_eq!(entry["playCount"], 7);
        assert_eq!(entry["played"], "2023-03-15T00:00:00Z");
        assert_eq!(*entry, song(&server, a).await);
    }
    let other = &playlist["entry"][1];
    assert_eq!(other.get("starred"), None);
    assert_eq!(other["playCount"], 0);
}

#[tokio::test]
async fn the_cover_of_a_playlist_is_the_cover_of_its_first_song() {
    let server = TestServer::standard().await;
    // The album of `low_tide` has a cover file, the album of `upstream` an
    // embedded picture, and the album of `meridian` no cover.
    let low_tide = server.track.low_tide.as_str();
    let upstream = server.track.upstream.as_str();
    let meridian = server.track.meridian.as_str();
    let cover = |playlist: &Value| playlist.get("coverArt").cloned();
    /// The first element of `getPlaylists`.
    async fn listed(server: &TestServer) -> Value {
        let mut response = server.get_json("getPlaylists", &[]).await;
        response["playlists"]["playlist"][0].take()
    }

    let id = create(&server, "Mix", &[low_tide, meridian, upstream]).await;
    let glass_harbour = Some(json!(server.album.glass_harbour));
    assert_eq!(cover(&playlist(&server, &id).await), glass_harbour);
    assert_eq!(cover(&listed(&server).await), glass_harbour);

    // The first song has no cover: the playlist has none, although a later
    // song has one.
    assert_eq!(
        update(&server, &id, &[("songIndexToRemove", "0")]).await,
        ok_empty()
    );
    assert_eq!(cover(&playlist(&server, &id).await), None);
    assert_eq!(cover(&listed(&server).await), None);

    assert_eq!(
        update(&server, &id, &[("songIndexToRemove", "0")]).await,
        ok_empty()
    );
    let slow_rivers = Some(json!(server.album.slow_rivers));
    assert_eq!(cover(&playlist(&server, &id).await), slow_rivers);
    assert_eq!(cover(&listed(&server).await), slow_rivers);
}

#[tokio::test]
async fn a_user_reads_and_changes_only_the_playlists_of_that_user() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    add_user(&server, "guest", "secret").await;
    /// `request` as a request of the guest.
    fn guest<'a>(request: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        as_user("guest", "secret", request)
    }

    // One playlist of each user. The one of the owner is public.
    let private = create(&server, "Private", &[a, b]).await;
    let public = create(&server, "Public", &[b]).await;
    assert_eq!(
        update(&server, &public, &[("public", "true")]).await,
        ok_empty()
    );
    let request = guest(&[("name", "Guest list"), ("songId", b)]);
    let created = server.get_json_noauth("createPlaylist", &request).await;
    // `owner` is the name of the user that created the playlist.
    assert_eq!(created["playlist"]["owner"], "guest");
    let of_guest = created["playlist"]["id"]
        .as_str()
        .expect("an ID")
        .to_owned();
    assert_eq!(playlist(&server, &private).await["owner"], OWNER);

    // Each user lists the playlists of that user.
    let names = |response: &Value| -> Vec<String> {
        let list = response["playlists"]["playlist"]
            .as_array()
            .expect("a list");
        let name = |item: &Value| item["name"].as_str().expect("a name").to_owned();
        list.iter().map(name).collect()
    };
    let listed = server.get_json_noauth("getPlaylists", &guest(&[])).await;
    assert_eq!(names(&listed), ["Guest list"]);
    let listed = server.get_json("getPlaylists", &[]).await;
    assert_eq!(names(&listed), ["Private", "Public"]);

    // The guest gets error 70 for a playlist of the owner from every method,
    // also for the public one, and changes nothing.
    let before = server.get_json("getPlaylists", &[]).await;
    for id in [private.as_str(), public.as_str()] {
        let requests: [(&str, Vec<(&str, &str)>); 6] = [
            ("getPlaylist", vec![("id", id)]),
            (
                "updatePlaylist",
                vec![("playlistId", id), ("name", "Taken")],
            ),
            (
                "updatePlaylist",
                vec![("playlistId", id), ("songIndexToRemove", "0")],
            ),
            (
                "updatePlaylist",
                vec![("playlistId", id), ("songIdToAdd", a)],
            ),
            ("createPlaylist", vec![("playlistId", id), ("songId", a)]),
            ("deletePlaylist", vec![("id", id)]),
        ];
        for (method, request) in &requests {
            let response = server.get_json_noauth(method, &guest(request)).await;
            assert_eq!(response, failed(70, NOT_FOUND), "{method} {request:?}");
        }
    }
    assert_eq!(server.get_json("getPlaylists", &[]).await, before);
    assert_songs(&server, &private, &[a, b]).await;
    assert_songs(&server, &public, &[b]).await;

    // The owner gets the same answer for the playlist of the guest.
    assert_eq!(
        server.get_json("getPlaylist", &[("id", &of_guest)]).await,
        failed(70, NOT_FOUND)
    );
    assert_eq!(
        server
            .get_json("deletePlaylist", &[("id", &of_guest)])
            .await,
        failed(70, NOT_FOUND)
    );
    let request = guest(&[("id", of_guest.as_str())]);
    let read = server.get_json_noauth("getPlaylist", &request).await;
    assert_eq!(read["playlist"]["name"], "Guest list");
    assert_eq!(read["playlist"]["entry"][0]["id"], b);
}

#[tokio::test]
async fn a_playlist_keeps_its_songs_through_a_rescan_and_a_moved_file() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let songs = [b, a, c, a];
    let id = create(&server, "Mix", &songs).await;
    set_times(&server, &id).await;
    let before = server.get_json("getPlaylist", &[("id", &id)]).await;

    // A scan that finds nothing new writes no playlist row.
    let summary = server.rescan().await;
    assert_eq!((summary.seen, summary.removed), (10, 0));
    assert_eq!(server.get_json("getPlaylist", &[("id", &id)]).await, before);
    assert_songs(&server, &id, &songs).await;

    // The file of `a` moves to another directory under another name. The
    // track keeps its ID, so the playlist keeps its two entries of it.
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

    assert_songs(&server, &id, &songs).await;
    let playlist = playlist(&server, &id).await;
    assert_eq!(playlist["songCount"], 4);
    assert_eq!(playlist["entry"][1]["path"], new);
    assert_eq!(playlist["entry"][3]["path"], new);
    assert_eq!(playlist["entry"][1], song(&server, a).await);
    // The scan is no change of the playlist.
    assert_eq!(times(&server, &id).await, (CREATED_AT, CHANGED_AT));
}

#[tokio::test]
async fn a_track_without_a_file_stays_in_the_playlist_and_is_listed() {
    let server = TestServer::standard().await;
    let a = server.track.upstream.as_str();
    let b = server.track.delta.as_str();
    let songs = [a, b, a];
    let id = create(&server, "Mix", &songs).await;
    let with_file = playlist(&server, &id).await;
    assert_eq!(
        with_file["entry"][0]["path"],
        "Marlow Quartet/Slow Rivers/01 Upstream.m4a"
    );

    remove_file(&server, a).await;

    // The track keeps its entries, its place and its tags. Its element has
    // no `path`, as the `song` element of `getSong` has none.
    assert_songs(&server, &id, &songs).await;
    let playlist = playlist(&server, &id).await;
    for place in [0, 2] {
        let entry = &playlist["entry"][place];
        assert_eq!(entry["id"], a);
        assert_eq!(entry["title"], "Upstream");
        assert_eq!(entry.get("path"), None);
        assert_eq!(*entry, song(&server, a).await);
    }
    assert_eq!(playlist["entry"][1], with_file["entry"][1]);
    // The counts and the cover are those from before.
    assert_eq!(playlist["songCount"], 3);
    assert_eq!(playlist["duration"], with_file["duration"]);
    assert_eq!(playlist["coverArt"], with_file["coverArt"]);
    let body = server.get_xml("getPlaylist", &[("id", &id)]).await;
    assert_eq!(body.matches("<entry ").count(), 3);
    assert!(!body.contains(&format!("/{a}")), "{body}");

    // The track is added and removed as every other track.
    let changes = [("songIndexToRemove", "0"), ("songIdToAdd", a)];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[b, a, a]).await;
    let other = create(&server, "Other", &[a]).await;
    assert_songs(&server, &other, &[a]).await;
}

#[tokio::test]
async fn a_track_that_a_scan_deletes_leaves_the_playlist_and_the_positions_follow() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let d = server.track.contour.as_str();
    let id = create(&server, "Mix", &[a, b, a, c, b]).await;
    let only_b = create(&server, "Only B", &[b, b]).await;
    set_times(&server, &id).await;

    // The file of `b` is deleted, and the scan deletes the track row.
    fs::remove_file(server.path("Aurora Field/Glass Harbour/02 Breakwater.flac"))
        .expect("delete the file");
    let summary = server.rescan().await;
    assert_eq!((summary.seen, summary.removed), (9, 1));
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!(
        server.get_json("getSong", &[("id", b)]).await,
        failed(70, NOT_FOUND)
    );

    // The two entries of `b` are gone. The other songs keep their order,
    // and the counts follow the list.
    assert_eq!(entry_ids(&server, &id).await, [a, a, c]);
    let playlist = playlist(&server, &id).await;
    assert_eq!(playlist["songCount"], 3);
    assert_eq!(playlist["duration"], duration(&server, &[a, a, c]).await);
    let listed = server.get_json("getPlaylists", &[]).await;
    let listed = &listed["playlists"]["playlist"];
    assert_eq!(listed[0]["songCount"], 3);
    assert_eq!(listed[0]["duration"], playlist["duration"]);
    // The playlist that held no other song stays, without a song.
    assert_eq!(listed[1]["id"], only_b.as_str());
    assert_eq!(listed[1]["songCount"], 0);
    assert_eq!(listed[1].get("coverArt"), None);
    assert_eq!(entry_ids(&server, &only_b).await, [] as [&str; 0]);
    // The scan is no change of the playlist.
    assert_eq!(times(&server, &id).await, (CREATED_AT, CHANGED_AT));

    // The positions of the API are the places in the list: 0, 1 and 2. The
    // playlist has no position 3, which was the stored position of `c`.
    let response = update(&server, &id, &[("songIndexToRemove", "3")]).await;
    assert_eq!(response, failed(0, NO_POSITION));
    assert_eq!(entry_ids(&server, &id).await, [a, a, c]);

    // Position 1 is the second `a`. The write stores the positions 0 to
    // n - 1 again.
    let changes = [("songIndexToRemove", "1"), ("songIdToAdd", d)];
    assert_eq!(update(&server, &id, &changes).await, ok_empty());
    assert_songs(&server, &id, &[a, c, d]).await;

    // A song is added to the playlist that the scan emptied.
    assert_eq!(
        update(&server, &only_b, &[("songIdToAdd", a)]).await,
        ok_empty()
    );
    assert_songs(&server, &only_b, &[a]).await;
}

#[tokio::test]
async fn a_song_is_added_after_the_songs_that_a_scan_left() {
    let server = TestServer::standard_with_own_files().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();
    let c = server.track.meridian.as_str();
    let d = server.track.contour.as_str();
    let id = create(&server, "Mix", &[a, b, c]).await;

    fs::remove_file(server.path("Aurora Field/Glass Harbour/02 Breakwater.flac"))
        .expect("delete the file");
    assert_eq!(server.rescan().await.removed, 1);
    // The rows keep the positions they had: the row of `b` left a gap.
    let rows = stored(&server, &id).await;
    assert_eq!(rows, [(0, a.to_owned()), (2, c.to_owned())]);

    // A request that only adds a song closes the gap as well.
    assert_eq!(
        update(&server, &id, &[("songIdToAdd", d)]).await,
        ok_empty()
    );
    assert_songs(&server, &id, &[a, c, d]).await;
}

#[tokio::test]
async fn two_calls_return_the_same_document() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.open_road.as_str();
    let id = create(&server, "Mix", &[b, a, b]).await;
    create(&server, "Mix", &[a]).await;
    create(&server, "mix", &[]).await;

    let calls: [(&str, Vec<(&str, &str)>); 2] =
        [("getPlaylists", vec![]), ("getPlaylist", vec![("id", &id)])];
    for (method, request) in &calls {
        let first = server.get_xml(method, request).await;
        assert!(first.contains(r#" status="ok" "#), "{method}: {first}");
        for _ in 0..3 {
            assert_eq!(server.get_xml(method, request).await, first, "{method}");
        }
        let first = server.get_json(method, request).await;
        assert_eq!(server.get_json(method, request).await, first, "{method}");
    }
}
