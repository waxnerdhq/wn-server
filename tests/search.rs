//! Router tests of the search methods `search2`, `search3` and `search`
//! (`docs/design.md` section 4).

mod common;

use std::collections::HashSet;
use std::path::PathBuf;

use axum::http::StatusCode;
use rusqlite::params;
use serde_json::{Value, json};
use wn_server::api::{self, model, xml};
use wn_server::config::Config;
use wn_server::db::queries::tracks::{self, TrackContent};
use wn_server::db::queries::{albums, artists, folders};
use wn_server::db::{self};
use wn_server::library::tags::TrackMeta;

use common::{
    Catalogue, LibraryBuilder, OWNER, StandardServer, TestServer, body_string, child_names,
    content_type, error, first_element, json_ok, remove_file, schema_names, subsonic_response,
    to_json, xml_failed, xml_ok,
};

/// The two methods that search the library, each with the key of its answer.
const METHODS: [(&str, &str); 2] = [("search3", "searchResult3"), ("search2", "searchResult2")];

/// The names of the artists of the standard library, in the order of a
/// search.
const ARTISTS: [&str; 3] = ["Aurora Field", "Marlow Quartet", "Various Artists"];

/// The titles of the albums of the standard library, in the order of a
/// search.
const ALBUMS: [&str; 4] = [
    "Glass Harbour",
    "Paper Maps",
    "Slow Rivers",
    "Summer Sampler",
];

/// The titles of the tracks of the standard library, in the order of a
/// search.
const SONGS: [&str; 10] = [
    "Breakwater",
    "Contour",
    "Delta",
    "Last Train",
    "Legend",
    "Low Tide",
    "Meridian",
    "Night Ferry",
    "Open Road",
    "Upstream",
];

/// The message of the error for a query that is too long.
const QUERY_TOO_LONG: &str = "parameter query must be at most 1024 characters";

/// The message of the error for a query of too many terms.
const TOO_MANY_TERMS: &str = "parameter query must have at most 32 terms";

/// The error 0 for a count or an offset that is not a number from 0 to
/// 4294967295.
fn not_a_number(name: &str) -> Value {
    error(
        0,
        &format!("parameter {name} must be an integer from 0 to 4294967295"),
    )
}

/// The text between the start tag and the end tag of the element `name` of
/// the XML document `xml`. The start tag has no attribute.
fn element_content<'a>(xml: &'a str, name: &str) -> &'a str {
    let (_, rest) = xml
        .split_once(&format!("<{name}>"))
        .unwrap_or_else(|| panic!("no <{name}> in {xml}"));
    let (content, _) = rest
        .split_once(&format!("</{name}>"))
        .unwrap_or_else(|| panic!("no </{name}> in {xml}"));
    content
}

/// The values of `field` of the items of `list`, in the order of the list.
fn field(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .unwrap_or_else(|| panic!("not a list: {list}"))
        .iter()
        .map(|item| {
            item[field]
                .as_str()
                .unwrap_or_else(|| panic!("no {field} in {item}"))
                .to_owned()
        })
        .collect()
}

/// The names of the artists, the titles of the albums and the titles of the
/// songs of a `searchResult3` or a `searchResult2`.
fn names(method: &str, result: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let album_title = if method == "search3" { "name" } else { "title" };
    (
        field(&result["artist"], "name"),
        field(&result["album"], album_title),
        field(&result["song"], "title"),
    )
}

/// Sends `method` with `query` and a count of 500 for each list. Returns
/// the names of the answer.
async fn find(
    server: &TestServer,
    method: &str,
    query: &str,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let key = result_key(method);
    let response = server
        .get_json(
            method,
            &[
                ("query", query),
                ("artistCount", "500"),
                ("albumCount", "500"),
                ("songCount", "500"),
            ],
        )
        .await;
    assert_eq!(response["status"], "ok", "{method} {query:?}: {response}");
    names(method, &response[key])
}

/// The key of the answer of `method`.
fn result_key(method: &str) -> &'static str {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, key)| *key)
        .unwrap_or_else(|| panic!("{method} is no search method"))
}

/// What [`find`] returns for these names.
fn found(
    artists: &[&str],
    albums: &[&str],
    songs: &[&str],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let owned = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
    (owned(artists), owned(albums), owned(songs))
}

/// The `searchResult3` of the whole library `catalogue`.
fn whole_result3(catalogue: &Catalogue) -> Value {
    json!({
        "artist": ARTISTS
            .iter()
            .map(|name| to_json(model::artist(catalogue.artist(name), None)))
            .collect::<Vec<_>>(),
        "album": ALBUMS
            .iter()
            .map(|title| to_json(model::album(catalogue.album(title), None)))
            .collect::<Vec<_>>(),
        "song": SONGS
            .iter()
            .map(|title| to_json(model::child(catalogue.song(title), None)))
            .collect::<Vec<_>>(),
    })
}

/// The `searchResult2` of the whole library `catalogue`.
fn whole_result2(catalogue: &Catalogue) -> Value {
    json!({
        "artist": ARTISTS
            .iter()
            .map(|name| to_json(model::artist_dir(catalogue.artist(name), None)))
            .collect::<Vec<_>>(),
        "album": ALBUMS
            .iter()
            .map(|title| to_json(model::child_dir_for_album(catalogue.album(title), None)))
            .collect::<Vec<_>>(),
        "song": SONGS
            .iter()
            .map(|title| to_json(model::child(catalogue.song(title), None)))
            .collect::<Vec<_>>(),
    })
}

/// Every ID of the standard library: the artists, the albums and the tracks.
fn standard_ids(server: &StandardServer) -> HashSet<String> {
    [
        &server.artist.aurora_field,
        &server.artist.marlow_quartet,
        &server.artist.various_artists,
        &server.album.glass_harbour,
        &server.album.paper_maps,
        &server.album.slow_rivers,
        &server.album.summer_sampler,
        &server.track.low_tide,
        &server.track.breakwater,
        &server.track.night_ferry,
        &server.track.meridian,
        &server.track.contour,
        &server.track.legend,
        &server.track.upstream,
        &server.track.delta,
        &server.track.open_road,
        &server.track.last_train,
    ]
    .into_iter()
    .cloned()
    .collect()
}

/// Stores the track `n` in the music folder `folder` as the scanner stores
/// a track, with the rows of its artist and of its album. A test uses it for
/// names that are no file names on every OS and for libraries of many rows.
fn add_track(
    conn: &rusqlite::Connection,
    n: u32,
    folder: &str,
    artist: &str,
    album: &str,
    title: &str,
) -> db::Result<String> {
    let folder = folders::sync_folders(conn, &[PathBuf::from(folder)])?;
    let artist_id = artists::upsert_by_key(conn, artist)?;
    let album_id = albums::upsert_by_key(conn, &artist_id, album, artist, false)?;
    let meta = TrackMeta {
        title: title.to_owned(),
        artist: artist.to_owned(),
        album_artist: artist.to_owned(),
        album: album.to_owned(),
        track_no: None,
        disc_no: None,
        year: None,
        genre: None,
        compilation: false,
        duration_ms: 1000,
        bitrate_kbps: None,
        sample_rate: None,
        bit_depth: None,
        channels: None,
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
    let id = format!("tr-{n:032x}");
    tracks::insert_track(conn, &id, folder[0].id, &format!("{n}.flac"), &content)?;
    Ok(id)
}

/// A server without music files whose database holds the tracks that `add`
/// stores.
async fn server_with(
    add: impl FnOnce(&rusqlite::Connection) -> db::Result<()> + Send + 'static,
) -> TestServer {
    let server = TestServer::bare(Config::default()).await;
    server
        .db()
        .write(move |conn| {
            add(conn)?;
            albums::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .await
        .expect("store the library");
    server
}

#[tokio::test]
async fn search3_with_an_empty_query_returns_every_artist_album_and_song() {
    let server = TestServer::standard().await;
    let expected = whole_result3(&server.catalogue().await);

    // `query=`, the two quotes that some clients send, and white space.
    for query in ["", "\"\"", " ", "\" \"", "\t"] {
        let response = server.get_json("search3", &[("query", query)]).await;
        assert_eq!(response["status"], "ok", "{query:?}");
        assert_eq!(response["searchResult3"], expected, "{query:?}");
        // The answer has the envelope fields and the result, and no other.
        assert_eq!(
            response.as_object().map(serde_json::Map::len),
            Some(6),
            "{response}"
        );
    }

    let result = &server.get_json("search3", &[("query", "")]).await["searchResult3"];
    assert_eq!(
        names("search3", result),
        found(&ARTISTS, &ALBUMS, &SONGS),
        "{result}"
    );
    // An artist is an `ArtistID3`, an album is an `AlbumID3` and a song is
    // a `Child`.
    assert_eq!(
        result["artist"][0],
        json!({
            "id": server.artist.aurora_field,
            "name": "Aurora Field",
            "albumCount": 2,
        })
    );
    let album = &result["album"][0];
    assert_eq!(album["id"], json!(server.album.glass_harbour));
    assert_eq!(album["name"], "Glass Harbour");
    assert_eq!(album["artist"], "Aurora Field");
    assert_eq!(album["artistId"], json!(server.artist.aurora_field));
    assert_eq!(album["songCount"], 3);
    assert_eq!(album["coverArt"], json!(server.album.glass_harbour));
    assert_eq!(album["year"], 2019);
    assert!(album.get("isDir").is_none(), "{album}");
    assert!(album.get("song").is_none(), "{album}");
    let song = &result["song"][0];
    assert_eq!(song["id"], json!(server.track.breakwater));
    assert_eq!(song["title"], "Breakwater");
    assert_eq!(song["isDir"], false);
    assert_eq!(song["parent"], json!(server.album.glass_harbour));
    assert_eq!(song["albumId"], json!(server.album.glass_harbour));
    assert_eq!(song["artistId"], json!(server.artist.aurora_field));
    assert_eq!(
        song["path"],
        "Aurora Field/Glass Harbour/02 Breakwater.flac"
    );
    assert_eq!(song["type"], "music");
}

#[tokio::test]
async fn search2_returns_the_same_matches_as_folder_elements() {
    let server = TestServer::standard().await;
    let expected = whole_result2(&server.catalogue().await);

    for query in ["", "\"\""] {
        let response = server.get_json("search2", &[("query", query)]).await;
        assert_eq!(response["status"], "ok", "{query:?}");
        assert_eq!(response["searchResult2"], expected, "{query:?}");
        assert!(response.get("searchResult3").is_none(), "{response}");
    }

    let result = &server.get_json("search2", &[("query", "")]).await["searchResult2"];
    assert_eq!(names("search2", result), found(&ARTISTS, &ALBUMS, &SONGS));
    // An artist is the `Artist` of the folder methods, without an album
    // count. An album is a `Child` that is a directory.
    assert_eq!(
        result["artist"][0],
        json!({ "id": server.artist.aurora_field, "name": "Aurora Field" })
    );
    let album = &result["album"][0];
    assert_eq!(album["id"], json!(server.album.glass_harbour));
    assert_eq!(album["isDir"], true);
    assert_eq!(album["title"], "Glass Harbour");
    assert_eq!(album["album"], "Glass Harbour");
    assert_eq!(album["artist"], "Aurora Field");
    assert_eq!(album["parent"], json!(server.artist.aurora_field));
    assert_eq!(album["coverArt"], json!(server.album.glass_harbour));
    assert!(album.get("name").is_none(), "{album}");
    assert!(album.get("songCount").is_none(), "{album}");
    let song = &result["song"][0];
    assert_eq!(song["id"], json!(server.track.breakwater));
    assert_eq!(song["isDir"], false);

    // The songs of the two methods are the same elements.
    let result3 = &server.get_json("search3", &[("query", "")]).await["searchResult3"];
    assert_eq!(result["song"], result3["song"]);
}

#[tokio::test]
async fn counts_and_offsets_select_a_part_of_each_list() {
    let server = TestServer::standard().await;
    for (method, key) in METHODS {
        let page = |params: &'static [(&'static str, &'static str)]| {
            let server = &server;
            async move {
                let mut all = vec![("query", "")];
                all.extend_from_slice(params);
                let response = server.get_json(method, &all).await;
                assert_eq!(response["status"], "ok", "{method} {params:?}");
                names(method, &response[key])
            }
        };

        // Each list has its own count and its own offset.
        assert_eq!(
            page(&[
                ("artistCount", "1"),
                ("albumCount", "2"),
                ("songCount", "3")
            ])
            .await,
            found(&ARTISTS[..1], &ALBUMS[..2], &SONGS[..3])
        );
        assert_eq!(
            page(&[
                ("artistCount", "1"),
                ("artistOffset", "2"),
                ("albumCount", "2"),
                ("albumOffset", "1"),
                ("songCount", "3"),
                ("songOffset", "4"),
            ])
            .await,
            found(&ARTISTS[2..], &ALBUMS[1..3], &SONGS[4..7])
        );
        // An offset alone leaves the default count of 20.
        assert_eq!(
            page(&[("songOffset", "8")]).await,
            found(&ARTISTS, &ALBUMS, &SONGS[8..])
        );
        // A count of 0 is an empty list, and so is an offset past the end.
        assert_eq!(
            page(&[
                ("artistCount", "0"),
                ("albumOffset", "4"),
                ("songOffset", "4294967295"),
            ])
            .await,
            found(&[], &[], &[])
        );
        // An empty value is an absent parameter.
        assert_eq!(
            page(&[
                ("artistCount", ""),
                ("artistOffset", ""),
                ("albumCount", ""),
                ("albumOffset", ""),
                ("songCount", ""),
                ("songOffset", ""),
                ("musicFolderId", ""),
            ])
            .await,
            found(&ARTISTS, &ALBUMS, &SONGS)
        );
    }
}

#[tokio::test]
async fn default_count_is_20_and_the_default_offset_is_0() {
    // 25 artists, 25 albums and 25 tracks whose names sort by their number.
    let server = server_with(|conn| {
        for n in 0..25 {
            let name = format!("{n:02}");
            add_track(conn, n, "/music", &name, &name, &name)?;
        }
        Ok(())
    })
    .await;
    let first_20: Vec<String> = (0..20).map(|n| format!("{n:02}")).collect();
    let last_5: Vec<String> = (20..25).map(|n| format!("{n:02}")).collect();

    for (method, key) in METHODS {
        let response = server.get_json(method, &[("query", "")]).await;
        assert_eq!(
            names(method, &response[key]),
            (first_20.clone(), first_20.clone(), first_20.clone()),
            "{method}"
        );
        let response = server
            .get_json(
                method,
                &[
                    ("query", ""),
                    ("artistOffset", "20"),
                    ("albumOffset", "20"),
                    ("songOffset", "20"),
                ],
            )
            .await;
        assert_eq!(
            names(method, &response[key]),
            (last_5.clone(), last_5.clone(), last_5.clone()),
            "{method}"
        );
    }
}

#[tokio::test]
async fn count_above_500_is_500() {
    // 501 artists, 501 albums and 501 tracks.
    let server = server_with(|conn| {
        for n in 0..501 {
            let name = format!("{n:03}");
            add_track(conn, n, "/music", &name, &name, &name)?;
        }
        Ok(())
    })
    .await;
    let lengths = |response: &Value, key: &str| {
        ["artist", "album", "song"].map(|list| {
            response[key][list]
                .as_array()
                .unwrap_or_else(|| panic!("no {list} in {response}"))
                .len()
        })
    };

    for (method, key) in METHODS {
        for count in ["500", "501", "1000", "100000", "4294967295"] {
            let response = server
                .get_json(
                    method,
                    &[
                        ("query", ""),
                        ("artistCount", count),
                        ("albumCount", count),
                        ("songCount", count),
                    ],
                )
                .await;
            assert_eq!(response["status"], "ok", "{method} {count}");
            assert_eq!(lengths(&response, key), [500, 500, 500], "{method} {count}");
            // The lists are the first 500 matches.
            let (artists, albums, songs) = names(method, &response[key]);
            for list in [artists, albums, songs] {
                assert_eq!(list[0], "000", "{method} {count}");
                assert_eq!(list[499], "499", "{method} {count}");
            }
        }

        // The rest is on the next page, also when that page asks for more
        // than 500.
        let response = server
            .get_json(
                method,
                &[
                    ("query", ""),
                    ("artistCount", "1000"),
                    ("artistOffset", "500"),
                    ("albumCount", "1000"),
                    ("albumOffset", "500"),
                    ("songCount", "1000"),
                    ("songOffset", "500"),
                ],
            )
            .await;
        assert_eq!(lengths(&response, key), [1, 1, 1], "{method}");
        let last = || vec!["500".to_owned()];
        assert_eq!(names(method, &response[key]), (last(), last(), last()));
    }
}

#[tokio::test]
async fn count_or_offset_that_is_no_number_from_0_is_error_0() {
    let server = TestServer::standard().await;
    let numbers = [
        "artistCount",
        "artistOffset",
        "albumCount",
        "albumOffset",
        "songCount",
        "songOffset",
    ];
    for (method, key) in METHODS {
        for name in numbers {
            // A negative limit is no limit in SQLite, and a negative offset
            // is no offset. Neither reaches a statement.
            for value in [
                "-1",
                "-500",
                "-9223372036854775808",
                "4294967296",
                "99999999999999999999",
                "1.5",
                "1e3",
                "0x10",
                "ten",
                " 1",
                "1;DROP TABLE tracks",
                "NULL",
            ] {
                let response = server
                    .get_json(method, &[("query", ""), (name, value)])
                    .await;
                assert_eq!(response["status"], "failed", "{method} {name}={value}");
                assert_eq!(
                    response["error"],
                    not_a_number(name),
                    "{method} {name}={value}"
                );
                assert!(response.get(key).is_none(), "{method} {name}={value}");
            }
        }
        for value in ["one", "1.5", "99999999999999999999", "1 OR 1=1"] {
            let response = server
                .get_json(method, &[("query", ""), ("musicFolderId", value)])
                .await;
            assert_eq!(
                response["error"],
                error(0, "parameter musicFolderId must be an integer"),
                "{method} {value}"
            );
        }
    }
}

#[tokio::test]
async fn paging_through_the_standard_library_returns_every_id_once() {
    let server = TestServer::standard().await;
    let every_id = standard_ids(&server);
    assert_eq!(every_id.len(), 17);

    for (method, key) in METHODS {
        for query in ["", "\"\""] {
            let whole = server
                .get_json(
                    method,
                    &[
                        ("query", query),
                        ("artistCount", "500"),
                        ("albumCount", "500"),
                        ("songCount", "500"),
                    ],
                )
                .await;
            for size in [1, 2, 3, 4, 7, 10, 500] {
                // The client asks for pages of `size` until a page is
                // shorter than that.
                let mut seen = HashSet::new();
                for list in ["artist", "album", "song"] {
                    let count = format!("{list}Count");
                    let offset = format!("{list}Offset");
                    let other: Vec<String> = ["artist", "album", "song"]
                        .into_iter()
                        .filter(|name| *name != list)
                        .map(|name| format!("{name}Count"))
                        .collect();
                    let size_text = size.to_string();
                    let mut ids = Vec::new();
                    let mut pages = 0;
                    loop {
                        let offset_text = (pages * size).to_string();
                        let response = server
                            .get_json(
                                method,
                                &[
                                    ("query", query),
                                    (count.as_str(), size_text.as_str()),
                                    (offset.as_str(), offset_text.as_str()),
                                    (other[0].as_str(), "0"),
                                    (other[1].as_str(), "0"),
                                ],
                            )
                            .await;
                        assert_eq!(response["status"], "ok", "{method} {list} {size}");
                        let page = field(&response[key][list], "id");
                        assert!(page.len() <= size, "{method} {list} {size}");
                        // The two other lists are empty: their count is 0.
                        for name in ["artist", "album", "song"] {
                            if name != list {
                                assert_eq!(response[key][name], json!([]), "{method}");
                            }
                        }
                        let last = page.len() < size;
                        ids.extend(page);
                        pages += 1;
                        if last {
                            break;
                        }
                        assert!(pages < 20, "{method} {list} {size}: the pages do not end");
                    }
                    // The pages put together are the list of one request.
                    assert_eq!(
                        ids,
                        field(&whole[key][list], "id"),
                        "{method} {query:?} {list} in pages of {size}"
                    );
                    for id in ids {
                        assert!(seen.insert(id.clone()), "{id} is on two pages");
                    }
                }
                assert_eq!(seen, every_id, "{method} {query:?} in pages of {size}");
            }
        }
    }
}

#[tokio::test]
async fn term_matches_a_part_of_a_name_and_two_terms_require_both() {
    let library = LibraryBuilder::new()
        .track("The Beatles", "Help!", "Yesterday", 1, "flac")
        .track("The Beatles", "Abbey Road", "Come Together", 1, "mp3")
        .track("Michael Jackson", "Thriller", "Beat It", 1, "ogg")
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "opus")
        .build();
    let server = TestServer::scanned(library).await;

    for (method, _) in METHODS {
        // `beat` is a part of "The Beatles" and of "Beat It".
        let beat = found(
            &["The Beatles"],
            &["Abbey Road", "Help!"],
            &["Beat It", "Come Together", "Yesterday"],
        );
        // One `*` at the end of a query is no part of the term.
        for query in [
            "beat",
            "BEAT",
            "Beat",
            "\"beat\"",
            " beat ",
            "beat*",
            "\"beat*\"",
        ] {
            assert_eq!(find(&server, method, query).await, beat, "{method} {query}");
        }
        assert_eq!(
            find(&server, method, "beat**").await,
            found(&[], &[], &[]),
            "{method}"
        );

        // Two terms: both are part of a name of the item. No artist name
        // holds `road`.
        let both = found(&[], &["Abbey Road"], &["Come Together"]);
        for query in ["beat road", "road beat", "ROAD  bEAT", "\"beat road\""] {
            assert_eq!(find(&server, method, query).await, both, "{method} {query}");
        }
        assert_eq!(
            find(&server, method, "beat zzz").await,
            found(&[], &[], &[]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "the beatles").await,
            found(
                &["The Beatles"],
                &["Abbey Road", "Help!"],
                &["Come Together", "Yesterday"]
            ),
            "{method}"
        );
        // A song by its title, by the title of its album and by its artist.
        assert_eq!(
            find(&server, method, "tide").await,
            found(&[], &[], &["Low Tide"])
        );
        assert_eq!(
            find(&server, method, "harbour").await,
            found(&[], &["Glass Harbour"], &["Low Tide"])
        );
        assert_eq!(
            find(&server, method, "aurora").await,
            found(&["Aurora Field"], &["Glass Harbour"], &["Low Tide"])
        );
        assert_eq!(
            find(&server, method, "yesterday beatles help").await,
            found(&[], &[], &["Yesterday"])
        );
    }
}

#[tokio::test]
async fn like_wildcards_in_a_query_match_only_themselves() {
    let library = LibraryBuilder::new()
        .track("Per%cent", "100% Hits", "100% Pure", 1, "flac")
        .track("Percent", "100 Hits", "100 Miles", 1, "flac")
        .track("Under_Score", "Snake_Case", "a_b", 1, "flac")
        .track("UnderXScore", "SnakeXCase", "axb", 1, "flac")
        .build();
    let server = TestServer::scanned(library).await;

    for (method, _) in METHODS {
        assert_eq!(
            find(&server, method, "100%").await,
            found(&[], &["100% Hits"], &["100% Pure"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "%").await,
            found(&["Per%cent"], &["100% Hits"], &["100% Pure"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "_").await,
            found(&["Under_Score"], &["Snake_Case"], &["a_b"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "a_b").await,
            found(&[], &[], &["a_b"]),
            "{method}"
        );
        for query in ["%%", "__", "100%p", "per_cent", "%_%", "\\%", "\\_", "\\"] {
            assert_eq!(
                find(&server, method, query).await,
                found(&[], &[], &[]),
                "{method} {query}"
            );
        }
    }
}

#[tokio::test]
async fn hostile_query_is_text_and_changes_nothing() {
    let server = TestServer::standard().await;
    let everything = found(&ARTISTS, &ALBUMS, &SONGS);
    let nothing = found(&[], &[], &[]);
    let many_terms = "x ".repeat(512);
    let longest = "a".repeat(1024);
    let queries = [
        // Wildcards.
        "%",
        "_",
        "%%%%",
        "%_%_%",
        "a%",
        "_ide",
        "[a-z]*",
        "?",
        // Quotes and the escape character.
        "'",
        "''",
        "\\",
        "\\\\",
        "\\'",
        "'\"'\"",
        "`",
        // Statements.
        "'; DROP TABLE tracks; --",
        "' OR '1'='1",
        "\" OR \"\"=\"",
        "') OR 1=1 --",
        "%' UNION SELECT password FROM users --",
        "?1",
        ":query",
        "]') --",
        "\\\"]",
        // A NUL character alone, inside a term and next to a term that
        // matches.
        "\0",
        "glass\0",
        "\0glass",
        "gl\0ass",
        "glass \0",
        // Control characters and characters outside ASCII.
        "\u{1}\u{2}\u{1f}",
        "\u{7f}",
        "\u{feff}",
        "\u{202e}glass",
        "\u{fffd}",
        "zażółć",
        "Ελληνικά",
        "音楽",
        "🎵🎶",
        "e\u{301}\u{301}\u{301}",
        // The longest queries.
        &longest,
        &many_terms,
    ];
    for (method, key) in METHODS {
        for query in queries {
            let response = server.get_json(method, &[("query", query)]).await;
            assert_eq!(response["status"], "ok", "{method} {query:?}: {response}");
            assert_eq!(names(method, &response[key]), nothing, "{method} {query:?}");
            // XML as well: the body is a well-formed empty result.
            assert_eq!(
                server.get_xml(method, &[("query", query)]).await,
                xml_ok(&format!("<{key}/>")),
                "{method} {query:?}"
            );
        }
        // The library is as it was.
        assert_eq!(find(&server, method, "").await, everything, "{method}");
    }
    // The account is as it was: the next request authenticates.
    let response = server.get_json("getUser", &[("username", OWNER)]).await;
    assert_eq!(response["status"], "ok");
}

#[tokio::test]
async fn quotes_in_a_query_are_removed_and_other_characters_are_part_of_a_term() {
    // The names are no file names on Windows, so the rows are stored
    // without files.
    let server = server_with(|conn| {
        add_track(
            conn,
            1,
            "/music",
            "Guns N' Roses",
            "Don't Cry",
            "It's So Easy",
        )?;
        add_track(
            conn,
            2,
            "/music",
            "The \"Quoted\" Band",
            "12\" Mixes",
            "Say \"Hi\"",
        )?;
        add_track(conn, 3, "/music", "AC/DC", "Back\\Slash", "What?*")?;
        add_track(
            conn,
            4,
            "/music",
            "Robert'); DROP TABLE tracks;--",
            "x' OR '1'='1",
            "<b>&amp;</b>",
        )?;
        Ok(())
    })
    .await;

    for (method, key) in METHODS {
        let cases = [
            (
                "n'",
                found(&["Guns N' Roses"], &["Don't Cry"], &["It's So Easy"]),
            ),
            ("it's", found(&[], &[], &["It's So Easy"])),
            (
                "\"quoted\"",
                found(&["The \"Quoted\" Band"], &["12\" Mixes"], &["Say \"Hi\""]),
            ),
            ("12\"", found(&[], &["12\" Mixes"], &["Say \"Hi\""])),
            ("\"say hi\"", found(&[], &[], &["Say \"Hi\""])),
            ("ac/dc", found(&["AC/DC"], &["Back\\Slash"], &["What?*"])),
            ("k\\s", found(&[], &["Back\\Slash"], &["What?*"])),
            ("?*", found(&[], &[], &["What?*"])),
            (
                "'); drop table tracks;--",
                found(
                    &["Robert'); DROP TABLE tracks;--"],
                    &["x' OR '1'='1"],
                    &["<b>&amp;</b>"],
                ),
            ),
            ("'1'='1", found(&[], &["x' OR '1'='1"], &["<b>&amp;</b>"])),
            ("<b>&amp;", found(&[], &[], &["<b>&amp;</b>"])),
        ];
        for (query, expected) in cases {
            assert_eq!(
                find(&server, method, query).await,
                expected,
                "{method} {query}"
            );
        }

        // The names are escaped in XML.
        let xml = server
            .get_xml(method, &[("query", "robert"), ("songCount", "0")])
            .await;
        assert!(
            xml.contains(r#"name="Robert'); DROP TABLE tracks;--""#)
                || xml.contains(r#"name="Robert&apos;); DROP TABLE tracks;--""#),
            "{xml}"
        );
        assert!(
            xml.ends_with(&format!("</{key}></subsonic-response>")),
            "{xml}"
        );
    }
    assert_eq!(
        find(&server, "search3", "").await.2,
        ["<b>&amp;</b>", "It's So Easy", "Say \"Hi\"", "What?*"]
    );
}

#[tokio::test]
async fn names_outside_ascii_are_found_without_regard_to_case() {
    let library = LibraryBuilder::new()
        .track("Édith Piaf", "La Vie en rose", "Milord", 1, "flac")
        .track("Edith Day", "Rio Rita", "Following the Sun", 1, "mp3")
        .track("Кино", "Группа крови", "Звезда по имени Солнце", 1, "m4a")
        .track("坂本龍一", "音楽図鑑", "戦場のメリークリスマス", 1, "ogg")
        .build();
    let server = TestServer::scanned(library).await;

    for (method, _) in METHODS {
        let piaf = found(&["Édith Piaf"], &["La Vie en rose"], &["Milord"]);
        // Lower case, upper case, and the decomposed spelling of the query.
        for query in ["édith", "ÉDITH", "Édith", "e\u{301}dith", "E\u{301}DITH"] {
            assert_eq!(find(&server, method, query).await, piaf, "{method} {query}");
        }
        // A letter with a diacritic is not its base letter.
        assert_eq!(
            find(&server, method, "edith").await,
            found(&["Edith Day"], &["Rio Rita"], &["Following the Sun"]),
            "{method}"
        );
        let kino = found(&["Кино"], &["Группа крови"], &["Звезда по имени Солнце"]);
        for query in ["кино", "КИНО", "Кино", "ин"] {
            assert_eq!(find(&server, method, query).await, kino, "{method} {query}");
        }
        assert_eq!(
            find(&server, method, "ГРУППА звезда").await,
            found(&[], &[], &["Звезда по имени Солнце"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "龍一").await,
            found(&["坂本龍一"], &["音楽図鑑"], &["戦場のメリークリスマス"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "メリー").await,
            found(&[], &[], &["戦場のメリークリスマス"]),
            "{method}"
        );
    }

    // The names are the same text in XML.
    let xml = server
        .get_xml("search3", &[("query", "КИНО"), ("songCount", "0")])
        .await;
    assert!(xml.contains(r#"name="Кино""#), "{xml}");
    assert!(xml.contains(r#"name="Группа крови""#), "{xml}");
}

/// The titles of the songs that `method` finds for `query`. The query must
/// match no artist and no album.
async fn songs(server: &TestServer, method: &str, query: &str) -> Vec<String> {
    let (artists, albums, songs) = find(server, method, query).await;
    assert_eq!(artists, [] as [&str; 0], "{method} {query:?}");
    assert_eq!(albums, [] as [&str; 0], "{method} {query:?}");
    songs
}

/// A title whose tag is in decomposed form: `e` and U+0301.
const DECOMPOSED_TITLE: &str = "Cafe\u{301} del Mar";

/// A title whose tag is in composed form: U+00E9.
const COMPOSED_TITLE: &str = "Fianc\u{e9}e";

#[tokio::test]
async fn a_title_is_found_in_either_unicode_form_of_its_tag_and_of_the_query() {
    let upper_case = "U\u{308}BER A\u{30a}NGSTRO\u{308}M";
    let dotted = "\u{130}stanbul DIYARBAKIR";
    let sigma = "\u{39f}\u{394}\u{39f}\u{3a3}";
    let library = LibraryBuilder::new()
        .track("Nadia Ortiz", "Formas", DECOMPOSED_TITLE, 1, "flac")
        .track("Nadia Ortiz", "Formas", COMPOSED_TITLE, 2, "mp3")
        .track("Nadia Ortiz", "Formas", "Cafe Society", 3, "m4a")
        .track("Nadia Ortiz", "Formas", upper_case, 4, "ogg")
        .track("Nadia Ortiz", "Formas", dotted, 5, "opus")
        .track("Nadia Ortiz", "Formas", sigma, 6, "flac")
        .build();
    let server = TestServer::scanned(library).await;

    let cases: &[(&str, &[&str])] = &[
        // A tag in decomposed form: a composed query and a decomposed
        // query, in lower case and in upper case.
        ("caf\u{e9}", &[DECOMPOSED_TITLE]),
        ("cafe\u{301}", &[DECOMPOSED_TITLE]),
        ("CAF\u{c9}", &[DECOMPOSED_TITLE]),
        ("CAFE\u{301}", &[DECOMPOSED_TITLE]),
        ("Caf\u{e9} DEL", &[DECOMPOSED_TITLE]),
        // A tag in composed form: the same four queries.
        ("fianc\u{e9}e", &[COMPOSED_TITLE]),
        ("fiance\u{301}e", &[COMPOSED_TITLE]),
        ("FIANC\u{c9}E", &[COMPOSED_TITLE]),
        ("FIANCE\u{301}E", &[COMPOSED_TITLE]),
        // A letter with a diacritic is not its base letter, in either form
        // of the tag: `e` does not find U+00E9, and U+00E9 does not find
        // `e`.
        ("cafe", &["Cafe Society"]),
        ("fiancee", &[]),
        ("caf\u{e9} society", &[]),
        // The letters before the diacritic are part of both titles. The
        // songs are in the order of the normalised titles.
        ("caf", &["Cafe Society", DECOMPOSED_TITLE]),
        // ASCII letters match in any letter case.
        ("CAFE society", &["Cafe Society"]),
        ("sOcIeTy", &["Cafe Society"]),
        // Letters outside ASCII match in any letter case, by the default
        // case mapping of Unicode.
        ("\u{fc}ber", &[upper_case]),
        ("u\u{308}ber", &[upper_case]),
        ("\u{dc}BER", &[upper_case]),
        ("\u{e5}ngstr\u{f6}m \u{fc}ber", &[upper_case]),
        ("A\u{30a}NGSTRO\u{308}M", &[upper_case]),
        // The mapping has no rule of a language: `I` is `i`, and U+0130 is
        // `i` with U+0307, which is neither `i` nor U+0131.
        ("diyarbakir", &[dotted]),
        ("d\u{131}yarbak\u{131}r", &[]),
        ("\u{130}STANBUL", &[dotted]),
        ("i\u{307}stanbul", &[dotted]),
        ("istanbul", &[]),
        // U+03A3 at the end of a word is U+03C2 in lower case.
        ("\u{3bf}\u{3b4}\u{3bf}\u{3c2}", &[sigma]),
        ("\u{39f}\u{394}\u{39f}\u{3a3}", &[sigma]),
        ("\u{3bf}\u{3b4}\u{3bf}\u{3c3}", &[]),
    ];
    for (method, _) in METHODS {
        for (query, expected) in cases {
            assert_eq!(
                songs(&server, method, query).await,
                *expected,
                "{method} {query:?}"
            );
        }
    }

    // A song has the title that its tag spells, in both formats.
    let response = server.get_json("search3", &[("query", "caf\u{e9}")]).await;
    assert_eq!(
        response["searchResult3"]["song"][0]["title"],
        DECOMPOSED_TITLE
    );
    let xml = server.get_xml("search3", &[("query", "caf\u{e9}")]).await;
    assert!(
        xml.contains(&format!(r#"title="{DECOMPOSED_TITLE}""#)),
        "{xml}"
    );
}

#[tokio::test]
async fn a_title_that_was_stored_without_nfc_is_found_after_the_next_scan_with_what_is_attached() {
    let library = LibraryBuilder::new()
        .track("Nadia Ortiz", "Formas", DECOMPOSED_TITLE, 1, "flac")
        .track("Nadia Ortiz", "Formas", COMPOSED_TITLE, 2, "mp3")
        .build();
    let server = TestServer::scanned(library).await;
    let catalogue = server.catalogue().await;
    let id = catalogue.song(DECOMPOSED_TITLE).track.id.clone();
    let other = catalogue.song(COMPOSED_TITLE).track.id.clone();

    // The user stars, rates and plays the track and puts it into a
    // playlist.
    let rate = [("id", id.as_str()), ("rating", "4")];
    let playlist = [
        ("name", "Mix"),
        ("songId", other.as_str()),
        ("songId", id.as_str()),
    ];
    let writes: [(&str, &[(&str, &str)]); 4] = [
        ("star", &rate[..1]),
        ("setRating", &rate),
        ("scrobble", &rate[..1]),
        ("createPlaylist", &playlist),
    ];
    for (method, params) in writes {
        let response = server.get_json(method, params).await;
        assert_eq!(response["status"], "ok", "{method}: {response}");
    }
    let song = server.get_json("getSong", &[("id", &id)]).await["song"].clone();
    assert_eq!(song["id"], json!(id));
    assert_eq!(song["title"], DECOMPOSED_TITLE);
    assert_eq!(song["userRating"], 4);
    assert_eq!(song["playCount"], 1);
    assert!(song["starred"].is_string(), "{song}");
    let playlists = server.get_json("getPlaylists", &[]).await;
    let playlist_id = playlists["playlists"]["playlist"][0]["id"]
        .as_str()
        .expect("the ID of the playlist")
        .to_owned();
    let playlist = server
        .get_json("getPlaylist", &[("id", &playlist_id)])
        .await["playlist"]
        .clone();
    assert_eq!(field(&playlist["entry"], "id"), [other.clone(), id.clone()]);
    let album = server
        .get_json("getAlbum", &[("id", song["albumId"].as_str().unwrap())])
        .await["album"]
        .clone();

    // The rows as a build stored them that lower-cased a title without
    // bringing it into Unicode form NFC.
    let stale = server
        .db()
        .write(|conn| {
            let titles: Vec<(String, String)> = conn
                .prepare("SELECT id, title FROM tracks")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut stale = Vec::new();
            for (id, title) in titles {
                let lower_case = title.to_lowercase();
                let changed = conn.execute(
                    "UPDATE tracks SET title_lc = ?2 WHERE id = ?1 AND title_lc <> ?2",
                    params![id, lower_case],
                )?;
                if changed == 1 {
                    stale.push(id);
                }
            }
            Ok(stale)
        })
        .await
        .expect("store the titles without NFC");
    assert_eq!(stale, std::slice::from_ref(&id));

    // The stored text of the title is decomposed and a term is composed, so
    // neither spelling of the query finds the title. The title whose tag is
    // composed is found.
    for (method, _) in METHODS {
        for query in ["caf\u{e9}", "cafe\u{301}", "CAFE\u{301}"] {
            assert_eq!(
                songs(&server, method, query).await,
                [] as [&str; 0],
                "{method} {query:?}"
            );
        }
        assert_eq!(
            songs(&server, method, "fiance\u{301}e").await,
            [COMPOSED_TITLE]
        );
    }

    // The next scan sets the stored text. It reads no file, and it counts
    // no track as added, changed, moved or removed.
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [] as [&str; 0]);
    assert_eq!((summary.seen, summary.tags_read), (2, 0));
    assert_eq!(
        (
            summary.added,
            summary.changed,
            summary.moved,
            summary.removed
        ),
        (0, 0, 0, 0)
    );

    // Both spellings of the query find the song, and the song is the one
    // from before the scan: its ID, its star, its rating and its plays.
    for (method, key) in METHODS {
        for query in ["caf\u{e9}", "cafe\u{301}", "CAFE\u{301}", "CAF\u{c9} del"] {
            let response = server.get_json(method, &[("query", query)]).await;
            assert_eq!(
                response[key]["song"],
                json!([song]),
                "{method} {query:?}: {response}"
            );
        }
    }
    assert_eq!(
        server.get_json("getSong", &[("id", &id)]).await["song"],
        song
    );
    // The playlist has its two entries, and the album has its ID and its
    // songs.
    assert_eq!(
        server
            .get_json("getPlaylist", &[("id", &playlist_id)])
            .await["playlist"],
        playlist
    );
    assert_eq!(
        server
            .get_json("getAlbum", &[("id", album["id"].as_str().unwrap())])
            .await["album"],
        album
    );

    // A second scan finds every row with its text and changes nothing.
    assert_eq!(server.rescan().await, summary);
    assert_eq!(
        songs(&server, "search3", "caf\u{e9}").await,
        [DECOMPOSED_TITLE]
    );
}

#[tokio::test]
async fn query_of_more_than_1024_characters_is_error_0() {
    let server = TestServer::standard().await;
    for (method, key) in METHODS {
        // 1024 characters are a query, whatever their size in bytes.
        for query in ["a".repeat(1024), "é".repeat(1024), "🎵".repeat(1024)] {
            let response = server.get_json(method, &[("query", &query)]).await;
            assert_eq!(response["status"], "ok", "{method}");
            assert_eq!(names(method, &response[key]), found(&[], &[], &[]));
        }
        for query in ["a".repeat(1025), " ".repeat(1025), "\"".repeat(1025)] {
            let response = server.get_json(method, &[("query", &query)]).await;
            assert_eq!(response["status"], "failed", "{method}");
            assert_eq!(response["error"], error(0, QUERY_TOO_LONG), "{method}");
            assert!(response.get(key).is_none(), "{method}");
        }

        // A megabyte of text in a form body.
        for query in [
            "a".repeat(1_048_576),
            "a ".repeat(524_288),
            "%".repeat(1_048_576),
        ] {
            let response = server
                .post_form(method, &[("query", &query), ("f", "json")])
                .await;
            assert_eq!(response.status(), StatusCode::OK, "{method}");
            assert_eq!(content_type(&response), "application/json", "{method}");
            let response = subsonic_response(&body_string(response).await);
            assert_eq!(response["error"], error(0, QUERY_TOO_LONG), "{method}");
        }
    }
}

#[tokio::test]
async fn query_of_more_than_32_terms_is_error_0() {
    let server = TestServer::standard().await;
    let numbered: Vec<String> = (0..33).map(|n| format!("t{n}")).collect();
    let at_the_bound = numbered[..32].join(" ");
    let above_the_bound = numbered.join(" ");
    // 33 terms of which two are one term in another letter case.
    let repeated = format!("{at_the_bound} T0 \"t1\"");
    // 32 parts of the names of "Low Tide" and "Breakwater" on "Glass
    // Harbour" by Aurora Field. Both titles hold a `w`, and the third track
    // of the album, "Night Ferry", holds none.
    let parts = "a u r o f i e l d g s h b w t au ur ro or ra fi ie el ld gl la as ss ha ar rb bo";
    assert_eq!(parts.split(' ').count(), 32);

    for (method, key) in METHODS {
        for query in [at_the_bound.as_str(), repeated.as_str()] {
            let response = server.get_json(method, &[("query", query)]).await;
            assert_eq!(response["status"], "ok", "{method} {query}");
            assert_eq!(names(method, &response[key]), found(&[], &[], &[]));
        }
        assert_eq!(
            find(&server, method, parts).await,
            found(&[], &[], &["Breakwater", "Low Tide"]),
            "{method}"
        );

        for query in [
            above_the_bound.clone(),
            format!("{parts} n"),
            format!("{above_the_bound} \0"),
            // 250 terms in 890 characters.
            (0..250).map(|n| format!("{n} ")).collect(),
        ] {
            let response = server.get_json(method, &[("query", &query)]).await;
            assert_eq!(response["status"], "failed", "{method} {query:?}");
            assert_eq!(response["error"], error(0, TOO_MANY_TERMS), "{method}");
            assert!(response.get(key).is_none(), "{method}");
            assert_eq!(
                server.get_xml(method, &[("query", &query)]).await,
                xml_failed(0, TOO_MANY_TERMS),
                "{method}"
            );
        }
    }
}

#[tokio::test]
async fn query_is_required_and_may_be_empty() {
    let server = TestServer::standard().await;
    for (method, key) in METHODS {
        for params in [&[][..], &[("songCount", "5")], &[("Query", "glass")]] {
            let response = server.get_json(method, params).await;
            assert_eq!(response["status"], "failed", "{method} {params:?}");
            assert_eq!(
                response["error"],
                error(10, "required parameter is missing: query"),
                "{method} {params:?}"
            );
            assert!(response.get(key).is_none(), "{method} {params:?}");
        }
        assert_eq!(
            server.get_xml(method, &[]).await,
            xml_failed(10, "required parameter is missing: query"),
            "{method}"
        );

        // `query` without `=` and `query=` are the empty query.
        let path = format!("/rest/{method}?query&f=json&u=owner&p=sesame&v=1.16.1&c=test");
        let response = server.get_path(&path).await;
        let response = subsonic_response(&body_string(response).await);
        assert_eq!(response["status"], "ok", "{method}");
        assert_eq!(
            names(method, &response[key]),
            found(&ARTISTS, &ALBUMS, &SONGS),
            "{method}"
        );
    }
}

#[tokio::test]
async fn search_methods_answer_a_form_post_and_the_view_suffix() {
    let server = TestServer::standard().await;
    for (method, key) in METHODS {
        // The third song of Aurora Field in title order is "Last Train".
        let expected = found(&["Aurora Field"], &["Glass Harbour"], &["Last Train"]);
        let response = server
            .post_form(
                method,
                &[
                    ("query", "aurora"),
                    ("artistCount", "1"),
                    ("albumCount", "1"),
                    ("songCount", "1"),
                    ("songOffset", "2"),
                    ("f", "json"),
                ],
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        let response = subsonic_response(&body_string(response).await);
        assert_eq!(response["status"], "ok", "{method}");
        assert_eq!(names(method, &response[key]), expected, "{method}");

        let response = server
            .get_json(
                &format!("{method}.view"),
                &[
                    ("query", "aurora"),
                    ("artistCount", "1"),
                    ("albumCount", "1"),
                    ("songCount", "1"),
                    ("songOffset", "2"),
                ],
            )
            .await;
        assert_eq!(names(method, &response[key]), expected, "{method}");
    }
}

#[tokio::test]
async fn music_folder_id_limits_the_lists_to_that_folder() {
    let server = server_with(|conn| {
        add_track(
            conn,
            1,
            "/music",
            "Aurora Field",
            "Glass Harbour",
            "Low Tide",
        )?;
        add_track(
            conn,
            2,
            "/other",
            "Aurora Field",
            "Glass Harbour",
            "Breakwater",
        )?;
        add_track(
            conn,
            3,
            "/other",
            "Marlow Quartet",
            "Slow Rivers",
            "Upstream",
        )?;
        add_track(conn, 4, "/music", "The Beatles", "Help!", "Yesterday")?;
        Ok(())
    })
    .await;
    let folder_ids: Vec<String> = server
        .db()
        .read(folders::list)
        .await
        .expect("read the music folders")
        .into_iter()
        .map(|folder| folder.id.to_string())
        .collect();
    let (music, other) = (folder_ids[0].as_str(), folder_ids[1].as_str());

    for (method, key) in METHODS {
        let in_folder = |query: &'static str, folder: &str| {
            let server = &server;
            let folder = folder.to_owned();
            async move {
                let response = server
                    .get_json(method, &[("query", query), ("musicFolderId", &folder)])
                    .await;
                assert_eq!(response["status"], "ok", "{method} {query} {folder}");
                names(method, &response[key])
            }
        };
        assert_eq!(
            in_folder("", music).await,
            found(
                &["Aurora Field", "The Beatles"],
                &["Glass Harbour", "Help!"],
                &["Low Tide", "Yesterday"]
            ),
            "{method}"
        );
        assert_eq!(
            in_folder("", other).await,
            found(
                &["Aurora Field", "Marlow Quartet"],
                &["Glass Harbour", "Slow Rivers"],
                &["Breakwater", "Upstream"]
            ),
            "{method}"
        );
        assert_eq!(
            in_folder("aurora", other).await,
            found(&["Aurora Field"], &["Glass Harbour"], &["Breakwater"]),
            "{method}"
        );
        assert_eq!(in_folder("beatles", other).await, found(&[], &[], &[]));
        // The ID of no folder is no error.
        for unknown in ["0", "-1", "99", "9223372036854775807"] {
            assert_eq!(
                in_folder("", unknown).await,
                found(&[], &[], &[]),
                "{method} {unknown}"
            );
        }
        // Without the parameter the lists hold both folders.
        assert_eq!(
            find(&server, method, "").await,
            found(
                &["Aurora Field", "Marlow Quartet", "The Beatles"],
                &["Glass Harbour", "Help!", "Slow Rivers"],
                &["Breakwater", "Low Tide", "Upstream", "Yesterday"]
            ),
            "{method}"
        );
    }
}

#[tokio::test]
async fn music_folder_of_the_standard_library_holds_everything() {
    let server = TestServer::standard().await;
    let folder = server.folder_id.to_string();
    let other = (server.folder_id + 1).to_string();
    let expected = whole_result3(&server.catalogue().await);

    let response = server
        .get_json("search3", &[("query", ""), ("musicFolderId", &folder)])
        .await;
    assert_eq!(response["searchResult3"], expected);
    let response = server
        .get_json("search3", &[("query", ""), ("musicFolderId", &other)])
        .await;
    assert_eq!(
        response["searchResult3"],
        json!({ "artist": [], "album": [], "song": [] })
    );
}

#[tokio::test]
async fn track_without_a_file_is_listed_without_a_path_and_is_in_no_folder() {
    let server = TestServer::standard().await;
    let folder = server.folder_id.to_string();
    // The two tracks of "Slow Rivers" and one of the two tracks of "Summer
    // Sampler" lose their files.
    remove_file(&server, &server.track.upstream).await;
    remove_file(&server, &server.track.delta).await;
    remove_file(&server, &server.track.open_road).await;

    for (method, key) in METHODS {
        // Every row is a match as before.
        let response = server.get_json(method, &[("query", "")]).await;
        assert_eq!(
            names(method, &response[key]),
            found(&ARTISTS, &ALBUMS, &SONGS),
            "{method}"
        );
        for song in response[key]["song"].as_array().expect("the songs") {
            let id = song["id"].as_str().expect("an ID");
            let without_file = [
                &server.track.upstream,
                &server.track.delta,
                &server.track.open_road,
            ]
            .iter()
            .any(|track| *track == id);
            // The element keeps its tags and has no path. No field holds
            // the placeholder path.
            assert_eq!(song.get("path").is_none(), without_file, "{song}");
            assert!(!song.to_string().contains(&format!("/{id}")), "{song}");
            assert_ne!(song["coverArt"], json!(id), "{song}");
            assert!(song["suffix"].is_string(), "{song}");
        }
        assert_eq!(
            find(&server, method, "upstream").await,
            found(&[], &[], &["Upstream"]),
            "{method}"
        );
        assert_eq!(
            find(&server, method, "tessa").await,
            found(&[], &[], &["Open Road"]),
            "{method}"
        );

        // In the music folder the three tracks are left out. "Slow Rivers"
        // and its artist have no other track. "Summer Sampler" has one.
        let response = server
            .get_json(method, &[("query", ""), ("musicFolderId", &folder)])
            .await;
        assert_eq!(
            names(method, &response[key]),
            found(
                &["Aurora Field", "Various Artists"],
                &["Glass Harbour", "Paper Maps", "Summer Sampler"],
                &[
                    "Breakwater",
                    "Contour",
                    "Last Train",
                    "Legend",
                    "Low Tide",
                    "Meridian",
                    "Night Ferry",
                ]
            ),
            "{method}"
        );
        let response = server
            .get_json(method, &[("query", "marlow"), ("musicFolderId", &folder)])
            .await;
        assert_eq!(names(method, &response[key]), found(&[], &[], &[]));
    }
}

#[tokio::test]
async fn elements_carry_the_annotations_of_the_user() {
    let server = TestServer::standard().await;
    let (artist, album, track) = (
        server.artist.marlow_quartet.clone(),
        server.album.slow_rivers.clone(),
        server.track.delta.clone(),
    );
    server
        .db()
        .write(move |conn| {
            let user: i64 =
                conn.query_row("SELECT id FROM users WHERE name = ?1", [OWNER], |row| {
                    row.get(0)
                })?;
            let mut insert = conn.prepare(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            insert.execute(params![
                user,
                artist,
                "artist",
                1_700_000_000,
                4,
                0,
                None::<i64>
            ])?;
            insert.execute(params![
                user,
                album,
                "album",
                1_700_000_100,
                5,
                0,
                None::<i64>
            ])?;
            insert.execute(params![
                user,
                track,
                "track",
                1_700_000_300,
                3,
                9,
                1_700_000_400
            ])?;
            Ok(())
        })
        .await
        .expect("store the annotations");

    let response = server.get_json("search3", &[("query", "marlow")]).await;
    let result = &response["searchResult3"];
    assert_eq!(
        result["artist"],
        json!([{
            "id": server.artist.marlow_quartet,
            "name": "Marlow Quartet",
            "albumCount": 1,
            "starred": "2023-11-14T22:13:20Z",
        }])
    );
    // The album has the star and the rating of its row and the plays of
    // its track "Delta".
    let album = &result["album"][0];
    assert_eq!(album["starred"], "2023-11-14T22:15:00Z");
    assert_eq!(album["userRating"], 5);
    assert_eq!(album["playCount"], 9);
    assert_eq!(album["played"], "2023-11-14T22:20:00Z");
    // The songs are "Delta" and "Upstream". Only "Delta" has an annotation.
    let delta = &result["song"][0];
    assert_eq!(delta["id"], json!(server.track.delta));
    assert_eq!(delta["starred"], "2023-11-14T22:18:20Z");
    assert_eq!(delta["userRating"], 3);
    assert_eq!(delta["playCount"], 9);
    assert_eq!(delta["played"], "2023-11-14T22:20:00Z");
    let upstream = &result["song"][1];
    assert_eq!(upstream["id"], json!(server.track.upstream));
    assert_eq!(upstream["playCount"], 0);
    for absent in ["starred", "userRating", "played"] {
        assert!(upstream.get(absent).is_none(), "{upstream}");
    }

    let response = server.get_json("search2", &[("query", "marlow")]).await;
    let result = &response["searchResult2"];
    assert_eq!(
        result["artist"],
        json!([{
            "id": server.artist.marlow_quartet,
            "name": "Marlow Quartet",
            "starred": "2023-11-14T22:13:20Z",
            "userRating": 4,
        }])
    );
    let album = &result["album"][0];
    assert_eq!(album["isDir"], true);
    assert_eq!(album["starred"], "2023-11-14T22:15:00Z");
    assert_eq!(album["userRating"], 5);
    assert_eq!(album["playCount"], 9);
    assert_eq!(result["song"][0]["userRating"], 3);
}

#[tokio::test]
async fn search3_in_xml_has_the_elements_of_the_subsonic_schema() {
    let server = TestServer::standard().await;
    let aurora = &server.artist.aurora_field;

    // One list at a time: the whole document.
    let artist = format!(r#"<artist albumCount="2" id="{aurora}" name="Aurora Field"/>"#);
    let xml = server
        .get_xml(
            "search3",
            &[("query", "aurora"), ("albumCount", "0"), ("songCount", "0")],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!("<searchResult3>{artist}</searchResult3>"))
    );

    let catalogue = server.catalogue().await;
    let created = model::iso8601(catalogue.album("Paper Maps").created_at);
    let album = format!(
        concat!(
            r#"<album artist="Aurora Field" artistId="{}" created="{}""#,
            r#" duration="2" genre="Electronic" id="{}" name="Paper Maps" playCount="0""#,
            r#" songCount="3" year="2021"/>"#
        ),
        aurora, created, server.album.paper_maps
    );
    let xml = server
        .get_xml(
            "search3",
            &[("query", "paper"), ("artistCount", "0"), ("songCount", "0")],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!("<searchResult3>{album}</searchResult3>"))
    );

    let created = model::iso8601(catalogue.song("Legend").track.created_at);
    let legend = catalogue.song("Legend");
    let song = format!(
        concat!(
            r#"<song album="Paper Maps" albumId="{album}" artist="Aurora Field""#,
            r#" artistId="{artist}" bitDepth="0" bitRate="{bit_rate}" channelCount="1""#,
            r#" contentType="audio/mpeg" created="{created}" discNumber="2" duration="1""#,
            r#" genre="Electronic" id="{id}" isDir="false" mediaType="song" parent="{album}""#,
            r#" path="Aurora Field/Paper Maps/01 Legend.mp3" playCount="0""#,
            r#" samplingRate="22050" size="{size}" suffix="mp3" title="Legend" track="1""#,
            r#" type="music" year="2021"/>"#
        ),
        album = server.album.paper_maps,
        artist = aurora,
        bit_rate = legend
            .track
            .bitrate_kbps
            .expect("an MP3 file has a bitrate"),
        created = created,
        id = server.track.legend,
        size = legend.track.size,
    );
    let xml = server
        .get_xml(
            "search3",
            &[
                ("query", "legend"),
                ("artistCount", "0"),
                ("albumCount", "0"),
            ],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!("<searchResult3>{song}</searchResult3>"))
    );

    // The three lists in one document, in the order of the Subsonic schema:
    // the artist, then the album, then the song. "Paper Maps" is the second
    // album and "Legend" the fourth song that `aurora` matches.
    let xml = server
        .get_xml(
            "search3",
            &[
                ("query", "aurora"),
                ("artistCount", "1"),
                ("albumCount", "1"),
                ("albumOffset", "1"),
                ("songCount", "1"),
                ("songOffset", "3"),
            ],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!(
            "<searchResult3>{artist}{album}{song}</searchResult3>"
        ))
    );

    // Every match of `aurora`: one `artist`, then two `album`, then seven
    // `song` elements inside `searchResult3`.
    let xml = server.get_xml("search3", &[("query", "aurora")]).await;
    let start = xml_ok("<searchResult3>");
    let start = start.trim_end_matches("</subsonic-response>");
    assert!(xml.starts_with(start), "{xml}");
    assert!(
        xml.ends_with("</searchResult3></subsonic-response>"),
        "{xml}"
    );
    assert_eq!(
        child_names(&xml, "searchResult3"),
        schema_names(1, 2, 7),
        "{xml}"
    );

    // No match: an empty element.
    assert_eq!(
        server.get_xml("search3", &[("query", "zzz")]).await,
        xml_ok("<searchResult3/>")
    );
}

#[tokio::test]
async fn search2_in_xml_has_the_elements_of_the_subsonic_schema() {
    let server = TestServer::standard().await;
    let aurora = &server.artist.aurora_field;

    let artist = format!(r#"<artist id="{aurora}" name="Aurora Field"/>"#);
    let xml = server
        .get_xml(
            "search2",
            &[("query", "aurora"), ("albumCount", "0"), ("songCount", "0")],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!("<searchResult2>{artist}</searchResult2>"))
    );

    let catalogue = server.catalogue().await;
    let created = model::iso8601(catalogue.album("Paper Maps").created_at);
    let album = format!(
        concat!(
            r#"<album album="Paper Maps" artist="Aurora Field""#,
            r#" artistId="{artist}" created="{created}" duration="2" genre="Electronic""#,
            r#" id="{id}" isDir="true" mediaType="album" parent="{artist}" playCount="0""#,
            r#" title="Paper Maps" year="2021"/>"#
        ),
        artist = aurora,
        created = created,
        id = server.album.paper_maps
    );
    let xml = server
        .get_xml(
            "search2",
            &[("query", "paper"), ("artistCount", "0"), ("songCount", "0")],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!("<searchResult2>{album}</searchResult2>"))
    );

    // A song is the same element as in `search3`.
    let params = [
        ("query", "legend"),
        ("artistCount", "0"),
        ("albumCount", "0"),
    ];
    let songs = server.get_xml("search2", &params).await;
    assert_eq!(
        songs,
        server
            .get_xml("search3", &params)
            .await
            .replace("searchResult3", "searchResult2")
    );
    let song = element_content(&songs, "searchResult2");
    assert_eq!(child_names(&songs, "searchResult2"), ["song"], "{songs}");

    // The three lists in one document, in the order of the Subsonic schema:
    // the artist, then the album, then the song. "Paper Maps" is the second
    // album and "Legend" the fourth song that `aurora` matches.
    let xml = server
        .get_xml(
            "search2",
            &[
                ("query", "aurora"),
                ("artistCount", "1"),
                ("albumCount", "1"),
                ("albumOffset", "1"),
                ("songCount", "1"),
                ("songOffset", "3"),
            ],
        )
        .await;
    assert_eq!(
        xml,
        xml_ok(&format!(
            "<searchResult2>{artist}{album}{song}</searchResult2>"
        ))
    );
    assert_eq!(
        server.get_xml("search2", &[("query", "zzz")]).await,
        xml_ok("<searchResult2/>")
    );
}

/// Checks the order of the child elements of the XML answer of `method`:
/// every `artist` element, then every `album` element, then every `song`
/// element, which is the sequence of the Subsonic schema. The standard
/// library has three artists, four albums and ten songs.
async fn xml_lists_follow_the_schema_sequence(method: &str) {
    let server = TestServer::standard().await;
    let key = result_key(method);

    // The three lists.
    let xml = server.get_xml(method, &[("query", "")]).await;
    let artist = first_element(&xml, "artist");
    let album = first_element(&xml, "album");
    let song = first_element(&xml, "song");
    assert!(artist.is_some(), "{method}: {xml}");
    assert!(artist < album && album < song, "{method}: {xml}");
    assert_eq!(
        child_names(&xml, key),
        schema_names(3, 4, 10),
        "{method}: {xml}"
    );

    // One list is empty: the two others keep their order.
    for (empty, expected) in [
        ("artistCount", schema_names(0, 4, 10)),
        ("albumCount", schema_names(3, 0, 10)),
        ("songCount", schema_names(3, 4, 0)),
    ] {
        let xml = server.get_xml(method, &[("query", ""), (empty, "0")]).await;
        assert_eq!(child_names(&xml, key), expected, "{method} {empty}: {xml}");
    }
    let xml = server
        .get_xml(method, &[("query", ""), ("songCount", "0")])
        .await;
    let artist = first_element(&xml, "artist");
    assert!(artist.is_some(), "{method}: {xml}");
    assert!(artist < first_element(&xml, "album"), "{method}: {xml}");
    assert_eq!(first_element(&xml, "song"), None, "{method}: {xml}");

    // A page of each list: the order does not depend on the items.
    let xml = server
        .get_xml(
            method,
            &[
                ("query", ""),
                ("artistCount", "2"),
                ("artistOffset", "1"),
                ("albumCount", "3"),
                ("albumOffset", "1"),
                ("songCount", "5"),
                ("songOffset", "5"),
            ],
        )
        .await;
    assert_eq!(
        child_names(&xml, key),
        schema_names(2, 3, 5),
        "{method}: {xml}"
    );
}

#[tokio::test]
async fn search3_in_xml_writes_artists_then_albums_then_songs() {
    xml_lists_follow_the_schema_sequence("search3").await;
}

#[tokio::test]
async fn search2_in_xml_writes_artists_then_albums_then_songs() {
    xml_lists_follow_the_schema_sequence("search2").await;
}

/// The order of the XML child elements has no effect on JSON: the keys of
/// every object are in byte order, so the lists are `album`, `artist` and
/// `song`.
#[tokio::test]
async fn json_documents_keep_the_byte_order_of_their_keys() {
    let server = TestServer::standard().await;
    let catalogue = server.catalogue().await;
    let body = async |method: &str, query: &str| {
        let response = server
            .get_raw(method, &[("query", query), ("f", "json")])
            .await;
        assert_eq!(content_type(&response), "application/json", "{method}");
        body_string(response).await
    };
    for (method, key) in METHODS {
        // No match: the whole document as text.
        assert_eq!(
            body(method, "zzz").await,
            json_ok(&format!(r#""{key}":{{"album":[],"artist":[],"song":[]}}"#)),
            "{method}"
        );

        // The whole library: the whole document as text. A `Value` writes
        // its keys in byte order.
        let whole = if method == "search3" {
            whole_result3(&catalogue)
        } else {
            whole_result2(&catalogue)
        };
        let text = body(method, "").await;
        assert_eq!(text, json_ok(&format!(r#""{key}":{whole}"#)), "{method}");
        let list = |name: &str| text.find(&format!(r#""{name}":["#));
        let (album, artist, song) = (list("album"), list("artist"), list("song"));
        assert!(album.is_some(), "{method}: {text}");
        assert!(album < artist && artist < song, "{method}: {text}");
    }
}

/// The fields of the envelope of a JSON answer. Every other key of the
/// response object of a successful answer is the key of the method.
const ENVELOPE_FIELDS: [&str; 5] = ["status", "version", "type", "serverVersion", "openSubsonic"];

/// The method table (`api::METHODS`) holds the handlers and not the keys of
/// their answers, so this test calls every method and reads the key of each
/// successful answer. `query` is the one parameter that the methods of the
/// table of `api::xml::CHILD_ORDER` require.
#[tokio::test]
async fn every_element_of_the_xml_child_order_is_the_key_of_an_answer() {
    let server = TestServer::bare(Config::default()).await;
    let mut keys = HashSet::new();
    for (method, _) in api::METHODS {
        let response = server.get_json(method, &[("query", "")]).await;
        if response["status"] != "ok" {
            continue;
        }
        let response = response.as_object().expect("the response is an object");
        keys.extend(
            response
                .keys()
                .filter(|key| !ENVELOPE_FIELDS.contains(&key.as_str()))
                .cloned(),
        );
    }
    for (element, _) in xml::CHILD_ORDER {
        assert!(
            keys.contains(*element),
            "no method of the table answers with `{element}`: {keys:?}"
        );
    }
}

#[tokio::test]
async fn search_is_deprecated_and_answers_error_0() {
    let server = TestServer::standard().await;
    let deprecated = error(0, "deprecated; use search3");
    for name in ["search", "search.view"] {
        for params in [
            &[][..],
            &[("any", "glass")],
            &[("artist", "aurora"), ("album", "glass"), ("title", "tide")],
            &[("count", "20"), ("offset", "0"), ("newerThan", "0")],
            &[("query", "glass")],
        ] {
            let response = server.get_json(name, params).await;
            assert_eq!(response["status"], "failed", "{name} {params:?}");
            assert_eq!(response["error"], deprecated, "{name} {params:?}");
            assert_eq!(
                response.as_object().map(serde_json::Map::len),
                Some(6),
                "{name} {params:?}: {response}"
            );
        }
        assert_eq!(
            server.get_xml(name, &[("any", "glass")]).await,
            xml_failed(0, "deprecated; use search3"),
            "{name}"
        );
        let response = server
            .post_form(name, &[("any", "glass"), ("f", "json")])
            .await;
        let response = subsonic_response(&body_string(response).await);
        assert_eq!(response["error"], deprecated, "{name}");
    }
}
