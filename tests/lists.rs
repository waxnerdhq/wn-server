//! Router tests of the list methods `getAlbumList`, `getAlbumList2`,
//! `getRandomSongs` and `getSongsByGenre` (`docs/design.md` section 4).
//!
//! A test of a random list checks what holds for every draw: the number of
//! the rows, that no row is there twice, and that every row is one that the
//! request allows. No test depends on which rows a draw returns.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::params;
use serde_json::{Value, json};
use wn_server::VERSION;
use wn_server::api::model;
use wn_server::config::Config;
use wn_server::db::queries::annotations::Annotation;
use wn_server::db::queries::tracks::{self, TrackContent};
use wn_server::db::queries::{albums, artists, folders, users};
use wn_server::ids::{self, Kind};
use wn_server::library::tags::TrackMeta;

use common::{Catalogue, StandardServer, TestServer};

/// The message of the error for a `type` that is no list type.
const UNKNOWN_TYPE: &str = "parameter type must be one of random, newest, highest, frequent, \
     recent, alphabeticalByName, alphabeticalByArtist, starred, byYear, byGenre";

/// The message of the error for a value that is no integer of the range of
/// the parameter.
const NOT_A_U32: &str = "must be an integer from 0 to 4294967295";

/// The titles of the albums of the standard library in the order of their
/// titles, which is also the order of their artists and then their titles.
const BY_NAME: [&str; 4] = [
    "Glass Harbour",
    "Paper Maps",
    "Slow Rivers",
    "Summer Sampler",
];

/// The parameters of a request, without those that authenticate it.
type Query = [(&'static str, &'static str)];

/// The parameters of a request for each list type on the standard library,
/// with the number of albums that the list holds while the library has no
/// annotation.
const LIST_TYPES: [(&Query, usize); 10] = [
    (&[("type", "random")], 4),
    (&[("type", "newest")], 4),
    (&[("type", "highest")], 0),
    (&[("type", "frequent")], 0),
    (&[("type", "recent")], 0),
    (&[("type", "alphabeticalByName")], 4),
    (&[("type", "alphabeticalByArtist")], 4),
    (&[("type", "starred")], 0),
    (
        &[("type", "byYear"), ("fromYear", "2000"), ("toYear", "2030")],
        4,
    ),
    (&[("type", "byGenre"), ("genre", "Electronic")], 2),
];

/// A list without an entry.
const NONE: [Value; 0] = [];

/// The two album list methods: the name, the key of the element in the
/// answer, and the field of an album that holds its title.
const ALBUM_METHODS: [(&str, &str, &str); 2] = [
    ("getAlbumList2", "albumList2", "name"),
    ("getAlbumList", "albumList", "title"),
];

/// The `error` object with `code` and `message`.
fn error(code: u32, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

/// The `error` object of error 10 that names `parameter`.
fn error_10(parameter: &str) -> Value {
    error(10, &format!("required parameter is missing: {parameter}"))
}

/// The `error` object of error 0 for a value of `parameter` that is no
/// integer from 0 to 4294967295.
fn error_not_u32(parameter: &str) -> Value {
    error(0, &format!("parameter {parameter} {NOT_A_U32}"))
}

/// The XML body that has the status `ok` and then `rest`. `rest` is the text
/// that follows the last attribute of the root element.
fn xml_document(rest: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#,
            r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true"{}"#
        ),
        VERSION, rest
    )
}

/// The attributes that the XML body holds for the JSON object `element`:
/// each field with a scalar value, in the byte order of the names. The
/// values of the tests need no escape.
fn xml_attributes(element: &Value) -> String {
    let fields = element.as_object().expect("an element is an object");
    fields
        .iter()
        .map(|(name, value)| match value {
            Value::String(text) => format!(r#" {name}="{text}""#),
            scalar => format!(r#" {name}="{scalar}""#),
        })
        .collect()
}

/// Sends `method` with `params` and returns the list `list` of the element
/// `key` of the answer. The answer must have the status `ok`, and the
/// element must hold the list and nothing else.
async fn list_of(
    server: &TestServer,
    method: &str,
    params: &[(&str, &str)],
    key: &str,
    list: &str,
) -> Vec<Value> {
    let response = server.get_json(method, params).await;
    assert_eq!(response["status"], "ok", "{method} {params:?}: {response}");
    let element = response[key]
        .as_object()
        .unwrap_or_else(|| panic!("{method} {params:?} has no {key}: {response}"));
    assert_eq!(element.keys().collect::<Vec<_>>(), [list], "{response}");
    element[list]
        .as_array()
        .unwrap_or_else(|| panic!("{key}.{list} is no list: {response}"))
        .clone()
}

/// The `album` list of `getAlbumList2` with `params`.
async fn album_list2(server: &TestServer, params: &[(&str, &str)]) -> Vec<Value> {
    list_of(server, "getAlbumList2", params, "albumList2", "album").await
}

/// The `album` list of `getAlbumList` with `params`.
async fn album_list(server: &TestServer, params: &[(&str, &str)]) -> Vec<Value> {
    list_of(server, "getAlbumList", params, "albumList", "album").await
}

/// The `song` list of `getRandomSongs` with `params`.
async fn random_songs(server: &TestServer, params: &[(&str, &str)]) -> Vec<Value> {
    list_of(server, "getRandomSongs", params, "randomSongs", "song").await
}

/// The `song` list of `getSongsByGenre` with `params`.
async fn songs_by_genre(server: &TestServer, params: &[(&str, &str)]) -> Vec<Value> {
    list_of(server, "getSongsByGenre", params, "songsByGenre", "song").await
}

/// The value of the text field `field` of each element.
fn texts(elements: &[Value], field: &str) -> Vec<String> {
    elements
        .iter()
        .map(|element| {
            element[field]
                .as_str()
                .unwrap_or_else(|| panic!("no {field}: {element}"))
                .to_owned()
        })
        .collect()
}

/// The `name` of each album of `getAlbumList2` with `params`.
async fn names(server: &TestServer, params: &[(&str, &str)]) -> Vec<String> {
    texts(&album_list2(server, params).await, "name")
}

/// The IDs of `elements` as a set. Panics when an ID is there twice.
fn distinct_ids(elements: &[Value]) -> HashSet<String> {
    let ids = texts(elements, "id");
    let set: HashSet<String> = ids.iter().cloned().collect();
    assert_eq!(set.len(), ids.len(), "an ID is listed twice: {ids:?}");
    set
}

/// The error of `method` with `params`. The answer must have the status
/// `failed`.
async fn error_of(server: &TestServer, method: &str, params: &[(&str, &str)]) -> Value {
    let response = server.get_json(method, params).await;
    assert_eq!(
        response["status"], "failed",
        "{method} {params:?}: {response}"
    );
    response["error"].clone()
}

/// The `AlbumID3` element of the album `id` of `catalogue` with
/// `annotation`, as JSON.
fn album_element(catalogue: &Catalogue, id: &str, annotation: Option<&Annotation>) -> Value {
    let row = catalogue
        .albums
        .iter()
        .find(|album| album.id == id)
        .unwrap_or_else(|| panic!("no album has the ID {id}"));
    serde_json::to_value(model::album(row, annotation)).expect("an element serialises")
}

/// The `Child` element that lists the album `id` of `catalogue` as a
/// directory, without an annotation, as JSON.
fn album_dir_element(catalogue: &Catalogue, id: &str) -> Value {
    let row = catalogue
        .albums
        .iter()
        .find(|album| album.id == id)
        .unwrap_or_else(|| panic!("no album has the ID {id}"));
    serde_json::to_value(model::child_dir_for_album(row, None)).expect("an element serialises")
}

/// The `Child` element of the track `id` of `catalogue` with `annotation`,
/// as JSON.
fn song_element(catalogue: &Catalogue, id: &str, annotation: Option<&Annotation>) -> Value {
    let row = catalogue
        .songs
        .iter()
        .find(|song| song.track.id == id)
        .unwrap_or_else(|| panic!("no track has the ID {id}"));
    serde_json::to_value(model::child(row, annotation)).expect("an element serialises")
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

/// Writes an annotation row of the owner for each item of `rows`: the ID,
/// the item type and the values.
async fn annotate(server: &TestServer, rows: Vec<(String, &'static str, Annotation)>) {
    let user_id = owner_id(server).await;
    server
        .db()
        .write(move |conn| {
            for (item_id, item_type, annotation) in rows {
                conn.execute(
                    "INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating,
                       play_count, last_played_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        user_id,
                        item_id,
                        item_type,
                        annotation.starred_at,
                        annotation.rating,
                        i64::try_from(annotation.play_count).expect("a small play count"),
                        annotation.last_played_at,
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .expect("write the annotations");
}

/// An annotation that is a star set at `starred_at`.
fn star(starred_at: i64) -> Annotation {
    Annotation {
        starred_at: Some(starred_at),
        ..Annotation::default()
    }
}

/// An annotation that is the rating `rating`.
fn rating(rating: u8) -> Annotation {
    Annotation {
        rating: Some(rating),
        ..Annotation::default()
    }
}

/// An annotation of `play_count` plays, the last at `last_played_at`.
fn plays(play_count: u64, last_played_at: i64) -> Annotation {
    Annotation {
        play_count,
        last_played_at: Some(last_played_at),
        ..Annotation::default()
    }
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

/// One track that [`written`] stores.
#[derive(Debug, Clone)]
struct Row {
    folder: &'static str,
    artist: String,
    album: String,
    title: String,
    year: Option<u32>,
    genre: Option<&'static str>,
}

/// A track of the album `album` by `artist` with the title `title`, in the
/// folder `/music`, without year and genre.
fn row(artist: &str, album: &str, title: &str) -> Row {
    Row {
        folder: "/music",
        artist: artist.to_owned(),
        album: album.to_owned(),
        title: title.to_owned(),
        year: None,
        genre: None,
    }
}

/// A server whose database holds `rows` as the scanner stores tracks, with
/// the counts of the albums and of the artists. No file exists for a track.
/// A test of more rows than a library of files can hold in the time of a
/// test, or of rows in two music folders, uses it.
async fn written(rows: Vec<Row>) -> TestServer {
    let server = TestServer::bare(Config::default()).await;
    server
        .db()
        .write(move |conn| {
            for (number, row) in rows.iter().enumerate() {
                let folder = folders::sync_folders(conn, &[PathBuf::from(row.folder)])?;
                let artist_id = artists::upsert_by_key(conn, &row.artist)?;
                let album_id =
                    albums::upsert_by_key(conn, &artist_id, &row.album, &row.artist, false)?;
                let meta = TrackMeta {
                    title: row.title.clone(),
                    artist: row.artist.clone(),
                    album_artist: row.artist.clone(),
                    album: row.album.clone(),
                    track_no: None,
                    disc_no: None,
                    year: row.year,
                    genre: row.genre.map(str::to_owned),
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
                let rel_path = format!("{number}.flac");
                tracks::insert_track(conn, &id, folder[0].id, &rel_path, &content)?;
            }
            albums::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .await
        .expect("write the library");
    server
}

/// The ID of the music folder row with the path `path`.
async fn folder_id(server: &TestServer, path: &str) -> String {
    let path = PathBuf::from(path);
    server
        .db()
        .read(folders::list)
        .await
        .expect("read the folders")
        .into_iter()
        .find(|folder| folder.path == path)
        .expect("the folder has a row")
        .id
        .to_string()
}

/// `params` followed by `more`.
fn with<'a>(params: &[(&'a str, &'a str)], more: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    [params, more].concat()
}

#[tokio::test]
async fn every_list_type_returns_a_well_formed_album_list2() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    for (params, count) in LIST_TYPES {
        let albums = album_list2(&server, params).await;
        assert_eq!(albums.len(), count, "{params:?}");
        distinct_ids(&albums);
        // Each entry is the whole `AlbumID3` element of its album.
        for album in &albums {
            let id = album["id"].as_str().expect("an ID");
            assert_eq!(*album, album_element(&catalogue, id, None), "{params:?}");
            assert!(album.get("song").is_none(), "{album}");
        }

        let xml = server.get_xml("getAlbumList2", params).await;
        assert_eq!(xml.matches("<album ").count(), count, "{params:?}: {xml}");
        if count == 0 {
            assert_eq!(xml, xml_document("><albumList2/></subsonic-response>"));
        } else {
            assert!(xml.contains("><albumList2><album "), "{params:?}: {xml}");
            assert!(
                xml.ends_with("/></albumList2></subsonic-response>"),
                "{xml}"
            );
        }
    }
}

#[tokio::test]
async fn album_list2_element_has_the_fields_of_album_id3() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let albums = album_list2(&server, &[("type", "alphabeticalByName"), ("size", "1")]).await;
    let row = catalogue.album("Glass Harbour");
    assert_eq!(
        albums,
        [json!({
            "id": server.album.glass_harbour,
            "name": "Glass Harbour",
            "artist": "Aurora Field",
            "artistId": server.artist.aurora_field,
            "coverArt": server.album.glass_harbour,
            "songCount": 3,
            "duration": model::seconds(row.duration_ms),
            "created": model::iso8601(row.created_at),
            "year": 2019,
            "genre": "Electronic",
            "playCount": 0,
        })]
    );
}

#[tokio::test]
async fn album_list_lists_each_album_as_a_directory() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let albums = album_list(&server, &[("type", "alphabeticalByName")]).await;
    assert_eq!(texts(&albums, "title"), BY_NAME);
    for album in &albums {
        let id = album["id"].as_str().expect("an ID");
        assert_eq!(*album, album_dir_element(&catalogue, id));
        assert_eq!(album["isDir"], true);
    }

    let row = catalogue.album("Paper Maps");
    assert_eq!(
        albums[1],
        json!({
            "id": server.album.paper_maps,
            "parent": server.artist.aurora_field,
            "isDir": true,
            "title": "Paper Maps",
            "album": "Paper Maps",
            "artist": "Aurora Field",
            "year": 2021,
            "genre": "Electronic",
            "duration": model::seconds(row.duration_ms),
            "created": model::iso8601(row.created_at),
            "artistId": server.artist.aurora_field,
            "mediaType": "album",
            "playCount": 0,
        })
    );
    assert_eq!(albums[3]["parent"], json!(server.artist.various_artists));
}

#[tokio::test]
async fn album_list_and_album_list2_hold_the_same_albums_in_the_same_order() {
    let server = TestServer::standard().await;
    annotate(
        &server,
        vec![
            (server.album.slow_rivers.clone(), "album", star(300)),
            (server.album.paper_maps.clone(), "album", rating(4)),
            (server.track.low_tide.clone(), "track", plays(2, 700)),
        ],
    )
    .await;
    for (params, _) in LIST_TYPES {
        if params[0].1 == "random" {
            continue;
        }
        let id3 = texts(&album_list2(&server, params).await, "id");
        let dirs = texts(&album_list(&server, params).await, "id");
        assert_eq!(dirs, id3, "{params:?}");
        assert!(!id3.is_empty(), "{params:?}");
    }
    let random = album_list(&server, &[("type", "random")]).await;
    assert_eq!(distinct_ids(&random).len(), 4);
}

#[tokio::test]
async fn alphabetical_lists_order_albums_by_title_or_by_artist_and_title() {
    let server = TestServer::standard().await;
    assert_eq!(
        names(&server, &[("type", "alphabeticalByName")]).await,
        BY_NAME
    );
    assert_eq!(
        names(&server, &[("type", "alphabeticalByArtist")]).await,
        BY_NAME
    );
    let albums = album_list2(&server, &[("type", "alphabeticalByArtist")]).await;
    assert_eq!(
        texts(&albums, "artist"),
        [
            "Aurora Field",
            "Aurora Field",
            "Marlow Quartet",
            "Various Artists"
        ]
    );

    // A library in which the two orders differ. Two artists have an album
    // with one title, and the names differ in case.
    let server = written(vec![
        row("Zed", "alpha", "One"),
        row("abe", "Omega", "Two"),
        row("abe", "Beta", "Three"),
        row("Mia", "Beta", "Four"),
    ])
    .await;
    let pairs = |albums: Vec<Value>| -> Vec<(String, String)> {
        texts(&albums, "artist")
            .into_iter()
            .zip(texts(&albums, "name"))
            .collect()
    };
    let pair = |artist: &str, name: &str| (artist.to_owned(), name.to_owned());
    assert_eq!(
        pairs(album_list2(&server, &[("type", "alphabeticalByName")]).await),
        [
            pair("Zed", "alpha"),
            pair("abe", "Beta"),
            pair("Mia", "Beta"),
            pair("abe", "Omega"),
        ]
    );
    assert_eq!(
        pairs(album_list2(&server, &[("type", "alphabeticalByArtist")]).await),
        [
            pair("abe", "Beta"),
            pair("abe", "Omega"),
            pair("Mia", "Beta"),
            pair("Zed", "alpha"),
        ]
    );
}

#[tokio::test]
async fn newest_orders_albums_by_created_descending() {
    let server = TestServer::standard().await;
    // The scan saw the four albums within one moment. The test gives them
    // the times that it orders by. Two albums have one time.
    let times = [
        (server.album.summer_sampler.clone(), 1_600_000_100_i64),
        (server.album.paper_maps.clone(), 1_600_000_300),
        (server.album.glass_harbour.clone(), 1_600_000_300),
        (server.album.slow_rivers.clone(), 1_600_000_400),
    ];
    server
        .db()
        .write(move |conn| {
            for (id, created_at) in times {
                conn.execute(
                    "UPDATE albums SET created_at = ?2 WHERE id = ?1",
                    params![id, created_at],
                )?;
            }
            Ok(())
        })
        .await
        .expect("set the creation times");

    let albums = album_list2(&server, &[("type", "newest")]).await;
    assert_eq!(
        texts(&albums, "name"),
        [
            "Slow Rivers",
            "Glass Harbour",
            "Paper Maps",
            "Summer Sampler"
        ]
    );
    assert_eq!(
        texts(&albums, "created"),
        [
            "2020-09-13T12:33:20Z",
            "2020-09-13T12:31:40Z",
            "2020-09-13T12:31:40Z",
            "2020-09-13T12:28:20Z"
        ]
    );
    // The albums of one time are in the order of their titles in every
    // request, so a page does not repeat one of them.
    let second = names(
        &server,
        &[("type", "newest"), ("size", "1"), ("offset", "1")],
    )
    .await;
    let third = names(
        &server,
        &[("type", "newest"), ("size", "1"), ("offset", "2")],
    )
    .await;
    assert_eq!(
        (second, third),
        (
            vec!["Glass Harbour".to_owned()],
            vec!["Paper Maps".to_owned()]
        )
    );
}

#[tokio::test]
async fn by_year_lists_a_year_range_ascending_or_descending() {
    let server = TestServer::standard().await;
    let by_year = |from: &'static str, to: &'static str| {
        let server = &server;
        async move {
            let params = [("type", "byYear"), ("fromYear", from), ("toYear", to)];
            names(server, &params).await
        }
    };
    assert_eq!(
        by_year("2000", "2030").await,
        [
            "Slow Rivers",
            "Glass Harbour",
            "Summer Sampler",
            "Paper Maps"
        ]
    );
    assert_eq!(
        by_year("2015", "2020").await,
        ["Slow Rivers", "Glass Harbour", "Summer Sampler"]
    );
    // `fromYear` after `toYear` is the same range with the latest year
    // first.
    assert_eq!(
        by_year("2020", "2000").await,
        ["Summer Sampler", "Glass Harbour", "Slow Rivers"]
    );
    assert_eq!(
        by_year("2030", "0").await,
        [
            "Paper Maps",
            "Summer Sampler",
            "Glass Harbour",
            "Slow Rivers"
        ]
    );
    assert_eq!(by_year("2019", "2019").await, ["Glass Harbour"]);
    assert_eq!(by_year("2016", "2018").await, [] as [&str; 0]);
    assert_eq!(by_year("4294967295", "2021").await, ["Paper Maps"]);

    let albums = album_list2(
        &server,
        &[("type", "byYear"), ("fromYear", "2020"), ("toYear", "2000")],
    )
    .await;
    let years: Vec<u64> = albums
        .iter()
        .map(|album| album["year"].as_u64().expect("a year"))
        .collect();
    assert_eq!(years, [2020, 2019, 2015]);
}

#[tokio::test]
async fn by_year_requires_both_years() {
    let server = TestServer::standard().await;
    for method in ["getAlbumList", "getAlbumList2"] {
        let by_year = [("type", "byYear")];
        assert_eq!(
            error_of(&server, method, &by_year).await,
            error_10("fromYear")
        );
        assert_eq!(
            error_of(&server, method, &with(&by_year, &[("toYear", "2020")])).await,
            error_10("fromYear")
        );
        assert_eq!(
            error_of(&server, method, &with(&by_year, &[("fromYear", "2000")])).await,
            error_10("toYear")
        );
        // An empty value is an absent value.
        assert_eq!(
            error_of(
                &server,
                method,
                &with(&by_year, &[("fromYear", ""), ("toYear", "2020")])
            )
            .await,
            error_10("fromYear")
        );
        // A value that is no year is error 0 and names the parameter.
        for (from, to, parameter) in [
            ("abc", "2020", "fromYear"),
            ("2000", "20x0", "toYear"),
            ("-1", "2020", "fromYear"),
            ("2000", "4294967296", "toYear"),
        ] {
            let params = with(&by_year, &[("fromYear", from), ("toYear", to)]);
            assert_eq!(
                error_of(&server, method, &params).await,
                error_not_u32(parameter),
                "{from} {to}"
            );
        }
    }
    // The other types ignore the years.
    let params = [("type", "newest"), ("fromYear", "abc")];
    assert_eq!(album_list2(&server, &params).await.len(), 4);
}

#[tokio::test]
async fn by_year_counts_an_album_without_a_year_as_year_0() {
    let server = written(vec![
        Row {
            year: Some(1990),
            ..row("Aurora Field", "Glass Harbour", "Low Tide")
        },
        row("Aurora Field", "Paper Maps", "Meridian"),
    ])
    .await;
    let by_year = |from: &'static str, to: &'static str| {
        let server = &server;
        async move {
            let params = [("type", "byYear"), ("fromYear", from), ("toYear", to)];
            album_list2(server, &params).await
        }
    };
    let albums = by_year("0", "2000").await;
    assert_eq!(texts(&albums, "name"), ["Paper Maps", "Glass Harbour"]);
    assert!(albums[0].get("year").is_none(), "{}", albums[0]);
    assert_eq!(albums[1]["year"], 1990);
    assert_eq!(
        texts(&by_year("2000", "0").await, "name"),
        ["Glass Harbour", "Paper Maps"]
    );
    assert_eq!(
        texts(&by_year("1", "2000").await, "name"),
        ["Glass Harbour"]
    );
}

#[tokio::test]
async fn by_genre_lists_the_albums_of_a_genre_without_regard_to_case() {
    let server = TestServer::standard().await;
    let by_genre = |genre: &'static str| {
        let server = &server;
        async move { names(server, &[("type", "byGenre"), ("genre", genre)]).await }
    };
    assert_eq!(
        by_genre("Electronic").await,
        ["Glass Harbour", "Paper Maps"]
    );
    assert_eq!(
        by_genre("eLECTRONIC").await,
        ["Glass Harbour", "Paper Maps"]
    );
    assert_eq!(by_genre("Jazz").await, ["Slow Rivers"]);
    assert_eq!(by_genre("Pop").await, ["Summer Sampler"]);
    // The match is of the whole genre and no pattern.
    for genre in ["Rock", "Electro", "Electronic%", "%", "_azz", ""] {
        assert_eq!(by_genre(genre).await, [] as [&str; 0], "{genre}");
    }

    for method in ["getAlbumList", "getAlbumList2"] {
        assert_eq!(
            error_of(&server, method, &[("type", "byGenre")]).await,
            error_10("genre")
        );
    }
}

#[tokio::test]
async fn by_genre_binds_the_genre_as_a_value() {
    let server = written(vec![
        Row {
            genre: Some("Rock'n'Roll"),
            ..row("Aurora Field", "Glass Harbour", "Low Tide")
        },
        Row {
            genre: Some("R&B; DROP TABLE albums"),
            ..row("Aurora Field", "Paper Maps", "Meridian")
        },
    ])
    .await;
    for (genre, album) in [
        ("rock'n'roll", "Glass Harbour"),
        ("R&B; DROP TABLE ALBUMS", "Paper Maps"),
    ] {
        let albums = names(&server, &[("type", "byGenre"), ("genre", genre)]).await;
        assert_eq!(albums, [album]);
        let songs = songs_by_genre(&server, &[("genre", genre)]).await;
        assert_eq!(texts(&songs, "album"), [album]);
        let songs = random_songs(&server, &[("genre", genre)]).await;
        assert_eq!(texts(&songs, "album"), [album]);
    }
    for genre in ["' OR '1'='1", "\"]", "[\"Rock'n'Roll\"]"] {
        let params = [("type", "byGenre"), ("genre", genre)];
        assert_eq!(names(&server, &params).await, [] as [&str; 0], "{genre}");
        assert_eq!(songs_by_genre(&server, &[("genre", genre)]).await, NONE);
        assert_eq!(random_songs(&server, &[("genre", genre)]).await, NONE);
    }
    assert_eq!(
        names(&server, &[("type", "alphabeticalByName")]).await,
        ["Glass Harbour", "Paper Maps"]
    );
}

#[tokio::test]
async fn starred_lists_the_starred_albums_with_the_latest_star_first() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let starred = [("type", "starred")];
    assert_eq!(album_list2(&server, &starred).await, NONE);
    assert_eq!(album_list(&server, &starred).await, NONE);

    annotate(
        &server,
        vec![
            (server.album.paper_maps.clone(), "album", star(200)),
            (server.album.slow_rivers.clone(), "album", star(300)),
            (server.album.glass_harbour.clone(), "album", star(200)),
            // A rating is no star, and the star of a track is not the star
            // of its album.
            (server.album.summer_sampler.clone(), "album", rating(5)),
            (server.track.open_road.clone(), "track", star(900)),
        ],
    )
    .await;
    let albums = album_list2(&server, &starred).await;
    assert_eq!(
        texts(&albums, "name"),
        ["Slow Rivers", "Glass Harbour", "Paper Maps"]
    );
    assert_eq!(
        albums[0],
        album_element(&catalogue, &server.album.slow_rivers, Some(&star(300)))
    );
    assert_eq!(albums[0]["starred"], "1970-01-01T00:05:00Z");
    assert_eq!(
        texts(&album_list(&server, &starred).await, "starred"),
        [
            "1970-01-01T00:05:00Z",
            "1970-01-01T00:03:20Z",
            "1970-01-01T00:03:20Z"
        ]
    );
}

#[tokio::test]
async fn highest_lists_the_rated_albums_with_the_highest_rating_first() {
    let server = TestServer::standard().await;
    let highest = [("type", "highest")];
    assert_eq!(album_list2(&server, &highest).await, NONE);
    assert_eq!(album_list(&server, &highest).await, NONE);

    annotate(
        &server,
        vec![
            (server.album.paper_maps.clone(), "album", rating(3)),
            (server.album.summer_sampler.clone(), "album", rating(5)),
            (server.album.glass_harbour.clone(), "album", rating(3)),
            // A star is no rating, and the rating of a track is not the
            // rating of its album.
            (server.album.slow_rivers.clone(), "album", star(100)),
            (server.track.upstream.clone(), "track", rating(5)),
        ],
    )
    .await;
    let albums = album_list2(&server, &highest).await;
    assert_eq!(
        texts(&albums, "name"),
        ["Summer Sampler", "Glass Harbour", "Paper Maps"]
    );
    let ratings: Vec<&Value> = albums.iter().map(|album| &album["userRating"]).collect();
    assert_eq!(ratings, [5, 3, 3]);
    assert_eq!(
        texts(&album_list(&server, &highest).await, "title"),
        ["Summer Sampler", "Glass Harbour", "Paper Maps"]
    );
}

#[tokio::test]
async fn frequent_and_recent_list_the_played_albums() {
    let server = TestServer::standard().await;
    for list in ["frequent", "recent"] {
        assert_eq!(
            album_list2(&server, &[("type", list)]).await,
            NONE,
            "{list}"
        );
        assert_eq!(album_list(&server, &[("type", list)]).await, NONE, "{list}");
    }

    // "Paper Maps" has 2 + 2 plays, the last at 900. "Glass Harbour" and
    // "Summer Sampler" have 3 plays each, the last at 500 and at 700. "Slow
    // Rivers" has a track with a star and no play.
    annotate(
        &server,
        vec![
            (server.track.meridian.clone(), "track", plays(2, 100)),
            (server.track.contour.clone(), "track", plays(2, 900)),
            (server.track.low_tide.clone(), "track", plays(3, 500)),
            (server.track.open_road.clone(), "track", plays(3, 700)),
            (server.track.delta.clone(), "track", star(950)),
        ],
    )
    .await;
    for (method, key, _) in ALBUM_METHODS {
        let ids = |list: &'static str| {
            let server = &server;
            async move {
                let params = [("type", list)];
                texts(&list_of(server, method, &params, key, "album").await, "id")
            }
        };
        // The most plays first. Albums with as many plays are in the order
        // of their titles.
        assert_eq!(
            ids("frequent").await,
            [
                server.album.paper_maps.as_str(),
                &server.album.glass_harbour,
                &server.album.summer_sampler
            ],
            "{method}"
        );
        // The latest play first.
        assert_eq!(
            ids("recent").await,
            [
                server.album.paper_maps.as_str(),
                &server.album.summer_sampler,
                &server.album.glass_harbour
            ],
            "{method}"
        );
    }
}

#[tokio::test]
async fn random_lists_distinct_albums_of_the_library() {
    let server = TestServer::standard().await;
    let all: HashSet<String> = [
        &server.album.glass_harbour,
        &server.album.paper_maps,
        &server.album.slow_rivers,
        &server.album.summer_sampler,
    ]
    .into_iter()
    .cloned()
    .collect();

    for (method, key, _) in ALBUM_METHODS {
        for _ in 0..10 {
            // A list of at least as many albums as the library has holds
            // every album once, in whatever order.
            let albums = list_of(&server, method, &[("type", "random")], key, "album").await;
            assert_eq!(distinct_ids(&albums), all, "{method}");

            let params = [("type", "random"), ("size", "2")];
            let albums = list_of(&server, method, &params, key, "album").await;
            let ids = distinct_ids(&albums);
            assert_eq!(ids.len(), 2, "{method}");
            assert!(ids.is_subset(&all), "{method}: {ids:?}");
        }
    }
    assert_eq!(
        album_list2(&server, &[("type", "random"), ("size", "0")]).await,
        NONE
    );

    // An offset selects no rows of a random list. It leaves as many albums
    // as the list has after the offset, so the pages come to an end.
    for (offset, count) in [("1", 3), ("3", 1), ("4", 0), ("4000000000", 0)] {
        let params = [("type", "random"), ("offset", offset)];
        let albums = album_list2(&server, &params).await;
        assert_eq!(distinct_ids(&albums).len(), count, "{offset}");
    }
    let params = [("type", "random"), ("offset", "1"), ("size", "2")];
    assert_eq!(album_list2(&server, &params).await.len(), 2);
}

#[tokio::test]
async fn offset_pages_through_a_list_without_a_repeat_or_a_gap() {
    let server = TestServer::standard().await;
    // Every album has a star, a rating and a play, so each list holds its
    // albums. Two albums share each value, so each order has a tie.
    let both = |starred_at: i64, rating: u8| Annotation {
        starred_at: Some(starred_at),
        rating: Some(rating),
        ..Annotation::default()
    };
    annotate(
        &server,
        vec![
            (server.album.glass_harbour.clone(), "album", both(200, 3)),
            (server.album.paper_maps.clone(), "album", both(200, 3)),
            (server.album.slow_rivers.clone(), "album", both(300, 5)),
            (server.album.summer_sampler.clone(), "album", both(100, 1)),
            (server.track.low_tide.clone(), "track", plays(3, 500)),
            (server.track.meridian.clone(), "track", plays(3, 500)),
            (server.track.upstream.clone(), "track", plays(1, 900)),
            (server.track.open_road.clone(), "track", plays(5, 100)),
        ],
    )
    .await;

    for (params, _) in LIST_TYPES {
        let list = params[0].1;
        if list == "random" {
            continue;
        }
        let all = texts(&album_list2(&server, params).await, "id");
        let count = if list == "byGenre" { 2 } else { 4 };
        assert_eq!(all.len(), count, "{params:?}");
        for size in [1, 3] {
            let mut paged = Vec::new();
            for start in 0..6 {
                let (size_text, offset) = (size.to_string(), start.to_string());
                let page = with(params, &[("size", &size_text), ("offset", &offset)]);
                let page = texts(&album_list2(&server, &page).await, "id");
                let end = (start + size).min(all.len());
                let expected = all.get(start..end).unwrap_or_default();
                assert_eq!(page, expected, "{params:?} {size} {start}");
                // The directory form of the list has the same pages.
                let dirs = with(params, &[("size", &size_text), ("offset", &offset)]);
                let dirs = texts(&album_list(&server, &dirs).await, "id");
                assert_eq!(dirs, expected, "{params:?} {size} {start}");
                if size == 1 {
                    paged.extend(page);
                }
            }
            // The pages of one album each are the list: no album is there
            // twice and none is missing.
            if size == 1 {
                assert_eq!(paged, all, "{params:?}");
            }
        }
    }
}

#[tokio::test]
async fn size_is_10_without_a_value_and_at_most_500() {
    // 510 albums of one track each, all of one genre.
    let rows = (0..510)
        .map(|number| Row {
            genre: Some("Jazz"),
            year: Some(2000),
            ..row(
                "Aurora Field",
                &format!("Album {number:03}"),
                &format!("Title {number:03}"),
            )
        })
        .collect();
    let server = written(rows).await;
    let by_name = [("type", "alphabeticalByName")];
    let expected: Vec<String> = (0..510)
        .map(|number| format!("Album {number:03}"))
        .collect();

    for (method, key, field) in ALBUM_METHODS {
        let list = |params: Vec<(&'static str, &'static str)>| {
            let server = &server;
            async move { list_of(server, method, &params, key, "album").await }
        };
        assert_eq!(texts(&list(by_name.to_vec()).await, field), expected[..10]);
        // An empty value is an absent value.
        let empty = with(&by_name, &[("size", ""), ("offset", "")]);
        assert_eq!(texts(&list(empty).await, field), expected[..10]);
        let params = with(&by_name, &[("size", "500")]);
        assert_eq!(texts(&list(params).await, field), expected[..500]);
        for size in ["501", "1000", "4294967295"] {
            let params = with(&by_name, &[("size", size)]);
            assert_eq!(texts(&list(params).await, field), expected[..500], "{size}");
        }
        // The second page of 500 holds the rest.
        let params = with(&by_name, &[("size", "1000"), ("offset", "500")]);
        assert_eq!(texts(&list(params).await, field), expected[500..]);
        let params = with(&by_name, &[("size", "0")]);
        assert_eq!(list(params).await, NONE);

        for size in ["1000", "4294967295"] {
            let albums = list(vec![("type", "random"), ("size", size)]).await;
            assert_eq!(distinct_ids(&albums).len(), 500, "{method} {size}");
        }
        let albums = list(vec![("type", "random")]).await;
        assert_eq!(distinct_ids(&albums).len(), 10, "{method}");
        // 510 albums less the offset of 500.
        let albums = list(vec![
            ("type", "random"),
            ("size", "1000"),
            ("offset", "500"),
        ])
        .await;
        assert_eq!(distinct_ids(&albums).len(), 10, "{method}");
    }

    for size in ["1000", "4294967295"] {
        let songs = random_songs(&server, &[("size", size)]).await;
        assert_eq!(distinct_ids(&songs).len(), 500, "{size}");
        let songs = songs_by_genre(&server, &[("genre", "Jazz"), ("count", size)]).await;
        assert_eq!(distinct_ids(&songs).len(), 500, "{size}");
    }
    assert_eq!(random_songs(&server, &[]).await.len(), 10);
    assert_eq!(random_songs(&server, &[("size", "")]).await.len(), 10);
    assert_eq!(
        songs_by_genre(&server, &[("genre", "Jazz")]).await.len(),
        10
    );

    let titles: Vec<String> = (0..510)
        .map(|number| format!("Title {number:03}"))
        .collect();
    let params = [("genre", "Jazz"), ("count", "1000")];
    assert_eq!(
        texts(&songs_by_genre(&server, &params).await, "title"),
        titles[..500]
    );
    let params = [("genre", "Jazz"), ("count", "1000"), ("offset", "500")];
    assert_eq!(
        texts(&songs_by_genre(&server, &params).await, "title"),
        titles[500..]
    );
}

#[tokio::test]
async fn music_folder_id_restricts_every_list_to_one_folder() {
    let server = TestServer::standard().await;
    let folder = server.folder_id.to_string();
    let other = (server.folder_id + 1).to_string();
    for (params, count) in LIST_TYPES {
        let here = with(params, &[("musicFolderId", &folder)]);
        assert_eq!(album_list2(&server, &here).await.len(), count, "{params:?}");
        assert_eq!(album_list(&server, &here).await.len(), count, "{params:?}");
        let elsewhere = with(params, &[("musicFolderId", &other)]);
        assert_eq!(album_list2(&server, &elsewhere).await, NONE, "{params:?}");
        assert_eq!(album_list(&server, &elsewhere).await, NONE, "{params:?}");
    }
    assert_eq!(
        random_songs(&server, &[("musicFolderId", &folder)])
            .await
            .len(),
        10
    );
    assert_eq!(
        random_songs(&server, &[("musicFolderId", &other)]).await,
        NONE
    );
    let jazz = [("genre", "Jazz"), ("musicFolderId", folder.as_str())];
    assert_eq!(songs_by_genre(&server, &jazz).await.len(), 2);
    let jazz = [("genre", "Jazz"), ("musicFolderId", other.as_str())];
    assert_eq!(songs_by_genre(&server, &jazz).await, NONE);

    // Two music folders. "Glass Harbour" has a track in each.
    let server = written(vec![
        Row {
            genre: Some("Jazz"),
            ..row("Aurora Field", "Glass Harbour", "Low Tide")
        },
        Row {
            folder: "/other",
            genre: Some("Jazz"),
            ..row("Aurora Field", "Glass Harbour", "Breakwater")
        },
        Row {
            folder: "/other",
            genre: Some("Jazz"),
            ..row("Marlow Quartet", "Slow Rivers", "Upstream")
        },
    ])
    .await;
    let music = folder_id(&server, "/music").await;
    let other = folder_id(&server, "/other").await;
    let in_folder: [(&str, &[&str]); 2] = [
        (&music, &["Glass Harbour"]),
        (&other, &["Glass Harbour", "Slow Rivers"]),
    ];
    for list in ["alphabeticalByName", "random", "newest", "byGenre"] {
        for (folder, expected) in in_folder {
            let params = [("type", list), ("genre", "Jazz"), ("musicFolderId", folder)];
            // The random list has no order, so the names are compared in
            // the order of their text.
            let mut found = names(&server, &params).await;
            found.sort();
            assert_eq!(found, expected, "{list} {folder}");
        }
    }
    for _ in 0..10 {
        let songs = random_songs(&server, &[("musicFolderId", &music)]).await;
        assert_eq!(texts(&songs, "title"), ["Low Tide"]);
        let songs = random_songs(&server, &[("musicFolderId", &other)]).await;
        let titles: HashSet<String> = texts(&songs, "title").into_iter().collect();
        assert_eq!(
            titles,
            HashSet::from(["Breakwater".to_owned(), "Upstream".to_owned()])
        );
    }
    let songs = songs_by_genre(&server, &[("genre", "Jazz"), ("musicFolderId", &other)]).await;
    assert_eq!(texts(&songs, "title"), ["Breakwater", "Upstream"]);

    // A value that is no integer is error 0.
    for method in [
        "getAlbumList",
        "getAlbumList2",
        "getRandomSongs",
        "getSongsByGenre",
    ] {
        let params = [
            ("type", "newest"),
            ("genre", "Jazz"),
            ("musicFolderId", "x"),
        ];
        assert_eq!(
            error_of(&server, method, &params).await,
            error(0, "parameter musicFolderId must be an integer"),
            "{method}"
        );
    }
}

#[tokio::test]
async fn album_list_requires_a_type_that_is_a_list_type() {
    let server = TestServer::standard().await;
    for method in ["getAlbumList", "getAlbumList2"] {
        assert_eq!(error_of(&server, method, &[]).await, error_10("type"));
        assert_eq!(
            error_of(&server, method, &[("size", "5")]).await,
            error_10("type")
        );
        // The names are those of the API in their letter case.
        for unknown in ["", "all", "Newest", "byyear", "alphabeticalByName ", "by%"] {
            assert_eq!(
                error_of(&server, method, &[("type", unknown)]).await,
                error(0, UNKNOWN_TYPE),
                "{method} {unknown:?}"
            );
        }
        for (parameter, value) in [
            ("size", "ten"),
            ("size", "-1"),
            ("size", "4294967296"),
            ("offset", "1.5"),
            ("offset", "-1"),
        ] {
            let params = [("type", "newest"), (parameter, value)];
            assert_eq!(
                error_of(&server, method, &params).await,
                error_not_u32(parameter),
                "{method} {parameter}={value}"
            );
        }
    }

    // The errors are in the format of the request.
    let xml = server.get_xml("getAlbumList2", &[]).await;
    assert!(
        xml.contains(r#"status="failed""#)
            && xml.contains(r#"<error code="10" message="required parameter is missing: type"/>"#),
        "{xml}"
    );
    let xml = server.get_xml("getAlbumList", &[("type", "all")]).await;
    assert!(
        xml.contains(&format!(r#"<error code="0" message="{UNKNOWN_TYPE}"/>"#)),
        "{xml}"
    );
}

#[tokio::test]
async fn album_lists_in_xml_hold_one_album_element_for_each_album() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let jazz = [("type", "byGenre"), ("genre", "Jazz")];
    let row = catalogue.album("Slow Rivers");
    let id = &server.album.slow_rivers;

    // The whole document. The cover of the album is the picture in its
    // tracks.
    let expected = format!(
        concat!(
            r#"><albumList2><album artist="Marlow Quartet" artistId="{artist}" coverArt="{id}""#,
            r#" created="{created}" duration="{duration}" genre="Jazz" id="{id}""#,
            r#" name="Slow Rivers" playCount="0" songCount="2" year="2015"/></albumList2>"#,
            "</subsonic-response>"
        ),
        artist = server.artist.marlow_quartet,
        id = id,
        created = model::iso8601(row.created_at),
        duration = model::seconds(row.duration_ms),
    );
    assert_eq!(
        server.get_xml("getAlbumList2", &jazz).await,
        xml_document(&expected)
    );

    let expected = format!(
        concat!(
            r#"><albumList><album album="Slow Rivers" artist="Marlow Quartet""#,
            r#" artistId="{artist}" coverArt="{id}" created="{created}" duration="{duration}""#,
            r#" genre="Jazz" id="{id}" isDir="true" mediaType="album" parent="{artist}""#,
            r#" playCount="0" title="Slow Rivers" year="2015"/></albumList></subsonic-response>"#
        ),
        artist = server.artist.marlow_quartet,
        id = id,
        created = model::iso8601(row.created_at),
        duration = model::seconds(row.duration_ms),
    );
    assert_eq!(
        server.get_xml("getAlbumList", &jazz).await,
        xml_document(&expected)
    );

    // Every album of a list, in the order of the list.
    let by_name = [("type", "alphabeticalByName")];
    let elements: String = album_list2(&server, &by_name)
        .await
        .iter()
        .map(|album| format!("<album{}/>", xml_attributes(album)))
        .collect();
    assert_eq!(
        server.get_xml("getAlbumList2", &by_name).await,
        xml_document(&format!(
            "><albumList2>{elements}</albumList2></subsonic-response>"
        ))
    );
    let elements: String = album_list(&server, &by_name)
        .await
        .iter()
        .map(|album| format!("<album{}/>", xml_attributes(album)))
        .collect();
    assert_eq!(
        server.get_xml("getAlbumList", &by_name).await,
        xml_document(&format!(
            "><albumList>{elements}</albumList></subsonic-response>"
        ))
    );

    // A list without an album is an element without content.
    let rock = [("type", "byGenre"), ("genre", "Rock")];
    assert_eq!(
        server.get_xml("getAlbumList", &rock).await,
        xml_document("><albumList/></subsonic-response>")
    );
}

/// The IDs of the ten tracks of the standard library.
fn all_tracks(server: &StandardServer) -> HashSet<String> {
    let track = &server.track;
    [
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
    ]
    .into_iter()
    .cloned()
    .collect()
}

/// `ids` as a set of owned IDs.
fn id_set(ids: &[&String]) -> HashSet<String> {
    ids.iter().map(|id| (*id).clone()).collect()
}

#[tokio::test]
async fn random_songs_returns_distinct_songs_of_the_library() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let all = all_tracks(&server);

    for _ in 0..10 {
        // The library has ten tracks and the default size is 10, so the
        // list holds every track once.
        let songs = random_songs(&server, &[]).await;
        assert_eq!(distinct_ids(&songs), all);
        // Each entry is the whole `Child` element of its track.
        for song in &songs {
            let id = song["id"].as_str().expect("an ID");
            assert_eq!(*song, song_element(&catalogue, id, None));
        }

        let songs = random_songs(&server, &[("size", "3")]).await;
        let ids = distinct_ids(&songs);
        assert_eq!(ids.len(), 3);
        assert!(ids.is_subset(&all), "{ids:?}");

        let songs = random_songs(&server, &[("size", "1")]).await;
        assert!(distinct_ids(&songs).is_subset(&all));
        assert_eq!(songs.len(), 1);
    }
    assert_eq!(
        distinct_ids(&random_songs(&server, &[("size", "500")]).await),
        all
    );
    assert_eq!(random_songs(&server, &[("size", "0")]).await, NONE);

    for (parameter, value) in [
        ("size", "many"),
        ("size", "-3"),
        ("fromYear", "MCMXC"),
        ("toYear", "-1"),
    ] {
        assert_eq!(
            error_of(&server, "getRandomSongs", &[(parameter, value)]).await,
            error_not_u32(parameter),
            "{parameter}={value}"
        );
    }
}

#[tokio::test]
async fn random_songs_follows_genre_and_years() {
    let server = TestServer::standard().await;
    let track = &server.track;
    let electronic = id_set(&[
        &track.low_tide,
        &track.breakwater,
        &track.night_ferry,
        &track.meridian,
        &track.contour,
        &track.legend,
    ]);
    let jazz = id_set(&[&track.upstream, &track.delta]);
    let paper_maps = id_set(&[&track.meridian, &track.contour, &track.legend]);
    let from_2020 = id_set(&[
        &track.meridian,
        &track.contour,
        &track.legend,
        &track.open_road,
        &track.last_train,
    ]);
    let of_2019_and_2020 = id_set(&[
        &track.low_tide,
        &track.breakwater,
        &track.night_ferry,
        &track.open_road,
        &track.last_train,
    ]);
    // Each filter matches fewer tracks than the default size, so the list
    // holds every track that the filter allows.
    let cases: [(&Query, &HashSet<String>); 9] = [
        (&[("genre", "Electronic")], &electronic),
        (&[("genre", "Jazz")], &jazz),
        (&[("genre", "jAZZ")], &jazz),
        (&[("fromYear", "2020")], &from_2020),
        (&[("toYear", "2015")], &jazz),
        (
            &[("fromYear", "2019"), ("toYear", "2020")],
            &of_2019_and_2020,
        ),
        (
            &[("genre", "Electronic"), ("fromYear", "2021")],
            &paper_maps,
        ),
        (
            &[
                ("genre", "Electronic"),
                ("toYear", "2021"),
                ("fromYear", "2020"),
            ],
            &paper_maps,
        ),
        (
            &[("fromYear", "2015"), ("toYear", "2015"), ("size", "500")],
            &jazz,
        ),
    ];
    for _ in 0..5 {
        for (params, expected) in cases {
            let songs = random_songs(&server, params).await;
            assert_eq!(distinct_ids(&songs), *expected, "{params:?}");
        }
        let params = [("genre", "Electronic"), ("size", "4")];
        let ids = distinct_ids(&random_songs(&server, &params).await);
        assert_eq!(ids.len(), 4);
        assert!(ids.is_subset(&electronic), "{ids:?}");
    }

    let none: [&[(&str, &str)]; 5] = [
        &[("genre", "Rock")],
        &[("genre", "Jaz")],
        &[("genre", "Jazz"), ("fromYear", "2016")],
        &[("fromYear", "2022")],
        &[("fromYear", "2020"), ("toYear", "2019")],
    ];
    for params in none {
        assert_eq!(random_songs(&server, params).await, NONE, "{params:?}");
    }
    // An empty value restricts nothing.
    let params = [("genre", ""), ("fromYear", ""), ("toYear", "")];
    assert_eq!(
        distinct_ids(&random_songs(&server, &params).await),
        all_tracks(&server)
    );
}

#[tokio::test]
async fn random_songs_never_returns_a_track_without_a_file() {
    let server = TestServer::standard().await;
    remove_file(&server, &server.track.delta).await;
    let mut with_files = all_tracks(&server);
    with_files.remove(&server.track.delta);
    for _ in 0..10 {
        let songs = random_songs(&server, &[]).await;
        assert_eq!(distinct_ids(&songs), with_files);
        let songs = random_songs(&server, &[("genre", "Jazz")]).await;
        assert_eq!(texts(&songs, "id"), [server.track.upstream.as_str()]);
    }
}

#[tokio::test]
async fn songs_by_genre_lists_the_songs_of_a_genre_by_title() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let electronic = [
        "Breakwater",
        "Contour",
        "Legend",
        "Low Tide",
        "Meridian",
        "Night Ferry",
    ];
    let songs = songs_by_genre(&server, &[("genre", "Electronic")]).await;
    assert_eq!(texts(&songs, "title"), electronic);
    // Each entry is the whole `Child` element of its track.
    for song in &songs {
        let id = song["id"].as_str().expect("an ID");
        assert_eq!(*song, song_element(&catalogue, id, None));
        assert_eq!(song["genre"], "Electronic");
        assert_eq!(song["isDir"], false);
    }
    assert_eq!(songs[0]["id"], json!(server.track.breakwater));
    assert_eq!(songs[0]["parent"], json!(server.album.glass_harbour));
    assert_eq!(
        songs[0]["path"],
        "Aurora Field/Glass Harbour/02 Breakwater.flac"
    );

    // The genre matches without regard to case, as a whole and as no
    // pattern.
    let titles = |genre: &'static str| {
        let server = &server;
        async move { texts(&songs_by_genre(server, &[("genre", genre)]).await, "title") }
    };
    assert_eq!(titles("electronic").await, electronic);
    assert_eq!(titles("ELECTRONIC").await, electronic);
    assert_eq!(titles("Jazz").await, ["Delta", "Upstream"]);
    assert_eq!(titles("Pop").await, ["Last Train", "Open Road"]);
    for genre in ["Rock", "Electro", "Electronic%", "%", "_azz", ""] {
        assert_eq!(titles(genre).await, [] as [&str; 0], "{genre}");
    }
}

#[tokio::test]
async fn songs_by_genre_pages_with_count_and_offset() {
    let server = TestServer::standard().await;
    let all = texts(
        &songs_by_genre(&server, &[("genre", "Electronic")]).await,
        "id",
    );
    assert_eq!(all.len(), 6);

    let page = |count: &'static str, offset: &'static str| {
        let server = &server;
        async move {
            let params = [
                ("genre", "Electronic"),
                ("count", count),
                ("offset", offset),
            ];
            texts(&songs_by_genre(server, &params).await, "id")
        }
    };
    assert_eq!(page("2", "0").await, all[..2]);
    assert_eq!(page("2", "2").await, all[2..4]);
    assert_eq!(page("2", "4").await, all[4..]);
    assert_eq!(page("2", "6").await, [] as [&str; 0]);
    assert_eq!(page("4", "4").await, all[4..]);
    assert_eq!(page("0", "0").await, [] as [&str; 0]);
    assert_eq!(page("500", "5").await, all[5..]);
    assert_eq!(page("1", "4294967295").await, [] as [&str; 0]);
    let mut paged = Vec::new();
    for offset in ["0", "1", "2", "3", "4", "5", "6"] {
        paged.extend(page("1", offset).await);
    }
    assert_eq!(paged, all);

    assert_eq!(
        error_of(&server, "getSongsByGenre", &[]).await,
        error_10("genre")
    );
    assert_eq!(
        error_of(&server, "getSongsByGenre", &[("count", "5")]).await,
        error_10("genre")
    );
    for (parameter, value) in [("count", "x"), ("count", "-1"), ("offset", "x")] {
        let params = [("genre", "Jazz"), (parameter, value)];
        assert_eq!(
            error_of(&server, "getSongsByGenre", &params).await,
            error_not_u32(parameter),
            "{parameter}={value}"
        );
    }
}

#[tokio::test]
async fn songs_by_genre_lists_a_track_without_a_file_only_without_a_folder() {
    let server = TestServer::standard().await;
    remove_file(&server, &server.track.delta).await;

    let songs = songs_by_genre(&server, &[("genre", "Jazz")]).await;
    assert_eq!(texts(&songs, "title"), ["Delta", "Upstream"]);
    // The track without a file has no path. Its other fields stay.
    assert!(songs[0].get("path").is_none(), "{}", songs[0]);
    assert_eq!(songs[0]["suffix"], "m4a");
    assert_eq!(
        songs[1]["path"],
        "Marlow Quartet/Slow Rivers/01 Upstream.m4a"
    );

    let folder = server.folder_id.to_string();
    let params = [("genre", "Jazz"), ("musicFolderId", folder.as_str())];
    let songs = songs_by_genre(&server, &params).await;
    assert_eq!(texts(&songs, "title"), ["Upstream"]);
}

#[tokio::test]
async fn song_lists_report_the_annotations_of_the_user() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let annotation = Annotation {
        starred_at: Some(1_600_000_000),
        rating: Some(4),
        play_count: 7,
        last_played_at: Some(1_600_000_060),
    };
    annotate(
        &server,
        vec![(server.track.delta.clone(), "track", annotation)],
    )
    .await;

    let expected = song_element(&catalogue, &server.track.delta, Some(&annotation));
    assert_eq!(expected["starred"], "2020-09-13T12:26:40Z");
    assert_eq!(expected["userRating"], 4);
    assert_eq!(expected["playCount"], 7);
    assert_eq!(expected["played"], "2020-09-13T12:27:40Z");

    let songs = songs_by_genre(&server, &[("genre", "Jazz")]).await;
    assert_eq!(songs[0], expected);
    assert!(songs[1].get("starred").is_none(), "{}", songs[1]);
    assert_eq!(songs[1]["playCount"], 0);

    let songs = random_songs(&server, &[("genre", "Jazz")]).await;
    let delta = songs
        .iter()
        .find(|song| song["id"] == json!(server.track.delta))
        .expect("the list holds both Jazz tracks");
    assert_eq!(*delta, expected);
}

#[tokio::test]
async fn song_lists_in_xml_hold_one_song_element_for_each_song() {
    let server = TestServer::standard().await;

    // The whole document of a list in a fixed order.
    let jazz = [("genre", "Jazz")];
    let songs = songs_by_genre(&server, &jazz).await;
    assert_eq!(texts(&songs, "title"), ["Delta", "Upstream"]);
    let elements: String = songs
        .iter()
        .map(|song| format!("<song{}/>", xml_attributes(song)))
        .collect();
    let xml = server.get_xml("getSongsByGenre", &jazz).await;
    assert_eq!(
        xml,
        xml_document(&format!(
            "><songsByGenre>{elements}</songsByGenre></subsonic-response>"
        ))
    );
    assert!(
        xml.contains(r#"<song album="Slow Rivers" albumId=""#)
            && xml.contains(r#" isDir="false" mediaType="song" "#)
            && xml.contains(r#" title="Delta" track="2" type="music" year="2015"/>"#),
        "{xml}"
    );

    // One random song of one track is that track.
    let one = [("genre", "Jazz"), ("toYear", "2015"), ("size", "500")];
    remove_file(&server, &server.track.delta).await;
    let songs = random_songs(&server, &one).await;
    assert_eq!(texts(&songs, "title"), ["Upstream"]);
    assert_eq!(
        server.get_xml("getRandomSongs", &one).await,
        xml_document(&format!(
            "><randomSongs><song{}/></randomSongs></subsonic-response>",
            xml_attributes(&songs[0])
        ))
    );

    // A list without a song is an element without content.
    assert_eq!(
        server.get_xml("getRandomSongs", &[("genre", "Rock")]).await,
        xml_document("><randomSongs/></subsonic-response>")
    );
    assert_eq!(
        server
            .get_xml("getSongsByGenre", &[("genre", "Rock")])
            .await,
        xml_document("><songsByGenre/></subsonic-response>")
    );
    let xml = server.get_xml("getSongsByGenre", &[]).await;
    assert!(
        xml.contains(r#"<error code="10" message="required parameter is missing: genre"/>"#),
        "{xml}"
    );
}

#[tokio::test]
async fn list_methods_answer_a_post_with_a_form_body_and_the_view_suffix() {
    let server = TestServer::standard().await;
    let form = [("type", "byGenre"), ("genre", "Jazz"), ("f", "json")];
    let response = server.post_form("getAlbumList2.view", &form).await;
    let response = common::subsonic_response(&common::body_string(response).await);
    assert_eq!(response["status"], "ok");
    assert_eq!(
        response["albumList2"]["album"][0]["id"],
        json!(server.album.slow_rivers)
    );

    let form = [("genre", "Pop"), ("count", "1"), ("f", "json")];
    let response = server.post_form("getSongsByGenre", &form).await;
    let response = common::subsonic_response(&common::body_string(response).await);
    assert_eq!(
        response["songsByGenre"]["song"][0]["id"],
        json!(server.track.last_train)
    );
}
