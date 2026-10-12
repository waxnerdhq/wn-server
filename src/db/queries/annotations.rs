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

use std::collections::HashMap;

use rusqlite::{Connection, Row};

use super::browse::json_array;
use super::int;
use crate::db::Result;

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
}
