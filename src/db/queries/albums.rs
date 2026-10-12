//! Queries on the `albums` table. A row is one album of one album artist.
//!
//! A row is found by its natural key and keeps its ID for as long as it
//! exists. [`upsert_by_key`] is the one function that inserts a row, and
//! [`delete_orphans`] is the one function that deletes rows. The other
//! functions update rows in place.

use rusqlite::{Connection, params};

use super::int;
use super::tracks::HAS_FILE_SQL;
use crate::db::Result;
use crate::ids::{self, Kind};

/// The order of the tracks of one album: by disc number, where a track
/// without one counts as disc 1, then by track number, where a track without
/// one follows the numbered tracks, then by locator (`docs/design.md`
/// section 1.3). `browse::tracks_of_album` lists the tracks in this order.
pub(super) const TRACK_ORDER: &str =
    "coalesce(disc_no, 1), track_no IS NULL, track_no, folder_id, rel_path";

/// The stored cover of one album and the tracks that decide its cover.
///
/// Both tracks are the first of their kind in the order of the album: by disc
/// number, then by track number. A track without a file is never one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverInputs {
    pub album_id: String,
    /// The stored `cover_source`.
    pub cover_source: Option<String>,
    /// The stored `cover_mtime`.
    pub cover_mtime: Option<i64>,
    /// The folder ID and the relative path of the first track that has a
    /// file. `None` when no track of the album has a file.
    pub first_track: Option<(i64, String)>,
    /// The ID and the modification time of the first track that has a file
    /// and an embedded picture. `None` when the album has no such track.
    pub first_picture: Option<(String, i64)>,
}

/// Returns the ID of the album of the artist `artist_id` whose natural key is
/// the key of `title` (`ids::album_key`). Inserts the row when the artist has
/// no album with the key.
///
/// A new row takes `title` as its name, `artist_name` as its artist name,
/// the present time as its creation time, and the placeholder aggregates: no
/// year, no genre, no cover, a song count of 0 and a duration of 0. An
/// existing row keeps its values, except that `compilation = true` sets the
/// compilation flag. The flag is never cleared here. [`update_names`] is the
/// function that changes the spelling of a name.
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

/// Deletes every album that no track refers to and returns the IDs of the
/// deleted albums.
///
/// A track without a file refers to its album as every other track does, so
/// its album stays. The statement deletes no track, because it deletes an
/// album only when no track row has the ID of the album.
pub fn delete_orphans(conn: &Connection) -> Result<Vec<String>> {
    let mut delete = conn.prepare_cached(
        "DELETE FROM albums
         WHERE NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.album_id = albums.id)
         RETURNING id",
    )?;
    let ids = delete
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Sets `song_count`, `duration_ms`, `year` and `genre` of every album from
/// its tracks.
///
/// The song count and the duration cover every track row of the album, a
/// track without a file included. The year is the smallest year of a track.
/// The genre is the one that most tracks have, and the first in byte order
/// among genres with the same number of tracks. An album without a track gets
/// a count of 0, a duration of 0, no year and no genre.
///
/// A row whose values are already these is not written.
pub fn recompute_aggregates(conn: &Connection) -> Result<()> {
    conn.prepare_cached(
        "UPDATE albums SET song_count = agg.song_count, duration_ms = agg.duration_ms,
           year = agg.year, genre = agg.genre
         FROM (
           SELECT a.id AS id, count(t.id) AS song_count,
             coalesce(sum(t.duration_ms), 0) AS duration_ms, min(t.year) AS year,
             (SELECT g.genre FROM tracks g
              WHERE g.album_id = a.id AND g.genre IS NOT NULL
              GROUP BY g.genre ORDER BY count(*) DESC, g.genre LIMIT 1) AS genre
           FROM albums a LEFT JOIN tracks t ON t.album_id = a.id
           GROUP BY a.id
         ) AS agg
         WHERE albums.id = agg.id
           AND (albums.song_count IS NOT agg.song_count
             OR albums.duration_ms IS NOT agg.duration_ms
             OR albums.year IS NOT agg.year
             OR albums.genre IS NOT agg.genre)",
    )?
    .execute([])?;
    Ok(())
}

/// Sets the display names of the album rows from the tracks and from the
/// artist rows (`docs/design.md` section 3.3).
///
/// `name`: an album whose stored name is the album title of none of its
/// tracks (`tracks.album_name`) gets the title that the most tracks of the
/// album have, and the first in byte order among titles of equally many
/// tracks. A title counts only when it has the key of the album. A track
/// without a file counts as every other track does. An album without a track
/// keeps its name.
///
/// `artist_name`: an album gets the name of its artist row. The caller sets
/// the names of the artist rows first (`artists::update_names`).
///
/// The ID, the artist and the key of a row stay. A row that has its names is
/// not written.
pub fn update_names(conn: &Connection) -> Result<()> {
    let uncarried: Vec<(String, String)> = conn
        .prepare_cached(
            "SELECT id, key FROM albums
             WHERE NOT EXISTS (
               SELECT 1 FROM tracks
               WHERE tracks.album_id = albums.id AND tracks.album_name = albums.name)",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut spellings = conn.prepare_cached(
        "SELECT album_name FROM tracks WHERE album_id = ?1
         GROUP BY album_name ORDER BY count(*) DESC, album_name",
    )?;
    let mut rename = conn.prepare_cached("UPDATE albums SET name = ?2 WHERE id = ?1")?;
    for (id, key) in uncarried {
        let carried: Vec<String> = spellings
            .query_map([&id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let most_carried = carried
            .into_iter()
            .find(|title| ids::album_key(title) == key);
        if let Some(title) = most_carried {
            rename.execute([&id, &title])?;
        }
    }
    conn.prepare_cached(
        "UPDATE albums SET artist_name = artists.name
         FROM artists
         WHERE artists.id = albums.artist_id AND albums.artist_name <> artists.name",
    )?
    .execute([])?;
    Ok(())
}

/// Returns the cover inputs of every album in ID order.
pub fn cover_inputs(conn: &Connection) -> Result<Vec<CoverInputs>> {
    let mut select = conn.prepare_cached(&format!(
        "SELECT a.id, a.cover_source, a.cover_mtime, first.folder_id, first.rel_path,
           pictured.id, pictured.mtime
         FROM albums a
         LEFT JOIN tracks first ON first.id = (
           SELECT id FROM tracks WHERE album_id = a.id AND {HAS_FILE_SQL}
           ORDER BY {TRACK_ORDER} LIMIT 1)
         LEFT JOIN tracks pictured ON pictured.id = (
           SELECT id FROM tracks WHERE album_id = a.id AND {HAS_FILE_SQL} AND has_picture = 1
           ORDER BY {TRACK_ORDER} LIMIT 1)
         ORDER BY a.id"
    ))?;
    let inputs = select
        .query_map([], |row| {
            let folder_id: Option<i64> = row.get(3)?;
            let rel_path: Option<String> = row.get(4)?;
            let pictured: Option<String> = row.get(5)?;
            let pictured_mtime: Option<i64> = row.get(6)?;
            Ok(CoverInputs {
                album_id: row.get(0)?,
                cover_source: row.get(1)?,
                cover_mtime: row.get(2)?,
                first_track: folder_id.zip(rel_path),
                first_picture: pictured.zip(pictured_mtime),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(inputs)
}

/// Stores the cover of the album `album_id`: `source` is the absolute path
/// of an image file or `embedded:<track id>`, and `mtime` is the
/// modification time of that file or of that track in Unix seconds. `None`
/// for both stores that the album has no cover.
pub fn set_cover(
    conn: &Connection,
    album_id: &str,
    source: Option<&str>,
    mtime: Option<i64>,
) -> Result<()> {
    conn.prepare_cached("UPDATE albums SET cover_source = ?2, cover_mtime = ?3 WHERE id = ?1")?
        .execute(params![album_id, source, mtime])?;
    Ok(())
}

/// Returns the number of albums.
pub fn count(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT count(*) FROM albums", [], |row| {
        int::get_u64(row, 0)
    })?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::{self, TrackContent};
    use crate::db::queries::{artists, folders};
    use crate::library::tags::TrackMeta;

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

    /// One track of a test library. [`song`] returns the values that a test
    /// does not set.
    #[derive(Debug, Clone)]
    struct Song {
        rel_path: &'static str,
        /// The album title of the track.
        album: &'static str,
        disc_no: Option<u32>,
        track_no: Option<u32>,
        year: Option<u32>,
        genre: Option<&'static str>,
        duration_ms: u64,
        has_picture: bool,
        mtime: i64,
    }

    /// Returns a track at `rel_path` with the album title `Album`, without
    /// numbers, year, genre and picture, with a duration of 1000 ms and the
    /// modification time 100.
    fn song(rel_path: &'static str) -> Song {
        Song {
            rel_path,
            album: "Album",
            disc_no: None,
            track_no: None,
            year: None,
            genre: None,
            duration_ms: 1000,
            has_picture: false,
            mtime: 100,
        }
    }

    /// A database with one folder and one artist.
    struct Library {
        db: Db,
        folder_id: i64,
        artist_id: String,
        /// The number of tracks inserted so far, which numbers the IDs.
        tracks: std::cell::Cell<u32>,
    }

    impl Library {
        fn new() -> Self {
            let db = Db::open_memory().unwrap();
            let (folder_id, artist_id) = db
                .with_writer(|conn| {
                    let folder = folders::sync_folders(conn, &[PathBuf::from("/music")])?.remove(0);
                    Ok((folder.id, artists::upsert_by_key(conn, "Aurora Field")?))
                })
                .unwrap();
            Self {
                db,
                folder_id,
                artist_id,
                tracks: std::cell::Cell::new(0),
            }
        }

        fn album(&self, title: &str) -> String {
            upsert(&self.db, &self.artist_id, title, false)
        }

        /// Inserts a track of the album `album_id` and returns its ID. The
        /// IDs of the tracks of a library ascend in the order of the calls.
        fn add(&self, album_id: &str, song: &Song) -> String {
            self.tracks.set(self.tracks.get() + 1);
            let id = format!("tr-{:032x}", self.tracks.get());
            let meta = TrackMeta {
                title: song.rel_path.to_owned(),
                artist: "Aurora Field".to_owned(),
                album_artist: "Aurora Field".to_owned(),
                album: song.album.to_owned(),
                track_no: song.track_no,
                disc_no: song.disc_no,
                year: song.year,
                genre: song.genre.map(str::to_owned),
                compilation: false,
                duration_ms: song.duration_ms,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                has_picture: song.has_picture,
                has_lyrics: false,
                suffix: "flac".to_owned(),
                content_type: "audio/flac".to_owned(),
                lossless: true,
            };
            let content = TrackContent {
                fingerprint: "100:aa",
                mtime: song.mtime,
                size: 100,
                meta: &meta,
                artist_id: &self.artist_id,
                album_id,
            };
            self.db
                .with_writer(|conn| {
                    tracks::insert_track(conn, &id, self.folder_id, song.rel_path, &content)
                })
                .unwrap();
            id
        }

        /// Gives the track `id` its placeholder path, so that it has no
        /// file.
        fn detach(&self, id: &str) {
            let placeholder = tracks::placeholder_path(id);
            self.db
                .with_writer(|conn| {
                    tracks::relink_track(conn, id, self.folder_id, &placeholder, 100, 100)
                })
                .unwrap();
        }

        fn delete(&self, id: &str) {
            self.db
                .with_writer(|conn| tracks::delete_tracks(conn, &[id]))
                .unwrap();
        }

        fn recompute(&self) {
            self.db
                .with_writer(|conn| recompute_aggregates(conn))
                .unwrap();
        }

        /// Inserts a track of the album `album_id` for each pair of a
        /// relative path and an album title, and returns the IDs.
        fn add_titled(
            &self,
            album_id: &str,
            tracks: &[(&'static str, &'static str)],
        ) -> Vec<String> {
            tracks
                .iter()
                .map(|&(rel_path, album)| {
                    self.add(
                        album_id,
                        &Song {
                            album,
                            ..song(rel_path)
                        },
                    )
                })
                .collect()
        }

        /// Runs `update_names` and returns the name of the album with the
        /// key `key`.
        fn name_after_update(&self, key: &str) -> String {
            self.db.with_writer(|conn| update_names(conn)).unwrap();
            self.row(key).name
        }

        /// Returns the row of the album with the key `key`.
        fn row(&self, key: &str) -> AlbumRow {
            let mut rows = self.db.with_reader(rows).unwrap();
            let position = rows
                .iter()
                .position(|row| row.key == key)
                .unwrap_or_else(|| panic!("no album has the key {key}"));
            rows.swap_remove(position)
        }

        /// Returns the cover inputs of the album `album_id`.
        fn inputs(&self, album_id: &str) -> CoverInputs {
            self.db
                .with_reader(cover_inputs)
                .unwrap()
                .into_iter()
                .find(|inputs| inputs.album_id == album_id)
                .unwrap_or_else(|| panic!("no album has the ID {album_id}"))
        }
    }

    /// Returns `(song_count, duration_ms, year, genre)` of a row.
    fn aggregates(row: &AlbumRow) -> (i64, i64, Option<i64>, Option<&str>) {
        (
            row.song_count,
            row.duration_ms,
            row.year,
            row.genre.as_deref(),
        )
    }

    #[test]
    fn delete_orphans_deletes_the_albums_without_a_track_and_returns_their_ids() {
        let library = Library::new();
        let kept = library.album("Glass Harbour");
        let without_file = library.album("Paper Maps");
        let emptied = library.album("Slow Rivers");
        let never_filled = library.album("Summer Sampler");
        library.add(&kept, &song("a.flac"));
        // A track without a file refers to its album as any other track.
        let detached = library.add(&without_file, &song("b.flac"));
        library.detach(&detached);
        let removed = library.add(&emptied, &song("c.flac"));
        library.delete(&removed);

        let mut deleted = library.db.with_writer(|conn| delete_orphans(conn)).unwrap();
        deleted.sort();
        let mut expected = vec![emptied, never_filled];
        expected.sort();
        assert_eq!(deleted, expected);
        let keys: Vec<String> = library
            .db
            .with_reader(rows)
            .unwrap()
            .into_iter()
            .map(|row| row.key)
            .collect();
        assert_eq!(keys, ["glass harbour", "paper maps"]);
        // No track went with an album.
        assert_eq!(library.db.with_reader(tracks::count_tracks).unwrap(), 2);

        // Nothing is left to delete, and the rows keep their IDs.
        let deleted = library.db.with_writer(|conn| delete_orphans(conn)).unwrap();
        assert_eq!(deleted, [""; 0]);
        assert_eq!(library.album("Glass Harbour"), kept);
        assert_eq!(library.album("Paper Maps"), without_file);
    }

    #[test]
    fn recompute_sets_the_count_the_duration_the_smallest_year_and_the_most_common_genre() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        let other = library.album("Paper Maps");
        let songs = [
            Song {
                year: Some(2021),
                genre: Some("Rock"),
                duration_ms: 1000,
                ..song("a.flac")
            },
            Song {
                year: Some(2019),
                genre: Some("Pop"),
                duration_ms: 2000,
                ..song("b.flac")
            },
            Song {
                year: None,
                genre: Some("Rock"),
                duration_ms: 3000,
                ..song("c.flac")
            },
            Song {
                year: Some(2020),
                genre: None,
                duration_ms: 4000,
                ..song("d.flac")
            },
        ];
        for song in &songs {
            library.add(&album, song);
        }
        library.add(
            &other,
            &Song {
                year: Some(1999),
                genre: Some("Jazz"),
                duration_ms: 500,
                ..song("e.flac")
            },
        );
        library.recompute();

        assert_eq!(
            aggregates(&library.row("glass harbour")),
            (4, 10_000, Some(2019), Some("Rock"))
        );
        // The tracks of another album do not count.
        assert_eq!(
            aggregates(&library.row("paper maps")),
            (1, 500, Some(1999), Some("Jazz"))
        );
    }

    #[test]
    fn recompute_chooses_the_first_genre_in_byte_order_among_equally_common_ones() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        for (rel_path, genre) in [("a.flac", "Rock"), ("b.flac", "Jazz"), ("c.flac", "Pop")] {
            library.add(
                &album,
                &Song {
                    genre: Some(genre),
                    ..song(rel_path)
                },
            );
        }
        library.recompute();
        assert_eq!(library.row("glass harbour").genre.as_deref(), Some("Jazz"));

        // One more track makes another genre the most common one.
        library.add(
            &album,
            &Song {
                genre: Some("Rock"),
                ..song("d.flac")
            },
        );
        library.recompute();
        assert_eq!(library.row("glass harbour").genre.as_deref(), Some("Rock"));
    }

    #[test]
    fn recompute_gives_an_album_without_year_and_genre_tags_neither() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        library.add(&album, &song("a.flac"));
        library.add(&album, &song("b.flac"));
        library.recompute();
        assert_eq!(
            aggregates(&library.row("glass harbour")),
            (2, 2000, None, None)
        );
    }

    #[test]
    fn recompute_counts_a_track_without_a_file() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        library.add(&album, &song("a.flac"));
        let detached = library.add(
            &album,
            &Song {
                year: Some(2001),
                genre: Some("Rock"),
                duration_ms: 500,
                ..song("b.flac")
            },
        );
        library.detach(&detached);
        library.recompute();
        assert_eq!(
            aggregates(&library.row("glass harbour")),
            (2, 1500, Some(2001), Some("Rock"))
        );
    }

    #[test]
    fn recompute_resets_an_album_that_lost_its_tracks() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        let id = library.add(
            &album,
            &Song {
                year: Some(2019),
                genre: Some("Rock"),
                ..song("a.flac")
            },
        );
        library.recompute();
        assert_eq!(
            aggregates(&library.row("glass harbour")),
            (1, 1000, Some(2019), Some("Rock"))
        );

        library.delete(&id);
        library.recompute();
        assert_eq!(
            aggregates(&library.row("glass harbour")),
            (0, 0, None, None)
        );
        // The row and its ID stay. `delete_orphans` is what deletes a row.
        assert_eq!(library.album("Glass Harbour"), album);
    }

    #[test]
    fn recompute_writes_no_row_that_has_its_values() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        library.add(
            &album,
            &Song {
                year: Some(2019),
                genre: Some("Rock"),
                ..song("a.flac")
            },
        );
        // An album without year and genre, whose values are NULL.
        let bare = library.album("Paper Maps");
        library.add(&bare, &song("b.flac"));
        library.recompute();

        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER no_update BEFORE UPDATE ON albums
                     BEGIN SELECT RAISE(ABORT, 'an album row was updated'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        library.recompute();

        // The trigger is in force: a change of the tracks fails the update.
        library.add(&album, &song("c.flac"));
        let error = library
            .db
            .with_writer(|conn| recompute_aggregates(conn))
            .unwrap_err();
        assert!(
            error.to_string().contains("an album row was updated"),
            "{error}"
        );
    }

    #[test]
    fn update_names_gives_an_album_whose_name_no_track_carries_the_most_common_title() {
        let library = Library::new();
        let album = library.album("glass harbour");
        library.add_titled(
            &album,
            &[
                ("a.flac", "GLASS HARBOUR"),
                ("b.flac", "Glass Harbour"),
                ("c.flac", "Glass Harbour"),
            ],
        );
        assert_eq!(library.name_after_update("glass harbour"), "Glass Harbour");
        // The row has its ID and its key.
        assert_eq!(library.album("glass harbour"), album);
        assert_eq!(library.db.with_reader(count).unwrap(), 1);
    }

    #[test]
    fn update_names_keeps_a_name_that_one_track_carries() {
        let library = Library::new();
        let album = library.album("glass harbour");
        let ids = library.add_titled(
            &album,
            &[
                ("a.flac", "glass harbour"),
                ("b.flac", "Glass Harbour"),
                ("c.flac", "Glass Harbour"),
            ],
        );
        assert_eq!(library.name_after_update("glass harbour"), "glass harbour");

        // A track without a file carries its title as every other track.
        library.detach(&ids[0]);
        assert_eq!(library.name_after_update("glass harbour"), "glass harbour");

        // The track that carried the name is gone.
        library.delete(&ids[0]);
        assert_eq!(library.name_after_update("glass harbour"), "Glass Harbour");
        assert_eq!(library.album("glass harbour"), album);
    }

    #[test]
    fn update_names_chooses_the_first_title_in_byte_order_among_equally_common_ones() {
        let library = Library::new();
        let album = library.album("glass harbour");
        library.add_titled(
            &album,
            &[
                ("a.flac", "Glass Harbour"),
                ("b.flac", "glass Harbour"),
                ("c.flac", "GLASS HARBOUR"),
            ],
        );
        assert_eq!(library.name_after_update("glass harbour"), "GLASS HARBOUR");

        // The row keeps the name while a track carries it, also when another
        // title becomes the most common one.
        library.add_titled(
            &album,
            &[("d.flac", "Glass Harbour"), ("e.flac", "Glass Harbour")],
        );
        assert_eq!(library.name_after_update("glass harbour"), "GLASS HARBOUR");
    }

    #[test]
    fn update_names_leaves_out_a_title_with_another_key() {
        let library = Library::new();
        let album = library.album("glass harbour");
        // The key of a title with space inside differs from the key of the
        // album.
        library.add_titled(
            &album,
            &[("a.flac", "Glass  Harbour"), ("b.flac", "Glass  Harbour")],
        );
        assert_eq!(library.name_after_update("glass harbour"), "glass harbour");

        library.add_titled(&album, &[("c.flac", "Glass Harbour")]);
        assert_eq!(library.name_after_update("glass harbour"), "Glass Harbour");
    }

    #[test]
    fn update_names_keeps_the_name_of_an_album_without_a_track() {
        let library = Library::new();
        library.album("glass harbour");
        assert_eq!(library.name_after_update("glass harbour"), "glass harbour");
    }

    #[test]
    fn update_names_gives_an_album_the_name_of_its_artist_row() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        let other_artist = artist(&library.db, "Marlow Quartet");
        let other = library
            .db
            .with_writer(|conn| {
                upsert_by_key(conn, &other_artist, "Slow Rivers", "MARLOW quartet", false)
            })
            .unwrap();
        assert_eq!(library.row("glass harbour").artist_name, "Artist");

        library.db.with_writer(|conn| update_names(conn)).unwrap();
        assert_eq!(library.row("glass harbour").artist_name, "Aurora Field");
        assert_eq!(library.row("slow rivers").artist_name, "Marlow Quartet");
        // The rows have their IDs, and their titles are as they were.
        assert_eq!(library.album("Glass Harbour"), album);
        assert_eq!(
            upsert(&library.db, &other_artist, "Slow Rivers", false),
            other
        );
        assert_eq!(library.row("glass harbour").name, "Glass Harbour");
        assert_eq!(library.row("slow rivers").name, "Slow Rivers");
    }

    #[test]
    fn update_names_writes_no_row_that_has_its_names() {
        let library = Library::new();
        let carried = library.album("glass harbour");
        library.add_titled(
            &carried,
            &[("a.flac", "glass harbour"), ("b.flac", "Glass Harbour")],
        );
        // An album whose tracks carry no title with its key, and an album
        // without a track.
        let uncarried = library.album("Paper Maps");
        library.add(&uncarried, &song("c.flac"));
        library.album("Slow Rivers");
        library.db.with_writer(|conn| update_names(conn)).unwrap();

        library
            .db
            .with_writer(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER no_update BEFORE UPDATE ON albums
                     BEGIN SELECT RAISE(ABORT, 'an album row was updated'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        library.db.with_writer(|conn| update_names(conn)).unwrap();

        // The trigger is in force: a name that no track carries fails the
        // update.
        library.add_titled(&uncarried, &[("d.flac", "PAPER MAPS")]);
        let error = library
            .db
            .with_writer(|conn| update_names(conn))
            .unwrap_err();
        assert!(
            error.to_string().contains("an album row was updated"),
            "{error}"
        );
    }

    #[test]
    fn cover_inputs_name_the_first_track_by_disc_then_by_track_number() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        // A track without a disc number is on disc 1. A track without a
        // track number follows the numbered tracks of its disc.
        let numbered = [
            (Some(2), Some(1), "cd2/01.flac"),
            (Some(1), None, "cd1/bonus.flac"),
            (None, Some(2), "02.flac"),
            (Some(1), Some(3), "cd1/03.flac"),
        ];
        for (disc_no, track_no, rel_path) in numbered {
            library.add(
                &album,
                &Song {
                    disc_no,
                    track_no,
                    ..song(rel_path)
                },
            );
        }
        let inputs = library.inputs(&album);
        assert_eq!(
            inputs,
            CoverInputs {
                album_id: album.clone(),
                cover_source: None,
                cover_mtime: None,
                first_track: Some((library.folder_id, "02.flac".to_owned())),
                first_picture: None,
            }
        );

        // Track 1 of disc 1 precedes them.
        library.add(
            &album,
            &Song {
                disc_no: Some(1),
                track_no: Some(1),
                ..song("cd1/01.flac")
            },
        );
        assert_eq!(
            library.inputs(&album).first_track,
            Some((library.folder_id, "cd1/01.flac".to_owned()))
        );
    }

    #[test]
    fn cover_inputs_order_tracks_with_equal_numbers_by_their_path() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        library.add(&album, &song("b/untagged.flac"));
        library.add(&album, &song("a/untagged.flac"));
        assert_eq!(
            library.inputs(&album).first_track,
            Some((library.folder_id, "a/untagged.flac".to_owned()))
        );
    }

    #[test]
    fn cover_inputs_name_the_first_track_with_a_picture() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        library.add(
            &album,
            &Song {
                track_no: Some(1),
                ..song("01.flac")
            },
        );
        // Track 1 of disc 2 has a lower track number than track 2 of disc 1
        // and follows it in the order of the album.
        library.add(
            &album,
            &Song {
                disc_no: Some(2),
                track_no: Some(1),
                has_picture: true,
                mtime: 300,
                ..song("cd2/01.flac")
            },
        );
        let pictured = library.add(
            &album,
            &Song {
                disc_no: Some(1),
                track_no: Some(2),
                has_picture: true,
                mtime: 200,
                ..song("02.flac")
            },
        );
        let inputs = library.inputs(&album);
        assert_eq!(
            inputs.first_track,
            Some((library.folder_id, "01.flac".to_owned()))
        );
        assert_eq!(inputs.first_picture, Some((pictured, 200)));
    }

    #[test]
    fn cover_inputs_leave_out_a_track_without_a_file() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        let first = library.add(
            &album,
            &Song {
                track_no: Some(1),
                has_picture: true,
                ..song("01.flac")
            },
        );
        let second = library.add(
            &album,
            &Song {
                track_no: Some(2),
                has_picture: true,
                mtime: 200,
                ..song("sub/02.flac")
            },
        );
        assert_eq!(
            library.inputs(&album).first_picture,
            Some((first.clone(), 100))
        );

        // The first track has no file. It gives no directory and no picture.
        library.detach(&first);
        let inputs = library.inputs(&album);
        assert_eq!(
            inputs.first_track,
            Some((library.folder_id, "sub/02.flac".to_owned()))
        );
        assert_eq!(inputs.first_picture, Some((second.clone(), 200)));

        // No track has a file.
        library.detach(&second);
        let inputs = library.inputs(&album);
        assert_eq!(inputs.first_track, None);
        assert_eq!(inputs.first_picture, None);
    }

    #[test]
    fn cover_inputs_hold_every_album_in_id_order_also_one_without_tracks() {
        let library = Library::new();
        let mut albums = vec![library.album("Glass Harbour"), library.album("Paper Maps")];
        library.add(&albums[0], &song("a.flac"));
        albums.sort();
        let listed: Vec<String> = library
            .db
            .with_reader(cover_inputs)
            .unwrap()
            .into_iter()
            .map(|inputs| inputs.album_id)
            .collect();
        assert_eq!(listed, albums);
        let empty = library.album("Paper Maps");
        assert_eq!(library.inputs(&empty).first_track, None);
    }

    #[test]
    fn set_cover_stores_the_source_and_the_time_and_clears_them() {
        let library = Library::new();
        let album = library.album("Glass Harbour");
        let other = library.album("Paper Maps");
        let set = |source: Option<&str>, mtime: Option<i64>| {
            library
                .db
                .with_writer(|conn| set_cover(conn, &album, source, mtime))
                .unwrap();
            let inputs = library.inputs(&album);
            (inputs.cover_source, inputs.cover_mtime)
        };

        assert_eq!(
            set(Some("/music/Glass Harbour/cover.jpg"), Some(1234)),
            (
                Some("/music/Glass Harbour/cover.jpg".to_owned()),
                Some(1234)
            )
        );
        assert_eq!(
            library.row("glass harbour").cover_source.as_deref(),
            Some("/music/Glass Harbour/cover.jpg")
        );
        assert_eq!(set(None, None), (None, None));
        // The other album is not written.
        assert_eq!(library.inputs(&other).cover_source, None);
    }

    #[test]
    fn count_is_the_number_of_album_rows() {
        let library = Library::new();
        let count = || library.db.with_reader(count).unwrap();
        assert_eq!(count(), 0);
        library.album("Glass Harbour");
        library.album("Paper Maps");
        library.album("paper maps");
        assert_eq!(count(), 2);
    }
}
