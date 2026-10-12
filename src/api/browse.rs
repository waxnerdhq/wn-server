//! The browse methods of the ID3 model: `getMusicFolders`, `getArtists`,
//! `getArtist`, `getAlbum`, `getSong` and `getGenres` (`docs/design.md`
//! section 4).
//!
//! A handler reads rows with `db::queries::browse`, reads the annotations of
//! the user for the IDs of those rows with `annotations::get_many`, and
//! builds the elements of its answer with the constructors of
//! [`model`].
//!
//! # IDs
//!
//! `getArtist`, `getAlbum` and `getSong` require `id`. An absent `id` is
//! error 10. Error 70 answers a value that does not have the shape of an ID
//! of the kind of the method, and an ID that no row has (`item_id`). A
//! value of another shape does not reach SQL.
//!
//! # Order
//!
//! Every list of an answer has the order of its query, which ends with a
//! unique column. Two calls on a library that did not change return the same
//! document.
//!
//! # Tracks without a file
//!
//! `getAlbum` and `getSong` return a track without a file as every other
//! track. Its `song` element has no `path` (`docs/design.md` section 3.4).
//! `getArtists` with `musicFolderId` leaves out an artist whose tracks in
//! that folder all have no file.

use axum::extract::State;
use rusqlite::Connection;
use serde::Serialize;

use super::envelope::ApiError;
use super::model;
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::{annotations, browse};
use crate::db::{self, Db};
use crate::ids::{self, Kind};

/// The `musicFolders` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MusicFolders {
    music_folder: Vec<model::MusicFolder>,
}

/// The `artists` element, which is the `ArtistsID3` element of the API.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Artists {
    /// The articles that the indexes ignore at the start of a name,
    /// separated by spaces.
    ignored_articles: String,
    index: Vec<model::Index<model::ArtistID3>>,
}

/// The `genres` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Genres {
    genre: Vec<model::Genre>,
}

/// `getMusicFolders`: the rows of the `folders` table in ID order, each with
/// its integer ID and its name.
///
/// `run` stores the music folders of the config before it serves the first
/// request, so the list holds them before the first scan. A folder that left
/// the config is listed until a scan has removed its row. The IDs are the
/// ones that `getUser` reports as `folder`.
pub async fn get_music_folders(State(state): State<AppState>, _user: Authed) -> ApiResult {
    let music_folder = read_library(&state.db, |conn| {
        let folders = browse::folders(conn)?;
        Ok(folders.iter().map(model::music_folder).collect())
    })
    .await?;
    Ok(Payload::new("musicFolders", MusicFolders { music_folder }))
}

/// `getArtists`: the album artists, grouped into indexes by the first letter
/// of the name, with `ignoredArticles`.
///
/// `musicFolderId` limits the answer to the artists that have a track with a
/// file in that music folder. `albumCount` of an artist is the stored count
/// with and without the parameter. An ID that no music folder has gives an
/// answer without an index. An empty value counts as absent, and a value
/// that is no integer is error 0.
pub async fn get_artists(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let folder_id = params.opt_i64("musicFolderId")?;
    let index = read_library(&state.db, move |conn| {
        let artists = browse::list_artists(conn, folder_id)?;
        let ids: Vec<&str> = artists.iter().map(|artist| artist.id.as_str()).collect();
        let annotations = annotations::get_many(conn, user.id, &ids)?;
        Ok(model::index(&artists, |artist| {
            model::artist(artist, annotations.get(&artist.id))
        }))
    })
    .await?;
    let artists = Artists {
        ignored_articles: model::ignored_articles(),
        index,
    };
    Ok(Payload::new("artists", artists))
}

/// `getArtist`: one artist with its albums. The albums are in the order of
/// `browse::albums_of_artist`: by year, where an album without one follows
/// the others, then by title.
pub async fn get_artist(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = item_id(&params, Kind::Artist)?;
    let artist = read_library(&state.db, move |conn| {
        let Some(artist) = browse::artist(conn, &id)? else {
            return Ok(None);
        };
        let albums = browse::albums_of_artist(conn, &id)?;
        let mut ids: Vec<&str> = albums.iter().map(|album| album.id.as_str()).collect();
        ids.push(&id);
        let annotations = annotations::get_many(conn, user.id, &ids)?;
        let albums = albums
            .iter()
            .map(|album| model::album(album, annotations.get(&album.id)))
            .collect();
        Ok(Some(model::artist_with_albums(
            &artist,
            annotations.get(&id),
            albums,
        )))
    })
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("artist", artist))
}

/// `getAlbum`: one album with its songs. The songs are in the order of
/// `browse::tracks_of_album`: by disc number, then by track number, then by
/// path.
///
/// `songCount` and `duration` of the album are the values that the last scan
/// stored. The list holds every track row of the album, a track without a
/// file included.
pub async fn get_album(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = item_id(&params, Kind::Album)?;
    let album = read_library(&state.db, move |conn| {
        let Some(album) = browse::album(conn, &id)? else {
            return Ok(None);
        };
        let songs = browse::tracks_of_album(conn, &id)?;
        let mut ids: Vec<&str> = songs.iter().map(|song| song.track.id.as_str()).collect();
        ids.push(&id);
        let annotations = annotations::get_many(conn, user.id, &ids)?;
        let songs = songs
            .iter()
            .map(|song| model::child(song, annotations.get(&song.track.id)))
            .collect();
        Ok(Some(model::album_with_songs(
            &album,
            annotations.get(&id),
            songs,
        )))
    })
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("album", album))
}

/// `getSong`: one song. A track without a file is returned without `path`.
pub async fn get_song(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = item_id(&params, Kind::Track)?;
    let song = read_library(&state.db, move |conn| {
        let Some(song) = browse::track(conn, &id)? else {
            return Ok(None);
        };
        let annotations = annotations::get_many(conn, user.id, &[&id])?;
        Ok(Some(model::child(&song, annotations.get(&id))))
    })
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("song", song))
}

/// `getGenres`: the genre tags of the tracks with the number of songs and of
/// albums of each, in the order of the names without regard to ASCII case.
pub async fn get_genres(State(state): State<AppState>, _user: Authed) -> ApiResult {
    let genre = read_library(&state.db, |conn| {
        let genres = browse::genres(conn)?;
        Ok(genres.iter().map(model::genre).collect())
    })
    .await?;
    Ok(Payload::new("genres", Genres { genre }))
}

/// Returns the value of the required parameter `id` when it has the shape of
/// an ID of `kind` (`docs/design.md` section 3.3).
///
/// An absent `id` is error 10. Error 70 answers every value of another
/// shape: an empty value, a value that `ids::parse` rejects, and the ID of
/// an item of another kind. Such a value is the ID of no item of `kind`, and
/// it does not reach SQL.
fn item_id(params: &Params, kind: Kind) -> Result<String, ApiError> {
    let id = params.req_str("id")?;
    match ids::parse(id) {
        Some((found, _)) if found == kind => Ok(id.to_owned()),
        _ => Err(ApiError::NotFound),
    }
}

/// Runs `read` with a read connection of `db` inside one read transaction.
/// A database that cannot be read is error 0.
///
/// A handler reads the rows of its answer and their annotations with several
/// statements. The transaction gives every statement the same state of the
/// database. A scan that commits between two statements therefore changes
/// nothing in the answer: an artist is returned with the albums it had when
/// its row was read, and an album with its songs.
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
    use std::path::PathBuf;

    use super::*;
    use crate::db::queries::folders;

    const ARTIST: &str = "ar-0123456789abcdef0123456789abcdef";
    const ALBUM: &str = "al-0123456789abcdef0123456789abcdef";
    const TRACK: &str = "tr-0123456789abcdef0123456789abcdef";
    const PLAYLIST: &str = "pl-0123456789abcdef0123456789abcdef";

    fn params(query: &str) -> Params {
        Params::from_query(query)
    }

    #[test]
    fn item_id_returns_an_id_of_the_kind() {
        for (id, kind) in [
            (ARTIST, Kind::Artist),
            (ALBUM, Kind::Album),
            (TRACK, Kind::Track),
        ] {
            let query = format!("id={id}");
            assert_eq!(item_id(&params(&query), kind), Ok(id.to_owned()));
        }
        // The first value of `id` is the one that counts.
        let query = format!("id={ALBUM}&id={ARTIST}");
        assert_eq!(item_id(&params(&query), Kind::Album), Ok(ALBUM.to_owned()));
    }

    #[test]
    fn item_id_without_the_parameter_is_error_10() {
        for query in ["", "ID=1", "musicFolderId=1"] {
            assert_eq!(
                item_id(&params(query), Kind::Artist),
                Err(ApiError::MissingParam("id".to_owned())),
                "{query}"
            );
        }
    }

    #[test]
    fn item_id_of_another_shape_or_kind_is_error_70() {
        let upper = ARTIST.to_uppercase();
        let long = format!("{ARTIST}0");
        let padded = format!("%20{ARTIST}");
        let values = [
            "",
            "1",
            "ar-",
            "ar-0123",
            "0123456789abcdef0123456789abcdef",
            "ar_0123456789abcdef0123456789abcdef",
            "ar-0123456789abcdef0123456789abcdeg",
            "%27%20OR%201%3D1%20--",
            upper.as_str(),
            long.as_str(),
            padded.as_str(),
            ALBUM,
            TRACK,
            PLAYLIST,
        ];
        for value in values {
            let query = format!("id={value}");
            assert_eq!(
                item_id(&params(&query), Kind::Artist),
                Err(ApiError::NotFound),
                "{value:?}"
            );
        }
        // The ID of an artist is the ID of no album and of no track.
        let query = format!("id={ARTIST}");
        assert_eq!(
            item_id(&params(&query), Kind::Album),
            Err(ApiError::NotFound)
        );
        assert_eq!(
            item_id(&params(&query), Kind::Track),
            Err(ApiError::NotFound)
        );
    }

    #[tokio::test]
    async fn read_library_gives_every_statement_one_state_of_the_database() {
        // A file database has read connections of their own, so a write can
        // commit while a read is in progress.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");
        let count = |conn: &Connection| Ok(browse::folders(conn)?.len());

        let writer = db.clone();
        let (before, after_the_write) = read_library(&db, move |conn| {
            let before = count(conn)?;
            writer.with_writer(|conn| {
                folders::sync_folders(conn, &[PathBuf::from("/music")])?;
                Ok(())
            })?;
            Ok((before, count(conn)?))
        })
        .await
        .expect("read the folders");
        // The second statement reads the state that the first one read.
        assert_eq!((before, after_the_write), (0, 0));

        // The write was committed: the next read finds the folder.
        assert_eq!(read_library(&db, count).await, Ok(1));
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
