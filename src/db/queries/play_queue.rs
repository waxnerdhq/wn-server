//! Queries on the `play_queue` and `play_queue_tracks` tables
//! (`docs/design.md` section 3.4).
//!
//! # Ownership
//!
//! A user has one play queue. Every function takes the ID of a user and
//! reads or writes the rows of that user only, so no user reads or replaces
//! the queue of another user.
//!
//! # Entries and positions
//!
//! An entry is a row of `play_queue_tracks`: one track at one position of
//! the queue of one user. A track can have several entries. The entries of a
//! queue are its rows in the order of `position`.
//!
//! [`replace`] stores the entry of the current song at position 0, the
//! entries before it at -1, -2 and so on, and the entries after it at 1, 2
//! and so on. A request names the current song by its ID. When the queue
//! holds that song several times, its first entry is the one at position 0.
//!
//! The deletion of a track row deletes the entries of the track (`ON DELETE
//! CASCADE`) and writes no other row. The entries that are left keep their
//! positions and so their order. `play_queue.current_id` has no foreign key
//! and keeps the ID of a deleted track. [`get`] then reports the entry that
//! followed the deleted one as the current song, which the stored positions
//! name: it is the first entry at a position above 0.
//!
//! # Writes
//!
//! [`replace`] and [`clear`] each run in one `IMMEDIATE` transaction.
//! [`replace`] checks the whole request before its first write. A request
//! that it refuses ([`Refusal`]) and a write that fails change nothing, and
//! a reader finds the queue of one request and never rows of two.
//!
//! Every value of a request is a parameter of a statement. The list of track
//! IDs is one parameter, a JSON array that the statement reads with
//! `json_each`.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::browse::json_array;
use super::int;
use crate::db::Result;

/// The largest number of entries of a play queue. A request with more track
/// IDs is refused with [`Refusal::TooLong`].
///
/// The limit bounds the rows that one request writes and the rows that one
/// request reads. A list of this many track IDs is a form body of about
/// 4.4 MB, which the form body limit of the router admits
/// (`api::FORM_BODY_LIMIT`).
pub const MAX_ENTRIES: usize = 100_000;

/// The play queue of one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    /// The name of the user that owns the queue.
    pub username: String,
    /// The track ID of each entry, in the order of the entries. A track with
    /// two entries is in the list twice.
    pub track_ids: Vec<String>,
    /// The ID of the track of the current song, which is one of `track_ids`.
    /// `None` for a queue without an entry.
    pub current: Option<String>,
    /// The position within the current song in milliseconds. 0 for a queue
    /// without an entry.
    pub position_ms: u64,
    /// The time of the last request that stored the queue, in Unix seconds.
    /// The deletion of a track row does not change it. For a user that
    /// stored no queue it is the creation time of the account.
    pub changed_at: i64,
    /// The name of the client that stored the queue. Empty for a user that
    /// stored no queue.
    pub changed_by: String,
}

/// Why [`replace`] changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The request holds more than [`MAX_ENTRIES`] track IDs.
    TooLong,
    /// The current song is none of the track IDs of the request.
    NoCurrent,
    /// An ID of the request is the ID of no track row.
    NoTrack,
}

/// What [`replace`] returns when the database answered: nothing, or the
/// reason for which nothing was written.
pub type Outcome<T> = std::result::Result<T, Refusal>;

/// Returns the play queue of the user `user_id`, or `None` when no user has
/// the ID.
///
/// A user that stored no queue has a queue without an entry. Its change time
/// is the creation time of the account, and the name of its client is empty.
///
/// The current song is the one that the last [`replace`] stored, with its
/// position, while the queue holds an entry of that track. After the row of
/// that track was deleted, the current song is the track of the entry that
/// followed it, and the track of the last entry when none followed. The
/// position is then 0, because the stored one is a position within the
/// deleted track.
///
/// The function runs two statements. The caller runs it in one transaction,
/// so both read one state of the database.
pub fn get(conn: &Connection, user_id: i64) -> Result<Option<Queue>> {
    let head = conn
        .prepare_cached(
            "SELECT users.name, play_queue.current_id, coalesce(play_queue.position_ms, 0),
               coalesce(play_queue.changed_at, users.created_at),
               coalesce(play_queue.changed_by, '')
             FROM users LEFT JOIN play_queue ON play_queue.user_id = users.id
             WHERE users.id = ?1",
        )?
        .query_row([user_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                int::get_u64(row, 2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .optional()?;
    let Some((username, stored_current, stored_position_ms, changed_at, changed_by)) = head else {
        return Ok(None);
    };
    let entries = stored_entries(conn, user_id)?;
    let (current, position_ms) = current_song(stored_current, stored_position_ms, &entries);
    Ok(Some(Queue {
        username,
        track_ids: entries.into_iter().map(|(_, track_id)| track_id).collect(),
        current,
        position_ms,
        changed_at,
        changed_by,
    }))
}

/// Replaces the play queue of the user `user_id` with one entry for each ID
/// of `track_ids`, in that order. `current` is the ID of the current song,
/// `position_ms` the position within it in milliseconds, `client` the name
/// of the client that sends the request, and `at` the time of the request in
/// Unix seconds.
///
/// The request is refused, and the stored queue stays as it is, in these
/// cases, which are checked in this order:
///
/// - [`Refusal::TooLong`]: `track_ids` holds more than [`MAX_ENTRIES`] IDs.
/// - [`Refusal::NoCurrent`]: `current` is none of `track_ids`. That includes
///   a request without a track ID.
/// - [`Refusal::NoTrack`]: an ID of `track_ids` is the ID of no track row.
pub fn replace<S: AsRef<str>>(
    conn: &mut Connection,
    user_id: i64,
    track_ids: &[S],
    current: &str,
    position_ms: u64,
    client: &str,
    at: i64,
) -> Result<Outcome<()>> {
    if track_ids.len() > MAX_ENTRIES {
        return Ok(Err(Refusal::TooLong));
    }
    let Some(current_place) = track_ids.iter().position(|id| id.as_ref() == current) else {
        return Ok(Err(Refusal::NoCurrent));
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !tracks_exist(&tx, track_ids)? {
        return Ok(Err(Refusal::NoTrack));
    }
    store_head(&tx, user_id, Some(current), position_ms, client, at)?;
    delete_entries(&tx, user_id)?;
    // `key` is the index of an item in the JSON array, counted from 0, so
    // the entry of the current song gets position 0.
    tx.prepare_cached(
        "INSERT INTO play_queue_tracks (user_id, position, track_id)
         SELECT ?1, key - ?2, value FROM json_each(?3)",
    )?
    .execute(params![
        user_id,
        int::to_sql(current_place as u64)?,
        json_array(track_ids)
    ])?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Replaces the play queue of the user `user_id` with a queue without an
/// entry. `client` is the name of the client that sends the request, and
/// `at` the time of the request in Unix seconds. The queue then has no
/// current song and the position 0.
pub fn clear(conn: &mut Connection, user_id: i64, client: &str, at: i64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    store_head(&tx, user_id, None, 0, client, at)?;
    delete_entries(&tx, user_id)?;
    tx.commit()?;
    Ok(())
}

/// Returns the current song of a queue and the position within it in
/// milliseconds. `stored` and `position_ms` are the values of the
/// `play_queue` row, and `entries` the stored position and the track ID of
/// each entry in the order of the positions.
///
/// - While an entry holds the track `stored`, the result is the stored
///   values.
/// - Otherwise the row of that track was deleted. The result is the track of
///   the first entry at a position above 0, which is the entry that followed
///   the deleted one, and the track of the last entry when none followed.
///   The position is 0.
/// - A queue without an entry has no current song, and the position is 0.
fn current_song(
    stored: Option<String>,
    position_ms: u64,
    entries: &[(i64, String)],
) -> (Option<String>, u64) {
    let queued = |id: &String| entries.iter().any(|(_, track_id)| track_id == id);
    if let Some(id) = stored.filter(queued) {
        return (Some(id), position_ms);
    }
    let next = entries
        .iter()
        .find(|(position, _)| *position > 0)
        .or(entries.last());
    (next.map(|(_, track_id)| track_id.clone()), 0)
}

/// Returns the stored position and the track ID of each entry of the queue
/// of the user `user_id`, in the order of the positions.
fn stored_entries(conn: &Connection, user_id: i64) -> Result<Vec<(i64, String)>> {
    let mut select = conn.prepare_cached(
        "SELECT position, track_id FROM play_queue_tracks WHERE user_id = ?1 ORDER BY position",
    )?;
    let entries = select
        .query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(entries)
}

/// Whether every ID of `track_ids` is the ID of a track row. A track without
/// a file has a row (`docs/design.md` section 3.4).
fn tracks_exist<S: AsRef<str>>(conn: &Connection, track_ids: &[S]) -> Result<bool> {
    let exist = conn
        .prepare_cached(
            "SELECT NOT EXISTS (
               SELECT 1 FROM json_each(?1)
               WHERE NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.id = json_each.value))",
        )?
        .query_row([json_array(track_ids)], |row| row.get(0))?;
    Ok(exist)
}

/// Stores the `play_queue` row of the user `user_id`: it inserts the row, or
/// it replaces the four values of the row that the user has. The entries of
/// the queue refer to the row, and they stay.
fn store_head(
    conn: &Connection,
    user_id: i64,
    current: Option<&str>,
    position_ms: u64,
    client: &str,
    at: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO play_queue (user_id, current_id, position_ms, changed_at, changed_by)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (user_id) DO UPDATE SET
           current_id = excluded.current_id,
           position_ms = excluded.position_ms,
           changed_at = excluded.changed_at,
           changed_by = excluded.changed_by",
    )?
    .execute(params![
        user_id,
        current,
        int::to_sql(position_ms)?,
        at,
        client
    ])?;
    Ok(())
}

/// Deletes every entry of the queue of the user `user_id`.
fn delete_entries(conn: &Connection, user_id: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM play_queue_tracks WHERE user_id = ?1")?
        .execute([user_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::TrackContent;
    use crate::db::queries::{albums, artists, folders, tracks, users};
    use crate::library::tags::TrackMeta;

    const T1: &str = "tr-00000000000000000000000000000001";
    const T2: &str = "tr-00000000000000000000000000000002";
    const T3: &str = "tr-00000000000000000000000000000003";
    const T4: &str = "tr-00000000000000000000000000000004";
    const UNKNOWN_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";

    /// The creation time of the account `guest`.
    const GUEST_CREATED: i64 = 1_600_000_000;

    /// The time of the first request of a test.
    const NOW: i64 = 1_700_000_000;

    /// The time of a later request.
    const LATER: i64 = 1_700_000_600;

    /// A database with two users and four tracks.
    struct Library {
        db: Db,
        /// The ID of the owner account `owner`.
        owner: i64,
        /// The ID of a second account, `guest`.
        guest: i64,
    }

    /// Fills `db` with the owner, a second user and the tracks [`T1`] to
    /// [`T4`] of one album. The server has no function that inserts a second
    /// user, so the test inserts the row.
    fn library_in(db: Db) -> Library {
        let (owner, guest) = db
            .with_writer(|conn| {
                let owner = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
                let guest = conn.query_row(
                    "INSERT INTO users (name, role, password, created_at, updated_at)
                     VALUES ('guest', 'guest', 'secret', ?1, ?1) RETURNING id",
                    [GUEST_CREATED],
                    |row| row.get(0),
                )?;
                let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?.remove(0);
                let artist_id = artists::upsert_by_key(conn, "Aurora Field")?;
                let album_id = albums::upsert_by_key(
                    conn,
                    &artist_id,
                    "Glass Harbour",
                    "Aurora Field",
                    false,
                )?;
                for id in [T1, T2, T3, T4] {
                    let meta = TrackMeta {
                        title: id.to_owned(),
                        artist: "Aurora Field".to_owned(),
                        album_artist: "Aurora Field".to_owned(),
                        album: "Glass Harbour".to_owned(),
                        track_no: None,
                        disc_no: None,
                        year: None,
                        genre: None,
                        compilation: false,
                        duration_ms: 1_000,
                        bitrate_kbps: None,
                        sample_rate: None,
                        bit_depth: None,
                        channels: None,
                        has_picture: false,
                        has_lyrics: false,
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
                    tracks::insert_track(conn, id, folder.id, &format!("{id}.flac"), &content)?;
                }
                Ok((owner.id, guest))
            })
            .unwrap();
        Library { db, owner, guest }
    }

    /// A memory database with the rows of [`library_in`].
    fn library() -> Library {
        library_in(Db::open_memory().unwrap())
    }

    impl Library {
        /// Stores a queue of `user` with `track_ids`, the current song
        /// `current` and the position 0, as the client `client` at [`NOW`].
        fn save(&self, user: i64, track_ids: &[&str], current: &str, client: &str) -> Outcome<()> {
            self.db
                .with_writer(|conn| replace(conn, user, track_ids, current, 0, client, NOW))
                .unwrap()
        }

        fn get(&self, user: i64) -> Queue {
            self.db
                .with_reader(|conn| get(conn, user))
                .unwrap()
                .expect("the user has a row")
        }

        /// The stored position and the track ID of each entry of the queue
        /// of `user`, in the order of the positions.
        fn entries(&self, user: i64) -> Vec<(i64, String)> {
            self.db
                .with_reader(|conn| stored_entries(conn, user))
                .unwrap()
        }

        /// The `play_queue` row of `user`: the current song, the position,
        /// the change time and the client. `None` for a user without a row.
        fn head(&self, user: i64) -> Option<(Option<String>, i64, i64, String)> {
            self.db
                .with_reader(|conn| {
                    let head = conn
                        .query_row(
                            "SELECT current_id, position_ms, changed_at, changed_by
                             FROM play_queue WHERE user_id = ?1",
                            [user],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .optional()?;
                    Ok(head)
                })
                .unwrap()
        }

        /// Every value of the rows of `play_queue` and `play_queue_tracks`,
        /// as text. Two equal results are two equal states of the queues.
        fn everything(&self) -> Vec<String> {
            self.db
                .with_reader(|conn| {
                    let mut rows = Vec::new();
                    for sql in [
                        "SELECT format('%d|%s|%d|%d|%s', user_id, quote(current_id),
                           position_ms, changed_at, changed_by)
                         FROM play_queue ORDER BY user_id",
                        "SELECT format('%d|%d|%s', user_id, position, track_id)
                         FROM play_queue_tracks ORDER BY user_id, position",
                    ] {
                        let mut select = conn.prepare(sql)?;
                        let texts = select.query_map([], |row| row.get(0))?;
                        rows.extend(texts.collect::<rusqlite::Result<Vec<String>>>()?);
                    }
                    Ok(rows)
                })
                .unwrap()
        }

        /// Deletes the track rows `ids`, as a scan does.
        fn delete_tracks(&self, ids: &[&str]) {
            let deleted = self
                .db
                .with_writer(|conn| tracks::delete_tracks(conn, ids))
                .unwrap();
            assert_eq!(deleted, ids.len() as u64);
        }
    }

    /// The entries of a queue that holds `track_ids` with the song at the
    /// place `current_place` as its current song.
    fn around(current_place: i64, track_ids: &[&str]) -> Vec<(i64, String)> {
        (-current_place..)
            .zip(track_ids.iter().map(|id| (*id).to_owned()))
            .collect()
    }

    fn list_of(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_user_that_stored_no_queue_has_a_queue_without_an_entry() {
        let library = library();
        let created_at: i64 = library
            .db
            .with_reader(|conn| {
                let sql = "SELECT created_at FROM users WHERE id = ?1";
                Ok(conn.query_row(sql, [library.owner], |row| row.get(0))?)
            })
            .unwrap();

        assert_eq!(
            library.get(library.owner),
            Queue {
                username: "owner".to_owned(),
                track_ids: Vec::new(),
                current: None,
                position_ms: 0,
                changed_at: created_at,
                changed_by: String::new(),
            }
        );
        let of_guest = library.get(library.guest);
        assert_eq!(of_guest.username, "guest");
        assert_eq!(of_guest.changed_at, GUEST_CREATED);

        // The read stores nothing.
        assert_eq!(library.everything(), [] as [&str; 0]);
        // An ID that no user has is the ID of no queue.
        let unknown = library.owner + library.guest + 1;
        let queue = library.db.with_reader(|conn| get(conn, unknown)).unwrap();
        assert_eq!(queue, None);
    }

    #[test]
    fn replace_stores_the_entries_in_order_with_the_current_song_at_position_0() {
        let library = library();
        let owner = library.owner;
        let stored = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T3, T1, T3, T2], T1, 12_345, "Tuner", NOW))
            .unwrap();

        assert_eq!(stored, Ok(()));
        assert_eq!(library.entries(owner), around(1, &[T3, T1, T3, T2]));
        assert_eq!(
            library.head(owner),
            Some((Some(T1.to_owned()), 12_345, NOW, "Tuner".to_owned()))
        );
        assert_eq!(
            library.get(owner),
            Queue {
                username: "owner".to_owned(),
                track_ids: list_of(&[T3, T1, T3, T2]),
                current: Some(T1.to_owned()),
                position_ms: 12_345,
                changed_at: NOW,
                changed_by: "Tuner".to_owned(),
            }
        );

        // The queue holds the current song twice: its first entry is the
        // one at position 0.
        assert_eq!(library.save(owner, &[T2, T3, T1, T3], T3, "Tuner"), Ok(()));
        assert_eq!(library.entries(owner), around(1, &[T2, T3, T1, T3]));
        // The last entry and the only entry as the current song.
        assert_eq!(library.save(owner, &[T2, T3, T1], T1, "Tuner"), Ok(()));
        assert_eq!(library.entries(owner), around(2, &[T2, T3, T1]));
        assert_eq!(library.save(owner, &[T4], T4, "Tuner"), Ok(()));
        assert_eq!(library.entries(owner), around(0, &[T4]));
        assert_eq!(library.get(owner).current.as_deref(), Some(T4));
    }

    #[test]
    fn replace_replaces_the_whole_queue() {
        let library = library();
        let owner = library.owner;
        assert_eq!(library.save(owner, &[T1, T2, T3, T4], T4, "Phone"), Ok(()));

        let stored = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T3, T2], T3, 9, "Desktop", LATER))
            .unwrap();
        assert_eq!(stored, Ok(()));

        // No entry and no value of the first queue is left.
        assert_eq!(
            library.everything(),
            [
                format!("{owner}|'{T3}'|9|{LATER}|Desktop"),
                format!("{owner}|0|{T3}"),
                format!("{owner}|1|{T2}"),
            ]
        );
    }

    #[test]
    fn clear_stores_a_queue_without_an_entry() {
        let library = library();
        let Library { owner, guest, .. } = library;
        let stored = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T1, T2], T2, 777, "Phone", NOW))
            .unwrap();
        assert_eq!(stored, Ok(()));

        library
            .db
            .with_writer(|conn| clear(conn, owner, "Desktop", LATER))
            .unwrap();

        // The row stays and holds the request that emptied the queue.
        assert_eq!(library.entries(owner), []);
        assert_eq!(
            library.head(owner),
            Some((None, 0, LATER, "Desktop".to_owned()))
        );
        assert_eq!(
            library.get(owner),
            Queue {
                username: "owner".to_owned(),
                track_ids: Vec::new(),
                current: None,
                position_ms: 0,
                changed_at: LATER,
                changed_by: "Desktop".to_owned(),
            }
        );

        // A user that stored no queue gets the row as well.
        library
            .db
            .with_writer(|conn| clear(conn, guest, "Tablet", NOW))
            .unwrap();
        assert_eq!(
            library.head(guest),
            Some((None, 0, NOW, "Tablet".to_owned()))
        );

        // The next request stores songs again.
        assert_eq!(library.save(owner, &[T3], T3, "Phone"), Ok(()));
        assert_eq!(library.get(owner).track_ids, [T3]);
    }

    #[test]
    fn a_user_neither_reads_nor_replaces_the_queue_of_another_user() {
        let library = library();
        let Library { owner, guest, .. } = library;
        assert_eq!(library.save(owner, &[T1, T2, T3], T2, "Phone"), Ok(()));
        // The other user has no queue yet and reads none.
        assert_eq!(library.get(guest).track_ids, [] as [&str; 0]);
        assert_eq!(library.get(guest).changed_by, "");

        assert_eq!(library.save(guest, &[T4, T1], T4, "Tablet"), Ok(()));

        // Each user reads the queue that this user stored.
        let of_owner = library.get(owner);
        assert_eq!(of_owner.username, "owner");
        assert_eq!(of_owner.track_ids, [T1, T2, T3]);
        assert_eq!(of_owner.current.as_deref(), Some(T2));
        assert_eq!(of_owner.changed_by, "Phone");
        let of_guest = library.get(guest);
        assert_eq!(of_guest.username, "guest");
        assert_eq!(of_guest.track_ids, [T4, T1]);
        assert_eq!(of_guest.current.as_deref(), Some(T4));
        assert_eq!(of_guest.changed_by, "Tablet");

        // A write of one user leaves the rows of the other: a replacement, a
        // refused request and a request that empties the queue.
        let rows_of_owner = (library.head(owner), library.entries(owner));
        assert_eq!(library.save(guest, &[T2], T2, "Tablet"), Ok(()));
        assert_eq!(
            library.save(guest, &[T3, UNKNOWN_TRACK], T3, "Tablet"),
            Err(Refusal::NoTrack)
        );
        library
            .db
            .with_writer(|conn| clear(conn, guest, "Tablet", LATER))
            .unwrap();
        assert_eq!((library.head(owner), library.entries(owner)), rows_of_owner);
        assert_eq!(library.get(owner), of_owner);

        let rows_of_guest = (library.head(guest), library.entries(guest));
        assert_eq!(library.save(owner, &[T4], T4, "Phone"), Ok(()));
        assert_eq!((library.head(guest), library.entries(guest)), rows_of_guest);
    }

    #[test]
    fn a_refused_replace_changes_nothing() {
        let library = library();
        let owner = library.owner;
        assert_eq!(library.save(owner, &[T1, T2], T2, "Phone"), Ok(()));
        let before = library.everything();
        let save = |track_ids: &[&str], current: &str| {
            library
                .db
                .with_writer(|conn| replace(conn, owner, track_ids, current, 5, "Desktop", LATER))
                .unwrap()
        };

        // The current song is none of the tracks of the request.
        assert_eq!(save(&[T3, T4], T1), Err(Refusal::NoCurrent));
        assert_eq!(save(&[T3, T4], UNKNOWN_TRACK), Err(Refusal::NoCurrent));
        assert_eq!(save(&[T3, T4], ""), Err(Refusal::NoCurrent));
        assert_eq!(save(&[], T1), Err(Refusal::NoCurrent));
        assert_eq!(save(&[], ""), Err(Refusal::NoCurrent));
        // An unknown track as the first, a middle and the last value, also
        // as the current song.
        for track_ids in [
            [UNKNOWN_TRACK, T3, T4],
            [T3, UNKNOWN_TRACK, T4],
            [T3, T4, UNKNOWN_TRACK],
        ] {
            assert_eq!(save(&track_ids, T3), Err(Refusal::NoTrack), "{track_ids:?}");
            assert_eq!(
                save(&track_ids, UNKNOWN_TRACK),
                Err(Refusal::NoTrack),
                "{track_ids:?}"
            );
        }
        // A value that is no ID is the ID of no track row.
        assert_eq!(save(&[T3, "' OR 1=1 --"], T3), Err(Refusal::NoTrack));

        // The length is checked before the current song, and the current
        // song before the tracks.
        let too_long = vec![UNKNOWN_TRACK; MAX_ENTRIES + 1];
        assert_eq!(save(&too_long, T1), Err(Refusal::TooLong));
        assert_eq!(save(&[UNKNOWN_TRACK], T1), Err(Refusal::NoCurrent));

        assert_eq!(library.everything(), before);
    }

    #[test]
    fn a_write_that_fails_after_its_first_statement_changes_nothing() {
        let library = library();
        let owner = library.owner;
        assert_eq!(library.save(owner, &[T1, T2], T2, "Phone"), Ok(()));
        let before = library.everything();
        // The insert of an entry of `T4` fails. The statements before it
        // have run then: the write of the `play_queue` row and the deletion
        // of the old entries.
        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER refuse_t4 BEFORE INSERT ON play_queue_tracks
                     WHEN NEW.track_id = 'tr-00000000000000000000000000000004'
                     BEGIN SELECT RAISE(ABORT, 'the test refuses the entry'); END;",
                )?;
                Ok(())
            })
            .unwrap();

        let replaced = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T3, T4], T3, 5, "Desktop", LATER));
        assert!(replaced.is_err(), "{replaced:?}");
        assert_eq!(library.everything(), before);

        // The deletion of the entries fails after `clear` wrote the row.
        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER keep_entries BEFORE DELETE ON play_queue_tracks
                     BEGIN SELECT RAISE(ABORT, 'the test keeps the entry'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        let cleared = library
            .db
            .with_writer(|conn| clear(conn, owner, "Desktop", LATER));
        assert!(cleared.is_err(), "{cleared:?}");
        assert_eq!(library.everything(), before);

        // The connection takes the next write.
        library
            .db
            .with_writer(|conn| Ok(conn.execute_batch("DROP TRIGGER keep_entries")?))
            .unwrap();
        assert_eq!(library.save(owner, &[T3, T1], T1, "Desktop"), Ok(()));
        assert_eq!(library.entries(owner), around(1, &[T3, T1]));
    }

    #[test]
    fn a_position_above_the_largest_stored_integer_is_an_error_and_changes_nothing() {
        let library = library();
        let owner = library.owner;
        let save = |position_ms: u64| {
            library
                .db
                .with_writer(|conn| replace(conn, owner, &[T1], T1, position_ms, "Phone", NOW))
        };

        let largest = i64::MAX as u64;
        assert_eq!(save(largest).unwrap(), Ok(()));
        assert_eq!(library.get(owner).position_ms, largest);
        let before = library.everything();

        let stored = save(largest + 1);
        assert!(stored.is_err(), "{stored:?}");
        assert_eq!(library.everything(), before);
    }

    #[test]
    fn deleting_a_track_row_deletes_its_entries_and_keeps_the_order_of_the_others() {
        let library = library();
        let owner = library.owner;
        let stored = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T1, T2, T3, T2, T4], T3, 500, "Phone", NOW))
            .unwrap();
        assert_eq!(stored, Ok(()));
        let head = library.head(owner);

        library.delete_tracks(&[T2]);

        // The other rows keep their stored positions, and no other row is
        // written.
        assert_eq!(
            library.entries(owner),
            [(-2, T1.to_owned()), (0, T3.to_owned()), (2, T4.to_owned())]
        );
        assert_eq!(library.head(owner), head);
        // The queue keeps its order, its current song, the position and the
        // change time.
        assert_eq!(
            library.get(owner),
            Queue {
                username: "owner".to_owned(),
                track_ids: list_of(&[T1, T3, T4]),
                current: Some(T3.to_owned()),
                position_ms: 500,
                changed_at: NOW,
                changed_by: "Phone".to_owned(),
            }
        );
    }

    #[test]
    fn after_the_current_track_is_deleted_the_entry_that_followed_is_the_current_song() {
        let library = library();
        let owner = library.owner;
        let current = |library: &Library| {
            let queue = library.get(owner);
            (queue.track_ids, queue.current, queue.position_ms)
        };
        let stored = library
            .db
            .with_writer(|conn| replace(conn, owner, &[T1, T2, T3, T4], T2, 777, "Phone", NOW))
            .unwrap();
        assert_eq!(stored, Ok(()));

        // `T3` followed the deleted `T2`. The position was one within `T2`.
        library.delete_tracks(&[T2]);
        assert_eq!(
            current(&library),
            (list_of(&[T1, T3, T4]), Some(T3.to_owned()), 0)
        );
        // The row keeps the ID of the deleted track and the stored position.
        assert_eq!(
            library.head(owner),
            Some((Some(T2.to_owned()), 777, NOW, "Phone".to_owned()))
        );

        // The next entry that followed, and then the last entry that is
        // left, because none followed.
        library.delete_tracks(&[T3]);
        assert_eq!(
            current(&library),
            (list_of(&[T1, T4]), Some(T4.to_owned()), 0)
        );
        library.delete_tracks(&[T4]);
        assert_eq!(current(&library), (list_of(&[T1]), Some(T1.to_owned()), 0));

        // A queue whose tracks are all deleted has no current song. The row
        // stays, with the time and the client of the request that stored it.
        library.delete_tracks(&[T1]);
        assert_eq!(current(&library), (Vec::new(), None, 0));
        let queue = library.get(owner);
        assert_eq!(
            (queue.changed_at, queue.changed_by.as_str()),
            (NOW, "Phone")
        );
    }

    #[test]
    fn after_the_current_track_is_deleted_the_next_request_stores_a_queue_as_before() {
        let library = library();
        let owner = library.owner;
        // The current song is in the queue twice, and both entries go.
        assert_eq!(library.save(owner, &[T1, T2, T3, T2], T2, "Phone"), Ok(()));
        library.delete_tracks(&[T2]);
        assert_eq!(
            library.entries(owner),
            [(-1, T1.to_owned()), (1, T3.to_owned())]
        );
        assert_eq!(library.get(owner).current.as_deref(), Some(T3));

        // A request that names the deleted track is refused, and the queue
        // stays as the scan left it.
        let before = library.everything();
        assert_eq!(
            library.save(owner, &[T1, T2, T3], T3, "Phone"),
            Err(Refusal::NoTrack)
        );
        assert_eq!(library.everything(), before);

        assert_eq!(library.save(owner, &[T4, T1], T1, "Phone"), Ok(()));
        assert_eq!(library.entries(owner), around(1, &[T4, T1]));
        assert_eq!(library.get(owner).current.as_deref(), Some(T1));
    }

    #[test]
    fn current_song_is_the_stored_one_or_the_entry_at_its_place() {
        let entries = |stored: &[(i64, &str)]| -> Vec<(i64, String)> {
            stored
                .iter()
                .map(|(position, id)| (*position, (*id).to_owned()))
                .collect()
        };
        let current = |stored: Option<&str>, entries: &[(i64, String)]| {
            current_song(stored.map(str::to_owned), 42, entries)
        };
        let song = |id: &str, position_ms: u64| (Some(id.to_owned()), position_ms);

        // The stored song is in the queue.
        let queue = entries(&[(-1, "a"), (0, "b"), (1, "c")]);
        assert_eq!(current(Some("b"), &queue), song("b", 42));
        // Its entry at position 0 is gone and a later entry of it is left.
        let queue = entries(&[(-1, "a"), (1, "c"), (2, "b")]);
        assert_eq!(current(Some("b"), &queue), song("b", 42));

        // The stored song is gone: the entry that followed.
        let queue = entries(&[(-2, "a"), (-1, "c"), (1, "d"), (2, "e")]);
        assert_eq!(current(Some("b"), &queue), song("d", 0));
        let queue = entries(&[(-2, "a"), (3, "e")]);
        assert_eq!(current(Some("b"), &queue), song("e", 0));
        // No entry followed: the last entry.
        let queue = entries(&[(-3, "a"), (-1, "c")]);
        assert_eq!(current(Some("b"), &queue), song("c", 0));

        // A queue without an entry has no current song.
        assert_eq!(current(Some("b"), &[]), (None, 0));
        assert_eq!(current(None, &[]), (None, 0));
    }

    #[test]
    fn a_track_without_a_file_is_queued_and_stays_queued() {
        let library = library();
        let owner = library.owner;
        assert_eq!(library.save(owner, &[T1, T2], T1, "Phone"), Ok(()));

        // The scanner keeps a track that it can neither relink nor delete
        // under its placeholder path.
        let placeholder = tracks::placeholder_path(T1);
        library
            .db
            .with_writer(|conn| {
                let track = tracks::get_track(conn, T1)?.expect("the track exists");
                tracks::relink_track(conn, T1, track.folder_id, &placeholder, 1000, 100)
            })
            .unwrap();

        let queue = library.get(owner);
        assert_eq!(queue.track_ids, [T1, T2]);
        assert_eq!(queue.current.as_deref(), Some(T1));
        // The track has a row, so a request names it as every other track.
        assert_eq!(library.save(owner, &[T2, T1, T1], T1, "Phone"), Ok(()));
        assert_eq!(library.entries(owner), around(1, &[T2, T1, T1]));
    }

    #[test]
    fn a_queue_holds_at_most_max_entries() {
        let library = library();
        let owner = library.owner;
        // The number of the entries and their lowest and highest position.
        let stored = || {
            library
                .db
                .with_reader(|conn| {
                    let sql =
                        "SELECT count(*), min(position), max(position) FROM play_queue_tracks";
                    Ok(conn.query_row(sql, [], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    })?)
                })
                .unwrap()
        };
        let limit = MAX_ENTRIES as i64;

        // The limit itself is accepted. The current song is the last entry.
        let mut full = vec![T1; MAX_ENTRIES];
        full[MAX_ENTRIES - 1] = T2;
        assert_eq!(library.save(owner, &full, T2, "Phone"), Ok(()));
        assert_eq!(stored(), (limit, Some(1 - limit), Some(0)));
        let queue = library.get(owner);
        assert_eq!(queue.track_ids.len(), MAX_ENTRIES);
        assert_eq!(queue.current.as_deref(), Some(T2));

        // One entry more is refused, and the queue stays.
        full.push(T1);
        assert_eq!(
            library.save(owner, &full, T2, "Phone"),
            Err(Refusal::TooLong)
        );
        assert_eq!(stored(), (limit, Some(1 - limit), Some(0)));
    }

    #[test]
    fn requests_at_the_same_time_store_one_queue_whole() {
        // A file database has read connections of their own, so a read runs
        // while a write is in progress.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");
        let library = library_in(db);
        let owner = library.owner;
        // The queue of each client: its tracks, its current song and its
        // position.
        let phone: (&[&str], &str, u64) = (&[T1, T2, T3, T1], T2, 111);
        let desktop: (&[&str], &str, u64) = (&[T4, T3], T4, 222);
        // What a reader finds for the queue of a client.
        let whole = |client: &str, (track_ids, current, position_ms): (&[&str], &str, u64)| Queue {
            username: "owner".to_owned(),
            track_ids: list_of(track_ids),
            current: Some(current.to_owned()),
            position_ms,
            changed_at: NOW,
            changed_by: client.to_owned(),
        };
        let wholes = [whole("Phone", phone), whole("Desktop", desktop)];
        let store = |client: &str, (track_ids, current, position_ms): (&[&str], &str, u64)| {
            let stored = library.db.with_writer(|conn| {
                replace(conn, owner, track_ids, current, position_ms, client, NOW)
            });
            assert_eq!(stored.unwrap(), Ok(()));
        };
        store("Phone", phone);

        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writers = [("Phone", phone), ("Desktop", desktop)].map(|(client, queue)| {
                let store = &store;
                scope.spawn(move || {
                    for _ in 0..200 {
                        store(client, queue);
                    }
                })
            });
            let readers = [(); 2].map(|()| {
                let (db, done, wholes) = (&library.db, &done, &wholes);
                scope.spawn(move || {
                    // A reader reads until the writers have ended, and once
                    // more after that.
                    loop {
                        let last = done.load(Ordering::SeqCst);
                        let queue = db
                            .with_reader(|conn| {
                                // One transaction, as a handler reads.
                                let snapshot = conn.unchecked_transaction()?;
                                get(&snapshot, owner)
                            })
                            .unwrap()
                            .expect("the owner has a row");
                        assert!(wholes.contains(&queue), "{queue:?}");
                        if last {
                            break;
                        }
                    }
                })
            });
            // The readers end when the writers have ended, also when a
            // writer failed.
            let written = writers.map(|writer| writer.join());
            done.store(true, Ordering::SeqCst);
            for reader in readers {
                reader.join().expect("the reader does not panic");
            }
            for result in written {
                result.expect("the writer does not panic");
            }
        });

        // The queue that is stored at the end is the queue of one client.
        let queue = library.get(owner);
        assert!(wholes.contains(&queue), "{queue:?}");
        let entries = library.entries(owner);
        let of_one = [around(1, phone.0), around(0, desktop.0)];
        assert!(of_one.contains(&entries), "{entries:?}");
    }
}
