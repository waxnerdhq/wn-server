//! The folder methods `getIndexes` and `getMusicDirectory` (`docs/design.md`
//! section 4).
//!
//! The server stores artists, albums and songs, and no directory of the
//! music folders. The two methods list these rows as a tree of three levels,
//! for the clients that browse by directory:
//!
//! - `getIndexes` lists the album artists, which are the directories of the
//!   first level.
//! - `getMusicDirectory` for the ID of an artist lists the albums of the
//!   artist as directories.
//! - `getMusicDirectory` for the ID of an album lists the songs of the
//!   album.
//!
//! # IDs
//!
//! Every ID of the tree is the ID of the row: the artist, album and track
//! IDs of every other method (`docs/design.md` section 3.3). `parent` of a
//! song is the ID of its album, and `parent` of an album is the ID of its
//! artist, so every `parent` is an ID that `getMusicDirectory` takes. An
//! artist has no parent.
//!
//! # Rows and elements
//!
//! A handler reads rows with `db::queries::browse`, reads the annotations of
//! the user for their IDs with `annotations::get_many`, and builds the
//! elements with the constructors of [`model`]. `getIndexes` reads the
//! artists that `getArtists` reads and groups them with the same function,
//! so the two methods return the same artists in the same indexes.
//! `getMusicDirectory` reads the albums of an artist in the order of
//! `getArtist` and the songs of an album in the order of `getAlbum`.
//!
//! The number of statements of an answer does not depend on the size of the
//! directory. A handler reads the row of the directory with one statement
//! and its children with one, and it reads the annotations of the directory
//! and of all children with one call of `annotations::get_many`.
//!
//! # Tracks without a file
//!
//! A track row without a file is no child of the directory of its album
//! (`docs/design.md` section 3.4). The album row and the artist row of such
//! a track stay, so the album is listed in the directory of its artist and
//! the artist in the indexes. An album whose tracks all have no file is a
//! directory without a child. `getIndexes` with `musicFolderId` leaves out
//! an artist that has no track with a file in that folder.

use axum::extract::State;
use rusqlite::Connection;
use serde::Serialize;

use super::envelope::ApiError;
use super::model;
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::{annotations, browse, indexes};
use crate::db::{self, Db};
use crate::ids::{self, Kind};

/// The `indexes` element.
///
/// The API requires `ignoredArticles` and `lastModified`. The element has no
/// `shortcut` and no `child`: the server has no shortcut directories, and
/// every song is in the directory of its album.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Indexes {
    /// The articles that the indexes ignore at the start of a name,
    /// separated by spaces.
    ignored_articles: String,
    /// The time at which the artists last changed, in milliseconds since
    /// the Unix epoch. 0 when no scan has ended.
    last_modified: i64,
    /// The artists in their indexes. `None` in the answer to a request
    /// whose `ifModifiedSince` is not before `last_modified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<Vec<model::Index<model::Artist>>>,
}

/// `getIndexes`: the album artists, grouped into indexes by the first letter
/// of the name, with `ignoredArticles` and `lastModified`.
///
/// The artists, their indexes and their order are those of `getArtists`.
/// Each artist is the `Artist` element of the folder methods, with the star
/// and the rating of the user.
///
/// - `musicFolderId` limits the answer to the artists that have a track with
///   a file in that music folder. An ID that no music folder has gives an
///   answer without an index.
/// - `lastModified` is `indexes::last_modified` in milliseconds: the end
///   time of the last scan that can have changed the artists. It is 0 when
///   no scan has ended.
/// - `ifModifiedSince` is a time in milliseconds. When it is at or after
///   `lastModified`, the artists did not change since that time, and the
///   answer is `indexes` with `ignoredArticles` and `lastModified` and
///   without `index` ([`is_unchanged`]).
///
/// An empty value of either parameter counts as absent, and a value that is
/// no integer is error 0.
pub async fn get_indexes(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let folder_id = params.opt_i64("musicFolderId")?;
    let since = params.opt_i64("ifModifiedSince")?;
    let indexes = read_library(&state.db, move |conn| {
        let last_modified = indexes::last_modified(conn)?;
        let scan_is_open = indexes::scan_is_open(conn)?;
        let index = if is_unchanged(since, last_modified, scan_is_open) {
            None
        } else {
            let artists = browse::list_artists(conn, folder_id)?;
            let ids: Vec<&str> = artists.iter().map(|artist| artist.id.as_str()).collect();
            let annotations = annotations::get_many(conn, user.id, &ids)?;
            Some(model::index(&artists, |artist| {
                model::artist_dir(artist, annotations.get(&artist.id))
            }))
        };
        Ok(Indexes {
            ignored_articles: model::ignored_articles(),
            last_modified: last_modified.unwrap_or(0),
            index,
        })
    })
    .await?;
    Ok(Payload::new("indexes", indexes))
}

/// `getMusicDirectory`: the directory that `id` names, with its children.
///
/// - The ID of an artist gives the albums of the artist, each as a directory
///   (`isDir: true`, `parent` is the ID of the artist), in the order of
///   `browse::albums_of_artist`: by year, then by title.
/// - The ID of an album gives the songs of the album that have a file
///   (`isDir: false`, `parent` is the ID of the album), in the order of
///   `browse::tracks_of_album`: by disc number, then by track number, then
///   by path. `parent` of the directory is the ID of the album artist.
///
/// `id` is required, and an absent one is error 10. Error 70 answers every
/// value that names no directory ([`directory_id`]), and an artist or album
/// ID that no row has.
pub async fn get_music_directory(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = directory_id(&params)?;
    let directory = read_library(&state.db, move |conn| match &id {
        DirectoryId::Artist(id) => artist_directory(conn, user.id, id),
        DirectoryId::Album(id) => album_directory(conn, user.id, id),
    })
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("directory", directory))
}

/// Returns whether a request with the `ifModifiedSince` value `since` gets
/// an answer without the artists.
///
/// That is the case when `since` is at or after `last_modified`: the last
/// change of the artists is not later than the time the client names. A
/// client that sends the `lastModified` of its last answer back therefore
/// gets the artists only when they can have changed.
///
/// A request without `ifModifiedSince` always gets the artists. So does
/// every request while no scan has ended, because there is no time to
/// compare with, and while `scan_is_open`: a scan that has no end time can
/// have stored changes that `last_modified` does not cover yet.
fn is_unchanged(since: Option<i64>, last_modified: Option<i64>, scan_is_open: bool) -> bool {
    match (since, last_modified) {
        (Some(since), Some(last_modified)) => !scan_is_open && since >= last_modified,
        _ => false,
    }
}

/// The directory that the `id` of a `getMusicDirectory` request names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectoryId {
    /// The ID of an artist, whose directory holds albums.
    Artist(String),
    /// The ID of an album, whose directory holds songs.
    Album(String),
}

/// Returns the directory that the required parameter `id` names: an artist
/// when the value has the shape of an artist ID, and an album when it has
/// the shape of an album ID (`docs/design.md` section 3.3).
///
/// An absent `id` is error 10. Error 70 answers every other value: an empty
/// value, a value that `ids::parse` rejects, and the ID of a track or of a
/// playlist, which is no directory. Such a value does not reach SQL.
fn directory_id(params: &Params) -> Result<DirectoryId, ApiError> {
    let id = params.req_str("id")?;
    match ids::parse(id) {
        Some((Kind::Artist, _)) => Ok(DirectoryId::Artist(id.to_owned())),
        Some((Kind::Album, _)) => Ok(DirectoryId::Album(id.to_owned())),
        _ => Err(ApiError::NotFound),
    }
}

/// Returns the directory of the artist `id` with its albums as children, or
/// `None` when no artist has the ID.
///
/// The children are every album row of the artist, as `getArtist` lists
/// them. The artist, the albums and the annotations of all of them are one
/// read each, for an artist with any number of albums.
fn artist_directory(
    conn: &Connection,
    user_id: i64,
    id: &str,
) -> db::Result<Option<model::Directory>> {
    let Some(artist) = browse::artist(conn, id)? else {
        return Ok(None);
    };
    let albums = browse::albums_of_artist(conn, id)?;
    let mut ids: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
    ids.push(id);
    let annotations = annotations::get_many(conn, user_id, &ids)?;
    let children = albums
        .iter()
        .map(|album| model::child_dir_for_album(album, annotations.get(&album.id)))
        .collect();
    Ok(Some(model::directory_for_artist(
        &artist,
        annotations.get(id),
        children,
    )))
}

/// Returns the directory of the album `id` with its songs as children, or
/// `None` when no album has the ID.
///
/// The children are the tracks of the album that have a file: a track
/// without one is in no directory (`docs/design.md` section 3.4). The album,
/// the tracks and the annotations of all of them are one read each, for an
/// album with any number of tracks.
fn album_directory(
    conn: &Connection,
    user_id: i64,
    id: &str,
) -> db::Result<Option<model::Directory>> {
    let Some(album) = browse::album(conn, id)? else {
        return Ok(None);
    };
    let mut songs = browse::tracks_of_album(conn, id)?;
    songs.retain(|song| song.track.has_file());
    let mut ids: Vec<&str> = songs.iter().map(|song| song.track.id.as_str()).collect();
    ids.push(id);
    let annotations = annotations::get_many(conn, user_id, &ids)?;
    let children = songs
        .iter()
        .map(|song| model::child(song, annotations.get(&song.track.id)))
        .collect();
    Ok(Some(model::directory_for_album(
        &album,
        annotations.get(id),
        children,
    )))
}

/// Runs `read` with a read connection of `db` inside one read transaction.
/// A database that cannot be read is error 0.
///
/// A handler reads the time of the last change, the rows of its answer and
/// their annotations with several statements. The transaction gives every
/// statement the same state of the database. The `lastModified` of an
/// answer is therefore the time of the artists in that answer: a scan that
/// ends between two statements changes neither.
async fn read_library<T, F>(db: &Db, read: F) -> Result<T, ApiError>
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
    use super::*;
    use crate::db::queries::scans::{self, ScanCounts};

    const ARTIST: &str = "ar-0123456789abcdef0123456789abcdef";
    const ALBUM: &str = "al-0123456789abcdef0123456789abcdef";
    const TRACK: &str = "tr-0123456789abcdef0123456789abcdef";
    const PLAYLIST: &str = "pl-0123456789abcdef0123456789abcdef";

    fn params(query: &str) -> Params {
        Params::from_query(query)
    }

    #[test]
    fn directory_id_is_the_id_of_an_artist_or_of_an_album() {
        assert_eq!(
            directory_id(&params(&format!("id={ARTIST}"))),
            Ok(DirectoryId::Artist(ARTIST.to_owned()))
        );
        assert_eq!(
            directory_id(&params(&format!("id={ALBUM}"))),
            Ok(DirectoryId::Album(ALBUM.to_owned()))
        );
        // The first value of `id` is the one that counts.
        assert_eq!(
            directory_id(&params(&format!("id={ALBUM}&id={ARTIST}"))),
            Ok(DirectoryId::Album(ALBUM.to_owned()))
        );
    }

    #[test]
    fn directory_id_without_the_parameter_is_error_10() {
        for query in ["", "ID=1", "musicFolderId=1"] {
            assert_eq!(
                directory_id(&params(query)),
                Err(ApiError::MissingParam("id".to_owned())),
                "{query}"
            );
        }
    }

    #[test]
    fn directory_id_of_a_value_that_names_no_directory_is_error_70() {
        let upper = ARTIST.to_uppercase();
        let long = format!("{ALBUM}0");
        let padded = format!("%20{ARTIST}");
        let values = [
            "",
            "1",
            "ar-",
            "al-0123",
            "0123456789abcdef0123456789abcdef",
            "ar_0123456789abcdef0123456789abcdef",
            "al-0123456789abcdef0123456789abcdeg",
            "%27%20OR%201%3D1%20--",
            upper.as_str(),
            long.as_str(),
            padded.as_str(),
            // A song and a playlist are no directories.
            TRACK,
            PLAYLIST,
        ];
        for value in values {
            let query = format!("id={value}");
            assert_eq!(
                directory_id(&params(&query)),
                Err(ApiError::NotFound),
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_request_is_unchanged_when_its_time_is_at_or_after_the_last_change() {
        let last = Some(1_791_676_800_000);
        // (ifModifiedSince, lastModified, a scan is open) and the result.
        let cases = [
            // Without `ifModifiedSince` the artists are returned.
            (None, last, false, false),
            (None, None, false, false),
            // A time before the last change.
            (Some(0), last, false, false),
            (Some(-1), last, false, false),
            (Some(1_791_676_799_999), last, false, false),
            // The time of the last change, and a later one.
            (Some(1_791_676_800_000), last, false, true),
            (Some(1_791_676_800_001), last, false, true),
            (Some(i64::MAX), last, false, true),
            // No scan has ended: there is no time to compare with.
            (Some(0), None, false, false),
            (Some(i64::MAX), None, false, false),
            // A scan has no end time.
            (Some(1_791_676_800_000), last, true, false),
            (Some(i64::MAX), last, true, false),
            (Some(i64::MAX), None, true, false),
        ];
        for (since, last_modified, scan_is_open, expected) in cases {
            assert_eq!(
                is_unchanged(since, last_modified, scan_is_open),
                expected,
                "{since:?} {last_modified:?} {scan_is_open}"
            );
        }
    }

    #[tokio::test]
    async fn read_library_gives_every_statement_one_state_of_the_database() {
        // A file database has read connections of their own, so a write can
        // commit while a read is in progress.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");

        let writer = db.clone();
        let (before, after_the_scan, open) = read_library(&db, move |conn| {
            let before = indexes::last_modified(conn)?;
            // A scan starts and ends between two statements of the read.
            writer.with_writer(|conn| {
                let id = scans::insert(conn)?;
                let counts = ScanCounts {
                    added: 1,
                    ..ScanCounts::default()
                };
                scans::finish(conn, id, &counts, None)
            })?;
            Ok((
                before,
                indexes::last_modified(conn)?,
                indexes::scan_is_open(conn)?,
            ))
        })
        .await
        .expect("read the scans");
        // The later statements read the state that the first one read.
        assert_eq!((before, after_the_scan, open), (None, None, false));

        // The scan was committed: the next read finds its end time.
        let last_modified = read_library(&db, indexes::last_modified).await;
        assert!(matches!(last_modified, Ok(Some(_))), "{last_modified:?}");
    }

    #[tokio::test]
    async fn read_library_reports_a_database_error_as_error_0() {
        let db = Db::open_memory().expect("a memory database");
        let result = read_library(&db, |conn| {
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
