//! Queries on the `artists` table. A row is one album artist.
//!
//! A row is found by its natural key and keeps its ID for as long as it
//! exists. [`upsert_by_key`] is the one function that inserts a row, and
//! [`delete_orphans`] is the one function that deletes rows.

use rusqlite::{Connection, params};

use super::int;
use crate::db::Result;
use crate::ids::{self, Kind};

/// Returns the ID of the artist whose natural key is the key of `name`
/// (`ids::artist_key`). Inserts the row when no artist has the key.
///
/// A new row takes `name` as its name and as its sort name, and an album
/// count of 0. An existing row is not changed, so an artist keeps the
/// spelling of the name it was first seen with.
pub fn upsert_by_key(conn: &Connection, name: &str) -> Result<String> {
    let key = ids::artist_key(name);
    conn.prepare_cached(
        "INSERT INTO artists (id, key, name, sort_name) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT (key) DO NOTHING",
    )?
    .execute(params![ids::new_id(Kind::Artist), key, name])?;
    let id = conn
        .prepare_cached("SELECT id FROM artists WHERE key = ?1")?
        .query_row([key], |row| row.get(0))?;
    Ok(id)
}

/// Deletes every artist that no album and no track refers to and returns
/// the number of rows deleted.
///
/// The albums and the tracks of an artist are deleted with its row, so an
/// artist that either refers to stays. The caller deletes the albums without
/// tracks first (`albums::delete_orphans`).
pub fn delete_orphans(conn: &Connection) -> Result<u64> {
    let deleted = conn
        .prepare_cached(
            "DELETE FROM artists
             WHERE NOT EXISTS (SELECT 1 FROM albums WHERE albums.artist_id = artists.id)
               AND NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.artist_id = artists.id)",
        )?
        .execute([])?;
    Ok(deleted as u64)
}

/// Sets `album_count` of every artist to the number of its albums. A row
/// whose count is already that number is not written.
pub fn recompute_aggregates(conn: &Connection) -> Result<()> {
    conn.prepare_cached(
        "UPDATE artists SET album_count = counted.album_count
         FROM (
           SELECT ar.id AS id, count(al.id) AS album_count
           FROM artists ar LEFT JOIN albums al ON al.artist_id = ar.id
           GROUP BY ar.id
         ) AS counted
         WHERE artists.id = counted.id AND artists.album_count <> counted.album_count",
    )?
    .execute([])?;
    Ok(())
}

/// Returns the number of artists.
pub fn count(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT count(*) FROM artists", [], |row| {
        int::get_u64(row, 0)
    })?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::{self, TrackContent};
    use crate::db::queries::{albums, folders};
    use crate::library::tags::TrackMeta;

    /// Returns `(key, name, sort_name, album_count)` of every artist in key
    /// order.
    fn rows(conn: &Connection) -> Result<Vec<(String, String, String, i64)>> {
        let mut select =
            conn.prepare("SELECT key, name, sort_name, album_count FROM artists ORDER BY key")?;
        let rows = select
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    #[test]
    fn upsert_inserts_a_row_with_placeholder_aggregates() {
        let db = Db::open_memory().unwrap();
        let id = db
            .with_writer(|conn| upsert_by_key(conn, "Aurora Field"))
            .unwrap();
        assert_eq!(ids::parse(&id).map(|(kind, _)| kind), Some(Kind::Artist));
        assert_eq!(
            db.with_reader(rows).unwrap(),
            [(
                "aurora field".to_owned(),
                "Aurora Field".to_owned(),
                "Aurora Field".to_owned(),
                0
            )]
        );
    }

    #[test]
    fn upsert_returns_the_same_id_for_names_with_one_key() {
        let db = Db::open_memory().unwrap();
        let ids: Vec<String> = ["Beyonc\u{e9}", " beyonce\u{301} ", "BEYONC\u{c9}"]
            .iter()
            .map(|name| db.with_writer(|conn| upsert_by_key(conn, name)).unwrap())
            .collect();
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[0], ids[2]);
        // The row keeps the first spelling.
        let rows = db.with_reader(rows).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "Beyonc\u{e9}");
    }

    #[test]
    fn upsert_gives_artists_with_different_keys_different_ids() {
        let db = Db::open_memory().unwrap();
        let first = db
            .with_writer(|conn| upsert_by_key(conn, "Aurora Field"))
            .unwrap();
        let second = db
            .with_writer(|conn| upsert_by_key(conn, "Marlow Quartet"))
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(db.with_reader(rows).unwrap().len(), 2);
    }

    fn artist(db: &Db, name: &str) -> String {
        db.with_writer(|conn| upsert_by_key(conn, name)).unwrap()
    }

    fn album(db: &Db, artist_id: &str, title: &str) -> String {
        db.with_writer(|conn| albums::upsert_by_key(conn, artist_id, title, "Artist", false))
            .unwrap()
    }

    /// Inserts a track at `rel_path` that names the artist `artist_id` and
    /// the album `album_id`.
    fn track(db: &Db, id: &str, rel_path: &str, artist_id: &str, album_id: &str) {
        let meta = TrackMeta {
            title: "Low Tide".to_owned(),
            artist: "Aurora Field".to_owned(),
            album_artist: "Aurora Field".to_owned(),
            album: "Glass Harbour".to_owned(),
            track_no: None,
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
        };
        let content = TrackContent {
            fingerprint: "100:aa",
            mtime: 1000,
            size: 100,
            meta: &meta,
            artist_id,
            album_id,
        };
        db.with_writer(|conn| {
            let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?.remove(0);
            tracks::insert_track(conn, id, folder.id, rel_path, &content)
        })
        .unwrap();
    }

    fn names(db: &Db) -> Vec<String> {
        db.with_reader(rows)
            .unwrap()
            .into_iter()
            .map(|(_, name, _, _)| name)
            .collect()
    }

    #[test]
    fn delete_orphans_deletes_the_artists_without_an_album() {
        let db = Db::open_memory().unwrap();
        let kept = artist(&db, "Aurora Field");
        album(&db, &kept, "Glass Harbour");
        artist(&db, "Marlow Quartet");
        artist(&db, "Tessa Lund");

        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 2);
        assert_eq!(names(&db), ["Aurora Field"]);

        // Nothing is left to delete, and the row keeps its ID.
        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 0);
        assert_eq!(artist(&db, "Aurora Field"), kept);
        assert_eq!(db.with_reader(albums::count).unwrap(), 1);
    }

    #[test]
    fn delete_orphans_keeps_an_artist_that_a_track_names() {
        // The scanner gives a track the artist of its album. A track that
        // names an artist without an album is deleted with the artist row,
        // so the row stays.
        let db = Db::open_memory().unwrap();
        let with_album = artist(&db, "Aurora Field");
        let album_id = album(&db, &with_album, "Glass Harbour");
        let named = artist(&db, "Marlow Quartet");
        track(
            &db,
            "tr-00000000000000000000000000000001",
            "a.flac",
            &named,
            &album_id,
        );

        assert_eq!(db.with_writer(|conn| delete_orphans(conn)).unwrap(), 0);
        assert_eq!(names(&db), ["Aurora Field", "Marlow Quartet"]);
        assert_eq!(db.with_reader(tracks::count_tracks).unwrap(), 1);
    }

    #[test]
    fn recompute_sets_the_album_count_of_every_artist() {
        let db = Db::open_memory().unwrap();
        let two = artist(&db, "Aurora Field");
        album(&db, &two, "Glass Harbour");
        album(&db, &two, "Paper Maps");
        let one = artist(&db, "Marlow Quartet");
        album(&db, &one, "Slow Rivers");
        artist(&db, "Tessa Lund");
        let counts = |db: &Db| -> Vec<(String, i64)> {
            db.with_reader(rows)
                .unwrap()
                .into_iter()
                .map(|(_, name, _, album_count)| (name, album_count))
                .collect()
        };
        assert_eq!(
            counts(&db),
            [
                ("Aurora Field".to_owned(), 0),
                ("Marlow Quartet".to_owned(), 0),
                ("Tessa Lund".to_owned(), 0)
            ]
        );

        db.with_writer(|conn| recompute_aggregates(conn)).unwrap();
        assert_eq!(
            counts(&db),
            [
                ("Aurora Field".to_owned(), 2),
                ("Marlow Quartet".to_owned(), 1),
                ("Tessa Lund".to_owned(), 0)
            ]
        );

        // An artist that lost an album gets the lower count.
        db.with_writer(|conn| {
            albums::delete_orphans(conn)?;
            recompute_aggregates(conn)
        })
        .unwrap();
        assert_eq!(
            counts(&db),
            [
                ("Aurora Field".to_owned(), 0),
                ("Marlow Quartet".to_owned(), 0),
                ("Tessa Lund".to_owned(), 0)
            ]
        );
    }

    #[test]
    fn recompute_writes_no_row_that_has_its_count() {
        let db = Db::open_memory().unwrap();
        let id = artist(&db, "Aurora Field");
        album(&db, &id, "Glass Harbour");
        artist(&db, "Tessa Lund");
        db.with_writer(|conn| recompute_aggregates(conn)).unwrap();

        db.with_writer(|conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER no_update BEFORE UPDATE ON artists
                 BEGIN SELECT RAISE(ABORT, 'an artist row was updated'); END;",
            )?;
            Ok(())
        })
        .unwrap();
        db.with_writer(|conn| recompute_aggregates(conn)).unwrap();

        // The trigger is in force: a new album fails the update.
        album(&db, &id, "Paper Maps");
        let error = db
            .with_writer(|conn| recompute_aggregates(conn))
            .unwrap_err();
        assert!(
            error.to_string().contains("an artist row was updated"),
            "{error}"
        );
    }

    #[test]
    fn count_is_the_number_of_artist_rows() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.with_reader(count).unwrap(), 0);
        artist(&db, "Aurora Field");
        artist(&db, "aurora field");
        artist(&db, "Tessa Lund");
        assert_eq!(db.with_reader(count).unwrap(), 2);
    }
}
