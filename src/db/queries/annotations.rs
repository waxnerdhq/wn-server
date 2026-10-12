//! Queries on the `annotations` table. A row holds what one user attached to
//! one track, album or artist: a star, a rating and a play count.
//!
//! `item_id` has no foreign key, because it names a row of one of three
//! tables. The scanner removes the rows of items that no longer exist with
//! [`delete_orphans`].

use rusqlite::Connection;

use crate::db::Result;

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
}
