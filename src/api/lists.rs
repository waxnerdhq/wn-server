//! The list methods: `getAlbumList`, `getAlbumList2`, `getRandomSongs` and
//! `getSongsByGenre` (`docs/design.md` section 4).
//!
//! The queries are in `db::queries::lists`. A handler reads the parameters,
//! reads the rows and the annotations of the user for them, and builds the
//! elements with the constructors of [`model`].
//!
//! A handler reads its list inside one read transaction (`read_lists`), so
//! every statement of a request reads one state of the database.

use std::collections::HashMap;

use axum::extract::State;
use rusqlite::Connection;
use serde::Serialize;

use super::envelope::ApiError;
use super::model::{self, AlbumID3, Child};
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::annotations::{self, Annotation};
use crate::db::queries::browse::{Album, Song};
use crate::db::queries::lists::{self, AlbumList, Page, SongFilter};
use crate::db::{self, Db};

/// The number of albums or songs that a list returns when the request names
/// none.
const DEFAULT_SIZE: u32 = 10;

/// The largest number of albums or songs that one request returns. A larger
/// `size` or `count` is lowered to it.
const MAX_SIZE: u32 = 500;

/// The message of the error for a `type` that is no list type.
const UNKNOWN_TYPE: &str = "parameter type must be one of random, newest, highest, frequent, \
     recent, alphabeticalByName, alphabeticalByArtist, starred, byYear, byGenre";

/// The `albumList2` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AlbumList2 {
    album: Vec<AlbumID3>,
}

/// The `albumList` element. An album is a directory in it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AlbumListDirs {
    album: Vec<Child>,
}

/// The `randomSongs` element and the `songsByGenre` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Songs {
    song: Vec<Child>,
}

/// The parameters of `getAlbumList` and `getAlbumList2`.
struct AlbumListRequest {
    list: AlbumList,
    folder_id: Option<i64>,
    page: Page,
}

impl AlbumListRequest {
    /// Reads the parameters of an album list, which [`get_album_list2`]
    /// describes.
    fn from_params(params: &Params) -> Result<Self, ApiError> {
        let list = match params.req_str("type")? {
            "random" => AlbumList::Random,
            "newest" => AlbumList::Newest,
            "highest" => AlbumList::Highest,
            "frequent" => AlbumList::Frequent,
            "recent" => AlbumList::Recent,
            "alphabeticalByName" => AlbumList::AlphabeticalByName,
            "alphabeticalByArtist" => AlbumList::AlphabeticalByArtist,
            "starred" => AlbumList::Starred,
            "byYear" => AlbumList::ByYear {
                from: required_u32(params, "fromYear")?,
                to: required_u32(params, "toYear")?,
            },
            "byGenre" => AlbumList::ByGenre(params.req_str("genre")?.to_owned()),
            _ => return Err(ApiError::Other(UNKNOWN_TYPE.to_owned())),
        };
        Ok(Self {
            list,
            folder_id: params.opt_i64("musicFolderId")?,
            page: page(params, "size")?,
        })
    }
}

/// `getAlbumList2`: one page of an album list as `albumList2`, whose `album`
/// entries are `AlbumID3` elements. `lists::albums` describes the lists.
///
/// - `type` is required (error 10) and is one of the ten list types, spelled
///   as the API spells them. Another value is error 0.
/// - `byYear` requires `fromYear` and `toYear`, and `byGenre` requires
///   `genre` (error 10). The other types ignore the three.
/// - `size` is 10 without a value and at most 500. `offset` is 0 without a
///   value. `musicFolderId` restricts the list to one music folder.
/// - A value of `size`, `offset`, `fromYear`, `toYear` or `musicFolderId`
///   that is no integer of its range is error 0.
pub async fn get_album_list2(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let request = AlbumListRequest::from_params(&params)?;
    let (albums, annotations) = read_albums(&state.db, user.id, request).await?;
    let album = albums
        .iter()
        .map(|row| model::album(row, annotations.get(&row.id)))
        .collect();
    Ok(Payload::new("albumList2", AlbumList2 { album }))
}

/// `getAlbumList`: the list of [`get_album_list2`], with the same
/// parameters, as `albumList`. Its `album` entries list each album as a
/// directory: `isDir` is `true` and `parent` is the ID of the album artist.
pub async fn get_album_list(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let request = AlbumListRequest::from_params(&params)?;
    let (albums, annotations) = read_albums(&state.db, user.id, request).await?;
    let album = albums
        .iter()
        .map(|row| model::child_dir_for_album(row, annotations.get(&row.id)))
        .collect();
    Ok(Payload::new("albumList", AlbumListDirs { album }))
}

/// `getRandomSongs`: at most `size` songs drawn at random, as `randomSongs`.
///
/// `size` is 10 without a value and at most 500. `genre`, `fromYear`,
/// `toYear` and `musicFolderId` restrict the songs, and an empty `genre`
/// restricts nothing. A value of `size`, `fromYear`, `toYear` or
/// `musicFolderId` that is no integer of its range is error 0.
pub async fn get_random_songs(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let size = size(&params, "size")?;
    let filter = SongFilter {
        genre: params
            .opt_str("genre")
            .filter(|genre| !genre.is_empty())
            .map(str::to_owned),
        from_year: params.opt_u32("fromYear")?,
        to_year: params.opt_u32("toYear")?,
        folder_id: params.opt_i64("musicFolderId")?,
    };
    let read = move |conn: &Connection| {
        let songs = lists::random_songs(conn, &mut rand::rng(), &filter, size)?;
        with_song_annotations(conn, user.id, songs)
    };
    let song = song_elements(read_lists(&state.db, read).await?);
    Ok(Payload::new("randomSongs", Songs { song }))
}

/// `getSongsByGenre`: one page of the songs of a genre by title, as
/// `songsByGenre`.
///
/// `genre` is required (error 10) and matches without regard to case.
/// `count` is 10 without a value and at most 500. `offset` is 0 without a
/// value. `musicFolderId` restricts the songs to one music folder. A value
/// of `count`, `offset` or `musicFolderId` that is no integer of its range
/// is error 0.
pub async fn get_songs_by_genre(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let genre = params.req_str("genre")?.to_owned();
    let page = page(&params, "count")?;
    let folder_id = params.opt_i64("musicFolderId")?;
    let read = move |conn: &Connection| {
        let songs = lists::songs_by_genre(conn, &genre, folder_id, page)?;
        with_song_annotations(conn, user.id, songs)
    };
    let song = song_elements(read_lists(&state.db, read).await?);
    Ok(Payload::new("songsByGenre", Songs { song }))
}

/// The value of the required integer parameter `name`. An absent or empty
/// value is error 10, and a value that is no integer from 0 to 4294967295 is
/// error 0.
fn required_u32(params: &Params, name: &str) -> Result<u32, ApiError> {
    params
        .opt_u32(name)?
        .ok_or_else(|| ApiError::MissingParam(name.to_owned()))
}

/// The number of rows that a request asks for in the parameter `name`:
/// [`DEFAULT_SIZE`] without a value, and at most [`MAX_SIZE`].
fn size(params: &Params, name: &str) -> Result<u32, ApiError> {
    Ok(params.opt_u32(name)?.unwrap_or(DEFAULT_SIZE).min(MAX_SIZE))
}

/// The page that a request asks for: its size in the parameter `size_name`
/// and its offset in `offset`.
fn page(params: &Params, size_name: &str) -> Result<Page, ApiError> {
    Ok(Page {
        size: size(params, size_name)?,
        offset: params.opt_u32("offset")?.unwrap_or(0),
    })
}

/// Reads the albums of `request` and the annotations of the user `user_id`
/// for them.
async fn read_albums(
    db: &Db,
    user_id: i64,
    request: AlbumListRequest,
) -> Result<(Vec<Album>, HashMap<String, Annotation>), ApiError> {
    read_lists(db, move |conn| {
        let albums = lists::albums(
            conn,
            &mut rand::rng(),
            user_id,
            &request.list,
            request.folder_id,
            request.page,
        )?;
        let ids: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
        let annotations = annotations::get_many(conn, user_id, &ids)?;
        Ok((albums, annotations))
    })
    .await
}

/// Returns `songs` with the annotations of the user `user_id` for them.
fn with_song_annotations(
    conn: &Connection,
    user_id: i64,
    songs: Vec<Song>,
) -> db::Result<(Vec<Song>, HashMap<String, Annotation>)> {
    let ids: Vec<&str> = songs.iter().map(|song| song.track.id.as_str()).collect();
    let annotations = annotations::get_many(conn, user_id, &ids)?;
    Ok((songs, annotations))
}

/// The `Child` element of each song, in the order of the songs.
fn song_elements((songs, annotations): (Vec<Song>, HashMap<String, Annotation>)) -> Vec<Child> {
    songs
        .iter()
        .map(|song| model::child(song, annotations.get(&song.track.id)))
        .collect()
}

/// Runs `read` with a read connection of `db` inside one read transaction.
/// A database that cannot be read is error 0. The cause goes to the log and
/// not to the client.
///
/// A list is read with several statements: the IDs of its rows in the order
/// of the list, the rows of these IDs, and the annotations of the user for
/// them. The transaction gives every statement the same state of the
/// database. A scan that commits between two statements therefore takes no
/// row out of the answer, and a page of a list that has an order is shorter
/// than its size only where the list ends.
///
/// `api::browse` reads its rows the same way.
async fn read_lists<T, F>(db: &Db, read: F) -> Result<T, ApiError>
where
    F: FnOnce(&Connection) -> db::Result<T> + Send + 'static,
    T: Send + 'static,
{
    db.read(move |conn| {
        // The transaction is deferred: its first statement fixes the state
        // that the later ones read. It writes nothing, so dropping it ends
        // it without an effect on the database.
        let snapshot = conn.unchecked_transaction()?;
        read(&snapshot)
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "cannot read the library");
        ApiError::Other("the server cannot read its library".to_owned())
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db::queries::tracks::TrackContent;
    use crate::db::queries::{albums, artists, folders, tracks};
    use crate::library::tags::TrackMeta;

    fn params(query: &str) -> Params {
        Params::from_query(query)
    }

    /// Stores one Jazz track of the album `album` by "Aurora Field" with
    /// the ID `id`, as the scanner stores a track.
    fn insert_track(conn: &Connection, id: &str, album: &str) -> db::Result<()> {
        let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?;
        let artist_id = artists::upsert_by_key(conn, "Aurora Field")?;
        let album_id = albums::upsert_by_key(conn, &artist_id, album, "Aurora Field", false)?;
        let meta = TrackMeta {
            title: album.to_owned(),
            artist: "Aurora Field".to_owned(),
            album_artist: "Aurora Field".to_owned(),
            album: album.to_owned(),
            track_no: None,
            disc_no: None,
            year: None,
            genre: Some("Jazz".to_owned()),
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
        tracks::insert_track(conn, id, folder[0].id, &format!("{id}.flac"), &content)
    }

    #[test]
    fn size_is_10_without_a_value_and_500_at_most() {
        for (query, expected) in [
            ("", 10),
            ("size=", 10),
            ("size=0", 0),
            ("size=1", 1),
            ("size=500", 500),
            ("size=501", 500),
            ("size=4294967295", 500),
            ("count=7", 10),
        ] {
            assert_eq!(size(&params(query), "size"), Ok(expected), "{query}");
        }
        assert_eq!(size(&params("count=7&size=3"), "count"), Ok(7));
        for query in ["size=-1", "size=4294967296", "size=ten", "size=1.5"] {
            let error = size(&params(query), "size").expect_err("no size");
            assert_eq!(error.code(), 0, "{query}");
            assert_eq!(
                error.message(),
                "parameter size must be an integer from 0 to 4294967295"
            );
        }
        assert_eq!(
            page(&params("count=20&offset=40"), "count"),
            Ok(Page {
                size: 20,
                offset: 40
            })
        );
        assert_eq!(
            page(&params("offset="), "size"),
            Ok(Page {
                size: 10,
                offset: 0
            })
        );
    }

    #[tokio::test]
    async fn read_lists_gives_every_statement_one_state_of_the_database() {
        // A file database has read connections of their own, so a write can
        // commit while a read is in progress.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");
        db.write(|conn| insert_track(conn, "tr-00000000000000000000000000000001", "First"))
            .await
            .expect("write the first track");
        let page = Page {
            size: 10,
            offset: 0,
        };
        let titles = move |conn: &Connection| -> db::Result<Vec<String>> {
            let songs = lists::songs_by_genre(conn, "Jazz", None, page)?;
            Ok(songs.into_iter().map(|song| song.track.title).collect())
        };

        let writer = db.clone();
        let (before, after_the_write) = read_lists(&db, move |conn| {
            let before = titles(conn)?;
            writer.with_writer(|conn| {
                insert_track(conn, "tr-00000000000000000000000000000002", "Second")
            })?;
            Ok((before, titles(conn)?))
        })
        .await
        .expect("read the songs");
        // The second list is of the state that the first one read.
        assert_eq!(before, ["First"]);
        assert_eq!(after_the_write, ["First"]);

        // The write was committed: the next request finds the track.
        assert_eq!(
            read_lists(&db, titles).await,
            Ok(vec!["First".to_owned(), "Second".to_owned()])
        );
    }

    #[tokio::test]
    async fn read_lists_reports_a_database_error_as_error_0() {
        let db = Db::open_memory().expect("a memory database");
        let result = read_lists(&db, |conn| {
            let count: i64 =
                conn.query_row("SELECT count(*) FROM no_such_table", [], |row| row.get(0))?;
            Ok(count)
        })
        .await;
        // The message holds nothing of the statement that failed.
        assert_eq!(
            result,
            Err(ApiError::Other(
                "the server cannot read its library".to_owned()
            ))
        );
    }
}
