//! The play queue methods: `getPlayQueue` and `savePlayQueue`
//! (`docs/design.md` section 4).
//!
//! A client saves its queue, the current song and the position within that
//! song, and a client on another device reads them to go on from there.
//!
//! # Current song
//!
//! A request with `id` values and without `current` saves a queue with those
//! songs and without a current song. Supersonic sends that request while no
//! song is loaded. `getPlayQueue` returns such a queue with its first song
//! as `current` and with the position 0: the OpenSubsonic API requires a
//! `current` that is one of the songs when the queue has songs, and
//! Navidrome answers in the same way. A queue with songs therefore always
//! has a `current`.
//!
//! # Ownership
//!
//! A user has one play queue. Both methods read and write the queue of the
//! user of the request and no other (`db::queries::play_queue`).
//!
//! # Order
//!
//! The entries of the queue have the order of the `id` values of the request
//! that saved them. A song can be in the queue several times.
//!
//! # Writes
//!
//! `savePlayQueue` replaces the whole queue. It checks every value of the
//! request and writes in one transaction, so a request that is answered with
//! an error changes nothing, and of two requests at the same time one queue
//! is stored whole.
//!
//! # IDs
//!
//! A value of `id` that does not have the shape of a track ID is the ID of
//! no song and is error 70 (`docs/design.md` section 3.3). It does not reach
//! SQL.
//!
//! # Tracks without a file
//!
//! A track without a file keeps its entries and is listed as every other
//! song, without `path` (`docs/design.md` section 3.4).

use axum::extract::State;
use rusqlite::Connection;
use serde::Serialize;

use super::envelope::ApiError;
use super::model::{self, Child};
use super::params::Params;
use super::{ApiResult, AppState, Payload, log_field};
use crate::auth::extract::Authed;
use crate::db::queries::play_queue::{self, MAX_ENTRIES, Outcome, Refusal};
use crate::db::queries::{annotations, browse};
use crate::db::{self, Db};
use crate::ids::{self, Kind};

/// The message of the error for a `current` that is none of the `id` values.
const NO_CURRENT: &str = "parameter current must be one of the values of id";

/// The message of the error for a `position` that is no position.
const NO_POSITION: &str = "parameter position must be an integer from 0 to 9223372036854775807";

/// The `playQueue` element.
///
/// `username`, `changed` and `changedBy` are required by the API, and every
/// answer has them. `current` is absent for a queue without a song. The
/// server sends `position` for every queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayQueue {
    /// The ID of the current song, which is one of the songs of `entry`.
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,
    /// The position within the current song in milliseconds.
    position: u64,
    /// The name of the user that owns the queue.
    username: String,
    /// The time of the last request that saved the queue.
    changed: String,
    /// The name of the client that saved the queue.
    changed_by: String,
    /// The songs of the queue in the order of its entries.
    entry: Vec<Child>,
}

/// What a `savePlayQueue` request stores.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Save {
    /// A queue without a song.
    Clear,
    /// A queue with the songs `track_ids`, in that order.
    Queue {
        track_ids: Vec<String>,
        /// The ID of the current song, which is one of `track_ids`.
        current: String,
        /// The position within the current song in milliseconds.
        position_ms: u64,
    },
    /// A queue with the songs `track_ids`, in that order, and without a
    /// current song. `track_ids` holds one ID or more.
    WithoutCurrent { track_ids: Vec<String> },
}

/// `getPlayQueue`: the play queue of the user of the request. The method
/// reads no parameter.
///
/// The answer always holds the `playQueue` element, also for a queue without
/// a song. The OpenSubsonic API requires the element. The Subsonic API
/// 1.16.1 answers a user that saved no queue with the empty response, and a
/// client that reads the fields of the element fails on that answer.
///
/// - `entry` holds the songs in the order of the queue, each with the
///   annotation of the user. A song with two entries is listed twice.
/// - `current` is the ID of the current song, and `position` the position
///   within it in milliseconds. A queue without a song has no `current` and
///   the position 0. A queue that was saved without `current` has its first
///   song as `current` and the position 0.
/// - `changed` is the time of the last request that saved the queue, and
///   `changedBy` the name of its client. For a user that saved no queue,
///   `changed` is the creation time of the account and `changedBy` is empty.
pub async fn get_play_queue(State(state): State<AppState>, Authed(user): Authed) -> ApiResult {
    let queue = read(&state.db, move |conn| read_queue(conn, user.id))
        .await?
        // The account of the request has no row. No request deletes one.
        .ok_or(ApiError::NotFound)?;
    Ok(Payload::new("playQueue", queue))
}

/// `savePlayQueue`: replaces the play queue of the user of the request and
/// answers with the empty response.
///
/// - `id` may occur any number of times. The songs get the order of the
///   values, and a song that is named twice is in the queue twice.
/// - A request without `id` and without `current` stores a queue without a
///   song, as the OpenSubsonic API defines it. The Subsonic API 1.16.1
///   requires `id`.
/// - `current` must be one of the `id` values. Another value is error 0,
///   also in a request without `id`. An empty value counts as absent.
/// - A request with an `id` and without `current` stores a queue with the
///   songs and without a current song, and `getPlayQueue` returns its first
///   song as `current`. The OpenSubsonic API requires `current` in that
///   request, and Supersonic sends none while no song is loaded.
/// - `position` is the position within the current song in milliseconds, and
///   0 without a value. A value that is no integer of 0 or more is error 0.
///   A request without `current` stores the position 0.
/// - A value of `id` that is the ID of no song is error 70. More than
///   [`MAX_ENTRIES`] values are error 0.
///
/// `c`, the name of the client, is stored as `changedBy`, cut to the 64
/// characters that the request log holds of it. The time of the request is
/// stored as `changed`.
///
/// A request that is answered with an error changes nothing: the songs, the
/// current song, the position, `changed` and `changedBy` stay.
pub async fn save_play_queue(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let save = save_request(&params)?;
    let client = log_field(params.req_str("c")?).to_owned();
    write(&state.db, move |conn, now| match save {
        Save::Clear => play_queue::clear(conn, user.id, &client, now).map(Ok),
        Save::Queue {
            track_ids,
            current,
            position_ms,
        } => play_queue::replace(
            conn,
            user.id,
            &track_ids,
            &current,
            position_ms,
            &client,
            now,
        ),
        Save::WithoutCurrent { track_ids } => {
            play_queue::replace_without_current(conn, user.id, &track_ids, &client, now)
        }
    })
    .await?;
    Ok(Payload::empty())
}

/// Returns what the request `params` stores. The checks run in this order:
///
/// 1. More than [`MAX_ENTRIES`] values of `id` are error 0. The values are
///    counted before they are read, so a request cannot make the server
///    check more of them.
/// 2. A value of `id` without the shape of a track ID, an empty one
///    included, is the ID of no song and is error 70.
/// 3. A `current` that is none of the `id` values is error 0. That includes
///    a `current` in a request without `id`. An empty `current` counts as
///    absent.
/// 4. A `position` that is no integer from 0 to `i64::MAX` is error 0.
///
/// A request without `current` stores a queue without a current song: one
/// without a song when the request has no `id`, and one with the songs of
/// the `id` values otherwise. Its `position` is checked and not stored.
fn save_request(params: &Params) -> Result<Save, ApiError> {
    let values = params.all("id");
    let current = params.opt_str("current").filter(|id| !id.is_empty());
    if values.len() > MAX_ENTRIES {
        return Err(refused(Refusal::TooLong));
    }
    let track_ids = values
        .into_iter()
        .map(|value| match ids::parse(value) {
            Some((Kind::Track, _)) => Ok(value.to_owned()),
            _ => Err(ApiError::NotFound),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(current) = current
        && !track_ids.iter().any(|id| id == current)
    {
        return Err(refused(Refusal::NoCurrent));
    }
    let position_ms = position(params)?;
    Ok(match current {
        Some(current) => Save::Queue {
            track_ids,
            current: current.to_owned(),
            position_ms,
        },
        None if track_ids.is_empty() => Save::Clear,
        None => Save::WithoutCurrent { track_ids },
    })
}

/// Returns the value of `position` in milliseconds, and 0 for an absent or
/// empty value. A value that is no integer from 0 to `i64::MAX`, which is
/// the largest integer that the database stores, is error 0.
fn position(params: &Params) -> Result<u64, ApiError> {
    match params.opt_str("position") {
        None | Some("") => Ok(0),
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|position| i64::try_from(*position).is_ok())
            .ok_or_else(|| ApiError::Other(NO_POSITION.to_owned())),
    }
}

/// The error that answers a request that the queries refused.
///
/// The ID of no song is error 70. A queue that would be too long and a
/// current song that is not in the queue are error 0 with a message that
/// names the rule.
fn refused(refusal: Refusal) -> ApiError {
    match refusal {
        Refusal::NoTrack => ApiError::NotFound,
        Refusal::NoCurrent => ApiError::Other(NO_CURRENT.to_owned()),
        Refusal::TooLong => {
            ApiError::Other(format!("a play queue holds at most {MAX_ENTRIES} songs"))
        }
    }
}

/// Reads the play queue of the user `user_id` with its songs and returns its
/// element, or `None` when no user has the ID.
///
/// The songs are in the order of the entries, and a song with two entries is
/// in the list twice. Each carries the annotation of the user.
///
/// The caller runs the function in one transaction, so `current` is the ID
/// of a song of the list.
fn read_queue(conn: &Connection, user_id: i64) -> db::Result<Option<PlayQueue>> {
    let Some(queue) = play_queue::get(conn, user_id)? else {
        return Ok(None);
    };
    let songs = browse::tracks_by_ids(conn, &queue.track_ids)?;
    let annotations = annotations::get_many(conn, user_id, &queue.track_ids)?;
    let entry = songs
        .iter()
        .map(|song| model::child(song, annotations.get(&song.track.id)))
        .collect();
    Ok(Some(PlayQueue {
        current: queue.current,
        position: queue.position_ms,
        username: queue.username,
        changed: model::iso8601(queue.changed_at),
        changed_by: queue.changed_by,
        entry,
    }))
}

/// Runs `read` with a read connection of `db` inside one read transaction
/// ([`Db::read_snapshot`]), so every statement of an answer reads one state
/// of the database. A database that cannot be read is error 0.
async fn read<T, F>(db: &Db, read: F) -> Result<T, ApiError>
where
    F: FnOnce(&Connection) -> db::Result<T> + Send + 'static,
    T: Send + 'static,
{
    db.read_snapshot(read).await.map_err(|error| {
        tracing::error!(%error, "cannot read the play queue");
        ApiError::Other("the server cannot read its play queue".to_owned())
    })
}

/// Runs `write` with the write connection of `db` and the time of the write
/// in Unix seconds ([`Db::write_at`]). A request that the queries refused is
/// answered with the error of [`refused`], and a database that cannot be
/// written with error 0.
///
/// The time is read once the write connection is held. While the clock runs
/// forward, the request that writes later therefore does not store the
/// earlier time.
async fn write<T, F>(db: &Db, write: F) -> Result<T, ApiError>
where
    F: FnOnce(&mut Connection, i64) -> db::Result<Outcome<T>> + Send + 'static,
    T: Send + 'static,
{
    db.write_at(write)
        .await
        .map_err(|error| {
            tracing::error!(%error, "cannot write the play queue");
            ApiError::Other("the server cannot store the play queue".to_owned())
        })?
        .map_err(refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::queries::users;

    const ARTIST: &str = "ar-0123456789abcdef0123456789abcdef";
    const ALBUM: &str = "al-0123456789abcdef0123456789abcdef";
    const PLAYLIST: &str = "pl-0123456789abcdef0123456789abcdef";
    const TRACK: &str = "tr-0123456789abcdef0123456789abcdef";
    const OTHER_TRACK: &str = "tr-fedcba9876543210fedcba9876543210";

    fn save(query: &str) -> Result<Save, ApiError> {
        save_request(&Params::from_query(query))
    }

    fn queue(track_ids: &[&str], current: &str, position_ms: u64) -> Result<Save, ApiError> {
        Ok(Save::Queue {
            track_ids: track_ids.iter().map(|id| (*id).to_owned()).collect(),
            current: current.to_owned(),
            position_ms,
        })
    }

    fn without_current(track_ids: &[&str]) -> Result<Save, ApiError> {
        Ok(Save::WithoutCurrent {
            track_ids: track_ids.iter().map(|id| (*id).to_owned()).collect(),
        })
    }

    fn no_current() -> Result<Save, ApiError> {
        Err(ApiError::Other(NO_CURRENT.to_owned()))
    }

    fn no_position() -> Result<Save, ApiError> {
        Err(ApiError::Other(NO_POSITION.to_owned()))
    }

    /// The values that have the shape of no track ID.
    fn other_shapes() -> Vec<String> {
        let mut values: Vec<String> = [
            "",
            "1",
            "tr-",
            "tr-0123",
            "0123456789abcdef0123456789abcdef",
            "tr_0123456789abcdef0123456789abcdef",
            "tr-0123456789abcdef0123456789abcdeg",
            "' OR 1=1 --",
            ARTIST,
            ALBUM,
            PLAYLIST,
        ]
        .map(str::to_owned)
        .into();
        values.push(TRACK.to_uppercase());
        values.push(format!("{TRACK}0"));
        values.push(format!("%20{TRACK}"));
        values
    }

    #[test]
    fn a_request_stores_the_songs_in_the_order_of_the_id_values() {
        let query = format!("id={TRACK}&current={OTHER_TRACK}&id={OTHER_TRACK}&id={TRACK}");
        assert_eq!(
            save(&query),
            queue(&[TRACK, OTHER_TRACK, TRACK], OTHER_TRACK, 0)
        );

        let query = format!("id={TRACK}&current={TRACK}&position=12345");
        assert_eq!(save(&query), queue(&[TRACK], TRACK, 12_345));
        // The first value of `current` and of `position` is the one read.
        let query =
            format!("id={TRACK}&id={OTHER_TRACK}&current={TRACK}&current=x&position=7&position=x");
        assert_eq!(save(&query), queue(&[TRACK, OTHER_TRACK], TRACK, 7));
    }

    #[test]
    fn a_request_without_id_and_without_current_stores_a_queue_without_a_song() {
        assert_eq!(save(""), Ok(Save::Clear));
        // An empty `current` counts as absent, and the position is not
        // stored.
        assert_eq!(save("current="), Ok(Save::Clear));
        assert_eq!(save("position=5000"), Ok(Save::Clear));
        assert_eq!(save("current=&position="), Ok(Save::Clear));
        // The values of another parameter are not read.
        assert_eq!(save(&format!("songId={TRACK}")), Ok(Save::Clear));

        // The position is checked.
        assert_eq!(save("position=x"), no_position());
    }

    #[test]
    fn a_request_with_an_id_and_without_current_stores_a_queue_without_a_current_song() {
        assert_eq!(save(&format!("id={TRACK}")), without_current(&[TRACK]));
        // The songs have the order of the `id` values, and a song that is
        // named twice is in the queue twice.
        let query = format!("id={OTHER_TRACK}&id={TRACK}&id={OTHER_TRACK}");
        assert_eq!(
            save(&query),
            without_current(&[OTHER_TRACK, TRACK, OTHER_TRACK])
        );
        // An empty `current` counts as absent.
        assert_eq!(
            save(&format!("id={TRACK}&current=")),
            without_current(&[TRACK])
        );
        // The position is checked and not stored.
        assert_eq!(
            save(&format!("id={TRACK}&id={OTHER_TRACK}&position=5")),
            without_current(&[TRACK, OTHER_TRACK])
        );
        assert_eq!(save(&format!("id={TRACK}&position=x")), no_position());

        // An `id` that is no track ID is error 70 without `current` as well.
        for value in other_shapes() {
            let query = format!("id={TRACK}&id={value}");
            assert_eq!(save(&query), Err(ApiError::NotFound), "{value:?}");
        }
    }

    #[test]
    fn current_must_be_one_of_the_id_values() {
        let mut values = other_shapes();
        values.retain(|value| !value.is_empty());
        values.push(OTHER_TRACK.to_owned());
        for current in values {
            let query = format!("id={TRACK}&id={TRACK}&current={current}");
            assert_eq!(save(&query), no_current(), "{current:?}");
            // A request without `id` holds no song that `current` can name.
            assert_eq!(save(&format!("current={current}")), no_current());
        }
    }

    #[test]
    fn an_id_that_is_no_track_id_is_error_70() {
        for value in other_shapes() {
            // The value is the last of three, so the valid ones do not hide
            // it.
            let query = format!("id={TRACK}&id={OTHER_TRACK}&id={value}&current={TRACK}");
            assert_eq!(save(&query), Err(ApiError::NotFound), "{value:?}");
            let query = format!("id={value}&id={TRACK}&current={TRACK}");
            assert_eq!(save(&query), Err(ApiError::NotFound), "{value:?}");
        }
    }

    #[test]
    fn position_is_an_integer_from_0_to_the_largest_stored_integer() {
        let with =
            |position: &str| save(&format!("id={TRACK}&current={TRACK}&position={position}"));
        assert_eq!(with(""), queue(&[TRACK], TRACK, 0));
        assert_eq!(with("0"), queue(&[TRACK], TRACK, 0));
        assert_eq!(with("12345"), queue(&[TRACK], TRACK, 12_345));
        assert_eq!(
            with("9223372036854775807"),
            queue(&[TRACK], TRACK, i64::MAX as u64)
        );
        for position in [
            "-1",
            "1.5",
            "1e3",
            "0x10",
            "soon",
            "%201",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            assert_eq!(with(position), no_position(), "{position}");
        }
    }

    #[test]
    fn the_id_values_are_counted_before_they_are_read() {
        let limit = format!("id={TRACK}&").repeat(MAX_ENTRIES);
        let saved = save(&format!("{limit}current={TRACK}"));
        assert!(
            matches!(&saved, Ok(Save::Queue { track_ids, .. }) if track_ids.len() == MAX_ENTRIES),
            "the request is accepted"
        );

        // One value more is error 0. The last value is no ID, which is
        // error 70 in a shorter list: the number is checked first.
        let too_long = Err(ApiError::Other(
            "a play queue holds at most 100000 songs".to_owned(),
        ));
        assert_eq!(save(&format!("{limit}id=x&current={TRACK}")), too_long);
        assert_eq!(
            save(&format!("id={TRACK}&id=x&current={TRACK}")),
            Err(ApiError::NotFound)
        );
    }

    #[test]
    fn the_checks_of_a_request_run_in_one_order() {
        // The number of the values is reported before an `id` that is no
        // track ID, also in a request without `current`.
        let too_many = format!("id={TRACK}&").repeat(MAX_ENTRIES);
        let too_long = Err(ApiError::Other(
            "a play queue holds at most 100000 songs".to_owned(),
        ));
        assert_eq!(save(&format!("{too_many}id=x")), too_long);
        // An `id` that is no track ID is reported before a `position` that
        // is no integer, in a request without `current`.
        assert_eq!(
            save(&format!("id={TRACK}&id=x&position=x")),
            Err(ApiError::NotFound)
        );
        // An `id` that is no track ID is reported before a `current` that is
        // none of the values, and that before a `position` that is no
        // integer.
        let query = format!("id={TRACK}&id=x&current={OTHER_TRACK}&position=x");
        assert_eq!(save(&query), Err(ApiError::NotFound));
        let query = format!("id={TRACK}&current={OTHER_TRACK}&position=x");
        assert_eq!(save(&query), no_current());
        let query = format!("id={TRACK}&current={TRACK}&position=x");
        assert_eq!(save(&query), no_position());
    }

    #[test]
    fn a_refusal_is_error_70_for_a_song_and_error_0_for_a_rule() {
        assert_eq!(refused(Refusal::NoTrack), ApiError::NotFound);
        let current = refused(Refusal::NoCurrent);
        assert_eq!(
            (current.code(), current.message()),
            (0, NO_CURRENT.to_owned())
        );
        let length = refused(Refusal::TooLong);
        assert_eq!(
            (length.code(), length.message()),
            (0, format!("a play queue holds at most {MAX_ENTRIES} songs"))
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
        let client = move |conn: &Connection| {
            let queue = play_queue::get(conn, owner)?.expect("the owner has a row");
            Ok(queue.changed_by)
        };

        let writer = db.clone();
        let (before, after_the_write) = read(&db, move |conn| {
            let before = client(conn)?;
            writer.with_writer(|conn| play_queue::clear(conn, owner, "Phone", 0))?;
            Ok((before, client(conn)?))
        })
        .await
        .expect("read the queue");
        // The second statement reads the state that the first one read.
        assert_eq!((before.as_str(), after_the_write.as_str()), ("", ""));

        // The write was committed: the next read finds its queue.
        assert_eq!(read(&db, client).await, Ok("Phone".to_owned()));
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
                "the server cannot read its play queue".to_owned()
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
                "the server cannot store the play queue".to_owned()
            ))
        );
    }
}
