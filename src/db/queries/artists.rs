//! Queries on the `artists` table. A row is one album artist.
//!
//! A row is found by its natural key and keeps its ID for as long as it
//! exists. [`upsert_by_key`] is the one function that inserts a row, and
//! [`delete_orphans`] is the one function that deletes rows. The other
//! functions update rows in place.

use rusqlite::{Connection, params};

use super::int;
use crate::db::Result;
use crate::ids::{self, Kind};
use crate::library::tags::VARIOUS_ARTISTS;

/// Returns the ID of the artist whose natural key is the key of `name`
/// (`ids::artist_key`). Inserts the row when no artist has the key.
///
/// A new row takes `name` as its name and as its sort name, and an album
/// count of 0. An existing row is not changed. [`update_names`] is the
/// function that changes the spelling of a name.
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

/// Sets `name` and `sort_name` of every artist whose stored name no track
/// carries to the spelling that the most tracks of the artist carry
/// (`docs/design.md` section 3.3).
///
/// A track row does not hold the album artist of its file. The spelling that
/// a track carries is its track artist (`tracks.artist_name`) when that name
/// has the key of the artist, which is the case for a track without an album
/// artist tag and without the compilation flag. A track whose track artist
/// has another key carries no spelling. A track without a file carries its
/// spelling as every other track does. Among spellings that equally many
/// tracks carry, the first in byte order is taken.
///
/// An artist of which no track carries a spelling keeps its name. The row of
/// [`VARIOUS_ARTISTS`] keeps its name too: every track of it has that album
/// artist in that spelling, whatever its track artist is.
///
/// The ID and the key of a row stay. A row whose name a track carries is not
/// written.
pub fn update_names(conn: &Connection) -> Result<()> {
    let uncarried: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT id, key FROM artists
             WHERE key <> ?1 AND NOT EXISTS (
               SELECT 1 FROM tracks
               WHERE tracks.artist_id = artists.id AND tracks.artist_name = artists.name)",
        )?
        .query_map([ids::artist_key(VARIOUS_ARTISTS)], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut spellings = conn.prepare_cached(
        "SELECT artist_name FROM tracks WHERE artist_id = ?1
         GROUP BY artist_name ORDER BY count(*) DESC, artist_name",
    )?;
    let mut rename =
        conn.prepare_cached("UPDATE artists SET name = ?2, sort_name = ?2 WHERE id = ?1")?;
    for (id, key) in uncarried {
        let carried: Vec<String> = spellings
            .query_map([&id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let most_carried = carried
            .into_iter()
            .find(|name| ids::artist_key(name) == key);
        if let Some(name) = most_carried {
            rename.execute([&id, &name])?;
        }
    }
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
    /// the album `album_id`. Its track artist is `Aurora Field`.
    fn track(db: &Db, id: &str, rel_path: &str, artist_id: &str, album_id: &str) {
        track_by(db, id, rel_path, "Aurora Field", artist_id, album_id);
    }

    /// Inserts a track at `rel_path` with the track artist `track_artist`
    /// that names the artist `artist_id` and the album `album_id`.
    fn track_by(
        db: &Db,
        id: &str,
        rel_path: &str,
        track_artist: &str,
        artist_id: &str,
        album_id: &str,
    ) {
        let meta = TrackMeta {
            title: "Low Tide".to_owned(),
            artist: track_artist.to_owned(),
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

    /// An artist with one album, and the tracks that a test gives it.
    struct Named {
        db: Db,
        artist_id: String,
        album_id: String,
    }

    impl Named {
        /// Inserts the artist with the name `name` and one album of it.
        fn new(name: &str) -> Self {
            let db = Db::open_memory().unwrap();
            let artist_id = artist(&db, name);
            let album_id = album(&db, &artist_id, "Glass Harbour");
            Self {
                db,
                artist_id,
                album_id,
            }
        }

        /// Inserts a track of the artist for each pair of a number and a
        /// track artist. The number makes the ID and the path of the track.
        fn add(&self, tracks: &[(u32, &str)]) {
            for &(number, track_artist) in tracks {
                track_by(
                    &self.db,
                    &format!("tr-{number:032x}"),
                    &format!("{number}.flac"),
                    track_artist,
                    &self.artist_id,
                    &self.album_id,
                );
            }
        }

        fn delete(&self, number: u32) {
            let id = format!("tr-{number:032x}");
            self.db
                .with_writer(|conn| tracks::delete_tracks(conn, &[id]))
                .unwrap();
        }

        /// Runs `update_names` and returns the name and the sort name of
        /// every artist in key order.
        fn names_after_update(&self) -> Vec<(String, String)> {
            self.db.with_writer(|conn| update_names(conn)).unwrap();
            self.db
                .with_reader(rows)
                .unwrap()
                .into_iter()
                .map(|(_, name, sort_name, _)| (name, sort_name))
                .collect()
        }
    }

    /// Returns the name and the sort name of an artist with the name `name`.
    fn named(name: &str) -> (String, String) {
        (name.to_owned(), name.to_owned())
    }

    #[test]
    fn update_names_gives_an_artist_whose_name_no_track_carries_the_most_common_spelling() {
        let library = Named::new("aurora field");
        library.add(&[
            (1, "AURORA FIELD"),
            (2, "Aurora Field"),
            (3, "Aurora Field"),
        ]);
        assert_eq!(library.names_after_update(), [named("Aurora Field")]);
        // The row has its ID and its key.
        assert_eq!(artist(&library.db, "aurora field"), library.artist_id);
        assert_eq!(library.db.with_reader(rows).unwrap()[0].0, "aurora field");
    }

    #[test]
    fn update_names_keeps_a_name_that_one_track_carries() {
        let library = Named::new("aurora field");
        library.add(&[
            (1, "aurora field"),
            (2, "Aurora Field"),
            (3, "Aurora Field"),
        ]);
        assert_eq!(library.names_after_update(), [named("aurora field")]);

        // The track that carried the name is gone.
        library.delete(1);
        assert_eq!(library.names_after_update(), [named("Aurora Field")]);
        assert_eq!(artist(&library.db, "aurora field"), library.artist_id);
    }

    #[test]
    fn update_names_chooses_the_first_spelling_in_byte_order_among_equally_common_ones() {
        let library = Named::new("aurora field");
        library.add(&[
            (1, "Aurora Field"),
            (2, "aurora Field"),
            (3, "AURORA FIELD"),
        ]);
        assert_eq!(library.names_after_update(), [named("AURORA FIELD")]);

        // The row keeps the name while a track carries it, also when another
        // spelling becomes the most common one.
        library.add(&[(4, "Aurora Field"), (5, "Aurora Field")]);
        assert_eq!(library.names_after_update(), [named("AURORA FIELD")]);
    }

    #[test]
    fn update_names_leaves_out_a_track_artist_with_another_key() {
        // The tracks have an album artist tag, which the row does not hold,
        // and most of them have another track artist.
        let library = Named::new("aurora field");
        library.add(&[(1, "Tessa Lund"), (2, "Tessa Lund")]);
        assert_eq!(library.names_after_update(), [named("aurora field")]);

        library.add(&[(3, "Aurora Field")]);
        assert_eq!(library.names_after_update(), [named("Aurora Field")]);
    }

    #[test]
    fn update_names_keeps_the_name_of_various_artists() {
        let library = Named::new(VARIOUS_ARTISTS);
        library.add(&[(1, "various artists"), (2, "Tessa Lund")]);
        assert_eq!(library.names_after_update(), [named(VARIOUS_ARTISTS)]);
    }

    #[test]
    fn update_names_writes_no_row_whose_name_a_track_carries() {
        let library = Named::new("aurora field");
        library.add(&[(1, "aurora field"), (2, "Aurora Field")]);
        // An artist of which no track carries a spelling, and an artist
        // without a track.
        let uncarried = artist(&library.db, "Marlow Quartet");
        let album_id = album(&library.db, &uncarried, "Slow Rivers");
        let id = "tr-00000000000000000000000000000003";
        track_by(
            &library.db,
            id,
            "3.flac",
            "Tessa Lund",
            &uncarried,
            &album_id,
        );
        artist(&library.db, "Nadia Ortiz");
        library.names_after_update();

        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER no_update BEFORE UPDATE ON artists
                     BEGIN SELECT RAISE(ABORT, 'an artist row was updated'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        library.names_after_update();

        // The trigger is in force: a name that no track carries fails the
        // update.
        library.delete(1);
        let error = library
            .db
            .with_writer(|conn| update_names(conn))
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
