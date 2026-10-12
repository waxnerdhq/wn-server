//! Router tests of `scrobble` and of the plays that the browse methods and
//! the album lists report (`docs/design.md` section 4).
//!
//! A play is data of the owner. The tests check what a request counts, what
//! a failed request and a "now playing" notification leave untouched, what
//! the other methods report afterwards, and what a rescan keeps.

mod common;

use std::fs;

use lofty::tag::ItemKey;
use serde_json::{Value, json};
use wn_server::api::model;

use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, StandardServer, TestServer,
    body_string, content_type, error, form_request, get_album, get_song, is_time_between, owner_id,
    remove_file, retag, rows, subsonic_response, unix_now, xml_document,
};

/// IDs of the three kinds that no row has.
const NO_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";
const NO_ALBUM: &str = "al-ffffffffffffffffffffffffffffffff";
const NO_ARTIST: &str = "ar-ffffffffffffffffffffffffffffffff";

/// The ID of a playlist. No playlist is played.
const PLAYLIST: &str = "pl-00000000000000000000000000000001";

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// The message of error 10 for a request without `id`.
const MISSING_ID: &str = "required parameter is missing: id";

/// The message of the error for a `time` that is no integer of milliseconds
/// from 1970 to the end of the year 9999.
const TIME_RANGE: &str = "parameter time must be an integer from 0 to 253402300799999";

/// The message of the error for a request whose `time` values are fewer or
/// more than its `id` values.
const TIME_COUNT: &str = "parameter time must occur once for each id or not at all";

/// The message of the error for a `submission` that is no boolean.
const SUBMISSION: &str = "parameter submission must be true or false";

/// Times of plays in Unix seconds, with the text that the models write for
/// each.
const IN_2017: i64 = 1_500_000_000;
const IN_2017_TEXT: &str = "2017-07-14T02:40:00Z";
const IN_2020: i64 = 1_600_000_000;
const IN_2020_TEXT: &str = "2020-09-13T12:26:40Z";
const IN_2023: i64 = 1_700_000_000;
const IN_2023_TEXT: &str = "2023-11-14T22:13:20Z";

/// The two album list methods with the key of the element in the answer.
const ALBUM_METHODS: [(&str, &str); 2] = [
    ("getAlbumList2", "albumList2"),
    ("getAlbumList", "albumList"),
];

/// The value of `time` for a play at `seconds`: the time in milliseconds.
fn ms(seconds: i64) -> String {
    (seconds * 1000).to_string()
}

/// The play count and the time of the last play that the row of the owner
/// for the track `id` holds. `None` for a track without a row.
async fn stored_plays(server: &TestServer, id: &str) -> Option<(i64, Option<i64>)> {
    let owner = owner_id(server).await;
    rows(server)
        .await
        .into_iter()
        .find(|row| row.0 == owner && row.1 == id)
        .map(|row| (row.5, row.6))
}

/// Sends `scrobble` with `params` and checks that the answer is the empty
/// response: the status `ok` and the five fields of the envelope.
async fn ok(server: &TestServer, params: &[(&str, &str)]) {
    let response = server.get_json("scrobble", params).await;
    assert_eq!(response["status"], "ok", "{params:?}: {response}");
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(5),
        "{params:?}: {response}"
    );
}

/// Sends `scrobble` with `params` and checks that the answer is the error
/// `code` with `message`, and that the envelope holds nothing else.
async fn fails(server: &TestServer, params: &[(&str, &str)], code: u32, message: &str) {
    let response = server.get_json("scrobble", params).await;
    assert_eq!(response["status"], "failed", "{params:?}: {response}");
    assert_eq!(response["error"], error(code, message), "{params:?}");
    assert_eq!(
        response.as_object().map(serde_json::Map::len),
        Some(6),
        "{params:?}: {response}"
    );
}

/// Reports one play of the track `id` at the time `seconds`.
async fn play_at(server: &TestServer, id: &str, seconds: i64) {
    ok(server, &[("id", id), ("time", &ms(seconds))]).await;
}

/// `playCount` and `played` of `element`. `played` is `None` for an element
/// without the field.
fn plays_of(element: &Value) -> (u64, Option<&str>) {
    let count = element["playCount"]
        .as_u64()
        .unwrap_or_else(|| panic!("no playCount: {element}"));
    (count, element.get("played").and_then(Value::as_str))
}

/// The `album` list of the album list `list` of `method`, whose element has
/// the key `key`. `params` are added to `type`.
async fn album_list(
    server: &TestServer,
    (method, key): (&str, &str),
    list: &str,
    params: &[(&str, &str)],
) -> Vec<Value> {
    let mut all = vec![("type", list)];
    all.extend_from_slice(params);
    let response = server.get_json(method, &all).await;
    assert_eq!(response["status"], "ok", "{method} {list}: {response}");
    response[key]["album"]
        .as_array()
        .unwrap_or_else(|| panic!("{method} {list} has no album list: {response}"))
        .clone()
}

/// The IDs of the albums of `getAlbumList2` with the type `list`.
async fn listed(server: &TestServer, list: &str) -> Vec<String> {
    ids(&album_list(server, ALBUM_METHODS[0], list, &[]).await)
}

/// The value of `id` of each element.
fn ids(elements: &[Value]) -> Vec<String> {
    elements
        .iter()
        .map(|element| {
            element["id"]
                .as_str()
                .unwrap_or_else(|| panic!("no id: {element}"))
                .to_owned()
        })
        .collect()
}

/// Inserts a second account, `second` with the password `walnut`, and
/// returns the parameters that authenticate a request as that account.
async fn second_user(server: &TestServer) -> [(&'static str, &'static str); 4] {
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
        .expect("insert the second user");
    [
        ("u", "second"),
        ("p", "walnut"),
        ("v", API_VERSION),
        ("c", CLIENT),
    ]
}

#[tokio::test]
async fn two_scrobbles_of_one_track_give_play_count_2_and_put_its_album_first() {
    let server = TestServer::standard().await;
    let low_tide = server.track.low_tide.as_str();
    let glass_harbour = server.album.glass_harbour.as_str();
    let slow_rivers = server.album.slow_rivers.as_str();

    // Another album has one play from before. It is the one album of both
    // lists.
    play_at(&server, &server.track.upstream, IN_2023).await;
    for list in ["frequent", "recent"] {
        assert_eq!(listed(&server, list).await, [slow_rivers], "{list}");
    }

    let before = unix_now();
    ok(&server, &[("id", low_tide)]).await;
    ok(&server, &[("id", low_tide)]).await;
    let after = unix_now();

    let song = get_song(&server, low_tide).await;
    assert_eq!(song["playCount"], 2);
    assert!(is_time_between(&song["played"], before, after), "{song}");
    assert_eq!(
        stored_plays(&server, low_tide).await.map(|plays| plays.0),
        Some(2)
    );

    // The album of the track is the first of both lists, in both forms.
    for method in ALBUM_METHODS {
        for list in ["frequent", "recent"] {
            let albums = album_list(&server, method, list, &[]).await;
            assert_eq!(ids(&albums), [glass_harbour, slow_rivers], "{method:?}");
            assert_eq!(albums[0]["playCount"], 2, "{method:?} {list}");
            assert_eq!(albums[0]["played"], song["played"], "{method:?} {list}");
            assert_eq!(
                plays_of(&albums[1]),
                (1, Some(IN_2023_TEXT)),
                "{method:?} {list}"
            );
        }
    }
}

#[tokio::test]
async fn a_scrobble_counts_one_play_in_the_row_of_the_track() {
    let server = TestServer::standard().await;
    let owner = owner_id(&server).await;
    let track = server.track.meridian.as_str();
    assert_eq!(rows(&server).await, []);
    assert_eq!(plays_of(&get_song(&server, track).await), (0, None));

    let before = unix_now();
    ok(&server, &[("id", track)]).await;
    let after = unix_now();

    // One row, of the track. The album and the artist get none.
    let stored = rows(&server).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    let (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at) =
        stored[0].clone();
    assert_eq!(
        (user_id, item_id.as_str(), item_type.as_str()),
        (owner, track, "track")
    );
    assert_eq!((starred_at, rating, play_count), (None, None, 1));
    let last_played_at = last_played_at.expect("the play has a time");
    assert!(
        (before..=after).contains(&last_played_at),
        "{last_played_at}"
    );

    let song = get_song(&server, track).await;
    assert_eq!(
        plays_of(&song),
        (1, Some(model::iso8601(last_played_at).as_str()))
    );
    for absent in ["starred", "userRating"] {
        assert!(song.get(absent).is_none(), "{song}");
    }
}

#[tokio::test]
async fn scrobble_answers_with_the_empty_response_in_xml_and_for_the_view_suffix() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let empty = xml_document("ok", "/>");

    assert_eq!(server.get_xml("scrobble", &[("id", track)]).await, empty);
    assert_eq!(
        server
            .get_xml("scrobble.view", &[("id", track), ("time", &ms(IN_2020))])
            .await,
        empty
    );
    assert_eq!(
        server
            .get_xml("scrobble", &[("id", track), ("submission", "false")])
            .await,
        empty
    );
    assert_eq!(
        stored_plays(&server, track).await.map(|plays| plays.0),
        Some(2)
    );

    // The song reports its plays as attributes.
    let xml = server.get_xml("getSong", &[("id", track)]).await;
    assert!(xml.contains(r#" playCount="2" played=""#), "{xml}");

    assert_eq!(
        server.get_xml("scrobble", &[("id", NO_TRACK)]).await,
        xml_document(
            "failed",
            r#"><error code="70" message="not found"/></subsonic-response>"#
        )
    );
    assert_eq!(
        server
            .get_xml("scrobble", &[("id", track), ("time", "soon")])
            .await,
        xml_document(
            "failed",
            &format!(r#"><error code="0" message="{TIME_RANGE}"/></subsonic-response>"#)
        )
    );
    assert_eq!(
        stored_plays(&server, track).await.map(|plays| plays.0),
        Some(2)
    );
}

#[tokio::test]
async fn submission_false_is_accepted_and_counts_nothing() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let other = server.track.delta.as_str();

    // A "now playing" notification for a track without a play writes no
    // row.
    ok(&server, &[("id", track), ("submission", "false")]).await;
    assert_eq!(rows(&server).await, []);
    assert_eq!(plays_of(&get_song(&server, track).await), (0, None));

    // After a play, the notification leaves the count and the time.
    play_at(&server, track, IN_2020).await;
    let before = rows(&server).await;
    for params in [
        vec![("id", track), ("submission", "false")],
        vec![("id", track), ("submission", "FALSE")],
        vec![("submission", "False"), ("id", track), ("id", other)],
        vec![("id", track), ("submission", "false"), ("time", "1")],
    ] {
        ok(&server, &params).await;
        assert_eq!(rows(&server).await, before, "{params:?}");
    }
    let later = ms(unix_now());
    ok(
        &server,
        &[("id", track), ("time", &later), ("submission", "false")],
    )
    .await;
    assert_eq!(rows(&server).await, before);
    assert_eq!(
        plays_of(&get_song(&server, track).await),
        (1, Some(IN_2020_TEXT))
    );
    assert_eq!(plays_of(&get_song(&server, other).await), (0, None));
    for list in ["frequent", "recent"] {
        assert_eq!(
            listed(&server, list).await,
            [server.album.glass_harbour.as_str()],
            "{list}"
        );
    }

    // The server keeps no list of the songs that play.
    let response = server.get_json("getNowPlaying", &[]).await;
    assert_eq!(response["nowPlaying"], json!({ "entry": [] }));

    // The request is checked as a submission is.
    let notification = ("submission", "false");
    fails(&server, &[notification], 10, MISSING_ID).await;
    fails(&server, &[("id", NO_TRACK), notification], 70, NOT_FOUND).await;
    fails(
        &server,
        &[("id", track), ("id", NO_TRACK), notification],
        70,
        NOT_FOUND,
    )
    .await;
    let album = server.album.glass_harbour.as_str();
    fails(&server, &[("id", album), notification], 70, NOT_FOUND).await;
    fails(
        &server,
        &[("id", track), ("time", "x"), notification],
        0,
        TIME_RANGE,
    )
    .await;
    assert_eq!(rows(&server).await, before);

    // `true` and an empty value are submissions.
    ok(&server, &[("id", track), ("submission", "true")]).await;
    ok(&server, &[("id", track), ("submission", "")]).await;
    assert_eq!(
        stored_plays(&server, track).await.map(|plays| plays.0),
        Some(3)
    );
}

#[tokio::test]
async fn time_in_milliseconds_is_the_time_of_the_play() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();

    // A time in the past is stored as it is given, not as the time of the
    // request. The part below a second is dropped.
    ok(&server, &[("id", track), ("time", "1500000000123")]).await;
    assert_eq!(stored_plays(&server, track).await, Some((1, Some(IN_2017))));
    let song = get_song(&server, track).await;
    assert_eq!(plays_of(&song), (1, Some(IN_2017_TEXT)));
    assert_eq!(
        plays_of(&get_album(&server, album).await),
        (1, Some(IN_2017_TEXT))
    );
    let xml = server.get_xml("getSong", &[("id", track)]).await;
    assert!(
        xml.contains(r#" playCount="1" played="2017-07-14T02:40:00Z" "#),
        "{xml}"
    );

    // A later play moves the time.
    play_at(&server, track, IN_2023).await;
    assert_eq!(
        plays_of(&get_song(&server, track).await),
        (2, Some(IN_2023_TEXT))
    );

    // A play that is reported late is counted, and the last play stays.
    play_at(&server, track, IN_2020).await;
    assert_eq!(
        plays_of(&get_song(&server, track).await),
        (3, Some(IN_2023_TEXT))
    );
    assert_eq!(stored_plays(&server, track).await, Some((3, Some(IN_2023))));

    // The first and the last time that a request can name.
    let first = server.track.breakwater.as_str();
    ok(&server, &[("id", first), ("time", "0")]).await;
    assert_eq!(
        plays_of(&get_song(&server, first).await),
        (1, Some("1970-01-01T00:00:00Z"))
    );
    let last = server.track.night_ferry.as_str();
    ok(&server, &[("id", last), ("time", "253402300799999")]).await;
    assert_eq!(
        plays_of(&get_song(&server, last).await),
        (1, Some("9999-12-31T23:59:59Z"))
    );

    // An empty `time` is no time: the play has the time of the request.
    let now = server.track.meridian.as_str();
    let before = unix_now();
    ok(&server, &[("id", now), ("time", "")]).await;
    let after = unix_now();
    let played = &get_song(&server, now).await["played"];
    assert!(is_time_between(played, before, after), "{played}");
}

#[tokio::test]
async fn several_ids_with_their_times_are_counted_in_one_request() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.contour.as_str();
    let c = server.track.delta.as_str();

    // Each `time` belongs to the `id` at its place. A track that is named
    // twice has two plays.
    let (first, second, third, fourth) = (ms(IN_2020), ms(IN_2017), ms(IN_2023), ms(IN_2017));
    ok(
        &server,
        &[
            ("id", a),
            ("id", b),
            ("id", a),
            ("id", c),
            ("time", &first),
            ("time", &second),
            ("time", &third),
            ("time", &fourth),
        ],
    )
    .await;
    assert_eq!(stored_plays(&server, a).await, Some((2, Some(IN_2023))));
    assert_eq!(stored_plays(&server, b).await, Some((1, Some(IN_2017))));
    assert_eq!(stored_plays(&server, c).await, Some((1, Some(IN_2017))));
    assert_eq!(rows(&server).await.len(), 3);

    // The parameters may be in any order: the first `time` is still the
    // time of the first `id`.
    let d = server.track.open_road.as_str();
    let e = server.track.last_train.as_str();
    ok(
        &server,
        &[("time", &first), ("id", d), ("time", &second), ("id", e)],
    )
    .await;
    assert_eq!(stored_plays(&server, d).await, Some((1, Some(IN_2020))));
    assert_eq!(stored_plays(&server, e).await, Some((1, Some(IN_2017))));

    // Without `time` every play of the request has the time of the request.
    let before = unix_now();
    ok(&server, &[("id", b), ("id", c), ("id", b)]).await;
    let after = unix_now();
    let (count_b, at_b) = stored_plays(&server, b).await.expect("a row");
    let (count_c, at_c) = stored_plays(&server, c).await.expect("a row");
    assert_eq!((count_b, count_c), (3, 2));
    assert_eq!(at_b, at_c);
    assert!(
        (before..=after).contains(&at_b.expect("a time")),
        "{at_b:?}"
    );
    assert_eq!(rows(&server).await.len(), 5);
}

#[tokio::test]
async fn a_repeated_identical_request_counts_again() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let time = ms(IN_2020);
    let request = [("id", track), ("time", time.as_str())];

    for count in 1..=3 {
        ok(&server, &request).await;
        assert_eq!(
            plays_of(&get_song(&server, track).await),
            (count, Some(IN_2020_TEXT))
        );
    }
    // The same play twice in one request counts twice as well.
    ok(
        &server,
        &[
            ("id", track),
            ("id", track),
            ("time", &time),
            ("time", &time),
        ],
    )
    .await;
    assert_eq!(stored_plays(&server, track).await, Some((5, Some(IN_2020))));
    assert_eq!(rows(&server).await.len(), 1);
}

#[tokio::test]
async fn an_id_of_no_track_is_error_70_and_no_play_of_the_request_is_counted() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.upstream.as_str();
    let c = server.track.open_road.as_str();
    let album = server.album.glass_harbour.as_str();
    let artist = server.artist.aurora_field.as_str();

    // One track has a play and a star before the requests, and one album a
    // rating.
    play_at(&server, a, IN_2020).await;
    let response = server.get_json("star", &[("id", a)]).await;
    assert_eq!(response["status"], "ok", "{response}");
    let rate = [("id", album), ("rating", "4")];
    let response = server.get_json("setRating", &rate).await;
    assert_eq!(response["status"], "ok", "{response}");
    let before = rows(&server).await;
    assert_eq!(before.len(), 2);

    // A value that is not the ID of a track that exists.
    let bad = [
        NO_TRACK, NO_ALBUM, NO_ARTIST, PLAYLIST, album, artist, "", "low tide", "1",
    ];
    let good = [a, b, c, a];
    let time = ms(IN_2023);
    for bad in bad {
        // The value is the first, a middle and the last of the request.
        for place in [0, 2, good.len()] {
            let mut tracks = good.to_vec();
            tracks.insert(place, bad);
            let mut request: Vec<(&str, &str)> = tracks.iter().map(|id| ("id", *id)).collect();
            fails(&server, &request, 70, NOT_FOUND).await;
            assert_eq!(rows(&server).await, before, "{bad:?} at {place}");

            // The same with a time for each ID.
            request.extend(tracks.iter().map(|_| ("time", time.as_str())));
            fails(&server, &request, 70, NOT_FOUND).await;
            assert_eq!(rows(&server).await, before, "{bad:?} at {place} with times");
        }
        // The value alone.
        fails(&server, &[("id", bad)], 70, NOT_FOUND).await;
    }
    assert_eq!(rows(&server).await, before);
    assert_eq!(
        plays_of(&get_song(&server, a).await),
        (1, Some(IN_2020_TEXT))
    );
    assert_eq!(plays_of(&get_song(&server, b).await), (0, None));
    assert_eq!(listed(&server, "frequent").await, [album]);

    // The same tracks without the value are counted.
    let request: Vec<(&str, &str)> = good.iter().map(|id| ("id", *id)).collect();
    ok(&server, &request).await;
    assert_eq!(stored_plays(&server, a).await.map(|plays| plays.0), Some(3));
    assert_eq!(stored_plays(&server, b).await.map(|plays| plays.0), Some(1));
    assert_eq!(stored_plays(&server, c).await.map(|plays| plays.0), Some(1));
}

#[tokio::test]
async fn wrong_parameters_are_errors_and_count_nothing() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.upstream.as_str();
    play_at(&server, a, IN_2020).await;
    let before = rows(&server).await;

    // `id` is required.
    fails(&server, &[], 10, MISSING_ID).await;
    fails(&server, &[("time", "1")], 10, MISSING_ID).await;
    fails(&server, &[("songId", a)], 10, MISSING_ID).await;

    // `submission` is `true` or `false`.
    for value in ["1", "0", "yes", "maybe"] {
        fails(&server, &[("id", a), ("submission", value)], 0, SUBMISSION).await;
    }

    // `time` is an integer of milliseconds from 0 to the end of the year
    // 9999.
    for value in [
        "-1",
        "253402300800000",
        "9223372036854775808",
        "1.5",
        "now",
        "2023-11-14T22:13:20Z",
    ] {
        fails(&server, &[("id", a), ("time", value)], 0, TIME_RANGE).await;
        // A wrong value after a right one.
        fails(
            &server,
            &[("id", b), ("id", a), ("time", "1"), ("time", value)],
            0,
            TIME_RANGE,
        )
        .await;
    }

    // A request has no `time` or one for each `id`.
    for request in [
        vec![("id", a), ("time", "1"), ("time", "2")],
        vec![("id", a), ("id", b), ("time", "1")],
        vec![("id", a), ("id", b), ("time", ""), ("time", "2")],
        vec![
            ("id", a),
            ("id", b),
            ("time", "1"),
            ("time", "2"),
            ("time", "3"),
        ],
    ] {
        fails(&server, &request, 0, TIME_COUNT).await;
    }

    assert_eq!(rows(&server).await, before);
    assert_eq!(plays_of(&get_song(&server, b).await), (0, None));
}

#[tokio::test]
async fn scrobble_takes_the_ids_and_the_times_of_the_query_string_and_of_a_form_body() {
    let server = TestServer::standard().await;
    let a = server.track.low_tide.as_str();
    let b = server.track.breakwater.as_str();

    // The values of the query string come first, so its `time` is the time
    // of its `id`.
    let path = format!("/rest/scrobble?id={a}&time={}&f=json", ms(IN_2017));
    let time = ms(IN_2023);
    let form = [
        ("u", OWNER),
        ("p", OWNER_PASSWORD),
        ("v", API_VERSION),
        ("c", CLIENT),
        ("id", b),
        ("time", time.as_str()),
    ];
    let response = server.request(form_request(&path, &form)).await;
    assert_eq!(content_type(&response), "application/json");
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(stored_plays(&server, a).await, Some((1, Some(IN_2017))));
    assert_eq!(stored_plays(&server, b).await, Some((1, Some(IN_2023))));

    // A form body alone.
    let response = server
        .post_form(
            "scrobble",
            &[("id", b), ("submission", "true"), ("f", "json")],
        )
        .await;
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["status"], "ok", "{response}");
    assert_eq!(stored_plays(&server, b).await.map(|plays| plays.0), Some(2));

    // A form body with an unknown ID fails as a query string does, and the
    // ID of the query string is not counted either.
    let path = format!("/rest/scrobble?id={a}&f=json");
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
    assert_eq!(stored_plays(&server, a).await, Some((1, Some(IN_2017))));
}

#[tokio::test]
async fn plays_belong_to_the_user_of_the_request() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    let second = second_user(&server).await;
    let as_second = |method: &'static str, extra: Vec<(&'static str, String)>| {
        let server = &server;
        async move {
            let mut params = second.to_vec();
            params.extend(extra.iter().map(|(name, value)| (*name, value.as_str())));
            server.get_json_noauth(method, &params).await
        }
    };

    // The second user plays the track three times.
    let request = vec![
        ("id", track.to_owned()),
        ("id", track.to_owned()),
        ("id", track.to_owned()),
        ("time", ms(IN_2017)),
        ("time", ms(IN_2023)),
        ("time", ms(IN_2020)),
    ];
    let response = as_second("scrobble", request).await;
    assert_eq!(response["status"], "ok", "{response}");

    // The owner has no play: not on the song, not on the album, and in no
    // list.
    assert_eq!(plays_of(&get_song(&server, track).await), (0, None));
    assert_eq!(plays_of(&get_album(&server, album).await), (0, None));
    for list in ["frequent", "recent"] {
        assert_eq!(listed(&server, list).await, [] as [&str; 0], "{list}");
    }

    // A play of the owner is counted in the row of the owner.
    play_at(&server, track, IN_2020).await;
    assert_eq!(
        plays_of(&get_song(&server, track).await),
        (1, Some(IN_2020_TEXT))
    );
    assert_eq!(
        plays_of(&get_album(&server, album).await),
        (1, Some(IN_2020_TEXT))
    );

    // The second user has its three plays.
    let response = as_second("getSong", vec![("id", track.to_owned())]).await;
    assert_eq!(plays_of(&response["song"]), (3, Some(IN_2023_TEXT)));
    let response = as_second("getAlbum", vec![("id", album.to_owned())]).await;
    assert_eq!(plays_of(&response["album"]), (3, Some(IN_2023_TEXT)));
    let response = as_second("getAlbumList2", vec![("type", "frequent".to_owned())]).await;
    assert_eq!(
        plays_of(&response["albumList2"]["album"][0]),
        (3, Some(IN_2023_TEXT))
    );

    let owner = owner_id(&server).await;
    let stored = rows(&server).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    for row in &stored {
        let plays = if row.0 == owner {
            (1, Some(IN_2020))
        } else {
            (3, Some(IN_2023))
        };
        assert_eq!((row.1.as_str(), row.5, row.6), (track, plays.0, plays.1));
    }
}

#[tokio::test]
async fn a_request_without_a_user_counts_nothing() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let version = ("v", API_VERSION);
    let client = ("c", CLIENT);

    let response = server
        .get_json_noauth("scrobble", &[version, client, ("id", track)])
        .await;
    assert_eq!(response["error"]["code"], 10, "{response}");

    let params = [version, client, ("u", OWNER), ("p", "wrong"), ("id", track)];
    let response = server.get_json_noauth("scrobble", &params).await;
    assert_eq!(response["error"]["code"], 40, "{response}");

    assert_eq!(rows(&server).await, []);
}

/// Plays tracks of three albums of the standard library at fixed times:
/// "Paper Maps" has 2 + 2 plays, the last in 2023. "Glass Harbour" has 3
/// plays, the last in 2017, and "Summer Sampler" has 3, the last in 2020.
/// "Slow Rivers" has none.
async fn play_three_albums(server: &StandardServer) {
    let plays = [
        (&server.track.meridian, IN_2017 - 500),
        (&server.track.meridian, IN_2017 - 400),
        (&server.track.contour, IN_2023),
        (&server.track.contour, IN_2017 - 300),
        (&server.track.low_tide, IN_2017 - 200),
        (&server.track.low_tide, IN_2017),
        (&server.track.night_ferry, IN_2017 - 100),
        (&server.track.open_road, IN_2020),
        (&server.track.open_road, IN_2020 - 100),
        (&server.track.last_train, IN_2020 - 200),
    ];
    for (track, at) in plays {
        play_at(server, track, at).await;
    }
}

#[tokio::test]
async fn frequent_and_recent_list_the_albums_of_the_standard_library_by_their_plays() {
    let server = TestServer::standard().await;
    let glass_harbour = server.album.glass_harbour.as_str();
    let paper_maps = server.album.paper_maps.as_str();
    let slow_rivers = server.album.slow_rivers.as_str();
    let summer_sampler = server.album.summer_sampler.as_str();

    // Without a play both lists are empty.
    for method in ALBUM_METHODS {
        for list in ["frequent", "recent"] {
            assert_eq!(
                album_list(&server, method, list, &[]).await,
                [] as [Value; 0]
            );
        }
    }

    play_three_albums(&server).await;
    for method in ALBUM_METHODS {
        // The most plays first. Albums with as many plays are in the order
        // of their titles.
        let frequent = album_list(&server, method, "frequent", &[]).await;
        assert_eq!(
            ids(&frequent),
            [paper_maps, glass_harbour, summer_sampler],
            "{method:?}"
        );
        // The latest play first.
        let recent = album_list(&server, method, "recent", &[]).await;
        assert_eq!(
            ids(&recent),
            [paper_maps, summer_sampler, glass_harbour],
            "{method:?}"
        );

        // Each album reports the sum of the plays of its tracks and the
        // latest of them, in both lists.
        let plays: Vec<_> = frequent.iter().map(plays_of).collect();
        assert_eq!(
            plays,
            [
                (4, Some(IN_2023_TEXT)),
                (3, Some(IN_2017_TEXT)),
                (3, Some(IN_2020_TEXT)),
            ],
            "{method:?}"
        );
        assert_eq!(recent[0], frequent[0], "{method:?}");
        assert_eq!(recent[1], frequent[2], "{method:?}");
        assert_eq!(recent[2], frequent[1], "{method:?}");

        // The pages of a list follow the same order.
        let page = [("size", "1"), ("offset", "1")];
        let second = album_list(&server, method, "frequent", &page).await;
        assert_eq!(ids(&second), [glass_harbour], "{method:?}");
        let second = album_list(&server, method, "recent", &page).await;
        assert_eq!(ids(&second), [summer_sampler], "{method:?}");
    }

    // One play of the fourth album, now: it is the first of `recent` and
    // the last of `frequent`.
    ok(&server, &[("id", &server.track.delta)]).await;
    assert_eq!(
        listed(&server, "recent").await,
        [slow_rivers, paper_maps, summer_sampler, glass_harbour]
    );
    assert_eq!(
        listed(&server, "frequent").await,
        [paper_maps, glass_harbour, summer_sampler, slow_rivers]
    );

    // Two more plays of a track of "Glass Harbour" make it the most played
    // album.
    ok(
        &server,
        &[
            ("id", &server.track.breakwater),
            ("id", &server.track.breakwater),
        ],
    )
    .await;
    assert_eq!(
        listed(&server, "frequent").await,
        [glass_harbour, paper_maps, summer_sampler, slow_rivers]
    );
    assert_eq!(
        listed(&server, "recent").await,
        [glass_harbour, slow_rivers, paper_maps, summer_sampler]
    );

    // The other lists hold the albums they held, each with its plays.
    let by_name = album_list(&server, ALBUM_METHODS[0], "alphabeticalByName", &[]).await;
    assert_eq!(
        ids(&by_name),
        [glass_harbour, paper_maps, slow_rivers, summer_sampler]
    );
    let counts: Vec<u64> = by_name.iter().map(|album| plays_of(album).0).collect();
    assert_eq!(counts, [5, 4, 1, 3]);
}

#[tokio::test]
async fn browse_methods_report_the_plays_of_songs_and_albums() {
    let server = TestServer::standard().await;
    play_three_albums(&server).await;
    let paper_maps = server.album.paper_maps.as_str();

    // `getAlbum`: the album has the sum and the latest play of its songs,
    // and each song its own plays.
    let album = get_album(&server, paper_maps).await;
    assert_eq!(plays_of(&album), (4, Some(IN_2023_TEXT)));
    let songs = album["song"].as_array().expect("the album has songs");
    let titles: Vec<&str> = songs
        .iter()
        .map(|song| song["title"].as_str().expect("a title"))
        .collect();
    assert_eq!(titles, ["Meridian", "Contour", "Legend"]);
    let earlier = model::iso8601(IN_2017 - 400);
    assert_eq!(plays_of(&songs[0]), (2, Some(earlier.as_str())));
    assert_eq!(plays_of(&songs[1]), (2, Some(IN_2023_TEXT)));
    assert_eq!(plays_of(&songs[2]), (0, None));

    // `getSong` returns the element of the album.
    for song in songs {
        let id = song["id"].as_str().expect("an ID");
        assert_eq!(get_song(&server, id).await, *song);
    }

    // `getArtist`: each album of the artist with its plays.
    let artist = [("id", server.artist.aurora_field.as_str())];
    let response = server.get_json("getArtist", &artist).await;
    let albums = response["artist"]["album"]
        .as_array()
        .expect("the artist has albums");
    assert_eq!(
        ids(albums),
        [server.album.glass_harbour.as_str(), paper_maps]
    );
    assert_eq!(plays_of(&albums[0]), (3, Some(IN_2017_TEXT)));
    assert_eq!(plays_of(&albums[1]), (4, Some(IN_2023_TEXT)));
    // The element is the one of `getAlbum` without the songs.
    let mut without_songs = album.clone();
    without_songs
        .as_object_mut()
        .expect("an album is an object")
        .remove("song");
    assert_eq!(albums[1], without_songs);

    // An album without a played track has `playCount: 0` and no `played`.
    let unplayed = get_album(&server, &server.album.slow_rivers).await;
    assert_eq!(plays_of(&unplayed), (0, None));

    // `search3` and `search2` report the same plays for the album and its
    // songs.
    let response = server.get_json("search3", &[("query", "paper maps")]).await;
    let result = &response["searchResult3"];
    assert_eq!(result["album"], json!([without_songs]));
    let response = server.get_json("search2", &[("query", "paper maps")]).await;
    let directory = &response["searchResult2"]["album"][0];
    assert_eq!(directory["isDir"], true);
    assert_eq!(plays_of(directory), (4, Some(IN_2023_TEXT)));

    // In XML the plays are attributes of the album and of each song.
    let xml = server.get_xml("getAlbum", &[("id", paper_maps)]).await;
    let attributes = format!(r#" name="Paper Maps" playCount="4" played="{IN_2023_TEXT}" "#);
    assert!(xml.contains(&attributes), "{xml}");
    let attributes = format!(r#" playCount="2" played="{IN_2023_TEXT}" "#);
    assert!(xml.contains(&attributes), "{xml}");
    assert!(xml.contains(r#" playCount="0" "#), "{xml}");
}

#[tokio::test]
async fn a_star_and_a_rating_stay_when_a_track_is_played_and_plays_stay_when_they_change() {
    let server = TestServer::standard().await;
    let track = server.track.low_tide.as_str();
    let album = server.album.glass_harbour.as_str();
    let send = |method: &'static str, params: Vec<(&'static str, &'static str)>, id: &str| {
        let server = &server;
        let id = id.to_owned();
        async move {
            let mut all = vec![("id", id.as_str())];
            all.extend(params);
            let response = server.get_json(method, &all).await;
            assert_eq!(response["status"], "ok", "{method}: {response}");
        }
    };

    // The track and its album have a star and a rating before the play.
    for id in [track, album] {
        send("star", vec![], id).await;
        send("setRating", vec![("rating", "4")], id).await;
    }
    play_at(&server, track, IN_2020).await;
    play_at(&server, track, IN_2017).await;
    for element in [
        get_song(&server, track).await,
        get_album(&server, album).await,
    ] {
        assert_eq!(plays_of(&element), (2, Some(IN_2020_TEXT)), "{element}");
        assert_eq!(element["userRating"], 4, "{element}");
        assert!(element.get("starred").is_some(), "{element}");
    }

    // The removal of the star and of the rating keeps the plays.
    for id in [track, album] {
        send("unstar", vec![], id).await;
        send("setRating", vec![("rating", "0")], id).await;
    }
    for element in [
        get_song(&server, track).await,
        get_album(&server, album).await,
    ] {
        assert_eq!(plays_of(&element), (2, Some(IN_2020_TEXT)), "{element}");
        for absent in ["starred", "userRating"] {
            assert!(element.get(absent).is_none(), "{element}");
        }
    }
    // The plays are in the row of the track. The row of the album holds
    // none.
    let stored = rows(&server).await;
    let plays: Vec<_> = stored
        .iter()
        .map(|row| (row.2.as_str(), row.5, row.6))
        .collect();
    assert_eq!(plays, [("album", 0, None), ("track", 2, Some(IN_2020))]);
}

#[tokio::test]
async fn plays_survive_a_rescan_a_renamed_file_and_a_moved_file() {
    let server = TestServer::standard_with_own_files().await;
    let low_tide = server.track.low_tide.as_str();
    let breakwater = server.track.breakwater.as_str();
    let album = server.album.glass_harbour.as_str();
    play_at(&server, low_tide, IN_2017).await;
    play_at(&server, low_tide, IN_2023).await;
    play_at(&server, breakwater, IN_2020).await;
    play_at(&server, &server.track.upstream, IN_2020 + 100).await;

    let stored = rows(&server).await;
    assert_eq!(stored.len(), 3);
    let mut songs = [
        get_song(&server, low_tide).await,
        get_song(&server, breakwater).await,
    ];
    assert_eq!(plays_of(&songs[0]), (2, Some(IN_2023_TEXT)));
    assert_eq!(plays_of(&songs[1]), (1, Some(IN_2020_TEXT)));
    let lists = [
        album_list(&server, ALBUM_METHODS[0], "frequent", &[]).await,
        album_list(&server, ALBUM_METHODS[0], "recent", &[]).await,
    ];
    assert_eq!(ids(&lists[0]), [album, server.album.slow_rivers.as_str()]);
    assert_eq!(plays_of(&lists[0][0]), (3, Some(IN_2023_TEXT)));

    // A scan that finds nothing changed.
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.moved, summary.changed), (10, 0, 0));
    assert_eq!(rows(&server).await, stored);
    assert_eq!(get_song(&server, low_tide).await, songs[0]);

    // The file of one played track is renamed, and the file of the other
    // moves to another directory.
    let moves = [
        (
            0,
            low_tide,
            "Aurora Field/Glass Harbour/01 Low Tide.flac",
            "Aurora Field/Glass Harbour/01 High Tide.flac",
        ),
        (
            1,
            breakwater,
            "Aurora Field/Glass Harbour/02 Breakwater.flac",
            "Moved/Elsewhere/Breakwater.flac",
        ),
    ];
    fs::create_dir_all(server.path("Moved/Elsewhere")).unwrap();
    for (place, id, from, to) in moves {
        assert_eq!(songs[place]["path"], from);
        fs::rename(server.path(from), server.path(to)).unwrap();
        let summary = server.rescan().await;
        assert_eq!(summary.errors, [] as [&str; 0], "{to}");
        assert_eq!(
            (summary.seen, summary.moved, summary.added, summary.removed),
            (10, 1, 0, 0),
            "{to}"
        );

        // The annotations are the rows they were. The song has its new
        // path and the plays it had, and the lists hold the same elements.
        assert_eq!(rows(&server).await, stored, "{to}");
        songs[place]["path"] = json!(to);
        assert_eq!(get_song(&server, id).await, songs[place], "{to}");
        for (list, expected) in ["frequent", "recent"].into_iter().zip(&lists) {
            assert_eq!(
                album_list(&server, ALBUM_METHODS[0], list, &[]).await,
                *expected,
                "{to} {list}"
            );
        }
    }
    assert_eq!(
        plays_of(&get_album(&server, album).await),
        (3, Some(IN_2023_TEXT))
    );

    // The moved track takes a new play at its new path.
    play_at(&server, breakwater, IN_2023 + 100).await;
    assert_eq!(
        stored_plays(&server, breakwater).await,
        Some((2, Some(IN_2023 + 100)))
    );
    assert_eq!(get_album(&server, album).await["playCount"], 4);
}

#[tokio::test]
async fn plays_survive_a_change_of_the_display_names() {
    let library = LibraryBuilder::new()
        .track("aurora field", "glass harbour", "Low Tide", 1, "flac")
        .track("aurora field", "glass harbour", "Breakwater", 2, "flac")
        .track("aurora field", "paper maps", "Meridian", 1, "mp3")
        .build();
    let server = TestServer::scanned(library).await;
    let catalogue = server.catalogue().await;
    let album_id = catalogue.album("glass harbour").id.clone();
    let track_id = catalogue.song("Low Tide").track.id.clone();
    let other_id = catalogue.song("Breakwater").track.id.clone();

    play_at(&server, &track_id, IN_2017).await;
    play_at(&server, &track_id, IN_2020).await;
    play_at(&server, &other_id, IN_2023).await;
    let stored = rows(&server).await;
    assert_eq!(listed(&server, "frequent").await, [album_id.as_str()]);
    let before = get_album(&server, &album_id).await;
    assert_eq!(before["name"], "glass harbour");
    assert_eq!(plays_of(&before), (3, Some(IN_2023_TEXT)));

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

    // The rows of the tracks and of the album have their IDs and the new
    // names, and the plays are those from before.
    assert_eq!(rows(&server).await, stored);
    let after = get_album(&server, &album_id).await;
    assert_eq!(
        (&after["name"], &after["artist"]),
        (&json!("Glass Harbour"), &json!("Aurora Field"))
    );
    assert_eq!(plays_of(&after), (3, Some(IN_2023_TEXT)));
    let song = get_song(&server, &track_id).await;
    assert_eq!(
        (&song["album"], &song["artist"]),
        (&json!("Glass Harbour"), &json!("Aurora Field"))
    );
    assert_eq!(plays_of(&song), (2, Some(IN_2020_TEXT)));
    for list in ["frequent", "recent"] {
        let albums = album_list(&server, ALBUM_METHODS[0], list, &[]).await;
        assert_eq!(ids(&albums), [album_id.as_str()], "{list}");
        assert_eq!(albums[0]["name"], "Glass Harbour", "{list}");
        assert_eq!(plays_of(&albums[0]), (3, Some(IN_2023_TEXT)), "{list}");
    }
}

#[tokio::test]
async fn a_track_without_a_file_keeps_its_plays_and_takes_a_play() {
    let server = TestServer::standard().await;
    let played_before = server.track.upstream.as_str();
    let played_after = server.track.delta.as_str();
    let album = server.album.slow_rivers.as_str();
    let folder = server.folder_id.to_string();
    let in_folder = [("musicFolderId", folder.as_str())];

    // One track has its play before it loses its file.
    play_at(&server, played_before, IN_2017).await;
    remove_file(&server, played_before).await;
    let song = get_song(&server, played_before).await;
    assert!(song.get("path").is_none(), "{song}");
    assert_eq!(plays_of(&song), (1, Some(IN_2017_TEXT)));

    // The row takes a play that is reported afterwards.
    play_at(&server, played_before, IN_2020).await;
    assert_eq!(
        plays_of(&get_song(&server, played_before).await),
        (2, Some(IN_2020_TEXT))
    );
    assert_eq!(
        plays_of(&get_album(&server, album).await),
        (2, Some(IN_2020_TEXT))
    );

    // The album has a track with a file in the folder, so the lists of the
    // folder hold it, with the plays of both tracks.
    for list in ["frequent", "recent"] {
        let albums = album_list(&server, ALBUM_METHODS[0], list, &in_folder).await;
        assert_eq!(ids(&albums), [album], "{list}");
        assert_eq!(plays_of(&albums[0]), (2, Some(IN_2020_TEXT)), "{list}");
    }

    // The other track loses its file and is played afterwards. The album
    // is then in no music folder, and the lists without a folder hold it.
    remove_file(&server, played_after).await;
    play_at(&server, played_after, IN_2023).await;
    for list in ["frequent", "recent"] {
        let albums = album_list(&server, ALBUM_METHODS[0], list, &in_folder).await;
        assert_eq!(albums, [] as [Value; 0], "{list}");
        let albums = album_list(&server, ALBUM_METHODS[0], list, &[]).await;
        assert_eq!(ids(&albums), [album], "{list}");
        assert_eq!(plays_of(&albums[0]), (3, Some(IN_2023_TEXT)), "{list}");
    }
}

#[tokio::test]
async fn the_plays_of_an_album_follow_its_tracks_through_a_retag_and_a_deletion() {
    let server = TestServer::standard_with_own_files().await;
    let low_tide = server.track.low_tide.as_str();
    let breakwater = server.track.breakwater.as_str();
    let glass_harbour = server.album.glass_harbour.as_str();
    play_at(&server, low_tide, IN_2017).await;
    play_at(&server, low_tide, IN_2023).await;
    play_at(&server, breakwater, IN_2020).await;
    assert_eq!(
        plays_of(&get_album(&server, glass_harbour).await),
        (3, Some(IN_2023_TEXT))
    );

    // One played track gets the title of another album. The scan gives it
    // a new album row, and the track keeps its ID and its plays.
    retag(
        &server.path("Aurora Field/Glass Harbour/01 Low Tide.flac"),
        ItemKey::AlbumTitle,
        "Tide Tables",
    );
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.changed), (10, 1));
    let song = get_song(&server, low_tide).await;
    assert_eq!(song["album"], "Tide Tables");
    assert_eq!(plays_of(&song), (2, Some(IN_2023_TEXT)));
    let tide_tables = song["albumId"].as_str().expect("an album ID").to_owned();
    assert_ne!(tide_tables, glass_harbour);

    // The plays of the track count for the album it is in now.
    assert_eq!(
        plays_of(&get_album(&server, &tide_tables).await),
        (2, Some(IN_2023_TEXT))
    );
    assert_eq!(
        plays_of(&get_album(&server, glass_harbour).await),
        (1, Some(IN_2020_TEXT))
    );
    assert_eq!(
        listed(&server, "frequent").await,
        [tide_tables.as_str(), glass_harbour]
    );

    // The file of the other played track is deleted. The scan deletes the
    // track row and its annotation, and the album has no play left.
    fs::remove_file(server.path("Aurora Field/Glass Harbour/02 Breakwater.flac"))
        .expect("delete the file");
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.removed), (9, 1));
    let stored = rows(&server).await;
    assert_eq!(stored.len(), 1, "{stored:?}");
    assert_eq!(stored[0].1, low_tide);
    assert_eq!(
        plays_of(&get_album(&server, glass_harbour).await),
        (0, None)
    );
    for list in ["frequent", "recent"] {
        assert_eq!(
            listed(&server, list).await,
            [tide_tables.as_str()],
            "{list}"
        );
    }
    // The ID of the deleted track is the ID of no track.
    fails(&server, &[("id", breakwater)], 70, NOT_FOUND).await;
}
