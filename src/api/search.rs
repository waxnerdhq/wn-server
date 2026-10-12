//! The search methods: `search2`, `search3` and the deprecated `search`
//! (`docs/design.md` section 4).
//!
//! `search2` and `search3` take the same parameters and find the same rows
//! (`db::queries::search`). They differ in the elements of the answer:
//! `search3` returns the ID3 elements, and `search2` returns the artist of
//! the folder methods and lists an album as a directory.
//!
//! # Parameters
//!
//! - `query` is required. It may be empty, and then every artist, album and
//!   song is a match. A client reads the whole library that way, one page
//!   after the other. A query of more than 1024 characters or of more than
//!   32 different terms is error 0.
//! - `artistCount`, `albumCount` and `songCount` are the most items of a
//!   list. The default is `DEFAULT_COUNT`, and a larger value than
//!   `MAX_COUNT` counts as `MAX_COUNT`.
//! - `artistOffset`, `albumOffset` and `songOffset` are the number of
//!   matches that a list leaves out at its start. The default is 0.
//! - `musicFolderId` names a music folder. The lists then hold only what has
//!   a file in that folder. The ID of no folder gives three empty lists.
//!
//! The six numbers are integers from 0 to 4294967295, and `musicFolderId` is
//! an integer. Another value is error 0, so no request reaches a statement
//! with a negative limit, which SQLite reads as no limit.

use std::collections::HashMap;

use axum::extract::State;
use serde::Serialize;

use super::envelope::ApiError;
use super::model::{self, AlbumID3, ArtistID3, Child};
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::Db;
use crate::db::queries::annotations::{self, Annotation};
use crate::db::queries::browse::{Album, Artist, Song};
use crate::db::queries::search::{self, MAX_QUERY_CHARS, MAX_TERMS, QueryError, Terms};

/// The number of items of a list when the request names none.
const DEFAULT_COUNT: u32 = 20;

/// The most items of one list of an answer. A request cannot ask for more.
const MAX_COUNT: u32 = 500;

/// The message of the error that `search` returns.
const SEARCH_DEPRECATED: &str = "deprecated; use search3";

/// The `searchResult3` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult3 {
    artist: Vec<ArtistID3>,
    album: Vec<AlbumID3>,
    song: Vec<Child>,
}

/// The `searchResult2` element. An album is a `Child` that is a directory.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult2 {
    artist: Vec<model::Artist>,
    album: Vec<Child>,
    song: Vec<Child>,
}

/// The part of one list that a request asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Page {
    /// The most items of the list, [`MAX_COUNT`] at most.
    count: u32,
    /// The number of matches before the first item of the list.
    offset: u32,
}

/// The parameters of `search2` and `search3`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchRequest {
    terms: Terms,
    artists: Page,
    albums: Page,
    songs: Page,
    /// The value of `musicFolderId`.
    folder_id: Option<i64>,
}

impl SearchRequest {
    /// Reads the parameters of a search. An absent `query` is error 10. A
    /// `query` of more than [`MAX_QUERY_CHARS`] characters or of more than
    /// [`MAX_TERMS`] different terms and a number that is not one are
    /// error 0.
    fn from_params(params: &Params) -> Result<Self, ApiError> {
        let terms = Terms::parse(params.req_str("query")?).map_err(|error| {
            ApiError::Other(match error {
                QueryError::TooLong => {
                    format!("parameter query must be at most {MAX_QUERY_CHARS} characters")
                }
                QueryError::TooManyTerms => {
                    format!("parameter query must have at most {MAX_TERMS} terms")
                }
            })
        })?;
        Ok(Self {
            terms,
            artists: page(params, "artistCount", "artistOffset")?,
            albums: page(params, "albumCount", "albumOffset")?,
            songs: page(params, "songCount", "songOffset")?,
            folder_id: params.opt_i64("musicFolderId")?,
        })
    }
}

/// Reads the count parameter `count` and the offset parameter `offset` of
/// one list.
fn page(params: &Params, count: &str, offset: &str) -> Result<Page, ApiError> {
    Ok(Page {
        count: params
            .opt_u32(count)?
            .unwrap_or(DEFAULT_COUNT)
            .min(MAX_COUNT),
        offset: params.opt_u32(offset)?.unwrap_or(0),
    })
}

/// The rows of one answer and the annotations of the user for them.
struct Matches {
    artists: Vec<Artist>,
    albums: Vec<Album>,
    songs: Vec<Song>,
    /// The annotations by the ID of the artist, the album or the track.
    annotations: HashMap<String, Annotation>,
}

/// Reads the three lists of `request` and the annotations of the user
/// `user_id` for their rows. A database that cannot be read is error 0.
///
/// The statements run in one read transaction, so they see one state of the
/// library while a scan writes. A list is then shorter than its count only
/// when it holds the last match: no row can vanish between the statement
/// that selects its ID and the statement that reads it. A client that reads
/// the library page by page takes a short page for the last one.
async fn find(db: &Db, user_id: i64, request: SearchRequest) -> Result<Matches, ApiError> {
    db.read(move |conn| {
        let snapshot = conn.unchecked_transaction()?;
        let SearchRequest {
            terms,
            artists,
            albums,
            songs,
            folder_id,
        } = &request;
        let artists =
            search::search_artists(&snapshot, terms, artists.count, artists.offset, *folder_id)?;
        let albums =
            search::search_albums(&snapshot, terms, albums.count, albums.offset, *folder_id)?;
        let songs = search::search_tracks(&snapshot, terms, songs.count, songs.offset, *folder_id)?;

        let ids: Vec<&str> = artists
            .iter()
            .map(|artist| artist.id.as_str())
            .chain(albums.iter().map(|album| album.id.as_str()))
            .chain(songs.iter().map(|song| song.track.id.as_str()))
            .collect();
        let annotations = annotations::get_many(&snapshot, user_id, &ids)?;
        snapshot.commit()?;
        Ok(Matches {
            artists,
            albums,
            songs,
            annotations,
        })
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "cannot search the library");
        ApiError::Other("the server cannot read its library".to_owned())
    })
}

/// `search3`: the artists, the albums and the songs that match `query`, as
/// `ArtistID3`, `AlbumID3` and `Child` elements of a `searchResult3`.
pub async fn search3(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let request = SearchRequest::from_params(&params)?;
    let found = find(&state.db, user.id, request).await?;
    let annotation = |id: &str| found.annotations.get(id);
    let result = SearchResult3 {
        artist: found
            .artists
            .iter()
            .map(|artist| model::artist(artist, annotation(&artist.id)))
            .collect(),
        album: found
            .albums
            .iter()
            .map(|album| model::album(album, annotation(&album.id)))
            .collect(),
        song: found
            .songs
            .iter()
            .map(|song| model::child(song, annotation(&song.track.id)))
            .collect(),
    };
    Ok(Payload::new("searchResult3", result))
}

/// `search2`: the same matches as `search3`, as the elements of the folder
/// methods in a `searchResult2`. An artist is an `Artist` element, and an
/// album is a `Child` element that is a directory.
pub async fn search2(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let request = SearchRequest::from_params(&params)?;
    let found = find(&state.db, user.id, request).await?;
    let annotation = |id: &str| found.annotations.get(id);
    let result = SearchResult2 {
        artist: found
            .artists
            .iter()
            .map(|artist| model::artist_dir(artist, annotation(&artist.id)))
            .collect(),
        album: found
            .albums
            .iter()
            .map(|album| model::child_dir_for_album(album, annotation(&album.id)))
            .collect(),
        song: found
            .songs
            .iter()
            .map(|song| model::child(song, annotation(&song.track.id)))
            .collect(),
    };
    Ok(Payload::new("searchResult2", result))
}

/// `search`: error 0. The Subsonic API deprecated the method in version
/// 1.4.0, and the server does not serve it. The handler reads no parameter.
pub async fn search() -> ApiResult {
    Err(ApiError::Other(SEARCH_DEPRECATED.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::params;

    use super::*;
    use crate::db::queries::tracks::{self, TrackContent};
    use crate::db::queries::{albums, artists, folders, users};
    use crate::library::tags::TrackMeta;

    /// The request that `query` holds, a query string.
    fn request(query: &str) -> Result<SearchRequest, ApiError> {
        SearchRequest::from_params(&Params::from_query(query))
    }

    fn page(count: u32, offset: u32) -> Page {
        Page { count, offset }
    }

    /// The message of the error 0 that `query` gives.
    fn error_0(query: &str) -> String {
        let error = request(query).expect_err("the request is refused");
        assert_eq!(error.code(), 0, "{query}");
        error.message()
    }

    #[test]
    fn counts_default_to_20_and_offsets_to_0() {
        let defaults = SearchRequest {
            terms: Terms::parse("").unwrap(),
            artists: page(20, 0),
            albums: page(20, 0),
            songs: page(20, 0),
            folder_id: None,
        };
        assert_eq!(request("query="), Ok(defaults.clone()));
        assert_eq!(request("query"), Ok(defaults.clone()));
        assert_eq!(request("query=%22%22"), Ok(defaults.clone()));
        assert_eq!(
            request(
                "query=&artistCount=&artistOffset=&albumCount=&albumOffset=&songCount=\
                 &songOffset=&musicFolderId="
            ),
            Ok(defaults)
        );
    }

    #[test]
    fn each_list_has_its_count_and_its_offset() {
        assert_eq!(
            request(
                "query=Glass+Harbour&artistCount=1&artistOffset=2&albumCount=3&albumOffset=4\
                 &songCount=5&songOffset=6&musicFolderId=7"
            ),
            Ok(SearchRequest {
                terms: Terms::parse("glass harbour").unwrap(),
                artists: page(1, 2),
                albums: page(3, 4),
                songs: page(5, 6),
                folder_id: Some(7),
            })
        );
        let request = request("query=a&songCount=0&songOffset=4294967295&musicFolderId=-1");
        let request = request.expect("the numbers are in range");
        assert_eq!(request.songs, page(0, u32::MAX));
        assert_eq!(request.folder_id, Some(-1));
    }

    #[test]
    fn count_above_the_maximum_is_the_maximum() {
        for (count, expected) in [
            ("499", 499),
            ("500", 500),
            ("501", 500),
            ("100000", 500),
            ("4294967295", 500),
        ] {
            let query = format!("query=&artistCount={count}&albumCount={count}&songCount={count}");
            let request = request(&query).expect("a count in range");
            assert_eq!(request.artists, page(expected, 0), "{count}");
            assert_eq!(request.albums, page(expected, 0), "{count}");
            assert_eq!(request.songs, page(expected, 0), "{count}");
        }
        // The maximum does not apply to an offset.
        let request = request("query=&songOffset=100000").expect("an offset in range");
        assert_eq!(request.songs, page(20, 100_000));
    }

    #[test]
    fn absent_query_is_error_10() {
        for query in ["", "songCount=5", "Query=a", "q=a"] {
            let error = request(query).expect_err("the query is absent");
            assert_eq!(error, ApiError::MissingParam("query".to_owned()), "{query}");
        }
    }

    #[test]
    fn number_out_of_range_and_long_query_are_error_0() {
        for name in [
            "artistCount",
            "artistOffset",
            "albumCount",
            "albumOffset",
            "songCount",
            "songOffset",
        ] {
            for value in ["-1", "4294967296", "x", "1.0"] {
                assert_eq!(
                    error_0(&format!("query=&{name}={value}")),
                    format!("parameter {name} must be an integer from 0 to 4294967295"),
                );
            }
        }
        assert_eq!(
            error_0("query=&musicFolderId=x"),
            "parameter musicFolderId must be an integer"
        );
        assert_eq!(
            error_0(&format!("query={}", "a".repeat(MAX_QUERY_CHARS + 1))),
            "parameter query must be at most 1024 characters"
        );
        assert!(request(&format!("query={}", "a".repeat(MAX_QUERY_CHARS))).is_ok());

        // 32 different terms are a query, and 33 are none.
        let terms: Vec<String> = (0..33).map(|n| format!("t{n}")).collect();
        assert!(request(&format!("query={}", terms[..32].join("+"))).is_ok());
        assert_eq!(
            error_0(&format!("query={}", terms.join("+"))),
            "parameter query must have at most 32 terms"
        );
        assert!(request(&format!("query={}+T0+t1", terms[..32].join("+"))).is_ok());
        // The absent query is reported before a number that is none.
        let error = request("songCount=-1").expect_err("the query is absent");
        assert_eq!(error.code(), 10);
    }

    /// Stores one track of `artist` on `album` in `db` and returns its ID.
    fn add_track(
        conn: &rusqlite::Connection,
        n: u32,
        artist: &str,
        album: &str,
        title: &str,
    ) -> crate::db::Result<String> {
        let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?;
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

    #[tokio::test]
    async fn find_reads_the_lists_and_the_annotations_from_a_file_database() {
        // A file database has read-only connections for the reads, which a
        // memory database has not. The lists are read in a transaction on
        // one of them.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("create the database");
        let (user_id, low_tide) = db
            .write(|conn| {
                let user = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
                let low_tide = add_track(conn, 1, "Aurora Field", "Glass Harbour", "Low Tide")?;
                add_track(conn, 2, "Aurora Field", "Glass Harbour", "Breakwater")?;
                add_track(conn, 3, "Marlow Quartet", "Slow Rivers", "Upstream")?;
                conn.execute(
                    "INSERT INTO annotations (user_id, item_id, item_type, rating, play_count)
                     VALUES (?1, ?2, 'track', 5, 3)",
                    params![user.id, low_tide],
                )?;
                Ok((user.id, low_tide))
            })
            .await
            .expect("store the library");

        for _ in 0..3 {
            let found = find(&db, user_id, request("query=glass").unwrap())
                .await
                .expect("the search runs");
            assert_eq!(found.artists, []);
            let albums: Vec<&str> = found.albums.iter().map(|a| a.name.as_str()).collect();
            assert_eq!(albums, ["Glass Harbour"]);
            let songs: Vec<&str> = found
                .songs
                .iter()
                .map(|song| song.track.title.as_str())
                .collect();
            assert_eq!(songs, ["Breakwater", "Low Tide"]);
            // The one annotation row of the user is for "Low Tide". Its
            // album has the plays of the track and no rating.
            assert_eq!(found.annotations.len(), 2);
            let annotation = found.annotations[&low_tide];
            assert_eq!(annotation.rating, Some(5));
            assert_eq!(annotation.play_count, 3);
            let of_album = found.annotations[&found.albums[0].id];
            assert_eq!((of_album.rating, of_album.play_count), (None, 3));
        }

        // Another user has no annotation for the same rows.
        let found = find(&db, user_id + 1, request("query=").unwrap())
            .await
            .expect("the search runs");
        assert_eq!(found.artists.len(), 2);
        assert_eq!(found.albums.len(), 2);
        assert_eq!(found.songs.len(), 3);
        assert!(found.annotations.is_empty());

        db.close().await.expect("close the database");
    }
}
