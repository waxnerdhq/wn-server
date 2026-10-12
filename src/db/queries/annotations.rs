//! Queries on the `annotations` table. A row holds what one user attached to
//! one track, album or artist: a star and a rating, and for a track its
//! plays.
//!
//! `item_id` has no foreign key, because it names a row of one of three
//! tables. The scanner removes the rows of items that no longer exist with
//! [`delete_orphans`].
//!
//! [`get_many`] is the one lookup that the API uses to build a response: a
//! handler passes the IDs of the items of its response and gives each model
//! constructor the annotation of its item (`docs/design.md` section 3.8).
//!
//! # Plays
//!
//! A play is the play of a track. [`record_play`] counts it in the row of
//! the track and writes no other row. The play count of an album is the sum
//! of the play counts of its tracks, and the last play of an album is the
//! latest play of one of its tracks. Neither is stored: [`get_many`]
//! computes both from the rows of the tracks, and the album lists of `lists`
//! order the albums by the same two values of the same rows. The row of an
//! album and the row of an artist hold no play. No function writes
//! `play_count` or `last_played_at` of such a row, and [`get_many`] does not
//! read them.
//!
//! # Writes
//!
//! [`star`], [`unstar`], [`set_rating`] and [`record_play`] write the values
//! of one kind of one row. Each leaves the other values of the row as they
//! are, so a star, a rating and the plays of one item do not depend on the
//! order of the requests that set them. No function deletes a row of an item
//! that exists.
//!
//! [`apply`] and [`record_plays`] are what the API calls. Each checks that
//! every item of a request exists and writes them in one transaction, so a
//! request changes every item it names or none.

use std::collections::HashMap;

use rusqlite::{Connection, Row, TransactionBehavior};

use super::browse::json_array;
use super::int;
use super::tracks::HAS_FILE_SQL;
use crate::db::Result;
use crate::ids::{self, Kind};

/// What one user attached to one item: the star and the rating of the row
/// that the `annotations` table holds for the item, and the plays of the
/// item.
///
/// The default value is the annotation of an item that has no row and no
/// play: no star, no rating, a play count of 0 and no play.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotation {
    /// The time the user starred the item, in Unix seconds. `None` for an
    /// item without a star.
    pub starred_at: Option<i64>,
    /// The rating of the user, 1 to 5. `None` for an item without a rating.
    pub rating: Option<u8>,
    /// The number of plays. For a track it is the count of its row. For an
    /// album it is the sum of the counts of its tracks. For an artist it is
    /// 0, because no response reports the plays of an artist.
    pub play_count: u64,
    /// The time of the last play, in Unix seconds. For an album it is the
    /// latest play of one of its tracks. `None` for an item that was not
    /// played, and for an artist.
    pub last_played_at: Option<i64>,
}

/// The statement of [`get_many`] that reads the rows of the items. `?1` is
/// the ID of the user and `?2` the IDs of the items as a JSON array.
///
/// Only the row of a track holds plays (see the module documentation). The
/// plays of every other row read as none, whatever the row holds.
const OWN_ROWS: &str = "SELECT item_id, starred_at, rating,
       CASE item_type WHEN 'track' THEN play_count ELSE 0 END,
       CASE item_type WHEN 'track' THEN last_played_at END
     FROM annotations
     WHERE user_id = ?1 AND item_id IN (SELECT value FROM json_each(?2))";

/// The statement of [`get_many`] that reads the plays of the albums: for
/// each ID of `?2` that is the ID of an album with a played track, the sum
/// of the play counts of its tracks and their latest play. `?1` is the ID of
/// the user and `?2` the IDs of the albums as a JSON array.
///
/// An ID that the list holds twice counts once. The statement starts from
/// the IDs: it finds the tracks of each through the index `tracks_album` and
/// the row of each track through the primary key of `annotations`, so it
/// reads neither table as a whole. `CROSS JOIN` keeps that order of the
/// tables.
const ALBUM_PLAYS: &str = "SELECT wanted.value, sum(played.play_count), max(played.last_played_at)
     FROM (SELECT DISTINCT value FROM json_each(?2)) AS wanted
     CROSS JOIN tracks ON tracks.album_id = wanted.value
     CROSS JOIN annotations AS played
       ON played.user_id = ?1 AND played.item_id = tracks.id
     WHERE played.play_count > 0 OR played.last_played_at IS NOT NULL
     GROUP BY wanted.value";

/// Returns the annotations of the user `user_id` for the items `item_ids`,
/// keyed by item ID.
///
/// The map holds an entry for each ID that has a row, and for each ID of an
/// album of which the user played a track. An item that the user never
/// starred, rated or played has no entry, and its annotation is
/// `Annotation::default()`. The IDs can be of tracks, albums and artists at
/// once, and an ID that `item_ids` holds twice is one entry.
///
/// The star and the rating of an entry are those of the row of the item.
/// The plays of a track are those of its row. The plays of an album are the
/// sum of the play counts of its tracks and the latest of their plays, read
/// from the rows of the tracks (see the module documentation). A track row
/// without a file counts as every other track row.
///
/// The IDs are one parameter of a statement, so a list of any length is one
/// statement for the rows of the items. A second statement reads the plays
/// of the albums, and it runs only when the list holds the ID of an album. A
/// caller that needs the star, the rating and the plays of one state of the
/// database calls the function in a transaction.
pub fn get_many<S: AsRef<str>>(
    conn: &Connection,
    user_id: i64,
    item_ids: &[S],
) -> Result<HashMap<String, Annotation>> {
    if item_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut annotations: HashMap<String, Annotation> = conn
        .prepare_cached(OWN_ROWS)?
        .query_map((user_id, json_array(item_ids)), annotation_from_row)?
        .collect::<rusqlite::Result<_>>()?;

    let album_ids: Vec<&str> = item_ids
        .iter()
        .map(AsRef::as_ref)
        .filter(|id| matches!(ids::parse(id), Some((Kind::Album, _))))
        .collect();
    if album_ids.is_empty() {
        return Ok(annotations);
    }
    let mut select = conn.prepare_cached(ALBUM_PLAYS)?;
    let mut albums = select.query((user_id, json_array(&album_ids)))?;
    while let Some(row) = albums.next()? {
        let annotation = annotations.entry(row.get(0)?).or_default();
        annotation.play_count = int::get_u64(row, 1)?;
        annotation.last_played_at = row.get(2)?;
    }
    Ok(annotations)
}

fn annotation_from_row(row: &Row<'_>) -> rusqlite::Result<(String, Annotation)> {
    Ok((
        row.get(0)?,
        Annotation {
            starred_at: row.get(1)?,
            rating: row.get(2)?,
            play_count: int::get_u64(row, 3)?,
            last_played_at: row.get(4)?,
        },
    ))
}

/// Deletes every annotation whose `item_id` is the ID of no track, no album
/// and no artist, and returns the number of rows deleted.
///
/// The ID of a deleted item is never given to another item, so the
/// annotation of a deleted item belongs to nothing.
pub fn delete_orphans(conn: &Connection) -> Result<u64> {
    let deleted = conn
        .prepare_cached(
            "DELETE FROM annotations
             WHERE NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.id = annotations.item_id)
               AND NOT EXISTS (SELECT 1 FROM albums WHERE albums.id = annotations.item_id)
               AND NOT EXISTS (SELECT 1 FROM artists WHERE artists.id = annotations.item_id)",
        )?
        .execute([])?;
    Ok(deleted as u64)
}

/// The kinds of item that a user can star and rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Track,
    Album,
    Artist,
}

impl ItemKind {
    /// The value of `annotations.item_type` for an item of this kind.
    pub const fn item_type(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
        }
    }

    /// The table that holds the items of this kind.
    const fn table(self) -> &'static str {
        match self {
            Self::Track => "tracks",
            Self::Album => "albums",
            Self::Artist => "artists",
        }
    }

    /// The column of `tracks` that holds the ID of the item of this kind
    /// that a track belongs to: the track itself, its album or its artist.
    const fn track_column(self) -> &'static str {
        match self {
            Self::Track => "id",
            Self::Album => "album_id",
            Self::Artist => "artist_id",
        }
    }
}

/// One item that a request names: a track, an album or an artist, by its
/// ID. The kind of an item is the kind that the prefix of its ID names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Item {
    kind: ItemKind,
    id: String,
}

impl Item {
    /// Returns the item that `id` names. Returns `None` for a text that is
    /// not an ID (`ids::parse`) and for the ID of a playlist, which has no
    /// annotation. The item may have no row.
    pub fn parse(id: &str) -> Option<Self> {
        let kind = match ids::parse(id)?.0 {
            Kind::Track => ItemKind::Track,
            Kind::Album => ItemKind::Album,
            Kind::Artist => ItemKind::Artist,
            Kind::Playlist => return None,
        };
        Some(Self {
            kind,
            id: id.to_owned(),
        })
    }

    /// The kind that the prefix of the ID names.
    pub fn kind(&self) -> ItemKind {
        self.kind
    }

    /// The ID as the request holds it.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A rating as `setRating` takes it: 1 to 5, or 0, which removes the rating
/// of the item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rating(u8);

impl Rating {
    /// Returns the rating `value`. Returns `None` for a value outside 0 to
    /// 5.
    pub fn new(value: i64) -> Option<Self> {
        match u8::try_from(value) {
            Ok(value) if value <= 5 => Some(Self(value)),
            _ => None,
        }
    }

    /// The value that the row holds: `None` for 0, because a row holds no
    /// rating below 1.
    fn stored(self) -> Option<u8> {
        (self.0 != 0).then_some(self.0)
    }
}

/// What [`apply`] writes for each item of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Sets the star with the time `at`, in Unix seconds ([`star`]).
    Star { at: i64 },
    /// Removes the star ([`unstar`]).
    Unstar,
    /// Sets or removes the rating ([`set_rating`]).
    Rate(Rating),
}

/// Applies `change` to every item of `items` for the user `user_id`, in one
/// transaction.
///
/// Returns `false` and writes nothing when one item has no row: its ID is
/// the ID of no track, no album or no artist. The check and the writes are
/// in the same transaction, so a scan in another process does not delete an
/// item between the two. When a write fails, the transaction is rolled back
/// and the error is returned. The request therefore changes every item or
/// none.
///
/// An item that `items` holds twice is written twice with the same result.
/// An empty list writes nothing and returns `true`.
pub fn apply(conn: &mut Connection, user_id: i64, items: &[Item], change: Change) -> Result<bool> {
    if items.is_empty() {
        return Ok(true);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !all_exist(&tx, items)? {
        return Ok(false);
    }
    for item in items {
        match change {
            Change::Star { at } => star(&tx, user_id, &item.id, item.kind, at)?,
            Change::Unstar => unstar(&tx, user_id, &item.id)?,
            Change::Rate(rating) => set_rating(&tx, user_id, &item.id, item.kind, rating)?,
        }
    }
    tx.commit()?;
    Ok(true)
}

/// One play of a track, as a `scrobble` request reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Play {
    track: Item,
    at: Option<i64>,
}

impl Play {
    /// Returns the play of the track `track`. `at` is the time of the play
    /// in Unix seconds. A play without a time is counted at the time that
    /// [`record_plays`] is given.
    ///
    /// Returns `None` when `track` is an album or an artist: a play is the
    /// play of a track.
    pub fn new(track: Item, at: Option<i64>) -> Option<Self> {
        (track.kind == ItemKind::Track).then_some(Self { track, at })
    }

    /// The track that was played.
    pub fn track(&self) -> &Item {
        &self.track
    }
}

/// Records every play of `plays` for the user `user_id`, in one
/// transaction. `now` is the time of each play that has none, in Unix
/// seconds.
///
/// Returns `false` and writes nothing when the track of one play has no
/// row. The check and the writes are in the same transaction, so a scan in
/// another process does not delete a track between the two. When a write
/// fails, the transaction is rolled back and the error is returned. The
/// request therefore counts every play or none.
///
/// Each play is counted with [`record_play`]. A track that `plays` holds
/// several times gets one play for each, also when two of them have one
/// time. The result does not depend on the order of `plays`. An empty list
/// writes nothing and returns `true`.
pub fn record_plays(conn: &mut Connection, user_id: i64, plays: &[Play], now: i64) -> Result<bool> {
    if plays.is_empty() {
        return Ok(true);
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !all_exist(&tx, plays.iter().map(Play::track))? {
        return Ok(false);
    }
    for play in plays {
        record_play(&tx, user_id, &play.track.id, play.at.unwrap_or(now))?;
    }
    tx.commit()?;
    Ok(true)
}

/// Returns whether every item of `items` has a row in the table of its
/// kind. A list without an item has no missing item.
///
/// A caller that writes afterwards calls the function in the transaction of
/// its writes ([`apply`], [`record_plays`]).
pub fn all_exist<'a>(conn: &Connection, items: impl IntoIterator<Item = &'a Item>) -> Result<bool> {
    for item in items {
        if !exists(conn, item)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Returns whether `item` has a row in the table of its kind.
fn exists(conn: &Connection, item: &Item) -> Result<bool> {
    let found = conn
        .prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE id = ?1)",
            item.kind.table()
        ))?
        .query_row([&item.id], |row| row.get(0))?;
    Ok(found)
}

/// Counts one play of the track `track_id` by the user `user_id` at the
/// time `at`, in Unix seconds.
///
/// The play count of the row grows by 1 with every call. Two calls with one
/// track and one time are two plays: the API has no value that tells a
/// repeated report from a second play, and Navidrome and Airsonic count
/// every report as well.
///
/// The time of the last play becomes `at` when the track has no play, and
/// the later of the stored time and `at` otherwise. A client can report a
/// play long after it happened, and several plays in any order, so a play
/// that is reported late does not move the last play back. Navidrome stores
/// the later time as well. `at` is stored as it is given, also when it is
/// far from the time of the call.
///
/// The star and the rating of the row stay as they are. A track without a
/// row gets one. The row is that of the track: the function writes no row
/// of the album or of the artist (see the module documentation).
///
/// The function does not check that the track exists; [`record_plays`]
/// does.
pub fn record_play(conn: &Connection, user_id: i64, track_id: &str, at: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO annotations (user_id, item_id, item_type, play_count, last_played_at)
         VALUES (?1, ?2, ?3, 1, ?4)
         ON CONFLICT (user_id, item_id) DO UPDATE SET
           play_count = play_count + 1,
           last_played_at = max(coalesce(last_played_at, excluded.last_played_at),
             excluded.last_played_at)",
    )?
    .execute((user_id, track_id, ItemKind::Track.item_type(), at))?;
    Ok(())
}

/// Stars the item `item_id` of the kind `kind` for the user `user_id`, with
/// `at` as the time of the star in Unix seconds.
///
/// An item that has a star keeps it and takes `at` as its time: the time of
/// a star is the time of the last request that set it. The API does not
/// define that case, and Navidrome and Airsonic do the same. The rating, the
/// play count and the time of the last play of the row stay as they are. An
/// item without a row gets one.
///
/// The function does not check that the item exists; [`apply`] does.
pub fn star(conn: &Connection, user_id: i64, item_id: &str, kind: ItemKind, at: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO annotations (user_id, item_id, item_type, starred_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, item_id) DO UPDATE SET starred_at = excluded.starred_at",
    )?
    .execute((user_id, item_id, kind.item_type(), at))?;
    Ok(())
}

/// Removes the star of the item `item_id` for the user `user_id`.
///
/// An item without a star, and an item without a row, stay as they are, and
/// that is no error. The rating, the play count and the time of the last
/// play of the row stay as they are, and the row is not deleted.
pub fn unstar(conn: &Connection, user_id: i64, item_id: &str) -> Result<()> {
    conn.prepare_cached(
        "UPDATE annotations SET starred_at = NULL
         WHERE user_id = ?1 AND item_id = ?2 AND starred_at IS NOT NULL",
    )?
    .execute((user_id, item_id))?;
    Ok(())
}

/// Sets the rating of the item `item_id` of the kind `kind` for the user
/// `user_id`. A rating of 0 removes the rating.
///
/// The star, the play count and the time of the last play of the row stay
/// as they are. A rating from 1 to 5 gives an item without a row a row. A
/// rating of 0 writes nothing for an item without a rating, and deletes no
/// row.
///
/// The function does not check that the item exists; [`apply`] does.
pub fn set_rating(
    conn: &Connection,
    user_id: i64,
    item_id: &str,
    kind: ItemKind,
    rating: Rating,
) -> Result<()> {
    match rating.stored() {
        Some(rating) => conn
            .prepare_cached(
                "INSERT INTO annotations (user_id, item_id, item_type, rating)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (user_id, item_id) DO UPDATE SET rating = excluded.rating",
            )?
            .execute((user_id, item_id, kind.item_type(), rating))?,
        None => conn
            .prepare_cached(
                "UPDATE annotations SET rating = NULL
                 WHERE user_id = ?1 AND item_id = ?2 AND rating IS NOT NULL",
            )?
            .execute((user_id, item_id))?,
    };
    Ok(())
}

/// Returns the IDs of the items of the kind `kind` that the user `user_id`
/// starred: the item that was starred last is the first, and items with the
/// same time are in the order of their IDs.
///
/// With `folder_id` the result holds the items of that music folder: a track
/// whose file is in it, and an album or an artist that has such a track. A
/// track without a file is in no folder (`docs/design.md` section 3.4). An
/// ID that no folder has gives no item. Without `folder_id` a starred track
/// without a file is part of the result.
///
/// The result can hold the ID of an item that no longer has a row, until a
/// scan removes its annotation ([`delete_orphans`]). The `_by_ids` functions
/// of `browse` read the rows of the result and leave such an ID out.
pub fn starred(
    conn: &Connection,
    user_id: i64,
    kind: ItemKind,
    folder_id: Option<i64>,
) -> Result<Vec<String>> {
    let mut select = conn.prepare_cached(&format!(
        "SELECT item_id FROM annotations
         WHERE user_id = ?1 AND item_type = ?2 AND starred_at IS NOT NULL
           AND (?3 IS NULL OR EXISTS (
             SELECT 1 FROM tracks
             WHERE tracks.{} = annotations.item_id AND tracks.folder_id = ?3 AND {HAS_FILE_SQL}))
         ORDER BY starred_at DESC, item_id",
        kind.track_column()
    ))?;
    let ids = select
        .query_map((user_id, kind.item_type(), folder_id), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::params;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::TrackContent;
    use crate::db::queries::{albums, artists, folders, tracks, users};
    use crate::ids::{self, Kind};
    use crate::library::tags::TrackMeta;

    fn meta() -> TrackMeta {
        TrackMeta {
            title: "Low Tide".to_owned(),
            artist: "Aurora Field".to_owned(),
            album_artist: "Aurora Field".to_owned(),
            album: "Glass Harbour".to_owned(),
            track_no: Some(1),
            disc_no: None,
            year: None,
            genre: None,
            compilation: false,
            duration_ms: 500,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            has_picture: false,
            suffix: "flac".to_owned(),
            content_type: "audio/flac".to_owned(),
            lossless: true,
        }
    }

    /// Returns the `item_id` of every annotation in ID order.
    fn item_ids(conn: &Connection) -> Result<Vec<String>> {
        let mut select = conn.prepare("SELECT item_id FROM annotations ORDER BY item_id")?;
        let ids = select
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    #[test]
    fn delete_orphans_keeps_the_annotations_of_items_that_exist() {
        let db = Db::open_memory().unwrap();
        let track_id = ids::new_id(Kind::Track);
        let (artist_id, album_id) = db
            .with_writer(|conn| {
                let user = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
                let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?.remove(0);
                let artist_id = artists::upsert_by_key(conn, "Aurora Field")?;
                let album_id = albums::upsert_by_key(
                    conn,
                    &artist_id,
                    "Glass Harbour",
                    "Aurora Field",
                    false,
                )?;
                let meta = meta();
                let content = TrackContent {
                    fingerprint: "100:aa",
                    mtime: 1000,
                    size: 100,
                    meta: &meta,
                    artist_id: &artist_id,
                    album_id: &album_id,
                };
                tracks::insert_track(conn, &track_id, folder.id, "a.flac", &content)?;

                // One annotation for each item that exists and one for an ID
                // of each kind that no row has.
                let gone = [Kind::Track, Kind::Album, Kind::Artist].map(ids::new_id);
                let items = [
                    (track_id.as_str(), "track"),
                    (album_id.as_str(), "album"),
                    (artist_id.as_str(), "artist"),
                    (gone[0].as_str(), "track"),
                    (gone[1].as_str(), "album"),
                    (gone[2].as_str(), "artist"),
                ];
                for (item_id, item_type) in items {
                    conn.execute(
                        "INSERT INTO annotations (user_id, item_id, item_type, starred_at)
                         VALUES (?1, ?2, ?3, 100)",
                        params![user.id, item_id, item_type],
                    )?;
                }
                Ok((artist_id, album_id))
            })
            .unwrap();

        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 3);
        let mut expected = vec![track_id, album_id, artist_id];
        expected.sort();
        assert_eq!(db.with_reader(item_ids).unwrap(), expected);

        // Nothing is left to delete.
        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 0);
        assert_eq!(db.with_reader(item_ids).unwrap(), expected);
    }

    #[test]
    fn delete_orphans_on_an_empty_table_deletes_nothing() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 0);
    }

    const TRACK: &str = "tr-00000000000000000000000000000001";
    const ALBUM: &str = "al-00000000000000000000000000000002";
    const ARTIST: &str = "ar-00000000000000000000000000000003";
    const OTHER: &str = "tr-00000000000000000000000000000004";
    const PLAYED: &str = "tr-00000000000000000000000000000005";

    /// A database with the owner and a second user. Returns it with the IDs
    /// of the two.
    fn two_users() -> (Db, i64, i64) {
        let db = Db::open_memory().unwrap();
        let (owner, second) = db
            .with_writer(|conn| {
                let owner = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
                conn.execute(
                    "INSERT INTO users (name, role, password, created_at, updated_at)
                     VALUES ('second', 'guest', 'x', 1, 1)",
                    [],
                )?;
                Ok((owner.id, conn.last_insert_rowid()))
            })
            .unwrap();
        (db, owner, second)
    }

    /// Inserts one row of the table. `values` are `starred_at`, `rating`,
    /// `play_count` and `last_played_at`.
    fn annotate(
        db: &Db,
        user_id: i64,
        item_id: &str,
        item_type: &str,
        values: (Option<i64>, Option<u8>, i64, Option<i64>),
    ) {
        db.with_writer(|conn| {
            conn.execute(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    user_id, item_id, item_type, values.0, values.1, values.2, values.3
                ],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn lookup(db: &Db, user_id: i64, item_ids: &[&str]) -> HashMap<String, Annotation> {
        db.with_reader(|conn| get_many(conn, user_id, item_ids))
            .unwrap()
    }

    #[test]
    fn the_default_annotation_has_no_star_no_rating_and_no_play() {
        assert_eq!(
            Annotation::default(),
            Annotation {
                starred_at: None,
                rating: None,
                play_count: 0,
                last_played_at: None,
            }
        );
    }

    #[test]
    fn get_many_on_a_new_database_is_empty() {
        let (db, owner, _) = two_users();
        assert_eq!(lookup(&db, owner, &[TRACK, ALBUM, ARTIST]), HashMap::new());
        assert_eq!(lookup(&db, owner, &[]), HashMap::new());
    }

    #[test]
    fn get_many_returns_the_row_of_an_inserted_annotation() {
        let (db, owner, _) = two_users();
        annotate(
            &db,
            owner,
            TRACK,
            "track",
            (Some(1_700_000_000), Some(4), 7, Some(1_700_000_500)),
        );

        let expected = Annotation {
            starred_at: Some(1_700_000_000),
            rating: Some(4),
            play_count: 7,
            last_played_at: Some(1_700_000_500),
        };
        assert_eq!(
            lookup(&db, owner, &[TRACK]),
            HashMap::from([(TRACK.to_owned(), expected)])
        );
    }

    #[test]
    fn get_many_returns_one_entry_for_each_id_that_has_a_row() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (Some(100), None, 0, None));
        annotate(&db, owner, ALBUM, "album", (None, Some(5), 0, None));
        annotate(&db, owner, ARTIST, "artist", (Some(200), Some(2), 0, None));
        annotate(&db, owner, PLAYED, "track", (None, None, 3, Some(300)));
        // A row that the lookup does not ask for.
        annotate(
            &db,
            owner,
            OTHER,
            "track",
            (Some(400), Some(1), 1, Some(400)),
        );

        let found = lookup(
            &db,
            owner,
            &[
                ARTIST,
                "al-ffffffffffffffffffffffffffffffff",
                TRACK,
                PLAYED,
                ALBUM,
                TRACK,
            ],
        );
        let star = Annotation {
            starred_at: Some(100),
            ..Annotation::default()
        };
        let rating = Annotation {
            rating: Some(5),
            ..Annotation::default()
        };
        let both = Annotation {
            starred_at: Some(200),
            rating: Some(2),
            ..Annotation::default()
        };
        let plays = Annotation {
            play_count: 3,
            last_played_at: Some(300),
            ..Annotation::default()
        };
        assert_eq!(
            found,
            HashMap::from([
                (TRACK.to_owned(), star),
                (ALBUM.to_owned(), rating),
                (ARTIST.to_owned(), both),
                (PLAYED.to_owned(), plays),
            ])
        );
    }

    #[test]
    fn get_many_returns_the_rows_of_the_user_and_of_no_other() {
        let (db, owner, second) = two_users();
        annotate(
            &db,
            owner,
            TRACK,
            "track",
            (Some(100), Some(2), 1, Some(100)),
        );
        annotate(&db, second, TRACK, "track", (None, Some(5), 9, Some(900)));
        annotate(&db, second, ALBUM, "album", (Some(200), None, 0, None));

        let of_owner = lookup(&db, owner, &[TRACK, ALBUM]);
        assert_eq!(of_owner.len(), 1);
        assert_eq!(of_owner[TRACK].rating, Some(2));
        assert_eq!(of_owner[TRACK].play_count, 1);

        let of_second = lookup(&db, second, &[TRACK, ALBUM]);
        assert_eq!(of_second.len(), 2);
        assert_eq!(of_second[TRACK].rating, Some(5));
        assert_eq!(of_second[ALBUM].starred_at, Some(200));

        // An ID of a user that does not exist has no row.
        assert_eq!(
            lookup(&db, owner + second + 1, &[TRACK, ALBUM]),
            HashMap::new()
        );
    }

    #[test]
    fn get_many_reads_a_list_longer_than_the_parameter_limit_of_sqlite() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (Some(100), None, 0, None));
        let ids: Vec<&str> = [OTHER, TRACK].repeat(20_000);
        let found = lookup(&db, owner, &ids);
        assert_eq!(found.len(), 1);
        assert_eq!(found[TRACK].starred_at, Some(100));
    }

    #[test]
    fn get_many_takes_an_id_as_text_and_not_as_sql() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (Some(100), None, 0, None));
        let hostile = [
            "x' OR '1'='1",
            "\"]) OR 1=1 --",
            "%",
            "[\"tr-00000000000000000000000000000001\"]",
        ];
        assert_eq!(lookup(&db, owner, &hostile), HashMap::new());
    }

    /// Every row of the table as `(user_id, item_id, item_type, starred_at,
    /// rating, play_count, last_played_at)`, in the order of the users and
    /// then of the item IDs.
    type Stored = (
        i64,
        String,
        String,
        Option<i64>,
        Option<u8>,
        i64,
        Option<i64>,
    );

    fn stored(db: &Db) -> Vec<Stored> {
        db.with_reader(|conn| {
            let mut select = conn.prepare(
                "SELECT user_id, item_id, item_type, starred_at, rating, play_count,
                   last_played_at
                 FROM annotations ORDER BY user_id, item_id",
            )?;
            let rows = select
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap()
    }

    fn rating(value: i64) -> Rating {
        Rating::new(value).expect("a rating from 0 to 5")
    }

    fn item(id: &str) -> Item {
        Item::parse(id).expect("the ID of a track, an album or an artist")
    }

    #[test]
    fn an_item_is_a_track_an_album_or_an_artist() {
        assert_eq!(item(TRACK).kind(), ItemKind::Track);
        assert_eq!(item(ALBUM).kind(), ItemKind::Album);
        assert_eq!(item(ARTIST).kind(), ItemKind::Artist);
        assert_eq!(item(TRACK).id(), TRACK);

        for id in [
            "",
            "tr-",
            "1",
            "pl-00000000000000000000000000000001",
            "TR-00000000000000000000000000000001",
            "tr-0000000000000000000000000000000G",
            "tr-0000000000000000000000000000001",
            " tr-00000000000000000000000000000001",
            "tr-00000000000000000000000000000001' OR '1'='1",
        ] {
            assert_eq!(Item::parse(id), None, "{id:?}");
        }
    }

    #[test]
    fn item_kinds_have_the_item_types_of_the_schema() {
        assert_eq!(ItemKind::Track.item_type(), "track");
        assert_eq!(ItemKind::Album.item_type(), "album");
        assert_eq!(ItemKind::Artist.item_type(), "artist");
    }

    #[test]
    fn a_rating_is_from_0_to_5() {
        for value in 0..=5 {
            assert!(Rating::new(value).is_some(), "{value}");
        }
        assert_eq!(rating(0).stored(), None);
        assert_eq!(rating(1).stored(), Some(1));
        assert_eq!(rating(5).stored(), Some(5));
        for value in [-1, 6, 255, 256, 261, i64::MIN, i64::MAX] {
            assert_eq!(Rating::new(value), None, "{value}");
        }
    }

    #[test]
    fn star_gives_an_item_without_a_row_a_row_with_the_time() {
        let (db, owner, _) = two_users();
        db.with_writer(|conn| {
            star(conn, owner, TRACK, ItemKind::Track, 100)?;
            star(conn, owner, ALBUM, ItemKind::Album, 200)?;
            star(conn, owner, ARTIST, ItemKind::Artist, 300)
        })
        .unwrap();
        assert_eq!(
            stored(&db),
            [
                (
                    owner,
                    ALBUM.to_owned(),
                    "album".to_owned(),
                    Some(200),
                    None,
                    0,
                    None
                ),
                (
                    owner,
                    ARTIST.to_owned(),
                    "artist".to_owned(),
                    Some(300),
                    None,
                    0,
                    None
                ),
                (
                    owner,
                    TRACK.to_owned(),
                    "track".to_owned(),
                    Some(100),
                    None,
                    0,
                    None
                ),
            ]
        );
    }

    #[test]
    fn a_repeated_star_takes_the_time_of_the_last_call_and_keeps_the_other_values() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (None, Some(4), 7, Some(50)));
        let star_at = |at: i64| {
            db.with_writer(|conn| star(conn, owner, TRACK, ItemKind::Track, at))
                .unwrap();
            lookup(&db, owner, &[TRACK])[TRACK]
        };
        let with_star = |starred_at: i64| Annotation {
            starred_at: Some(starred_at),
            rating: Some(4),
            play_count: 7,
            last_played_at: Some(50),
        };

        assert_eq!(star_at(100), with_star(100));
        assert_eq!(star_at(200), with_star(200));
        // The same call again changes nothing.
        assert_eq!(star_at(200), with_star(200));
        // The time is the time of the last call, also when it is earlier.
        assert_eq!(star_at(150), with_star(150));
        assert_eq!(stored(&db).len(), 1);
    }

    #[test]
    fn unstar_removes_the_star_and_keeps_the_row_and_its_other_values() {
        let (db, owner, _) = two_users();
        annotate(
            &db,
            owner,
            TRACK,
            "track",
            (Some(100), Some(4), 7, Some(50)),
        );
        annotate(&db, owner, ALBUM, "album", (Some(100), None, 0, None));

        db.with_writer(|conn| {
            unstar(conn, owner, TRACK)?;
            unstar(conn, owner, ALBUM)
        })
        .unwrap();
        assert_eq!(
            stored(&db),
            [
                (
                    owner,
                    ALBUM.to_owned(),
                    "album".to_owned(),
                    None,
                    None,
                    0,
                    None
                ),
                (
                    owner,
                    TRACK.to_owned(),
                    "track".to_owned(),
                    None,
                    Some(4),
                    7,
                    Some(50)
                ),
            ]
        );
    }

    #[test]
    fn unstar_of_an_item_without_a_star_is_no_error_and_writes_no_row() {
        let (db, owner, _) = two_users();
        // An item without a row.
        db.with_writer(|conn| unstar(conn, owner, TRACK)).unwrap();
        assert_eq!(stored(&db), []);

        // An item with a row and no star, and a second call for it.
        annotate(&db, owner, ALBUM, "album", (None, Some(2), 3, Some(30)));
        let before = stored(&db);
        db.with_writer(|conn| {
            unstar(conn, owner, ALBUM)?;
            unstar(conn, owner, ALBUM)
        })
        .unwrap();
        assert_eq!(stored(&db), before);
    }

    #[test]
    fn set_rating_sets_replaces_and_removes_the_rating_and_keeps_the_other_values() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (Some(100), None, 7, Some(50)));
        let rate = |value: i64| {
            db.with_writer(|conn| set_rating(conn, owner, TRACK, ItemKind::Track, rating(value)))
                .unwrap();
            lookup(&db, owner, &[TRACK])[TRACK]
        };
        let with_rating = |rating: Option<u8>| Annotation {
            starred_at: Some(100),
            rating,
            play_count: 7,
            last_played_at: Some(50),
        };

        for value in [4, 1, 5, 5] {
            assert_eq!(rate(value), with_rating(u8::try_from(value).ok()));
        }
        assert_eq!(rate(0), with_rating(None));
        // A second removal changes nothing.
        assert_eq!(rate(0), with_rating(None));
        assert_eq!(rate(3), with_rating(Some(3)));
        assert_eq!(stored(&db).len(), 1);
    }

    #[test]
    fn set_rating_gives_an_item_without_a_row_a_row_and_a_removal_gives_none() {
        let (db, owner, _) = two_users();
        db.with_writer(|conn| set_rating(conn, owner, ALBUM, ItemKind::Album, rating(0)))
            .unwrap();
        assert_eq!(stored(&db), []);

        db.with_writer(|conn| set_rating(conn, owner, ALBUM, ItemKind::Album, rating(2)))
            .unwrap();
        assert_eq!(
            stored(&db),
            [(
                owner,
                ALBUM.to_owned(),
                "album".to_owned(),
                None,
                Some(2),
                0,
                None
            )]
        );

        // The removal keeps the row. Its values are those of an item
        // without an annotation.
        db.with_writer(|conn| set_rating(conn, owner, ALBUM, ItemKind::Album, rating(0)))
            .unwrap();
        assert_eq!(
            stored(&db),
            [(
                owner,
                ALBUM.to_owned(),
                "album".to_owned(),
                None,
                None,
                0,
                None
            )]
        );
        assert_eq!(lookup(&db, owner, &[ALBUM])[ALBUM], Annotation::default());
    }

    #[test]
    fn a_star_and_a_rating_do_not_depend_on_the_order_of_the_calls() {
        let star_it = |conn: &Connection, user: i64| star(conn, user, TRACK, ItemKind::Track, 100);
        let rate_it = |conn: &Connection, user: i64| {
            set_rating(conn, user, TRACK, ItemKind::Track, rating(4))
        };
        let unstar_it = |conn: &Connection, user: i64| unstar(conn, user, TRACK);
        let unrate_it = |conn: &Connection, user: i64| {
            set_rating(conn, user, TRACK, ItemKind::Track, rating(0))
        };
        type Call<'a> = &'a dyn Fn(&Connection, i64) -> Result<()>;

        let after = |calls: &[Call<'_>]| {
            let (db, owner, _) = two_users();
            db.with_writer(|conn| {
                for call in calls {
                    call(conn, owner)?;
                }
                Ok(())
            })
            .unwrap();
            lookup(&db, owner, &[TRACK])
                .get(TRACK)
                .copied()
                .unwrap_or_default()
        };
        let both = Annotation {
            starred_at: Some(100),
            rating: Some(4),
            ..Annotation::default()
        };
        assert_eq!(after(&[&star_it, &rate_it]), both);
        assert_eq!(after(&[&rate_it, &star_it]), both);

        // A removal of one value leaves the other, whichever was set first.
        let star_only = Annotation {
            starred_at: Some(100),
            ..Annotation::default()
        };
        let rating_only = Annotation {
            rating: Some(4),
            ..Annotation::default()
        };
        assert_eq!(after(&[&star_it, &rate_it, &unrate_it]), star_only);
        assert_eq!(after(&[&rate_it, &unrate_it, &star_it]), star_only);
        assert_eq!(after(&[&unrate_it, &star_it]), star_only);
        assert_eq!(after(&[&star_it, &rate_it, &unstar_it]), rating_only);
        assert_eq!(after(&[&star_it, &unstar_it, &rate_it]), rating_only);
        assert_eq!(after(&[&unstar_it, &rate_it]), rating_only);
    }

    #[test]
    fn the_writes_change_the_rows_of_the_user_and_of_no_other() {
        let (db, owner, second) = two_users();
        annotate(
            &db,
            second,
            TRACK,
            "track",
            (Some(900), Some(5), 9, Some(900)),
        );
        let of_second = stored(&db);

        db.with_writer(|conn| {
            star(conn, owner, TRACK, ItemKind::Track, 100)?;
            set_rating(conn, owner, TRACK, ItemKind::Track, rating(1))?;
            unstar(conn, owner, TRACK)?;
            set_rating(conn, owner, TRACK, ItemKind::Track, rating(0))
        })
        .unwrap();
        let rows = stored(&db);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.into_iter()
                .filter(|row| row.0 == second)
                .collect::<Vec<_>>(),
            of_second
        );

        // The second user stars the item. The star of the owner stays off.
        db.with_writer(|conn| star(conn, second, TRACK, ItemKind::Track, 950))
            .unwrap();
        assert_eq!(lookup(&db, owner, &[TRACK])[TRACK], Annotation::default());
        assert_eq!(lookup(&db, second, &[TRACK])[TRACK].starred_at, Some(950));
        assert_eq!(
            db.with_reader(|conn| starred(conn, owner, ItemKind::Track, None))
                .unwrap(),
            [] as [&str; 0]
        );
        assert_eq!(
            db.with_reader(|conn| starred(conn, second, ItemKind::Track, None))
                .unwrap(),
            [TRACK]
        );
    }

    #[test]
    fn a_write_for_a_user_that_does_not_exist_is_an_error() {
        let (db, owner, second) = two_users();
        let nobody = owner + second + 1;
        let error = db
            .with_writer(|conn| star(conn, nobody, TRACK, ItemKind::Track, 100))
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
        assert_eq!(stored(&db), []);
    }

    fn starred_ids(db: &Db, user_id: i64, kind: ItemKind, folder_id: Option<i64>) -> Vec<String> {
        db.with_reader(|conn| starred(conn, user_id, kind, folder_id))
            .unwrap()
    }

    #[test]
    fn starred_returns_the_starred_items_of_one_kind_with_the_last_star_first() {
        let (db, owner, _) = two_users();
        let id = |number: u8| format!("tr-{number:032x}");
        // The times are not in the order of the IDs, and two are equal.
        for (number, at) in [(1, 300), (2, 500), (3, 300), (4, 100), (5, 400)] {
            annotate(&db, owner, &id(number), "track", (Some(at), None, 0, None));
        }
        // Rows that are not a starred track of the owner.
        annotate(&db, owner, &id(6), "track", (None, Some(5), 2, Some(900)));
        annotate(&db, owner, ALBUM, "album", (Some(600), None, 0, None));
        annotate(&db, owner, ARTIST, "artist", (Some(700), None, 0, None));

        assert_eq!(
            starred_ids(&db, owner, ItemKind::Track, None),
            [id(2), id(5), id(1), id(3), id(4)]
        );
        assert_eq!(starred_ids(&db, owner, ItemKind::Album, None), [ALBUM]);
        assert_eq!(starred_ids(&db, owner, ItemKind::Artist, None), [ARTIST]);

        // An item leaves the list with its star and returns at the front.
        db.with_writer(|conn| unstar(conn, owner, &id(2))).unwrap();
        assert_eq!(
            starred_ids(&db, owner, ItemKind::Track, None),
            [id(5), id(1), id(3), id(4)]
        );
        db.with_writer(|conn| star(conn, owner, &id(4), ItemKind::Track, 800))
            .unwrap();
        assert_eq!(
            starred_ids(&db, owner, ItemKind::Track, None),
            [id(4), id(5), id(1), id(3)]
        );
    }

    #[test]
    fn starred_on_a_new_database_is_empty() {
        let (db, owner, _) = two_users();
        for kind in [ItemKind::Track, ItemKind::Album, ItemKind::Artist] {
            assert_eq!(starred_ids(&db, owner, kind, None), [] as [&str; 0]);
            assert_eq!(starred_ids(&db, owner, kind, Some(1)), [] as [&str; 0]);
        }
    }

    /// The rows of a library in two music folders.
    struct Library {
        db: Db,
        owner: i64,
        /// The IDs of the two music folders.
        folders: [i64; 2],
        /// The artist of both albums.
        artist: String,
        /// An artist whose one album is in the second folder.
        other_artist: String,
        /// An album with one track in each folder.
        split_album: String,
        /// An album whose one track is in the second folder.
        second_album: String,
        /// The track of `split_album` in the first folder.
        first_track: String,
        /// The track of `split_album` in the second folder.
        second_track: String,
        /// The track of `second_album`.
        other_track: String,
    }

    fn library() -> Library {
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| {
            let owner = users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
            let folders = folders::sync_folders(
                conn,
                &[PathBuf::from("/music/one"), PathBuf::from("/music/two")],
            )?;
            let folders = [folders[0].id, folders[1].id];
            let artist = artists::upsert_by_key(conn, "Aurora Field")?;
            let other_artist = artists::upsert_by_key(conn, "Marlow Quartet")?;
            let split_album =
                albums::upsert_by_key(conn, &artist, "Glass Harbour", "Aurora Field", false)?;
            let second_album =
                albums::upsert_by_key(conn, &other_artist, "Slow Rivers", "Marlow Quartet", false)?;

            let meta = meta();
            let insert = |folder_id: i64, rel_path: &str, artist_id: &str, album_id: &str| {
                let id = ids::new_id(Kind::Track);
                let content = TrackContent {
                    fingerprint: "100:aa",
                    mtime: 1000,
                    size: 100,
                    meta: &meta,
                    artist_id,
                    album_id,
                };
                tracks::insert_track(conn, &id, folder_id, rel_path, &content)?;
                Ok::<_, crate::db::DbError>(id)
            };
            let first_track = insert(folders[0], "a.flac", &artist, &split_album)?;
            let second_track = insert(folders[1], "b.flac", &artist, &split_album)?;
            let other_track = insert(folders[1], "c.flac", &other_artist, &second_album)?;
            Ok(Library {
                db: db.clone(),
                owner: owner.id,
                folders,
                artist,
                other_artist,
                split_album,
                second_album,
                first_track,
                second_track,
                other_track,
            })
        })
        .unwrap()
    }

    impl Library {
        fn items(&self) -> Vec<Item> {
            [
                &self.first_track,
                &self.second_track,
                &self.other_track,
                &self.split_album,
                &self.second_album,
                &self.artist,
                &self.other_artist,
            ]
            .into_iter()
            .map(|id| item(id))
            .collect()
        }

        fn apply(&self, items: &[Item], change: Change) -> Result<bool> {
            self.db
                .with_writer(|conn| apply(conn, self.owner, items, change))
        }

        /// The starred IDs of `kind` in ID order.
        fn starred(&self, kind: ItemKind, folder_id: Option<i64>) -> Vec<String> {
            let mut ids = starred_ids(&self.db, self.owner, kind, folder_id);
            ids.sort();
            ids
        }
    }

    fn sorted<const N: usize>(ids: [&String; N]) -> Vec<String> {
        let mut ids: Vec<String> = ids.into_iter().cloned().collect();
        ids.sort();
        ids
    }

    #[test]
    fn exists_finds_the_row_of_the_kind_of_the_item() {
        let library = library();
        let exists = |id: &str| {
            library
                .db
                .with_reader(|conn| exists(conn, &item(id)))
                .unwrap()
        };
        for present in library.items() {
            assert!(exists(present.id()), "{present:?}");
        }
        for absent in [TRACK, ALBUM, ARTIST] {
            assert!(!exists(absent), "{absent}");
        }
        // The hex part of an ID names a row of its own kind only.
        let as_album = library.first_track.replacen("tr-", "al-", 1);
        assert!(!exists(&as_album));
    }

    #[test]
    fn apply_writes_every_item_of_a_request_with_one_time() {
        let library = library();
        let items = library.items();
        assert!(library.apply(&items, Change::Star { at: 100 }).unwrap());

        let rows = stored(&library.db);
        assert_eq!(rows.len(), 7);
        assert!(rows.iter().all(|row| row.3 == Some(100)), "{rows:?}");
        let types = |item_type: &str| rows.iter().filter(|row| row.2 == item_type).count();
        assert_eq!((types("track"), types("album"), types("artist")), (3, 2, 2));

        assert!(library.apply(&items[..2], Change::Rate(rating(3))).unwrap());
        assert!(library.apply(&items[1..], Change::Unstar).unwrap());
        let rows = stored(&library.db);
        assert_eq!(rows.len(), 7);
        let row_of = |id: &str| rows.iter().find(|row| row.1 == id).expect("a row");
        assert_eq!(
            (
                row_of(&library.first_track).3,
                row_of(&library.first_track).4
            ),
            (Some(100), Some(3))
        );
        assert_eq!(
            (
                row_of(&library.second_track).3,
                row_of(&library.second_track).4
            ),
            (None, Some(3))
        );
        assert_eq!(
            (row_of(&library.artist).3, row_of(&library.artist).4),
            (None, None)
        );
    }

    #[test]
    fn apply_writes_nothing_when_one_item_has_no_row() {
        let library = library();
        let present = library.items();
        library
            .apply(&present[..1], Change::Star { at: 100 })
            .unwrap();
        library
            .apply(&present[..1], Change::Rate(rating(2)))
            .unwrap();
        let before = stored(&library.db);
        assert_eq!(before.len(), 1);

        // The missing item is the first, in the middle and the last.
        for missing in [TRACK, ALBUM, ARTIST] {
            for place in [0, 3, present.len()] {
                let mut items = present.clone();
                items.insert(place, item(missing));
                for change in [
                    Change::Star { at: 200 },
                    Change::Unstar,
                    Change::Rate(rating(5)),
                    Change::Rate(rating(0)),
                ] {
                    assert!(
                        !library.apply(&items, change).unwrap(),
                        "{missing} {place} {change:?}"
                    );
                    assert_eq!(stored(&library.db), before, "{missing} {place} {change:?}");
                }
            }
        }
    }

    #[test]
    fn apply_rolls_back_the_items_it_wrote_when_a_later_write_fails() {
        let library = library();
        let items = library.items();
        library
            .apply(&items[..1], Change::Star { at: 100 })
            .unwrap();
        let before = stored(&library.db);

        // The write of the fourth item fails.
        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(&format!(
                    "CREATE TRIGGER refuse BEFORE INSERT ON annotations
                     WHEN NEW.item_id = '{}'
                     BEGIN SELECT RAISE(ABORT, 'refused'); END;",
                    library.split_album
                ))?;
                Ok(())
            })
            .unwrap();
        for change in [Change::Star { at: 200 }, Change::Rate(rating(5))] {
            let error = library.apply(&items, change).unwrap_err();
            assert!(error.to_string().contains("refused"), "{error}");
            assert_eq!(stored(&library.db), before, "{change:?}");
        }

        // The connection is usable, and a request without that item is
        // stored.
        assert!(
            library
                .apply(&items[..3], Change::Star { at: 300 })
                .unwrap()
        );
        let rows = stored(&library.db);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.3 == Some(300)), "{rows:?}");
    }

    #[test]
    fn apply_takes_an_item_twice_and_an_empty_list() {
        let library = library();
        let twice = [item(&library.first_track), item(&library.first_track)];
        assert!(library.apply(&twice, Change::Star { at: 100 }).unwrap());
        assert_eq!(stored(&library.db).len(), 1);

        let before = stored(&library.db);
        for change in [
            Change::Star { at: 200 },
            Change::Unstar,
            Change::Rate(rating(1)),
        ] {
            assert!(library.apply(&[], change).unwrap());
        }
        assert_eq!(stored(&library.db), before);
    }

    #[test]
    fn starred_with_a_folder_returns_the_items_that_have_a_file_in_it() {
        let library = library();
        library
            .apply(&library.items(), Change::Star { at: 100 })
            .unwrap();
        let [one, two] = library.folders;

        assert_eq!(
            library.starred(ItemKind::Track, None),
            sorted([
                &library.first_track,
                &library.second_track,
                &library.other_track
            ])
        );
        assert_eq!(
            library.starred(ItemKind::Track, Some(one)),
            sorted([&library.first_track])
        );
        assert_eq!(
            library.starred(ItemKind::Track, Some(two)),
            sorted([&library.second_track, &library.other_track])
        );

        // The album with a track in each folder is in both.
        assert_eq!(
            library.starred(ItemKind::Album, Some(one)),
            sorted([&library.split_album])
        );
        assert_eq!(
            library.starred(ItemKind::Album, Some(two)),
            sorted([&library.split_album, &library.second_album])
        );
        assert_eq!(
            library.starred(ItemKind::Artist, Some(one)),
            sorted([&library.artist])
        );
        assert_eq!(
            library.starred(ItemKind::Artist, Some(two)),
            sorted([&library.artist, &library.other_artist])
        );

        // No item is in a folder that does not exist.
        for kind in [ItemKind::Track, ItemKind::Album, ItemKind::Artist] {
            assert_eq!(library.starred(kind, Some(one + two + 1)), [] as [&str; 0]);
        }
    }

    #[test]
    fn a_starred_track_without_a_file_is_listed_and_is_in_no_folder() {
        let library = library();
        library
            .apply(&library.items(), Change::Star { at: 100 })
            .unwrap();
        let [one, two] = library.folders;
        // The one track of the first folder loses its file.
        library
            .db
            .with_writer(|conn| {
                let placeholder = tracks::placeholder_path(&library.first_track);
                tracks::relink_track(conn, &library.first_track, one, &placeholder, 1000, 100)
            })
            .unwrap();

        assert_eq!(
            library.starred(ItemKind::Track, None),
            sorted([
                &library.first_track,
                &library.second_track,
                &library.other_track
            ])
        );
        assert_eq!(library.starred(ItemKind::Album, None).len(), 2);
        assert_eq!(library.starred(ItemKind::Artist, None).len(), 2);
        // Nothing has a file in the first folder.
        for kind in [ItemKind::Track, ItemKind::Album, ItemKind::Artist] {
            assert_eq!(library.starred(kind, Some(one)), [] as [&str; 0]);
        }
        assert_eq!(library.starred(ItemKind::Track, Some(two)).len(), 2);

        // The track takes a star, a rating and their removal as every other
        // track does.
        let track = [item(&library.first_track)];
        assert!(library.apply(&track, Change::Unstar).unwrap());
        assert!(library.apply(&track, Change::Rate(rating(5))).unwrap());
        assert!(library.apply(&track, Change::Star { at: 200 }).unwrap());
        assert_eq!(
            lookup(&library.db, library.owner, &[&library.first_track])[&library.first_track],
            Annotation {
                starred_at: Some(200),
                rating: Some(5),
                ..Annotation::default()
            }
        );
    }

    #[test]
    fn starred_lists_an_item_whose_row_is_gone_until_its_annotation_is_deleted() {
        let library = library();
        library
            .apply(&library.items(), Change::Star { at: 100 })
            .unwrap();
        library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[&library.other_track]))
            .unwrap();

        assert_eq!(library.starred(ItemKind::Track, None).len(), 3);
        // The item is in no folder, because no track row has its ID.
        assert_eq!(
            library.starred(ItemKind::Track, Some(library.folders[1])),
            sorted([&library.second_track])
        );
        library.db.with_writer(|conn| delete_orphans(conn)).unwrap();
        assert_eq!(
            library.starred(ItemKind::Track, None),
            sorted([&library.first_track, &library.second_track])
        );
    }

    /// An annotation of `play_count` plays, the last at `last_played_at`.
    fn played(play_count: u64, last_played_at: i64) -> Annotation {
        Annotation {
            play_count,
            last_played_at: Some(last_played_at),
            ..Annotation::default()
        }
    }

    /// Counts one play of `track_id` by `user_id` at each time of `times`,
    /// in that order.
    fn play_at(db: &Db, user_id: i64, track_id: &str, times: &[i64]) {
        db.with_writer(|conn| {
            for at in times {
                record_play(conn, user_id, track_id, *at)?;
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn record_play_gives_a_track_without_a_row_a_row_with_one_play() {
        let (db, owner, _) = two_users();
        play_at(&db, owner, TRACK, &[1_500_000_000]);
        assert_eq!(
            stored(&db),
            [(
                owner,
                TRACK.to_owned(),
                "track".to_owned(),
                None,
                None,
                1,
                Some(1_500_000_000)
            )]
        );
        assert_eq!(
            lookup(&db, owner, &[TRACK])[TRACK],
            played(1, 1_500_000_000)
        );
    }

    #[test]
    fn record_play_counts_every_call_and_keeps_the_star_and_the_rating() {
        let (db, owner, _) = two_users();
        annotate(&db, owner, TRACK, "track", (Some(100), Some(4), 0, None));
        let with_plays = |play_count: u64, last_played_at: i64| Annotation {
            starred_at: Some(100),
            rating: Some(4),
            ..played(play_count, last_played_at)
        };
        let play = |at: i64| {
            play_at(&db, owner, TRACK, &[at]);
            lookup(&db, owner, &[TRACK])[TRACK]
        };

        assert_eq!(play(500), with_plays(1, 500));
        assert_eq!(play(700), with_plays(2, 700));
        // A call with the track and the time of the call before it is one
        // more play.
        assert_eq!(play(700), with_plays(3, 700));
        assert_eq!(stored(&db).len(), 1);
    }

    #[test]
    fn the_last_play_is_the_latest_time_whatever_the_order_of_the_calls() {
        let (db, owner, _) = two_users();
        let play = |at: i64| {
            play_at(&db, owner, TRACK, &[at]);
            lookup(&db, owner, &[TRACK])[TRACK]
        };
        // A time long before the others is stored as it is given.
        assert_eq!(play(86_400), played(1, 86_400));
        assert_eq!(play(900), played(2, 86_400));
        assert_eq!(play(1_700_000_000), played(3, 1_700_000_000));
        // A play that is reported late counts and keeps the last play.
        assert_eq!(play(1_600_000_000), played(4, 1_700_000_000));
        assert_eq!(play(0), played(5, 1_700_000_000));

        // Every order of three times gives the same row.
        let orders: [[i64; 3]; 6] = [
            [100, 200, 300],
            [100, 300, 200],
            [200, 100, 300],
            [200, 300, 100],
            [300, 100, 200],
            [300, 200, 100],
        ];
        for order in orders {
            let (db, owner, _) = two_users();
            play_at(&db, owner, TRACK, &order);
            assert_eq!(
                lookup(&db, owner, &[TRACK])[TRACK],
                played(3, 300),
                "{order:?}"
            );
        }
    }

    #[test]
    fn a_play_a_star_and_a_rating_do_not_depend_on_the_order_of_the_calls() {
        let star_it = |conn: &Connection, user: i64| star(conn, user, TRACK, ItemKind::Track, 100);
        let rate_it = |conn: &Connection, user: i64| {
            set_rating(conn, user, TRACK, ItemKind::Track, rating(4))
        };
        let play_it = |conn: &Connection, user: i64| record_play(conn, user, TRACK, 500);
        let unstar_it = |conn: &Connection, user: i64| unstar(conn, user, TRACK);
        let unrate_it = |conn: &Connection, user: i64| {
            set_rating(conn, user, TRACK, ItemKind::Track, rating(0))
        };
        type Call<'a> = &'a dyn Fn(&Connection, i64) -> Result<()>;

        let after = |calls: &[Call<'_>]| {
            let (db, owner, _) = two_users();
            db.with_writer(|conn| {
                for call in calls {
                    call(conn, owner)?;
                }
                Ok(())
            })
            .unwrap();
            lookup(&db, owner, &[TRACK])
                .get(TRACK)
                .copied()
                .unwrap_or_default()
        };
        let all = Annotation {
            starred_at: Some(100),
            rating: Some(4),
            ..played(1, 500)
        };
        assert_eq!(after(&[&star_it, &rate_it, &play_it]), all);
        assert_eq!(after(&[&play_it, &star_it, &rate_it]), all);
        assert_eq!(after(&[&rate_it, &play_it, &star_it]), all);

        // The removal of the star and of the rating keeps the plays, and a
        // play keeps the removal.
        assert_eq!(
            after(&[&star_it, &rate_it, &play_it, &unstar_it, &unrate_it]),
            played(1, 500)
        );
        assert_eq!(
            after(&[&star_it, &unstar_it, &play_it, &play_it]),
            played(2, 500)
        );
    }

    #[test]
    fn record_play_changes_the_row_of_the_user_and_of_no_other() {
        let (db, owner, second) = two_users();
        annotate(&db, second, TRACK, "track", (None, Some(5), 9, Some(900)));
        play_at(&db, owner, TRACK, &[100, 200]);

        assert_eq!(lookup(&db, owner, &[TRACK])[TRACK], played(2, 200));
        assert_eq!(
            lookup(&db, second, &[TRACK])[TRACK],
            Annotation {
                rating: Some(5),
                ..played(9, 900)
            }
        );

        // A user that does not exist has no plays.
        let nobody = owner + second + 1;
        let error = db
            .with_writer(|conn| record_play(conn, nobody, TRACK, 100))
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
        assert_eq!(stored(&db).len(), 2);
    }

    fn play(id: &str, at: Option<i64>) -> Play {
        Play::new(item(id), at).expect("the ID of a track")
    }

    impl Library {
        fn record(&self, plays: &[Play], now: i64) -> Result<bool> {
            self.db
                .with_writer(|conn| record_plays(conn, self.owner, plays, now))
        }

        /// The annotation of the owner for the item `id`. The default
        /// annotation when the lookup has no entry for it.
        fn annotation(&self, id: &str) -> Annotation {
            lookup(&self.db, self.owner, &[id])
                .get(id)
                .copied()
                .unwrap_or_default()
        }
    }

    #[test]
    fn a_play_is_the_play_of_a_track() {
        assert_eq!(play(TRACK, Some(100)).track(), &item(TRACK));
        for id in [ALBUM, ARTIST] {
            assert_eq!(Play::new(item(id), Some(100)), None, "{id}");
            assert_eq!(Play::new(item(id), None), None, "{id}");
        }
    }

    #[test]
    fn all_exist_is_false_when_one_item_has_no_row() {
        let library = library();
        let all_exist = |items: &[Item]| {
            library
                .db
                .with_reader(|conn| all_exist(conn, items))
                .unwrap()
        };
        let present = library.items();
        assert!(all_exist(&present));
        assert!(all_exist(&[]));
        for missing in [TRACK, ALBUM, ARTIST] {
            for place in [0, 3, present.len()] {
                let mut items = present.clone();
                items.insert(place, item(missing));
                assert!(!all_exist(&items), "{missing} {place}");
            }
        }
    }

    #[test]
    fn record_plays_counts_every_play_of_a_request_in_the_rows_of_the_tracks() {
        let library = library();
        let plays = [
            play(&library.first_track, Some(300)),
            play(&library.other_track, None),
            play(&library.first_track, Some(200)),
            // The track and the time of the first play: one more play.
            play(&library.first_track, Some(300)),
        ];
        assert!(library.record(&plays, 1_000).unwrap());

        assert_eq!(library.annotation(&library.first_track), played(3, 300));
        // A play without a time has the time that the call is given.
        assert_eq!(library.annotation(&library.other_track), played(1, 1_000));
        assert_eq!(
            library.annotation(&library.second_track),
            Annotation::default()
        );

        // The rows are those of the two tracks. No album and no artist has
        // a row.
        let rows = stored(&library.db);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.2 == "track"), "{rows:?}");

        // The same request again counts every play again.
        assert!(library.record(&plays, 2_000).unwrap());
        assert_eq!(library.annotation(&library.first_track), played(6, 300));
        assert_eq!(library.annotation(&library.other_track), played(2, 2_000));
        assert_eq!(stored(&library.db).len(), 2);
    }

    #[test]
    fn record_plays_writes_nothing_when_one_track_has_no_row() {
        let library = library();
        let present = [
            play(&library.first_track, Some(100)),
            play(&library.second_track, None),
            play(&library.other_track, Some(300)),
        ];
        assert!(library.record(&present[..1], 50).unwrap());
        let before = stored(&library.db);
        assert_eq!(before.len(), 1);

        // The missing track is the first, in the middle and the last. An ID
        // of the hex part of an album is the ID of no track.
        let as_track = library.split_album.replacen("al-", "tr-", 1);
        for missing in [TRACK, as_track.as_str()] {
            for place in [0, 1, present.len()] {
                let mut plays = present.to_vec();
                plays.insert(place, play(missing, Some(200)));
                assert!(!library.record(&plays, 400).unwrap(), "{missing} {place}");
                assert_eq!(stored(&library.db), before, "{missing} {place}");
            }
            assert!(!library.record(&[play(missing, None)], 400).unwrap());
        }
        assert_eq!(stored(&library.db), before);

        // The same plays without that track are counted.
        assert!(library.record(&present, 400).unwrap());
        assert_eq!(library.annotation(&library.first_track), played(2, 100));
        assert_eq!(library.annotation(&library.second_track), played(1, 400));
        assert_eq!(library.annotation(&library.other_track), played(1, 300));
    }

    #[test]
    fn record_plays_rolls_back_the_plays_it_counted_when_a_later_write_fails() {
        let library = library();
        let plays = [
            play(&library.first_track, Some(100)),
            play(&library.second_track, Some(200)),
            play(&library.other_track, Some(300)),
        ];
        assert!(library.record(&plays[..1], 50).unwrap());
        let before = stored(&library.db);

        // The write of the third play fails, after an update of the row of
        // the first track and an insert for the second.
        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(&format!(
                    "CREATE TRIGGER refuse BEFORE INSERT ON annotations
                     WHEN NEW.item_id = '{}'
                     BEGIN SELECT RAISE(ABORT, 'refused'); END;",
                    library.other_track
                ))?;
                Ok(())
            })
            .unwrap();
        let error = library.record(&plays, 400).unwrap_err();
        assert!(error.to_string().contains("refused"), "{error}");
        assert_eq!(stored(&library.db), before);

        // The connection is usable, and a request without that track is
        // counted.
        assert!(library.record(&plays[..2], 400).unwrap());
        assert_eq!(library.annotation(&library.first_track), played(2, 100));
        assert_eq!(library.annotation(&library.second_track), played(1, 200));
        assert_eq!(stored(&library.db).len(), 2);
    }

    #[test]
    fn record_plays_with_an_empty_list_writes_nothing() {
        let library = library();
        assert!(library.record(&[], 100).unwrap());
        assert_eq!(stored(&library.db), []);
    }

    #[test]
    fn get_many_gives_an_album_the_plays_of_its_tracks() {
        let library = library();
        let (first, second, other) = (
            library.first_track.as_str(),
            library.second_track.as_str(),
            library.other_track.as_str(),
        );
        let (split, single) = (library.split_album.as_str(), library.second_album.as_str());

        // No track is played: no album has an entry.
        assert_eq!(
            lookup(&library.db, library.owner, &[split, single]),
            HashMap::new()
        );

        // Two tracks of one album: the sum of the counts and the latest
        // play. The other album has no played track.
        play_at(&library.db, library.owner, first, &[500, 700]);
        play_at(&library.db, library.owner, second, &[900, 100, 300]);
        let found = lookup(
            &library.db,
            library.owner,
            &[split, single, first, second, other, &library.artist],
        );
        assert_eq!(
            found,
            HashMap::from([
                (split.to_owned(), played(5, 900)),
                (first.to_owned(), played(2, 700)),
                (second.to_owned(), played(3, 900)),
            ])
        );

        // An ID that the list holds several times counts once.
        let found = lookup(&library.db, library.owner, &[split, split, first, split]);
        assert_eq!(found[split], played(5, 900));
        assert_eq!(found.len(), 2);

        // The album keeps the star and the rating of its own row.
        let album = [item(split)];
        library.apply(&album, Change::Star { at: 50 }).unwrap();
        library.apply(&album, Change::Rate(rating(4))).unwrap();
        assert_eq!(
            library.annotation(split),
            Annotation {
                starred_at: Some(50),
                rating: Some(4),
                ..played(5, 900)
            }
        );

        // The album of one track has the plays of that track.
        play_at(&library.db, library.owner, other, &[400]);
        assert_eq!(library.annotation(single), played(1, 400));
        assert_eq!(library.annotation(split).play_count, 5);
    }

    #[test]
    fn get_many_reads_no_plays_from_the_row_of_an_album_or_of_an_artist() {
        let library = library();
        let (album, artist) = (library.split_album.as_str(), library.artist.as_str());
        // Rows that no function of the module writes.
        annotate(
            &library.db,
            library.owner,
            album,
            "album",
            (Some(100), None, 7, Some(700)),
        );
        annotate(
            &library.db,
            library.owner,
            artist,
            "artist",
            (None, Some(3), 8, Some(800)),
        );

        let star = Annotation {
            starred_at: Some(100),
            ..Annotation::default()
        };
        let rated = Annotation {
            rating: Some(3),
            ..Annotation::default()
        };
        assert_eq!(library.annotation(album), star);
        assert_eq!(library.annotation(artist), rated);

        // The plays of the album are those of its track, and the artist has
        // none.
        play_at(&library.db, library.owner, &library.first_track, &[200]);
        assert_eq!(
            library.annotation(album),
            Annotation {
                starred_at: Some(100),
                ..played(1, 200)
            }
        );
        assert_eq!(library.annotation(artist), rated);
    }

    #[test]
    fn the_plays_of_an_album_are_those_of_the_user() {
        let library = library();
        let second = library
            .db
            .with_writer(|conn| {
                conn.execute(
                    "INSERT INTO users (name, role, password, created_at, updated_at)
                     VALUES ('second', 'guest', 'x', 1, 1)",
                    [],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .unwrap();
        play_at(&library.db, second, &library.first_track, &[100, 200, 300]);
        play_at(&library.db, library.owner, &library.second_track, &[50]);

        let album = library.split_album.as_str();
        assert_eq!(library.annotation(album), played(1, 50));
        assert_eq!(lookup(&library.db, second, &[album])[album], played(3, 300));
    }

    #[test]
    fn a_track_without_a_file_takes_a_play_and_counts_for_its_album() {
        let library = library();
        let track = library.first_track.as_str();
        play_at(&library.db, library.owner, track, &[100]);
        library
            .db
            .with_writer(|conn| {
                let placeholder = tracks::placeholder_path(track);
                tracks::relink_track(conn, track, library.folders[0], &placeholder, 1000, 100)
            })
            .unwrap();

        // The play from before stays, and a play of the row is counted.
        assert_eq!(library.annotation(track), played(1, 100));
        assert!(library.record(&[play(track, Some(200))], 300).unwrap());
        assert_eq!(library.annotation(track), played(2, 200));
        assert_eq!(library.annotation(&library.split_album), played(2, 200));
    }

    #[test]
    fn the_plays_of_a_track_whose_row_is_gone_leave_its_album() {
        let library = library();
        play_at(
            &library.db,
            library.owner,
            &library.first_track,
            &[100, 200],
        );
        play_at(&library.db, library.owner, &library.second_track, &[50]);
        let album = library.split_album.as_str();
        assert_eq!(library.annotation(album), played(3, 200));

        library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[&library.first_track]))
            .unwrap();
        // The row of the annotation is there until a scan deletes it. The
        // album has the plays of the track that is left.
        assert_eq!(stored(&library.db).len(), 2);
        assert_eq!(library.annotation(album), played(1, 50));
        library.db.with_writer(|conn| delete_orphans(conn)).unwrap();
        assert_eq!(stored(&library.db).len(), 1);
        assert_eq!(library.annotation(album), played(1, 50));
    }

    #[test]
    fn the_statement_of_the_album_plays_reads_no_table_as_a_whole() {
        let db = Db::open_memory().unwrap();
        let plan: Vec<String> = db
            .with_reader(|conn| {
                let mut explain = conn.prepare(&format!("EXPLAIN QUERY PLAN {ALBUM_PLAYS}"))?;
                let steps = explain
                    .query_map((1, "[]"), |row| row.get(3))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(steps)
            })
            .unwrap();
        // A step that reads every row of a table starts with `SCAN` and the
        // name of the table in the statement.
        for step in &plan {
            assert!(
                !step.starts_with("SCAN tracks") && !step.starts_with("SCAN played"),
                "{plan:?}"
            );
        }
        let reads = |start: &str| plan.iter().any(|step| step.starts_with(start));
        assert!(reads("SEARCH tracks USING INDEX tracks_album"), "{plan:?}");
        assert!(
            reads("SEARCH played USING INDEX sqlite_autoindex_annotations_1"),
            "{plan:?}"
        );
    }
}
