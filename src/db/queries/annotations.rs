//! Queries on the `annotations` table. A row holds what one user attached to
//! one track, album or artist: a star, a rating and a play count.
//!
//! `item_id` has no foreign key, because it names a row of one of three
//! tables. The scanner removes the rows of items that no longer exist with
//! [`delete_orphans`].
//!
//! [`get_many`] is the one lookup that the API uses to build a response: a
//! handler passes the IDs of the items of its response and gives each model
//! constructor the annotation of its item (`docs/design.md` section 3.8).
//!
//! # Writes
//!
//! [`star`], [`unstar`] and [`set_rating`] write one value of one row. Each
//! leaves the other values of the row as they are, so a star, a rating and a
//! play count of one item do not depend on the order of the requests that
//! set them. No function deletes a row of an item that exists.
//!
//! [`apply`] is what the API calls: it checks that every item of a request
//! exists and writes them in one transaction, so a request changes every
//! item it names or none.

use std::collections::HashMap;

use rusqlite::{Connection, Row, TransactionBehavior};

use super::browse::json_array;
use super::int;
use super::tracks::HAS_FILE_SQL;
use crate::db::Result;
use crate::ids::{self, Kind};

/// What one user attached to one item: the values of one row of the
/// `annotations` table.
///
/// The default value is the annotation of an item that has no row: no star,
/// no rating, a play count of 0 and no play.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotation {
    /// The time the user starred the item, in Unix seconds. `None` for an
    /// item without a star.
    pub starred_at: Option<i64>,
    /// The rating of the user, 1 to 5. `None` for an item without a rating.
    pub rating: Option<u8>,
    /// The number of plays that the row counts.
    pub play_count: u64,
    /// The time of the last play, in Unix seconds. `None` for an item that
    /// was not played.
    pub last_played_at: Option<i64>,
}

/// Returns the annotations of the user `user_id` for the items `item_ids`,
/// keyed by item ID.
///
/// The map holds an entry for each ID that has a row. An item that the user
/// never starred, rated or played has no row and no entry, and its
/// annotation is `Annotation::default()`. The IDs can be of tracks, albums
/// and artists at once, and an ID that `item_ids` holds twice is one entry.
///
/// The IDs are one parameter of the statement, so a list of any length is
/// one statement.
pub fn get_many<S: AsRef<str>>(
    conn: &Connection,
    user_id: i64,
    item_ids: &[S],
) -> Result<HashMap<String, Annotation>> {
    if item_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut select = conn.prepare_cached(
        "SELECT item_id, starred_at, rating, play_count, last_played_at FROM annotations
         WHERE user_id = ?1 AND item_id IN (SELECT value FROM json_each(?2))",
    )?;
    let annotations = select
        .query_map((user_id, json_array(item_ids)), annotation_from_row)?
        .collect::<rusqlite::Result<_>>()?;
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
    for item in items {
        if !exists(&tx, item)? {
            return Ok(false);
        }
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
            has_lyrics: false,
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
        annotate(&db, owner, ARTIST, "artist", (None, None, 3, Some(300)));
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
                (ARTIST.to_owned(), plays),
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
}
