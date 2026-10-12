//! Queries on the `albums` table. A row is one album of one album artist.

use rusqlite::{Connection, params};

use crate::db::Result;
use crate::ids::{self, Kind};

/// Returns the ID of the album of the artist `artist_id` whose natural key is
/// the key of `title` (`ids::album_key`). Inserts the row when the artist has
/// no album with the key.
///
/// A new row takes `title` as its name, `artist_name` as its artist name,
/// the present time as its creation time, and the placeholder aggregates: no
/// year, no genre, no cover, a song count of 0 and a duration of 0. An
/// existing row keeps its values, except that `compilation = true` sets the
/// compilation flag. The flag is never cleared here.
pub fn upsert_by_key(
    conn: &Connection,
    artist_id: &str,
    title: &str,
    artist_name: &str,
    compilation: bool,
) -> Result<String> {
    let key = ids::album_key(title);
    conn.prepare_cached(
        "INSERT INTO albums (id, artist_id, key, name, artist_name, compilation, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
         ON CONFLICT (artist_id, key) DO UPDATE SET compilation = 1
         WHERE excluded.compilation = 1 AND albums.compilation = 0",
    )?
    .execute(params![
        ids::new_id(Kind::Album),
        artist_id,
        key,
        title,
        artist_name,
        compilation
    ])?;
    let id = conn
        .prepare_cached("SELECT id FROM albums WHERE artist_id = ?1 AND key = ?2")?
        .query_row([artist_id, key.as_str()], |row| row.get(0))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::queries::artists;

    /// The columns of an album row that `upsert_by_key` sets or leaves at
    /// their placeholder.
    #[derive(Debug, PartialEq, Eq)]
    struct AlbumRow {
        key: String,
        name: String,
        artist_name: String,
        year: Option<i64>,
        genre: Option<String>,
        song_count: i64,
        duration_ms: i64,
        compilation: bool,
        cover_source: Option<String>,
    }

    fn rows(conn: &Connection) -> Result<Vec<AlbumRow>> {
        let mut select = conn.prepare(
            "SELECT key, name, artist_name, year, genre, song_count, duration_ms, compilation,
               cover_source
             FROM albums ORDER BY key",
        )?;
        let rows = select
            .query_map([], |row| {
                Ok(AlbumRow {
                    key: row.get(0)?,
                    name: row.get(1)?,
                    artist_name: row.get(2)?,
                    year: row.get(3)?,
                    genre: row.get(4)?,
                    song_count: row.get(5)?,
                    duration_ms: row.get(6)?,
                    compilation: row.get(7)?,
                    cover_source: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn artist(db: &Db, name: &str) -> String {
        db.with_writer(|conn| artists::upsert_by_key(conn, name))
            .unwrap()
    }

    fn upsert(db: &Db, artist_id: &str, title: &str, compilation: bool) -> String {
        db.with_writer(|conn| upsert_by_key(conn, artist_id, title, "Artist", compilation))
            .unwrap()
    }

    #[test]
    fn upsert_inserts_a_row_with_placeholder_aggregates() {
        let db = Db::open_memory().unwrap();
        let artist_id = artist(&db, "Aurora Field");
        let id = db
            .with_writer(|conn| {
                upsert_by_key(conn, &artist_id, "Glass Harbour", "Aurora Field", false)
            })
            .unwrap();
        assert_eq!(ids::parse(&id).map(|(kind, _)| kind), Some(Kind::Album));
        assert_eq!(
            db.with_reader(rows).unwrap(),
            [AlbumRow {
                key: "glass harbour".to_owned(),
                name: "Glass Harbour".to_owned(),
                artist_name: "Aurora Field".to_owned(),
                year: None,
                genre: None,
                song_count: 0,
                duration_ms: 0,
                compilation: false,
                cover_source: None,
            }]
        );
    }

    #[test]
    fn upsert_returns_the_same_id_for_titles_with_one_key() {
        let db = Db::open_memory().unwrap();
        let artist_id = artist(&db, "Aurora Field");
        let first = upsert(&db, &artist_id, "Glass Harbour", false);
        let second = upsert(&db, &artist_id, "  GLASS HARBOUR ", false);
        assert_eq!(first, second);
        // The row keeps the first spelling.
        let rows = db.with_reader(rows).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Glass Harbour");
    }

    #[test]
    fn upsert_keeps_albums_of_two_artists_apart() {
        let db = Db::open_memory().unwrap();
        let first = upsert(&db, &artist(&db, "Aurora Field"), "Greatest Hits", false);
        let second = upsert(&db, &artist(&db, "Marlow Quartet"), "Greatest Hits", false);
        assert_ne!(first, second);
        assert_eq!(db.with_reader(rows).unwrap().len(), 2);
    }

    #[test]
    fn upsert_sets_the_compilation_flag_and_does_not_clear_it() {
        let db = Db::open_memory().unwrap();
        let artist_id = artist(&db, "Various Artists");
        let flag = |db: &Db| db.with_reader(rows).unwrap()[0].compilation;

        let id = upsert(&db, &artist_id, "Summer Sampler", false);
        assert!(!flag(&db));
        assert_eq!(upsert(&db, &artist_id, "Summer Sampler", true), id);
        assert!(flag(&db));
        assert_eq!(upsert(&db, &artist_id, "Summer Sampler", false), id);
        assert!(flag(&db));
    }

    #[test]
    fn upsert_with_an_unknown_artist_is_an_error() {
        let db = Db::open_memory().unwrap();
        let error = db
            .with_writer(|conn| {
                upsert_by_key(conn, "ar-00000000000000000000000000000000", "A", "B", false)
            })
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
    }
}
