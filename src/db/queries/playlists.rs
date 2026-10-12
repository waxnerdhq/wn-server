//! Queries on the `playlists` and `playlist_tracks` tables (`docs/design.md`
//! section 3.4).
//!
//! # Ownership
//!
//! A playlist belongs to the user that created it. Every function takes the
//! ID of a user and reads or changes the playlists of that user only. A
//! playlist of another user is a playlist that the function does not find,
//! whatever its `public` value is.
//!
//! # Entries and positions
//!
//! An entry is a row of `playlist_tracks`: one track at one position of one
//! playlist. A track can have several entries in a playlist. The entries of
//! a playlist are its rows in the order of `position`, and the position of an
//! entry in the API is its place in that order, counted from 0.
//!
//! A write of the entries stores them at the positions 0 to n - 1:
//! [`create`], [`replace_tracks`], and [`update`] with a change that removes
//! or adds an entry. The deletion of a track row deletes the entries of the
//! track (`ON DELETE CASCADE`) and writes no other row, so the stored
//! positions of a playlist can then have a gap. No function depends on
//! stored positions without a gap: the readers order by `position`, and
//! [`update`] reads the entries in that order before it applies a change.
//! The next write of the entries stores the positions 0 to n - 1 again.
//!
//! # Writes
//!
//! [`create`], [`replace_tracks`] and [`update`] each run in one `IMMEDIATE`
//! transaction. A function checks the whole request before its first write.
//! A request that it refuses ([`Refusal`]) and a write that fails change
//! nothing. [`delete`] is one statement.
//!
//! Every value of a request is a parameter of a statement. A list of track
//! IDs is one parameter, a JSON array that the statement reads with
//! `json_each`.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use super::browse::json_array;
use super::int;
use crate::db::Result;
use crate::ids::{self, Kind};

/// The largest number of entries of one playlist. A request that would make
/// a playlist longer is refused with [`Refusal::TooLong`].
///
/// The limit bounds the rows that one request writes and the rows that one
/// request reads. A list of this many track IDs is a form body of about
/// 4.4 MB, which the form body limit of the router admits
/// (`api::FORM_BODY_LIMIT`).
pub const MAX_ENTRIES: usize = 100_000;

/// The statement that reads [`Playlist`] values, without its `WHERE` clause.
/// [`playlist_from_row`] reads the columns in this order.
const SELECT_PLAYLIST: &str = "SELECT playlists.id, users.name, playlists.name, \
       playlists.comment, playlists.public, playlists.created_at, playlists.updated_at, \
       (SELECT count(*) FROM playlist_tracks \
         WHERE playlist_tracks.playlist_id = playlists.id), \
       (SELECT coalesce(sum(tracks.duration_ms), 0) FROM playlist_tracks \
         JOIN tracks ON tracks.id = playlist_tracks.track_id \
         WHERE playlist_tracks.playlist_id = playlists.id), \
       (SELECT playlist_tracks.track_id FROM playlist_tracks \
         WHERE playlist_tracks.playlist_id = playlists.id \
         ORDER BY playlist_tracks.position LIMIT 1) \
     FROM playlists JOIN users ON users.id = playlists.owner_id";

/// A playlist with the values that follow from its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    pub id: String,
    /// The name of the user that owns the playlist.
    pub owner: String,
    pub name: String,
    /// `None` for a playlist without a comment.
    pub comment: Option<String>,
    /// The stored `public` value. It changes nothing in what a user reads.
    pub public: bool,
    /// The time of the request that created the playlist, in Unix seconds.
    pub created_at: i64,
    /// The time of the last request that changed the playlist, in Unix
    /// seconds. The deletion of a track row does not change it.
    pub changed_at: i64,
    /// The number of entries. A track with two entries counts twice.
    pub song_count: u64,
    /// The sum of the durations of the tracks of the entries in
    /// milliseconds. A track with two entries counts twice.
    pub duration_ms: u64,
    /// The ID of the track of the first entry. `None` for a playlist
    /// without an entry.
    pub first_track_id: Option<String>,
}

/// Why a write changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The user owns no playlist with the ID.
    NoPlaylist,
    /// An ID of the request is the ID of no track row.
    NoTrack,
    /// A position of the request is the position of no entry.
    NoPosition,
    /// The playlist would have more than [`MAX_ENTRIES`] entries.
    TooLong,
}

/// What a write returns when the database answered: the value of the write,
/// or the reason for which nothing was written.
pub type Outcome<T> = std::result::Result<T, Refusal>;

/// What one `updatePlaylist` request changes. The default value changes
/// nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Change {
    /// The new name. `None` keeps the name.
    pub name: Option<String>,
    /// The new comment. An empty text removes the comment. `None` keeps it.
    pub comment: Option<String>,
    /// The new `public` value. `None` keeps it.
    pub public: Option<bool>,
    /// The positions of the entries to remove, in any order. Each is a
    /// position in the playlist as it is before the request. A position that
    /// the list holds twice removes one entry.
    pub remove: Vec<usize>,
    /// The IDs of the tracks to append, in the order of their new entries.
    /// The new entries follow the entries that the request keeps.
    pub add: Vec<String>,
}

impl Change {
    /// Whether the change names nothing to change.
    fn is_empty(&self) -> bool {
        self.name.is_none() && self.comment.is_none() && self.public.is_none() && !self.entries()
    }

    /// Whether the change removes or adds an entry.
    fn entries(&self) -> bool {
        !self.remove.is_empty() || !self.add.is_empty()
    }
}

/// Returns the playlists of the user `user_id` in the order of their names
/// without regard to ASCII case. Names that differ by case only are in the
/// order of their bytes, and playlists with the same name in the order of
/// their IDs.
pub fn list(conn: &Connection, user_id: i64) -> Result<Vec<Playlist>> {
    let mut select = conn.prepare_cached(&format!(
        "{SELECT_PLAYLIST} WHERE playlists.owner_id = ?1
         ORDER BY playlists.name COLLATE NOCASE, playlists.name, playlists.id"
    ))?;
    let playlists = select
        .query_map([user_id], playlist_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(playlists)
}

/// Returns the playlist `id` of the user `user_id`, or `None` when the user
/// owns no playlist with the ID.
pub fn get(conn: &Connection, user_id: i64, id: &str) -> Result<Option<Playlist>> {
    let playlist = conn
        .prepare_cached(&format!(
            "{SELECT_PLAYLIST} WHERE playlists.id = ?1 AND playlists.owner_id = ?2"
        ))?
        .query_row(params![id, user_id], playlist_from_row)
        .optional()?;
    Ok(playlist)
}

/// Returns the track ID of each entry of the playlist `id` of the user
/// `user_id`, in the order of the entries. A track with two entries is in
/// the list twice. The list is empty when the user owns no playlist with the
/// ID.
pub fn track_ids(conn: &Connection, user_id: i64, id: &str) -> Result<Vec<String>> {
    let mut select = conn.prepare_cached(
        "SELECT playlist_tracks.track_id FROM playlist_tracks
         JOIN playlists ON playlists.id = playlist_tracks.playlist_id
         WHERE playlist_tracks.playlist_id = ?1 AND playlists.owner_id = ?2
         ORDER BY playlist_tracks.position",
    )?;
    let ids = select
        .query_map(params![id, user_id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Creates a playlist of the user `owner_id` with the name `name` and one
/// entry for each ID of `track_ids`, in that order, and returns its ID. The
/// playlist has no comment and is not public. `now` is its creation time and
/// its change time, in Unix seconds.
///
/// More than [`MAX_ENTRIES`] IDs are refused with [`Refusal::TooLong`], and
/// an ID that no track row has with [`Refusal::NoTrack`]. Nothing is written
/// then.
pub fn create<S: AsRef<str>>(
    conn: &mut Connection,
    owner_id: i64,
    name: &str,
    track_ids: &[S],
    now: i64,
) -> Result<Outcome<String>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if track_ids.len() > MAX_ENTRIES {
        return Ok(Err(Refusal::TooLong));
    }
    if !tracks_exist(&tx, track_ids)? {
        return Ok(Err(Refusal::NoTrack));
    }
    let id = ids::new_id(Kind::Playlist);
    tx.prepare_cached(
        "INSERT INTO playlists (id, owner_id, name, comment, public, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, 0, ?4, ?4)",
    )?
    .execute(params![id, owner_id, name, now])?;
    insert_entries(&tx, &id, 0, track_ids)?;
    tx.commit()?;
    Ok(Ok(id))
}

/// Replaces the entries of the playlist `id` of the user `user_id` with one
/// entry for each ID of `track_ids`, in that order, and sets the change time
/// to `now`. The name, the comment and the `public` value stay.
///
/// The request is refused with [`Refusal::NoPlaylist`] when the user owns no
/// playlist with the ID, with [`Refusal::TooLong`] for more than
/// [`MAX_ENTRIES`] IDs, and with [`Refusal::NoTrack`] for an ID that no
/// track row has. The playlist keeps its entries then.
pub fn replace_tracks<S: AsRef<str>>(
    conn: &mut Connection,
    user_id: i64,
    id: &str,
    track_ids: &[S],
    now: i64,
) -> Result<Outcome<()>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !owns(&tx, user_id, id)? {
        return Ok(Err(Refusal::NoPlaylist));
    }
    if track_ids.len() > MAX_ENTRIES {
        return Ok(Err(Refusal::TooLong));
    }
    if !tracks_exist(&tx, track_ids)? {
        return Ok(Err(Refusal::NoTrack));
    }
    delete_entries(&tx, id)?;
    insert_entries(&tx, id, 0, track_ids)?;
    set_meta(&tx, user_id, id, &Change::default(), now)?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Applies `change` to the playlist `id` of the user `user_id` and sets the
/// change time to `now`.
///
/// The API defines no order for a request that names several positions and
/// several tracks. The entries change in the order of Airsonic, Navidrome
/// and gonic:
///
/// 1. The entries at the positions of `change.remove` are removed. Every
///    position names an entry of the playlist as it is before the request,
///    so the result does not depend on the order of the positions. It is the
///    result of removing them one by one from the highest to the lowest.
/// 2. One entry for each ID of `change.add` is appended, in that order. A
///    position therefore never names an entry that the same request adds.
///
/// The request is refused, and nothing is written, in these cases, which are
/// checked in this order:
///
/// - [`Refusal::NoPlaylist`]: the user owns no playlist with the ID.
/// - [`Refusal::NoPosition`]: a position is not below the number of entries.
/// - [`Refusal::TooLong`]: the playlist would have more than
///   [`MAX_ENTRIES`] entries.
/// - [`Refusal::NoTrack`]: an ID of `change.add` is the ID of no track row.
///
/// A change that names nothing writes nothing and keeps the change time.
pub fn update(
    conn: &mut Connection,
    user_id: i64,
    id: &str,
    change: &Change,
    now: i64,
) -> Result<Outcome<()>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !owns(&tx, user_id, id)? {
        return Ok(Err(Refusal::NoPlaylist));
    }
    if change.is_empty() {
        return Ok(Ok(()));
    }
    if change.entries() {
        let stored = stored_entries(&tx, id)?;
        // The stored positions have a gap after the deletion of a track row.
        let dense = stored
            .iter()
            .zip(0i64..)
            .all(|((position, _), place)| *position == place);
        let before = stored.into_iter().map(|(_, track_id)| track_id).collect();
        let kept = match kept_entries(before, change) {
            Ok(kept) => kept,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if !tracks_exist(&tx, &change.add)? {
            return Ok(Err(Refusal::NoTrack));
        }
        // New entries follow rows whose positions are 0 to n - 1. Every
        // other change writes the kept entries again at these positions.
        let append_only = change.remove.is_empty() && dense;
        if !append_only {
            delete_entries(&tx, id)?;
            insert_entries(&tx, id, 0, &kept)?;
        }
        insert_entries(&tx, id, kept.len(), &change.add)?;
    }
    set_meta(&tx, user_id, id, change, now)?;
    tx.commit()?;
    Ok(Ok(()))
}

/// Deletes the playlist `id` of the user `user_id` with its entries. Returns
/// `false` and deletes nothing when the user owns no playlist with the ID.
pub fn delete(conn: &Connection, user_id: i64, id: &str) -> Result<bool> {
    let deleted = conn
        .prepare_cached("DELETE FROM playlists WHERE id = ?1 AND owner_id = ?2")?
        .execute(params![id, user_id])?;
    Ok(deleted == 1)
}

/// Returns the entries of `entries` that `change` keeps, in the order they
/// have: `entries` without the entries at the positions `change.remove`
/// ([`without_positions`]).
///
/// The change is refused with [`Refusal::NoPosition`] when a position is not
/// below the number of entries, and else with [`Refusal::TooLong`] when the
/// kept entries and the entries of `change.add` together are more than
/// [`MAX_ENTRIES`]. The length of the result is what counts: a change that
/// removes as many entries as it adds never makes a playlist too long.
fn kept_entries(entries: Vec<String>, change: &Change) -> Outcome<Vec<String>> {
    let kept = without_positions(entries, &change.remove).ok_or(Refusal::NoPosition)?;
    if kept.len() + change.add.len() > MAX_ENTRIES {
        return Err(Refusal::TooLong);
    }
    Ok(kept)
}

/// Returns `entries` without the entries at the positions `remove`, in the
/// order they had. A position that `remove` holds twice removes one entry.
/// Returns `None` when a position is not below the number of entries.
///
/// Every position names an entry of `entries` as the function receives it.
/// The result is the list that is left when the positions are removed one by
/// one from the highest to the lowest.
fn without_positions(mut entries: Vec<String>, remove: &[usize]) -> Option<Vec<String>> {
    if remove.iter().any(|position| *position >= entries.len()) {
        return None;
    }
    if !remove.is_empty() {
        let removed: HashSet<usize> = remove.iter().copied().collect();
        let mut position = 0;
        entries.retain(|_| {
            let keep = !removed.contains(&position);
            position += 1;
            keep
        });
    }
    Some(entries)
}

/// Whether the user `user_id` owns a playlist with the ID `id`.
fn owns(conn: &Connection, user_id: i64, id: &str) -> Result<bool> {
    let owns = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM playlists WHERE id = ?1 AND owner_id = ?2)")?
        .query_row(params![id, user_id], |row| row.get(0))?;
    Ok(owns)
}

/// Whether every ID of `track_ids` is the ID of a track row. A track without
/// a file has a row (`docs/design.md` section 3.4).
fn tracks_exist<S: AsRef<str>>(conn: &Connection, track_ids: &[S]) -> Result<bool> {
    if track_ids.is_empty() {
        return Ok(true);
    }
    let exist = conn
        .prepare_cached(
            "SELECT NOT EXISTS (
               SELECT 1 FROM json_each(?1)
               WHERE NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.id = json_each.value))",
        )?
        .query_row([json_array(track_ids)], |row| row.get(0))?;
    Ok(exist)
}

/// Returns the stored position and the track ID of each entry of the
/// playlist `id`, in the order of the positions.
fn stored_entries(conn: &Connection, id: &str) -> Result<Vec<(i64, String)>> {
    let mut select = conn.prepare_cached(
        "SELECT position, track_id FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position",
    )?;
    let entries = select
        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(entries)
}

/// Deletes every entry of the playlist `id`.
fn delete_entries(conn: &Connection, id: &str) -> Result<()> {
    conn.prepare_cached("DELETE FROM playlist_tracks WHERE playlist_id = ?1")?
        .execute([id])?;
    Ok(())
}

/// Inserts one entry of the playlist `id` for each ID of `track_ids`, at the
/// positions that start with `first_position`, in the order of the IDs.
fn insert_entries<S: AsRef<str>>(
    conn: &Connection,
    id: &str,
    first_position: usize,
    track_ids: &[S],
) -> Result<()> {
    if track_ids.is_empty() {
        return Ok(());
    }
    // `key` is the index of an item in the JSON array, counted from 0.
    conn.prepare_cached(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id)
         SELECT ?1, ?2 + key, value FROM json_each(?3)",
    )?
    .execute(params![
        id,
        int::to_sql(first_position as u64)?,
        json_array(track_ids)
    ])?;
    Ok(())
}

/// Stores the name, the comment and the `public` value that `change` names
/// in the playlist `id` of the user `user_id`, and sets the change time to
/// `now`. A value that `change` does not name stays. An empty comment is
/// stored as no comment.
fn set_meta(conn: &Connection, user_id: i64, id: &str, change: &Change, now: i64) -> Result<()> {
    conn.prepare_cached(
        "UPDATE playlists SET
           name = coalesce(?3, name),
           comment = CASE WHEN ?4 IS NULL THEN comment ELSE nullif(?4, '') END,
           public = coalesce(?5, public),
           updated_at = ?6
         WHERE id = ?1 AND owner_id = ?2",
    )?
    .execute(params![
        id,
        user_id,
        change.name,
        change.comment,
        change.public,
        now
    ])?;
    Ok(())
}

/// Reads a [`Playlist`] from a row of [`SELECT_PLAYLIST`].
fn playlist_from_row(row: &Row<'_>) -> rusqlite::Result<Playlist> {
    Ok(Playlist {
        id: row.get(0)?,
        owner: row.get(1)?,
        name: row.get(2)?,
        comment: row.get(3)?,
        public: row.get(4)?,
        created_at: row.get(5)?,
        changed_at: row.get(6)?,
        song_count: int::get_u64(row, 7)?,
        duration_ms: int::get_u64(row, 8)?,
        first_track_id: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
    const UNKNOWN_PLAYLIST: &str = "pl-ffffffffffffffffffffffffffffffff";

    /// The time of the requests that create a playlist.
    const NOW: i64 = 1_700_000_000;

    /// The time of the requests that change a playlist.
    const LATER: i64 = 1_700_000_600;

    /// The duration of the track `id` of [`library`] in milliseconds.
    fn duration_ms(id: &str) -> u64 {
        match id {
            T1 => 1_100,
            T2 => 2_200,
            T3 => 3_300,
            T4 => 4_400,
            _ => panic!("{id} is no track of the test library"),
        }
    }

    /// A database with two users and four tracks.
    struct Library {
        db: Db,
        /// The ID of the owner account `owner`.
        owner: i64,
        /// The ID of a second account, `guest`.
        guest: i64,
    }

    /// Returns a database that holds the owner, a second user and the tracks
    /// [`T1`] to [`T4`] of one album. The server has no function that
    /// inserts a second user, so the test inserts the row.
    fn library() -> Library {
        let db = Db::open_memory().unwrap();
        let (owner, guest) = db
            .with_writer(|conn| {
                let owner = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
                let guest = conn.query_row(
                    "INSERT INTO users (name, role, password, created_at, updated_at)
                     VALUES ('guest', 'guest', 'secret', 0, 0) RETURNING id",
                    [],
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
                        duration_ms: duration_ms(id),
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
                    tracks::insert_track(conn, id, folder.id, &format!("{id}.flac"), &content)?;
                }
                Ok((owner.id, guest))
            })
            .unwrap();
        Library { db, owner, guest }
    }

    impl Library {
        /// Creates a playlist of `user` at [`NOW`] and returns its ID.
        fn create(&self, user: i64, name: &str, track_ids: &[&str]) -> String {
            self.db
                .with_writer(|conn| create(conn, user, name, track_ids, NOW))
                .unwrap()
                .expect("the playlist is created")
        }

        /// Applies `change` to the playlist `id` as `user` at [`LATER`].
        fn update(&self, user: i64, id: &str, change: &Change) -> Outcome<()> {
            self.db
                .with_writer(|conn| update(conn, user, id, change, LATER))
                .unwrap()
        }

        /// Replaces the entries of the playlist `id` as `user` at [`LATER`].
        fn replace(&self, user: i64, id: &str, track_ids: &[&str]) -> Outcome<()> {
            self.db
                .with_writer(|conn| replace_tracks(conn, user, id, track_ids, LATER))
                .unwrap()
        }

        fn get(&self, user: i64, id: &str) -> Option<Playlist> {
            self.db.with_reader(|conn| get(conn, user, id)).unwrap()
        }

        fn list(&self, user: i64) -> Vec<Playlist> {
            self.db.with_reader(|conn| list(conn, user)).unwrap()
        }

        fn track_ids(&self, user: i64, id: &str) -> Vec<String> {
            self.db
                .with_reader(|conn| track_ids(conn, user, id))
                .unwrap()
        }

        /// The rows of `playlist_tracks` of the playlist `id` in the order of
        /// the positions: the row ID, the position and the track ID of each.
        fn rows(&self, id: &str) -> Vec<(i64, i64, String)> {
            self.db
                .with_reader(|conn| {
                    let mut select = conn.prepare(
                        "SELECT rowid, position, track_id FROM playlist_tracks
                         WHERE playlist_id = ?1 ORDER BY position",
                    )?;
                    let rows = select
                        .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    Ok(rows)
                })
                .unwrap()
        }

        /// The stored position and the track ID of each entry of the
        /// playlist `id`, in the order of the positions.
        fn stored(&self, id: &str) -> Vec<(i64, String)> {
            let rows = self.rows(id).into_iter();
            rows.map(|(_, position, track)| (position, track)).collect()
        }

        /// Every value of the rows of `playlists` and `playlist_tracks`, as
        /// text. Two equal results are two equal states of the playlists.
        fn everything(&self) -> Vec<String> {
            self.db
                .with_reader(|conn| {
                    let mut rows = Vec::new();
                    for sql in [
                        "SELECT format('%s|%d|%s|%s|%d|%d|%d', id, owner_id, name,
                           quote(comment), public, created_at, updated_at)
                         FROM playlists ORDER BY id",
                        "SELECT format('%s|%d|%s', playlist_id, position, track_id)
                         FROM playlist_tracks ORDER BY playlist_id, position",
                    ] {
                        let mut select = conn.prepare(sql)?;
                        let texts = select.query_map([], |row| row.get(0))?;
                        rows.extend(texts.collect::<rusqlite::Result<Vec<String>>>()?);
                    }
                    Ok(rows)
                })
                .unwrap()
        }
    }

    /// A change of the entries: the positions to remove and the tracks to
    /// add.
    fn entries(remove: &[usize], add: &[&str]) -> Change {
        Change {
            remove: remove.to_vec(),
            add: add.iter().map(|id| (*id).to_owned()).collect(),
            ..Change::default()
        }
    }

    /// `positions` as the stored rows of a playlist that holds `track_ids`
    /// at the positions 0 to n - 1.
    fn dense(track_ids: &[&str]) -> Vec<(i64, String)> {
        (0..)
            .zip(track_ids.iter().map(|id| (*id).to_owned()))
            .collect()
    }

    fn list_of(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_owned()).collect()
    }

    #[test]
    fn without_positions_removes_the_entries_at_the_positions_of_the_list_it_receives() {
        let entries = || list_of(&["a", "b", "c", "d", "e"]);
        let left = |remove: &[usize]| without_positions(entries(), remove);

        assert_eq!(left(&[]), Some(entries()));
        assert_eq!(left(&[0]), Some(list_of(&["b", "c", "d", "e"])));
        assert_eq!(left(&[4]), Some(list_of(&["a", "b", "c", "d"])));
        // Position 2 is `c` and position 3 is `d`, whichever is named first.
        // Removing one by one in ascending order would remove `c` and `e`.
        assert_eq!(left(&[2, 3]), Some(list_of(&["a", "b", "e"])));
        assert_eq!(left(&[3, 2]), Some(list_of(&["a", "b", "e"])));
        assert_eq!(left(&[0, 2, 4]), Some(list_of(&["b", "d"])));
        assert_eq!(left(&[4, 0, 2]), Some(list_of(&["b", "d"])));
        // A position that is named twice removes one entry.
        assert_eq!(left(&[1, 1]), Some(list_of(&["a", "c", "d", "e"])));
        assert_eq!(left(&[1, 3, 1, 3]), Some(list_of(&["a", "c", "e"])));
        assert_eq!(left(&[4, 3, 2, 1, 0]), Some(Vec::new()));

        // A position at or after the end removes nothing.
        assert_eq!(left(&[5]), None);
        assert_eq!(left(&[0, 5, 1]), None);
        assert_eq!(left(&[usize::MAX]), None);
        assert_eq!(without_positions(Vec::new(), &[0]), None);
        assert_eq!(without_positions(Vec::new(), &[]), Some(Vec::new()));
    }

    #[test]
    fn create_stores_the_entries_at_the_positions_0_to_n_minus_1() {
        let library = library();
        let id = library.create(library.owner, "Mix", &[T3, T1, T3, T2]);

        assert!(matches!(ids::parse(&id), Some((Kind::Playlist, _))), "{id}");
        assert_eq!(library.stored(&id), dense(&[T3, T1, T3, T2]));
        assert_eq!(library.track_ids(library.owner, &id), [T3, T1, T3, T2]);
        assert_eq!(
            library.get(library.owner, &id),
            Some(Playlist {
                id: id.clone(),
                owner: "owner".to_owned(),
                name: "Mix".to_owned(),
                comment: None,
                public: false,
                created_at: NOW,
                changed_at: NOW,
                song_count: 4,
                // The track with two entries counts twice.
                duration_ms: 3_300 + 1_100 + 3_300 + 2_200,
                first_track_id: Some(T3.to_owned()),
            })
        );

        let empty = library.create(library.owner, "Empty", &[]);
        assert_eq!(library.stored(&empty), []);
        let row = library.get(library.owner, &empty).expect("the playlist");
        assert_eq!(
            (row.song_count, row.duration_ms, row.first_track_id),
            (0, 0, None)
        );
        assert_ne!(empty, id);
    }

    #[test]
    fn list_returns_the_playlists_of_the_user_by_name_and_then_by_id() {
        let library = library();
        assert_eq!(library.list(library.owner), []);

        let b_side = library.create(library.owner, "b-side", &[T2]);
        let lower = library.create(library.owner, "alpha", &[]);
        let upper = library.create(library.owner, "Alpha", &[T1, T4]);
        let mut twins = [
            library.create(library.owner, "Twin", &[]),
            library.create(library.owner, "Twin", &[]),
        ];
        twins.sort();
        library.create(library.guest, "Aardvark", &[T3]);

        let listed = library.list(library.owner);
        let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, [&upper, &lower, &b_side, &twins[0], &twins[1]]);
        // Each row is the row that `get` returns.
        for row in &listed {
            assert_eq!(library.get(library.owner, &row.id).as_ref(), Some(row));
        }
        assert_eq!(listed[0].song_count, 2);
        assert_eq!(listed[0].duration_ms, 1_100 + 4_400);
        assert_eq!(listed[0].first_track_id.as_deref(), Some(T1));
    }

    #[test]
    fn a_user_does_not_read_the_playlist_of_another_user() {
        let library = library();
        let Library { owner, guest, .. } = library;
        let private = library.create(owner, "Private", &[T1, T2]);
        let public = library.create(owner, "Public", &[T3]);
        let published = Change {
            public: Some(true),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &public, &published), Ok(()));
        let of_guest = library.create(guest, "Guest list", &[T4]);

        // The owner of a playlist is its creator.
        let owner_of = |user, id: &str| library.get(user, id).map(|row| row.owner);
        assert_eq!(owner_of(owner, &private).as_deref(), Some("owner"));
        assert_eq!(owner_of(guest, &of_guest).as_deref(), Some("guest"));

        // The other user finds no playlist and no entry, also for the
        // playlist that is public.
        for id in [&private, &public] {
            assert_eq!(library.get(guest, id), None);
            assert_eq!(library.track_ids(guest, id), [] as [&str; 0]);
        }
        assert_eq!(library.get(owner, &of_guest), None);
        assert_eq!(library.track_ids(owner, &of_guest), [] as [&str; 0]);

        let names = |user| -> Vec<String> {
            let rows = library.list(user).into_iter();
            rows.map(|row| row.name).collect()
        };
        assert_eq!(names(owner), ["Private", "Public"]);
        assert_eq!(names(guest), ["Guest list"]);
        // An ID that no user has holds no playlist.
        assert_eq!(library.list(owner + guest + 1), []);
    }

    #[test]
    fn a_user_does_not_change_the_playlist_of_another_user() {
        let library = library();
        let Library { owner, guest, .. } = library;
        let private = library.create(owner, "Private", &[T1, T2]);
        let public = library.create(owner, "Public", &[T3]);
        let published = Change {
            public: Some(true),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &public, &published), Ok(()));
        let before = library.everything();

        for id in [&private, &public] {
            let changes = [
                Change {
                    name: Some("Taken".to_owned()),
                    ..Change::default()
                },
                Change {
                    comment: Some("Taken".to_owned()),
                    ..Change::default()
                },
                Change {
                    public: Some(false),
                    ..Change::default()
                },
                entries(&[0], &[]),
                entries(&[], &[T4]),
                // A change that names nothing finds no playlist either.
                Change::default(),
            ];
            for change in &changes {
                assert_eq!(
                    library.update(guest, id, change),
                    Err(Refusal::NoPlaylist),
                    "{change:?}"
                );
            }
            assert_eq!(library.replace(guest, id, &[T4]), Err(Refusal::NoPlaylist));
            assert_eq!(library.replace(guest, id, &[]), Err(Refusal::NoPlaylist));
            let deleted = library.db.with_writer(|conn| delete(conn, guest, id));
            assert!(!deleted.unwrap());
        }
        assert_eq!(library.everything(), before);

        // The owner changes and deletes both.
        assert_eq!(library.update(owner, &private, &entries(&[0], &[])), Ok(()));
        assert_eq!(library.track_ids(owner, &private), [T2]);
        let deleted = library.db.with_writer(|conn| delete(conn, owner, &public));
        assert!(deleted.unwrap());
        assert_eq!(library.get(owner, &public), None);
    }

    #[test]
    fn update_removes_the_positions_and_then_appends() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2, T3]);

        // The positions 0 and 2 leave the middle entry.
        assert_eq!(library.update(owner, &id, &entries(&[0, 2], &[])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T2]));

        assert_eq!(library.update(owner, &id, &entries(&[], &[T4, T2])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T2, T4, T2]));

        // The positions name entries of the list before the request, so
        // position 2 is the second `T2` and never the first added track.
        assert_eq!(
            library.update(owner, &id, &entries(&[2, 0], &[T1, T1, T3])),
            Ok(())
        );
        assert_eq!(library.stored(&id), dense(&[T4, T1, T1, T3]));

        let row = library.get(owner, &id).expect("the playlist");
        assert_eq!(row.song_count, 4);
        assert_eq!(row.duration_ms, 4_400 + 1_100 + 1_100 + 3_300);
        assert_eq!(row.first_track_id.as_deref(), Some(T4));
        assert_eq!((row.created_at, row.changed_at), (NOW, LATER));
    }

    #[test]
    fn update_sets_the_values_that_the_change_names_and_keeps_the_others() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1]);
        let meta = |library: &Library| {
            let row = library.get(owner, &id).expect("the playlist");
            (row.name, row.comment, row.public)
        };

        let change = Change {
            comment: Some("Summer".to_owned()),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &id, &change), Ok(()));
        assert_eq!(
            meta(&library),
            ("Mix".to_owned(), Some("Summer".to_owned()), false)
        );

        let change = Change {
            name: Some("Mix 2".to_owned()),
            public: Some(true),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &id, &change), Ok(()));
        assert_eq!(
            meta(&library),
            ("Mix 2".to_owned(), Some("Summer".to_owned()), true)
        );

        // An empty comment is stored as no comment.
        let change = Change {
            comment: Some(String::new()),
            public: Some(false),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &id, &change), Ok(()));
        assert_eq!(meta(&library), ("Mix 2".to_owned(), None, false));
        assert_eq!(library.stored(&id), dense(&[T1]));
    }

    #[test]
    fn a_change_that_names_nothing_writes_nothing() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1]);
        let before = library.everything();

        assert_eq!(library.update(owner, &id, &Change::default()), Ok(()));
        assert_eq!(library.everything(), before);
        let row = library.get(owner, &id).expect("the playlist");
        assert_eq!(row.changed_at, NOW);
        // The playlist is still checked.
        assert_eq!(
            library.update(owner, UNKNOWN_PLAYLIST, &Change::default()),
            Err(Refusal::NoPlaylist)
        );
    }

    #[test]
    fn replace_tracks_stores_the_new_entries_and_keeps_the_other_values() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2, T3]);
        let change = Change {
            comment: Some("kept".to_owned()),
            public: Some(true),
            ..Change::default()
        };
        assert_eq!(library.update(owner, &id, &change), Ok(()));
        let other = library.create(owner, "Other", &[T4]);

        let later = LATER + 60;
        let replaced = library
            .db
            .with_writer(|conn| replace_tracks(conn, owner, &id, &[T2, T2, T4], later))
            .unwrap();
        assert_eq!(replaced, Ok(()));
        assert_eq!(library.stored(&id), dense(&[T2, T2, T4]));
        let row = library.get(owner, &id).expect("the playlist");
        assert_eq!(
            (row.name.as_str(), row.comment.as_deref(), row.public),
            ("Mix", Some("kept"), true)
        );
        assert_eq!((row.created_at, row.changed_at), (NOW, later));
        // The other playlist is as it was.
        assert_eq!(library.stored(&other), dense(&[T4]));
        assert_eq!(
            library.get(owner, &other).map(|row| row.changed_at),
            Some(NOW)
        );

        assert_eq!(library.replace(owner, &id, &[]), Ok(()));
        assert_eq!(library.stored(&id), []);
    }

    #[test]
    fn a_refused_write_changes_nothing() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2]);
        let before = library.everything();
        let named = |remove: &[usize], add: &[&str]| Change {
            name: Some("Renamed".to_owned()),
            comment: Some("changed".to_owned()),
            public: Some(true),
            ..entries(remove, add)
        };

        // A position that the playlist does not have, with valid ones.
        for remove in [&[2][..], &[0, 2], &[2, 0], &[usize::MAX]] {
            assert_eq!(
                library.update(owner, &id, &named(remove, &[T3])),
                Err(Refusal::NoPosition),
                "{remove:?}"
            );
        }
        // An unknown track as the first, a middle and the last value.
        for add in [
            [UNKNOWN_TRACK, T3, T4],
            [T3, UNKNOWN_TRACK, T4],
            [T3, T4, UNKNOWN_TRACK],
        ] {
            assert_eq!(
                library.update(owner, &id, &named(&[0], &add)),
                Err(Refusal::NoTrack),
                "{add:?}"
            );
            assert_eq!(library.replace(owner, &id, &add), Err(Refusal::NoTrack));
            let created = library
                .db
                .with_writer(|conn| create(conn, owner, "New", &add, NOW))
                .unwrap();
            assert_eq!(created, Err(Refusal::NoTrack));
        }
        // The position is checked before the track, and the playlist before
        // both.
        assert_eq!(
            library.update(owner, &id, &named(&[2], &[UNKNOWN_TRACK])),
            Err(Refusal::NoPosition)
        );
        assert_eq!(
            library.update(owner, UNKNOWN_PLAYLIST, &named(&[2], &[UNKNOWN_TRACK])),
            Err(Refusal::NoPlaylist)
        );
        assert_eq!(
            library.replace(owner, UNKNOWN_PLAYLIST, &[UNKNOWN_TRACK]),
            Err(Refusal::NoPlaylist)
        );

        assert_eq!(library.everything(), before);
    }

    #[test]
    fn a_write_that_fails_after_its_first_statement_changes_nothing() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2]);
        let before = library.everything();
        // The insert of an entry of `T4` fails. The statements before it
        // have run then: the insert of the playlist row, the deletion of
        // the old entries, the insert of the kept entries.
        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER refuse_t4 BEFORE INSERT ON playlist_tracks
                     WHEN NEW.track_id = 'tr-00000000000000000000000000000004'
                     BEGIN SELECT RAISE(ABORT, 'the test refuses the entry'); END;",
                )?;
                Ok(())
            })
            .unwrap();

        let created = library
            .db
            .with_writer(|conn| create(conn, owner, "New", &[T3, T4], NOW));
        assert!(created.is_err(), "{created:?}");
        let replaced = library
            .db
            .with_writer(|conn| replace_tracks(conn, owner, &id, &[T3, T4], LATER));
        assert!(replaced.is_err(), "{replaced:?}");
        // A change that removes an entry writes the kept entries again
        // before it adds the new one.
        let change = Change {
            name: Some("Renamed".to_owned()),
            ..entries(&[0], &[T3, T4])
        };
        let updated = library
            .db
            .with_writer(|conn| update(conn, owner, &id, &change, LATER));
        assert!(updated.is_err(), "{updated:?}");

        assert_eq!(library.everything(), before);
        assert_eq!(library.stored(&id), dense(&[T1, T2]));

        // The connection takes the next write.
        assert_eq!(library.update(owner, &id, &entries(&[0], &[T3])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T2, T3]));
    }

    #[test]
    fn deleting_a_track_row_deletes_its_entries_and_the_next_write_closes_the_gap() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2, T1, T3, T2]);
        let only = library.create(owner, "Only", &[T2]);

        let deleted = library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[T2]))
            .unwrap();
        assert_eq!(deleted, 1);

        // The other rows keep their stored positions, and the readers return
        // the entries that are left in their order.
        assert_eq!(
            library.stored(&id),
            [(0, T1.to_owned()), (2, T1.to_owned()), (3, T3.to_owned())]
        );
        assert_eq!(library.track_ids(owner, &id), [T1, T1, T3]);
        let row = library.get(owner, &id).expect("the playlist");
        assert_eq!(row.song_count, 3);
        assert_eq!(row.duration_ms, 1_100 + 1_100 + 3_300);
        assert_eq!(row.changed_at, NOW);
        let row = library.get(owner, &only).expect("the playlist stays");
        assert_eq!((row.song_count, row.first_track_id), (0, None));

        // A position is a place in the list of the entries that are left:
        // the playlist has no position 3, and position 1 is the second `T1`.
        assert_eq!(
            library.update(owner, &id, &entries(&[3], &[])),
            Err(Refusal::NoPosition)
        );
        assert_eq!(library.update(owner, &id, &entries(&[1], &[])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T1, T3]));
    }

    #[test]
    fn update_appends_to_dense_rows_and_writes_the_rows_again_after_a_gap() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2, T3]);
        let row_ids = |library: &Library| -> Vec<i64> {
            let rows = library.rows(&id).into_iter();
            rows.map(|(row_id, _, _)| row_id).collect()
        };

        // An added entry follows the rows of the playlist, which stay.
        let before = row_ids(&library);
        assert_eq!(library.update(owner, &id, &entries(&[], &[T4, T1])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T1, T2, T3, T4, T1]));
        assert_eq!(row_ids(&library)[..3], before[..]);

        // The deletion of a track row leaves a gap. A change that only adds
        // an entry stores the positions 0 to n - 1 again.
        library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[T2]))
            .unwrap();
        assert_eq!(
            library
                .stored(&id)
                .iter()
                .map(|row| row.0)
                .collect::<Vec<_>>(),
            [0, 2, 3, 4]
        );
        assert_eq!(library.update(owner, &id, &entries(&[], &[T3])), Ok(()));
        assert_eq!(library.stored(&id), dense(&[T1, T3, T4, T1, T3]));

        // A gap at the start: the first entry is not at position 0.
        let other = library.create(owner, "Other", &[T1, T3]);
        library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[T1]))
            .unwrap();
        assert_eq!(library.stored(&other), [(1, T3.to_owned())]);
        assert_eq!(library.update(owner, &other, &entries(&[], &[T4])), Ok(()));
        assert_eq!(library.stored(&other), dense(&[T3, T4]));
    }

    #[test]
    fn delete_removes_the_playlist_with_its_entries() {
        let library = library();
        let owner = library.owner;
        let id = library.create(owner, "Mix", &[T1, T2]);
        let other = library.create(owner, "Other", &[T2]);
        let delete = |id: &str| {
            library
                .db
                .with_writer(|conn| delete(conn, owner, id))
                .unwrap()
        };

        assert!(delete(&id));
        assert_eq!(library.get(owner, &id), None);
        assert_eq!(library.stored(&id), []);
        assert_eq!(library.stored(&other), dense(&[T2]));
        // The tracks stay.
        let tracks = library.db.with_reader(tracks::count_tracks).unwrap();
        assert_eq!(tracks, 4);

        assert!(!delete(&id));
        assert!(!delete(UNKNOWN_PLAYLIST));
        assert_eq!(library.list(owner).len(), 1);
    }

    #[test]
    fn kept_entries_counts_the_entries_of_the_result() {
        let full = || vec!["a".to_owned(); MAX_ENTRIES];
        let kept = |remove: &[usize], add: &[&str]| {
            kept_entries(full(), &entries(remove, add)).map(|kept| kept.len())
        };

        // A full playlist takes no entry more.
        assert_eq!(kept(&[], &[]), Ok(MAX_ENTRIES));
        assert_eq!(kept(&[], &["b"]), Err(Refusal::TooLong));
        // A change that removes as many entries as it adds is accepted, and
        // one that adds one more is refused.
        assert_eq!(kept(&[0, 1], &["b", "c"]), Ok(MAX_ENTRIES - 2));
        assert_eq!(kept(&[1, 0, 1], &["b", "c"]), Ok(MAX_ENTRIES - 2));
        assert_eq!(kept(&[0, 1], &["b", "c", "d"]), Err(Refusal::TooLong));
        // A position that is named twice makes room for one entry.
        assert_eq!(kept(&[5, 5], &["b", "c"]), Err(Refusal::TooLong));
        // The positions are checked before the length.
        assert_eq!(kept(&[MAX_ENTRIES], &["b", "c"]), Err(Refusal::NoPosition));
        assert_eq!(kept(&[MAX_ENTRIES - 1], &["b"]), Ok(MAX_ENTRIES - 1));

        let short = vec!["a".to_owned(); 2];
        let room = vec!["b"; MAX_ENTRIES - 2];
        assert_eq!(
            kept_entries(short.clone(), &entries(&[], &room)).map(|kept| kept.len()),
            Ok(2)
        );
        let no_room = vec!["b"; MAX_ENTRIES - 1];
        assert_eq!(
            kept_entries(short, &entries(&[], &no_room)),
            Err(Refusal::TooLong)
        );
    }

    #[test]
    fn a_playlist_holds_at_most_max_entries() {
        let library = library();
        let owner = library.owner;
        let full = vec![T1; MAX_ENTRIES];
        let too_long = vec![T1; MAX_ENTRIES + 1];
        // The number of the entries of every playlist and their highest
        // position.
        let stored = || {
            library
                .db
                .with_reader(|conn| {
                    let sql = "SELECT count(*), max(position) FROM playlist_tracks";
                    Ok(conn.query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))?)
                })
                .unwrap()
        };
        let limit = MAX_ENTRIES as i64;

        // The limit itself is accepted.
        let id = library.create(owner, "Full", &full);
        assert_eq!(stored(), (limit, Some(limit - 1)));

        // One entry more is refused by every write, and nothing is written.
        let created = library
            .db
            .with_writer(|conn| create(conn, owner, "Long", &too_long, NOW))
            .unwrap();
        assert_eq!(created, Err(Refusal::TooLong));
        assert_eq!(
            library.replace(owner, &id, &too_long),
            Err(Refusal::TooLong)
        );
        assert_eq!(
            library.update(owner, &id, &entries(&[], &[T2])),
            Err(Refusal::TooLong)
        );
        assert_eq!(stored(), (limit, Some(limit - 1)));
        let names: Vec<String> = library
            .list(owner)
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["Full"]);
    }
}
