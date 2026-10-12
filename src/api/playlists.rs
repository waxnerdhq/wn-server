//! The playlist methods: `getPlaylists`, `getPlaylist`, `createPlaylist`,
//! `updatePlaylist` and `deletePlaylist` (`docs/design.md` section 4).
//!
//! # Ownership
//!
//! A playlist belongs to the user that created it. Every method reads and
//! changes the playlists of the user of the request and no other
//! (`db::queries::playlists`). The ID of a playlist of another user is
//! answered as an ID that no playlist has, with error 70.
//!
//! # Order
//!
//! The entries of a playlist have the order in which the requests of the
//! user put them. A song can be in a playlist several times. The position of
//! an entry is its place in the `entry` list of `getPlaylist`, counted from
//! 0, and `songIndexToRemove` names an entry by that position.
//!
//! # Writes
//!
//! `createPlaylist` and `updatePlaylist` check every value of the request
//! and write in one transaction. A request that is answered with an error
//! changes nothing.
//!
//! # IDs
//!
//! A value that does not have the shape of an ID of the kind its parameter
//! takes is the ID of no item and is error 70 (`docs/design.md` section
//! 3.3). It does not reach SQL.
//!
//! # Tracks without a file
//!
//! A track without a file keeps its entries and is listed as every other
//! song, without `path` (`docs/design.md` section 3.4).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use rusqlite::Connection;
use serde::Serialize;

use super::envelope::ApiError;
use super::model::{self, Child};
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::playlists::{self, Change, MAX_ENTRIES, Outcome, Refusal};
use crate::db::queries::{annotations, browse};
use crate::db::{self, Db};
use crate::ids::{self, Kind};

/// The message of the error that `getPlaylists` returns for the name of
/// another user.
const OTHER_USER: &str = "username must be the name of the user of the request";

/// The message of the error for a value of `songIndexToRemove` that is not
/// the position of an entry of the playlist.
const NO_POSITION: &str =
    "parameter songIndexToRemove must be the position of a song in the playlist, counted from 0";

/// The `playlist` element. With `entry` it is the `PlaylistWithSongs`
/// element that `getPlaylist` and `createPlaylist` return.
///
/// `id`, `name`, `songCount`, `duration`, `created` and `changed` are
/// required by the API. The element has no `allowedUser`, `readonly` and
/// `validUntil`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Playlist {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    /// The name of the user that owns the playlist.
    owner: String,
    public: bool,
    /// The number of entries.
    song_count: u64,
    /// The duration in seconds.
    duration: u64,
    created: String,
    changed: String,
    /// The cover art ID of the first song.
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_art: Option<String>,
    /// The songs of the playlist in the order of its entries. `None` where
    /// the playlist is an entry of `getPlaylists`.
    #[serde(skip_serializing_if = "Option::is_none")]
    entry: Option<Vec<Child>>,
}

/// The `playlists` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Playlists {
    playlist: Vec<Playlist>,
}

/// Which playlist a `createPlaylist` request writes.
enum Target {
    /// A new playlist with this name.
    New(String),
    /// The playlist with this ID, whose entries the request replaces.
    Existing(String),
}

/// `getPlaylists`: the playlists of the user of the request, without their
/// songs, in the order of their names without regard to ASCII case.
///
/// `username` is optional, and an empty value counts as absent. It must be
/// the name of the user of the request, compared without regard to ASCII
/// case. Another name is error 50, whether an account has that name or not:
/// the playlists of another user are not served. With `auth.disabled_unsafe`
/// every request acts as the owner, so every name returns the playlists of
/// the owner.
pub async fn get_playlists(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let username = params.opt_str("username").filter(|name| !name.is_empty());
    let own = username.is_none_or(|name| name.eq_ignore_ascii_case(&user.name));
    if !own && !state.config.auth.disabled_unsafe {
        return Err(ApiError::Forbidden(OTHER_USER.to_owned()));
    }
    let playlist = read(&state.db, move |conn| {
        let rows = playlists::list(conn, user.id)?;
        let first_ids: Vec<&str> = rows
            .iter()
            .filter_map(|row| row.first_track_id.as_deref())
            .collect();
        // The cover art ID of each first song that has one, by track ID.
        let covers: HashMap<String, String> = browse::tracks_by_ids(conn, &first_ids)?
            .iter()
            .filter_map(|song| Some((song.track.id.clone(), model::song_cover_art(song)?)))
            .collect();
        let elements = rows
            .iter()
            .map(|row| {
                let first = row.first_track_id.as_ref();
                let cover_art = first.and_then(|id| covers.get(id).cloned());
                element(row, cover_art, None)
            })
            .collect();
        Ok(elements)
    })
    .await?;
    Ok(Payload::new("playlists", Playlists { playlist }))
}

/// `getPlaylist`: one playlist of the user of the request with its songs in
/// the order of its entries.
///
/// `id` is required; the error has code 10. A value that is the ID of no
/// playlist of the user is error 70.
pub async fn get_playlist(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = playlist_id(params.req_str("id")?)?;
    let playlist = read(&state.db, move |conn| read_playlist(conn, user.id, &id))
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("playlist", playlist))
}

/// `createPlaylist`: creates a playlist, or replaces the songs of one, and
/// returns the playlist with its songs as `getPlaylist` does.
///
/// - Without `playlistId`, `name` is required, and the request creates a
///   playlist of the user with that name and the songs of `songId`. An empty
///   `playlistId` and an empty `name` count as absent. An absent `name` is
///   error 10.
/// - With `playlistId`, the request replaces the songs of that playlist with
///   the songs of `songId` and keeps its name, its comment and its `public`
///   value. `name` is not read. A value that is the ID of no playlist of the
///   user is error 70.
/// - `songId` may occur any number of times. The songs get the order of the
///   values, and a song that is named twice is in the playlist twice. A
///   request without `songId` gives a playlist without a song.
/// - A value of `songId` that is the ID of no song is error 70. More than
///   [`MAX_ENTRIES`] values are error 0. Nothing is written in both cases.
///
/// The answer is the one of API version 1.14.0 and later, whatever `v` the
/// request has: the server does not compare `v`.
pub async fn create_playlist(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let present = |name: &str| params.opt_str(name).filter(|value| !value.is_empty());
    let target = match present("playlistId") {
        Some(id) => Target::Existing(playlist_id(id)?),
        None => {
            let name = present("name").ok_or_else(|| ApiError::MissingParam("name".to_owned()))?;
            Target::New(name.to_owned())
        }
    };
    let song_ids = song_ids(&params, "songId")?;
    let playlist = write(&state.db, move |conn, now| {
        let written = match target {
            Target::New(name) => playlists::create(conn, user.id, &name, &song_ids, now)?,
            Target::Existing(id) => {
                playlists::replace_tracks(conn, user.id, &id, &song_ids, now)?.map(|()| id)
            }
        };
        let id = match written {
            Ok(id) => id,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // The write connection is still held, so no other request of the
        // server changes the playlist before it is read. The transaction
        // gives the statements one state of the database, as `read` does.
        let snapshot = conn.unchecked_transaction()?;
        Ok(read_playlist(&snapshot, user.id, &id)?.ok_or(Refusal::NoPlaylist))
    })
    .await?;
    Ok(Payload::new("playlist", playlist))
}

/// `updatePlaylist`: changes one playlist of the user of the request and
/// answers with the empty response.
///
/// - `playlistId` is required; the error has code 10. A value that is the ID
///   of no playlist of the user is error 70.
/// - `name` sets the name. An empty value counts as absent, so a playlist
///   always has a name.
/// - `comment` sets the comment, and an empty value removes it.
/// - `public` is `true` or `false` in any letter case. Another value is
///   error 0.
/// - `songIndexToRemove` and `songIdToAdd` may each occur any number of
///   times. Every `songIndexToRemove` is the position of a song in the
///   playlist as it is before the request, counted from 0. The songs at
///   these positions are removed, and then the songs of `songIdToAdd` are
///   added at the end in the order of the values (`playlists::update`). The
///   order of the `songIndexToRemove` values changes nothing, and a position
///   that is named twice removes one song.
/// - A `songIndexToRemove` that is no such position is error 0: a value that
///   is no integer, a negative one, and a position at or after the end of
///   the playlist. An empty value counts as absent.
/// - A value of `songIdToAdd` that is the ID of no song is error 70. A
///   request that would give the playlist more than [`MAX_ENTRIES`] songs is
///   error 0.
///
/// A request that is answered with an error changes nothing: the name, the
/// comment, the `public` value and every song stay.
pub async fn update_playlist(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = playlist_id(params.req_str("playlistId")?)?;
    let name = params.opt_str("name").filter(|name| !name.is_empty());
    let change = Change {
        name: name.map(str::to_owned),
        comment: params.opt_str("comment").map(str::to_owned),
        public: params.opt_bool("public")?,
        remove: positions(&params)?,
        add: song_ids(&params, "songIdToAdd")?,
    };
    write(&state.db, move |conn, now| {
        playlists::update(conn, user.id, &id, &change, now)
    })
    .await?;
    Ok(Payload::empty())
}

/// `deletePlaylist`: deletes one playlist of the user of the request with
/// its entries and answers with the empty response. The songs stay in the
/// library.
///
/// `id` is required; the error has code 10. A value that is the ID of no
/// playlist of the user is error 70.
pub async fn delete_playlist(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = playlist_id(params.req_str("id")?)?;
    write(&state.db, move |conn, _now| {
        let deleted = playlists::delete(conn, user.id, &id)?;
        Ok(if deleted {
            Ok(())
        } else {
            Err(Refusal::NoPlaylist)
        })
    })
    .await?;
    Ok(Payload::empty())
}

/// Returns `value` when it has the shape of a playlist ID. Every other
/// value is the ID of no playlist and is error 70: an empty value, a value
/// that `ids::parse` rejects, and the ID of an item of another kind.
fn playlist_id(value: &str) -> Result<String, ApiError> {
    match ids::parse(value) {
        Some((Kind::Playlist, _)) => Ok(value.to_owned()),
        _ => Err(ApiError::NotFound),
    }
}

/// Returns every value of the parameter `name`, in the order of the request,
/// when each has the shape of a track ID.
///
/// More than [`MAX_ENTRIES`] values are error 0. The values are counted
/// before they are read, so a request cannot make the server check more of
/// them. A value of another shape, an empty one included, is the ID of no
/// song and is error 70.
fn song_ids(params: &Params, name: &str) -> Result<Vec<String>, ApiError> {
    let values = params.all(name);
    if values.len() > MAX_ENTRIES {
        return Err(refused(Refusal::TooLong));
    }
    values
        .into_iter()
        .map(|value| match ids::parse(value) {
            Some((Kind::Track, _)) => Ok(value.to_owned()),
            _ => Err(ApiError::NotFound),
        })
        .collect()
}

/// Returns the values of `songIndexToRemove` as positions, in the order of
/// the request. An empty value counts as absent, as it does for every
/// parameter that holds a number. A value that is no integer of 0 or more is
/// error 0.
fn positions(params: &Params) -> Result<Vec<usize>, ApiError> {
    params
        .all("songIndexToRemove")
        .into_iter()
        .filter(|value| !value.is_empty())
        .map(|value| value.parse().map_err(|_| refused(Refusal::NoPosition)))
        .collect()
}

/// The error that answers a request that the queries refused.
///
/// The ID of no playlist of the user and the ID of no song are error 70. A
/// position that the playlist does not have and a playlist that would be too
/// long are error 0 with a message that names the rule.
fn refused(refusal: Refusal) -> ApiError {
    match refusal {
        Refusal::NoPlaylist | Refusal::NoTrack => ApiError::NotFound,
        Refusal::NoPosition => ApiError::Other(NO_POSITION.to_owned()),
        Refusal::TooLong => {
            ApiError::Other(format!("a playlist holds at most {MAX_ENTRIES} songs"))
        }
    }
}

/// Reads the playlist `id` of the user `user_id` with its songs and returns
/// its element, or `None` when the user owns no playlist with the ID.
///
/// The songs are in the order of the entries, and a song with two entries is
/// in the list twice. Each carries the annotation of the user. `songCount`
/// and `duration` count every entry.
///
/// The caller runs the function in one transaction, so the counts of the
/// element agree with its list.
fn read_playlist(conn: &Connection, user_id: i64, id: &str) -> db::Result<Option<Playlist>> {
    let Some(row) = playlists::get(conn, user_id, id)? else {
        return Ok(None);
    };
    let track_ids = playlists::track_ids(conn, user_id, id)?;
    let songs = browse::tracks_by_ids(conn, &track_ids)?;
    let annotations = annotations::get_many(conn, user_id, &track_ids)?;
    let cover_art = songs.first().and_then(model::song_cover_art);
    let entry = songs
        .iter()
        .map(|song| model::child(song, annotations.get(&song.track.id)))
        .collect();
    Ok(Some(element(&row, cover_art, Some(entry))))
}

/// Returns the `playlist` element of `row`. `cover_art` is the cover art ID
/// of its first song, and `entry` its songs, or `None` for an entry of
/// `getPlaylists`.
///
/// `duration` is the sum of the durations of the songs in milliseconds,
/// rounded once to seconds, as the duration of an album is (`model::seconds`).
/// It can differ from the sum of the rounded durations of the songs.
fn element(
    row: &playlists::Playlist,
    cover_art: Option<String>,
    entry: Option<Vec<Child>>,
) -> Playlist {
    Playlist {
        id: row.id.clone(),
        name: row.name.clone(),
        comment: row.comment.clone(),
        owner: row.owner.clone(),
        public: row.public,
        song_count: row.song_count,
        duration: model::seconds(row.duration_ms),
        created: model::iso8601(row.created_at),
        changed: model::iso8601(row.changed_at),
        cover_art,
        entry,
    }
}

/// Runs `read` with a read connection of `db` inside one read transaction,
/// so every statement of an answer reads one state of the database. A
/// database that cannot be read is error 0.
async fn read<T, F>(db: &Db, read: F) -> Result<T, ApiError>
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
        tracing::error!(%error, "cannot read the playlists");
        ApiError::Other("the server cannot read its playlists".to_owned())
    })
}

/// Runs `write` with the write connection of `db` and the time of the write
/// in Unix seconds. A request that the queries refused is answered with the
/// error of [`refused`], and a database that cannot be written with error 0.
///
/// The time is read once the write connection is held. While the clock runs
/// forward, the request that writes later therefore does not store the
/// earlier time.
async fn write<T, F>(db: &Db, write: F) -> Result<T, ApiError>
where
    F: FnOnce(&mut Connection, i64) -> db::Result<Outcome<T>> + Send + 'static,
    T: Send + 'static,
{
    db.write(move |conn| write(conn, unix_now()))
        .await
        .map_err(|error| {
            tracing::error!(%error, "cannot write the playlist");
            ApiError::Other("the server cannot store the playlist".to_owned())
        })?
        .map_err(refused)
}

/// The time now in whole Unix seconds.
fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        // The clock is set to a time before 1970.
        Err(error) => i64::try_from(error.duration().as_secs()).map_or(i64::MIN, |before| -before),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::users;

    const ARTIST: &str = "ar-0123456789abcdef0123456789abcdef";
    const ALBUM: &str = "al-0123456789abcdef0123456789abcdef";
    const TRACK: &str = "tr-0123456789abcdef0123456789abcdef";
    const OTHER_TRACK: &str = "tr-fedcba9876543210fedcba9876543210";
    const PLAYLIST: &str = "pl-0123456789abcdef0123456789abcdef";

    fn params(query: &str) -> Params {
        Params::from_query(query)
    }

    /// The values that have the shape of no ID of the kind `own`, which is
    /// an ID of that kind.
    fn other_shapes(own: &str) -> Vec<String> {
        let mut values: Vec<String> = [
            "",
            "1",
            "pl-",
            "tr-0123",
            "0123456789abcdef0123456789abcdef",
            "tr_0123456789abcdef0123456789abcdef",
            "tr-0123456789abcdef0123456789abcdeg",
            "' OR 1=1 --",
        ]
        .map(str::to_owned)
        .into();
        values.push(own.to_uppercase());
        values.push(format!("{own}0"));
        values.push(format!(" {own}"));
        values
    }

    #[test]
    fn playlist_id_takes_the_id_of_a_playlist_and_no_other_value() {
        assert_eq!(playlist_id(PLAYLIST), Ok(PLAYLIST.to_owned()));
        let mut values = other_shapes(PLAYLIST);
        values.extend([ARTIST, ALBUM, TRACK].map(str::to_owned));
        for value in values {
            assert_eq!(playlist_id(&value), Err(ApiError::NotFound), "{value:?}");
        }
    }

    #[test]
    fn song_ids_returns_the_values_in_the_order_of_the_request() {
        let query = format!("songId={TRACK}&name=x&songId={OTHER_TRACK}&songId={TRACK}");
        assert_eq!(
            song_ids(&params(&query), "songId"),
            Ok(vec![
                TRACK.to_owned(),
                OTHER_TRACK.to_owned(),
                TRACK.to_owned()
            ])
        );
        // The values of another parameter are not read.
        assert_eq!(song_ids(&params(&query), "songIdToAdd"), Ok(Vec::new()));
        assert_eq!(song_ids(&params(""), "songId"), Ok(Vec::new()));
    }

    #[test]
    fn song_ids_with_a_value_that_is_no_track_id_is_error_70() {
        let mut values = other_shapes(TRACK);
        values.extend([ARTIST, ALBUM, PLAYLIST].map(str::to_owned));
        for value in values {
            // The value is the last of three, so the valid ones do not hide
            // it.
            let mut request = params(&format!("songId={TRACK}&songId={OTHER_TRACK}"));
            request.append_form(format!("songId={}", value.replace(' ', "+")).as_bytes());
            assert_eq!(
                song_ids(&request, "songId"),
                Err(ApiError::NotFound),
                "{value:?}"
            );
        }
    }

    #[test]
    fn song_ids_counts_the_values_before_it_reads_them() {
        let limit = format!("songId={TRACK}&").repeat(MAX_ENTRIES);
        assert_eq!(
            song_ids(&params(&limit), "songId").map(|ids| ids.len()),
            Ok(MAX_ENTRIES)
        );

        // One value more is error 0. The last value is no ID, which is
        // error 70 in a shorter list: the number is checked first.
        let too_long = ApiError::Other("a playlist holds at most 100000 songs".to_owned());
        let query = format!("{limit}songId=x");
        assert_eq!(song_ids(&params(&query), "songId"), Err(too_long));
        assert_eq!(
            song_ids(&params(&format!("songId={TRACK}&songId=x")), "songId"),
            Err(ApiError::NotFound)
        );
    }

    #[test]
    fn positions_returns_the_integers_of_0_or_more_in_the_order_of_the_request() {
        let read = |query: &str| positions(&params(query));
        assert_eq!(read(""), Ok(Vec::new()));
        assert_eq!(read("songIndexToRemove=0"), Ok(vec![0]));
        assert_eq!(
            read("songIndexToRemove=7&songIdToAdd=x&songIndexToRemove=0&songIndexToRemove=7"),
            Ok(vec![7, 0, 7])
        );
        // An empty value counts as absent.
        assert_eq!(
            read("songIndexToRemove=&songIndexToRemove=3&songIndexToRemove"),
            Ok(vec![3])
        );

        let no_position = ApiError::Other(NO_POSITION.to_owned());
        for value in [
            "-1",
            "1.5",
            "two",
            "0x1",
            "1e3",
            "%201",
            "99999999999999999999",
        ] {
            let query = format!("songIndexToRemove=0&songIndexToRemove={value}");
            assert_eq!(read(&query), Err(no_position.clone()), "{value}");
        }
    }

    #[test]
    fn a_refusal_is_error_70_for_an_item_and_error_0_for_a_rule() {
        assert_eq!(refused(Refusal::NoPlaylist), ApiError::NotFound);
        assert_eq!(refused(Refusal::NoTrack), ApiError::NotFound);
        let position = refused(Refusal::NoPosition);
        assert_eq!(
            (position.code(), position.message()),
            (0, NO_POSITION.to_owned())
        );
        let length = refused(Refusal::TooLong);
        assert_eq!(
            (length.code(), length.message()),
            (0, format!("a playlist holds at most {MAX_ENTRIES} songs"))
        );
    }

    #[tokio::test]
    async fn read_gives_every_statement_one_state_of_the_database() {
        // A file database has read connections of their own, so a write can
        // commit while a read is in progress.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");
        let owner = db
            .write(|conn| users::insert_owner(conn, "owner", "sesame"))
            .await
            .expect("insert the owner")
            .expect("no owner yet")
            .id;
        let count = move |conn: &Connection| Ok(playlists::list(conn, owner)?.len());

        let writer = db.clone();
        let (before, after_the_write) = read(&db, move |conn| {
            let before = count(conn)?;
            let no_songs: [&str; 0] = [];
            let created =
                writer.with_writer(|conn| playlists::create(conn, owner, "Mix", &no_songs, 0))?;
            assert!(created.is_ok(), "{created:?}");
            Ok((before, count(conn)?))
        })
        .await
        .expect("read the playlists");
        // The second statement reads the state that the first one read.
        assert_eq!((before, after_the_write), (0, 0));

        // The write was committed: the next read finds the playlist.
        assert_eq!(read(&db, count).await, Ok(1));
    }

    #[tokio::test]
    async fn a_database_error_is_error_0_without_the_statement() {
        let db = Db::open_memory().expect("a memory database");
        let read_result = read(&db, |conn| {
            let count: i64 =
                conn.query_row("SELECT count(*) FROM no_such_table", [], |row| row.get(0))?;
            Ok(count)
        })
        .await;
        assert_eq!(
            read_result,
            Err(ApiError::Other(
                "the server cannot read its playlists".to_owned()
            ))
        );

        let write_result = write(&db, |conn, _now| {
            conn.execute("DELETE FROM no_such_table", [])?;
            Ok(Ok(()))
        })
        .await;
        assert_eq!(
            write_result,
            Err(ApiError::Other(
                "the server cannot store the playlist".to_owned()
            ))
        );
    }
}
