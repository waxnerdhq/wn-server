//! Queries on the `artists` table. A row is one album artist.

use rusqlite::{Connection, params};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

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
}
