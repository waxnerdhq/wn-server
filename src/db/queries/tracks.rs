//! Queries on the `tracks` table. A row is one audio file.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::int;
use crate::db::Result;
use crate::library::tags::TrackMeta;

/// What the scanner compares with the files on disk: the ID of a track, its
/// locator and the state of its file at the last scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: String,
    pub folder_id: i64,
    /// The path below the folder, components joined with `/`.
    pub rel_path: String,
    /// The modification time of the file in Unix seconds.
    pub mtime: i64,
    /// The size of the file in bytes.
    pub size: u64,
    pub fingerprint: String,
}

/// A row of the `tracks` table.
///
/// The column `has_lyrics` has no field. The server has no lyrics
/// (`docs/design.md` section 4), and no statement reads or writes the
/// column. A row that is inserted holds the default of the schema, which
/// is 0.
///
/// The columns `title_lc` and `album_artist_name` have no field either:
/// statements compare them, and no caller reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: String,
    pub folder_id: i64,
    /// The path below the folder, components joined with `/`.
    pub rel_path: String,
    pub fingerprint: String,
    pub title: String,
    pub album_id: String,
    /// The ID of the album artist.
    pub artist_id: String,
    /// The track artist tag.
    pub artist_name: String,
    pub album_name: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub duration_ms: u64,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    /// The size of the file in bytes.
    pub size: u64,
    /// The extension of the file in lower case.
    pub suffix: String,
    pub content_type: String,
    pub lossless: bool,
    pub has_picture: bool,
    /// The modification time of the file in Unix seconds.
    pub mtime: i64,
    /// The time of the first scan that saw the track, in Unix seconds.
    pub created_at: i64,
    /// The time of the last scan that changed the row, in Unix seconds.
    pub updated_at: i64,
}

impl Track {
    /// Returns whether the track has a file. [`has_file`] is the test.
    pub fn has_file(&self) -> bool {
        has_file(&self.rel_path)
    }
}

/// Returns the `rel_path` of the track `id` while the track has no file: a
/// `/` and the ID.
///
/// A relative path is built from the names of path components and never
/// starts with `/`, so no file has this path. No other row has it, because
/// the ID is unique. The scanner gives it to a row that it can neither
/// relink nor delete (`docs/design.md` section 3.10).
pub fn placeholder_path(id: &str) -> String {
    format!("/{id}")
}

/// Returns whether a track with this `rel_path` has a file, which is the
/// case when the value is not a placeholder path.
///
/// Every reader of `tracks` asks here before it builds a file path
/// (`docs/design.md` section 3.4). A placeholder path is never joined to a
/// folder path, because `Path::join` replaces the folder with a root path.
/// In SQL the same test is [`HAS_FILE_SQL`].
pub fn has_file(rel_path: &str) -> bool {
    !rel_path.starts_with('/')
}

/// The SQL test for a track row that has a file: the test of [`has_file`] as
/// a condition on the `rel_path` column.
///
/// Every statement of a query module that needs files holds this text, and
/// no module spells the test itself (`docs/design.md` section 3.4). The
/// column name carries no table name. `tracks` is the one table with a
/// `rel_path` column, so the text also serves a statement that joins other
/// tables or names `tracks` by an alias.
pub(super) const HAS_FILE_SQL: &str = "rel_path NOT LIKE '/%'";

/// The values of a track row that come from the content of its file.
#[derive(Debug, Clone, Copy)]
pub struct TrackContent<'a> {
    pub fingerprint: &'a str,
    /// The modification time of the file in Unix seconds.
    pub mtime: i64,
    /// The size of the file in bytes.
    pub size: u64,
    /// The tags and the audio properties. `meta.artist` is stored in the
    /// column `artist_name` and `meta.album_artist` in the column
    /// `album_artist_name`, which is the spelling that the track carries for
    /// its artist row (`docs/design.md` section 3.3).
    pub meta: &'a TrackMeta,
    /// The ID of the artist row of `meta.album_artist`.
    pub artist_id: &'a str,
    /// The ID of the album row of `meta.album`.
    pub album_id: &'a str,
}

/// The columns of [`Track`] in the order [`track_from_row`] reads them. The
/// names carry no table name, so the text serves a statement whose `FROM`
/// clause names `tracks` and no other table.
pub(super) const TRACK_COLUMNS: &str = "id, folder_id, rel_path, fingerprint, title, album_id, \
     artist_id, artist_name, album_name, track_no, disc_no, year, genre, duration_ms, \
     bitrate_kbps, sample_rate, bit_depth, channels, size, suffix, content_type, lossless, \
     has_picture, mtime, created_at, updated_at";

/// Returns the index entry of every track, in no particular order.
pub fn load_index(conn: &Connection) -> Result<Vec<IndexEntry>> {
    let mut select =
        conn.prepare("SELECT id, folder_id, rel_path, mtime, size, fingerprint FROM tracks")?;
    let entries = select
        .query_map([], |row| {
            Ok(IndexEntry {
                id: row.get(0)?,
                folder_id: row.get(1)?,
                rel_path: row.get(2)?,
                mtime: row.get(3)?,
                size: int::get_u64(row, 4)?,
                fingerprint: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(entries)
}

/// Inserts a track with the ID `id` at the locator `(folder_id, rel_path)`.
/// The creation time and the update time are the present time.
///
/// A locator that another track holds is an error.
pub fn insert_track(
    conn: &Connection,
    id: &str,
    folder_id: i64,
    rel_path: &str,
    content: &TrackContent<'_>,
) -> Result<()> {
    let meta = content.meta;
    conn.prepare_cached(
        "INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc, album_id,
           artist_id, artist_name, album_name, track_no, disc_no, year, genre, duration_ms,
           bitrate_kbps, sample_rate, bit_depth, channels, size, suffix, content_type, lossless,
           has_picture, mtime, album_artist_name, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
           ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, unixepoch(), unixepoch())",
    )?
    .execute(params![
        id,
        folder_id,
        rel_path,
        content.fingerprint,
        meta.title,
        meta.title.to_lowercase(),
        content.album_id,
        content.artist_id,
        meta.artist,
        meta.album,
        meta.track_no,
        meta.disc_no,
        meta.year,
        meta.genre,
        int::to_sql(meta.duration_ms)?,
        meta.bitrate_kbps,
        meta.sample_rate,
        meta.bit_depth,
        meta.channels,
        int::to_sql(content.size)?,
        meta.suffix,
        meta.content_type,
        meta.lossless,
        meta.has_picture,
        content.mtime,
        meta.album_artist,
    ])?;
    Ok(())
}

/// Replaces the values of the track `id` that come from the content of its
/// file and sets the update time to the present time. The ID, the locator
/// and the creation time stay.
pub fn update_track_meta(conn: &Connection, id: &str, content: &TrackContent<'_>) -> Result<()> {
    let meta = content.meta;
    conn.prepare_cached(
        "UPDATE tracks SET fingerprint = ?2, title = ?3, title_lc = ?4, album_id = ?5,
           artist_id = ?6, artist_name = ?7, album_name = ?8, track_no = ?9, disc_no = ?10,
           year = ?11, genre = ?12, duration_ms = ?13, bitrate_kbps = ?14, sample_rate = ?15,
           bit_depth = ?16, channels = ?17, size = ?18, suffix = ?19, content_type = ?20,
           lossless = ?21, has_picture = ?22, mtime = ?23, album_artist_name = ?24,
           updated_at = unixepoch()
         WHERE id = ?1",
    )?
    .execute(params![
        id,
        content.fingerprint,
        meta.title,
        meta.title.to_lowercase(),
        content.album_id,
        content.artist_id,
        meta.artist,
        meta.album,
        meta.track_no,
        meta.disc_no,
        meta.year,
        meta.genre,
        int::to_sql(meta.duration_ms)?,
        meta.bitrate_kbps,
        meta.sample_rate,
        meta.bit_depth,
        meta.channels,
        int::to_sql(content.size)?,
        meta.suffix,
        meta.content_type,
        meta.lossless,
        meta.has_picture,
        content.mtime,
        meta.album_artist,
    ])?;
    Ok(())
}

/// Moves the track `id` to the locator `(folder_id, rel_path)`, stores the
/// modification time and the size of the file there, and sets the update
/// time to the present time. Every other value stays.
///
/// A locator that another track holds is an error.
pub fn relink_track(
    conn: &Connection,
    id: &str,
    folder_id: i64,
    rel_path: &str,
    mtime: i64,
    size: u64,
) -> Result<()> {
    conn.prepare_cached(
        "UPDATE tracks SET folder_id = ?2, rel_path = ?3, mtime = ?4, size = ?5,
           updated_at = unixepoch()
         WHERE id = ?1",
    )?
    .execute(params![id, folder_id, rel_path, mtime, int::to_sql(size)?])?;
    Ok(())
}

/// Deletes the tracks with the IDs `ids` and returns the number of rows
/// deleted. The playlist entries and the play queue entries of a deleted
/// track go with it.
pub fn delete_tracks<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<u64> {
    let mut delete = conn.prepare_cached("DELETE FROM tracks WHERE id = ?1")?;
    let mut deleted = 0;
    for id in ids {
        deleted += delete.execute([id.as_ref()])? as u64;
    }
    Ok(deleted)
}

/// Returns the track `id`, or `None` when no track has the ID.
pub fn get_track(conn: &Connection, id: &str) -> Result<Option<Track>> {
    let track = conn
        .prepare_cached(&format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE id = ?1"))?
        .query_row([id], track_from_row)
        .optional()?;
    Ok(track)
}

/// Returns the number of tracks.
pub fn count_tracks(conn: &Connection) -> Result<u64> {
    Ok(conn.query_row("SELECT count(*) FROM tracks", [], |row| {
        int::get_u64(row, 0)
    })?)
}

/// Returns the number of tracks in the folder `folder_id`.
pub fn count_tracks_in_folder(conn: &Connection, folder_id: i64) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM tracks WHERE folder_id = ?1",
        [folder_id],
        |row| int::get_u64(row, 0),
    )?)
}

/// Reads a [`Track`] from the first columns of `row`, which are
/// [`TRACK_COLUMNS`].
pub(super) fn track_from_row(row: &Row<'_>) -> rusqlite::Result<Track> {
    Ok(Track {
        id: row.get(0)?,
        folder_id: row.get(1)?,
        rel_path: row.get(2)?,
        fingerprint: row.get(3)?,
        title: row.get(4)?,
        album_id: row.get(5)?,
        artist_id: row.get(6)?,
        artist_name: row.get(7)?,
        album_name: row.get(8)?,
        track_no: row.get(9)?,
        disc_no: row.get(10)?,
        year: row.get(11)?,
        genre: row.get(12)?,
        duration_ms: int::get_u64(row, 13)?,
        bitrate_kbps: row.get(14)?,
        sample_rate: row.get(15)?,
        bit_depth: row.get(16)?,
        channels: row.get(17)?,
        size: int::get_u64(row, 18)?,
        suffix: row.get(19)?,
        content_type: row.get(20)?,
        lossless: row.get(21)?,
        has_picture: row.get(22)?,
        mtime: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::ErrorCode;

    use super::*;
    use crate::db::queries::{albums, artists, folders};
    use crate::db::{Db, DbError};
    use crate::ids::{self, Kind};

    const FIRST: &str = "tr-00000000000000000000000000000001";
    const SECOND: &str = "tr-00000000000000000000000000000002";

    /// A database with two folders, one artist and one album.
    struct Fixture {
        db: Db,
        folders: [i64; 2],
        artist_id: String,
        album_id: String,
    }

    fn fixture() -> Fixture {
        let db = Db::open_memory().unwrap();
        let (folders, artist_id, album_id) = db
            .with_writer(|conn| {
                let paths = [PathBuf::from("/music/a"), PathBuf::from("/music/b")];
                let folders = folders::sync_folders(conn, &paths)?;
                let artist_id = artists::upsert_by_key(conn, "Aurora Field")?;
                let album_id = albums::upsert_by_key(
                    conn,
                    &artist_id,
                    "Glass Harbour",
                    "Aurora Field",
                    false,
                )?;
                Ok(([folders[0].id, folders[1].id], artist_id, album_id))
            })
            .unwrap();
        Fixture {
            db,
            folders,
            artist_id,
            album_id,
        }
    }

    /// Returns metadata in which every optional field has a value.
    fn full_meta() -> TrackMeta {
        TrackMeta {
            title: "Low Tide".to_owned(),
            artist: "Aurora Field".to_owned(),
            album_artist: "Aurora Field".to_owned(),
            album: "Glass Harbour".to_owned(),
            track_no: Some(1),
            disc_no: Some(2),
            year: Some(2019),
            genre: Some("Electronic".to_owned()),
            compilation: false,
            duration_ms: 500,
            bitrate_kbps: Some(58),
            sample_rate: Some(22050),
            bit_depth: Some(16),
            channels: Some(1),
            has_picture: true,
            suffix: "flac".to_owned(),
            content_type: "audio/flac".to_owned(),
            lossless: true,
        }
    }

    /// Returns metadata in which every optional field is absent and every
    /// other field differs from [`full_meta`].
    fn bare_meta() -> TrackMeta {
        TrackMeta {
            title: "ÉTUDE".to_owned(),
            artist: "Tessa Lund".to_owned(),
            album_artist: "Tessa Lund".to_owned(),
            album: "Open Road".to_owned(),
            track_no: None,
            disc_no: None,
            year: None,
            genre: None,
            compilation: false,
            duration_ms: 1234,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            has_picture: false,
            suffix: "mp3".to_owned(),
            content_type: "audio/mpeg".to_owned(),
            lossless: false,
        }
    }

    impl Fixture {
        fn content<'a>(
            &'a self,
            meta: &'a TrackMeta,
            fingerprint: &'a str,
            mtime: i64,
            size: u64,
        ) -> TrackContent<'a> {
            TrackContent {
                fingerprint,
                mtime,
                size,
                meta,
                artist_id: &self.artist_id,
                album_id: &self.album_id,
            }
        }

        fn insert(&self, id: &str, folder_id: i64, rel_path: &str) {
            let meta = full_meta();
            let content = self.content(&meta, "100:aa", 1000, 100);
            self.db
                .with_writer(|conn| insert_track(conn, id, folder_id, rel_path, &content))
                .unwrap();
        }

        fn get(&self, id: &str) -> Option<Track> {
            self.db.with_reader(|conn| get_track(conn, id)).unwrap()
        }

        /// Sets the creation time and the update time of every track to 10,
        /// so that a test sees which statement writes them.
        fn age_rows(&self) {
            self.db
                .with_writer(|conn| {
                    conn.execute("UPDATE tracks SET created_at = 10, updated_at = 10", [])?;
                    Ok(())
                })
                .unwrap();
        }

        fn title_lc(&self, id: &str) -> String {
            self.db
                .with_reader(|conn| {
                    Ok(conn.query_row(
                        "SELECT title_lc FROM tracks WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
        }

        fn album_artist_name(&self, id: &str) -> Option<String> {
            self.db
                .with_reader(|conn| {
                    Ok(conn.query_row(
                        "SELECT album_artist_name FROM tracks WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
        }
    }

    fn constraint_violation(error: DbError) -> String {
        let DbError::Sqlite(error) = error else {
            panic!("{error}");
        };
        assert_eq!(
            error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        error.to_string()
    }

    #[test]
    fn insert_stores_every_column_and_get_reads_it_back() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "Glass Harbour/01 Low Tide.flac");

        let track = fixture.get(FIRST).unwrap();
        assert!(track.created_at > 10, "{track:?}");
        assert_eq!(
            track,
            Track {
                id: FIRST.to_owned(),
                folder_id: fixture.folders[0],
                rel_path: "Glass Harbour/01 Low Tide.flac".to_owned(),
                fingerprint: "100:aa".to_owned(),
                title: "Low Tide".to_owned(),
                album_id: fixture.album_id.clone(),
                artist_id: fixture.artist_id.clone(),
                artist_name: "Aurora Field".to_owned(),
                album_name: "Glass Harbour".to_owned(),
                track_no: Some(1),
                disc_no: Some(2),
                year: Some(2019),
                genre: Some("Electronic".to_owned()),
                duration_ms: 500,
                bitrate_kbps: Some(58),
                sample_rate: Some(22050),
                bit_depth: Some(16),
                channels: Some(1),
                size: 100,
                suffix: "flac".to_owned(),
                content_type: "audio/flac".to_owned(),
                lossless: true,
                has_picture: true,
                mtime: 1000,
                created_at: track.created_at,
                updated_at: track.created_at,
            }
        );
        assert_eq!(fixture.title_lc(FIRST), "low tide");
    }

    #[test]
    fn has_file_is_false_for_a_placeholder_path_only() {
        assert!(has_file("Glass Harbour/01 Low Tide.flac"));
        assert!(has_file("a.flac"));
        assert!(has_file("tr-00000000000000000000000000000001"));
        assert_eq!(placeholder_path(FIRST), format!("/{FIRST}"));
        assert!(!has_file(&placeholder_path(FIRST)));

        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        assert!(fixture.get(FIRST).unwrap().has_file());
        fixture
            .db
            .with_writer(|conn| {
                let placeholder = placeholder_path(FIRST);
                relink_track(conn, FIRST, fixture.folders[0], &placeholder, 1000, 100)
            })
            .unwrap();
        assert!(!fixture.get(FIRST).unwrap().has_file());
    }

    #[test]
    fn the_sql_filter_for_tracks_with_a_file_agrees_with_has_file() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "sub/a.flac");
        fixture.insert(SECOND, fixture.folders[0], "b.flac");
        fixture
            .db
            .with_writer(|conn| {
                let placeholder = placeholder_path(SECOND);
                relink_track(conn, SECOND, fixture.folders[0], &placeholder, 1000, 100)
            })
            .unwrap();

        let with_file: Vec<String> = fixture
            .db
            .with_reader(|conn| {
                let mut select = conn.prepare(&format!(
                    "SELECT id FROM tracks WHERE {HAS_FILE_SQL} ORDER BY id"
                ))?;
                let ids = select
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                Ok(ids)
            })
            .unwrap();
        let mut expected: Vec<String> = fixture
            .db
            .with_reader(load_index)
            .unwrap()
            .into_iter()
            .filter(|entry| has_file(&entry.rel_path))
            .map(|entry| entry.id)
            .collect();
        expected.sort();
        assert_eq!(with_file, expected);
        assert_eq!(with_file, [FIRST]);
    }

    #[test]
    fn the_sql_test_for_a_file_agrees_with_has_file_for_each_kind_of_path() {
        let db = Db::open_memory().unwrap();
        let placeholder = placeholder_path(FIRST);
        // `%` and `_` are the wildcards of `LIKE`. In a path they are text.
        let paths = [
            "a.flac",
            "a/b.flac",
            "%/a.flac",
            "_",
            "a//b.flac",
            placeholder.as_str(),
            "/",
            "/%",
        ];
        for rel_path in paths {
            let in_sql: bool = db
                .with_reader(|conn| {
                    let sql = format!("SELECT ?1 AS rel_path WHERE {HAS_FILE_SQL}");
                    Ok(conn.prepare(&sql)?.exists([rel_path])?)
                })
                .unwrap();
            assert_eq!(in_sql, has_file(rel_path), "{rel_path}");
        }
    }

    #[test]
    fn get_track_is_none_for_an_unknown_id() {
        let fixture = fixture();
        assert_eq!(fixture.get(FIRST), None);
    }

    #[test]
    fn insert_at_a_locator_that_is_taken_is_an_error() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        let meta = full_meta();
        let content = fixture.content(&meta, "100:aa", 1000, 100);
        let error = fixture
            .db
            .with_writer(|conn| insert_track(conn, SECOND, fixture.folders[0], "a.flac", &content))
            .unwrap_err();
        let message = constraint_violation(error);
        assert!(message.contains("tracks.rel_path"), "{message}");

        // The same path in the other folder is another locator.
        fixture
            .db
            .with_writer(|conn| insert_track(conn, SECOND, fixture.folders[1], "a.flac", &content))
            .unwrap();
    }

    #[test]
    fn update_track_meta_replaces_the_content_and_keeps_the_identity() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.age_rows();
        let (artist_id, album_id) = fixture
            .db
            .with_writer(|conn| {
                let artist_id = artists::upsert_by_key(conn, "Tessa Lund")?;
                let album_id =
                    albums::upsert_by_key(conn, &artist_id, "Open Road", "Tessa Lund", false)?;
                Ok((artist_id, album_id))
            })
            .unwrap();

        let meta = bare_meta();
        let content = TrackContent {
            fingerprint: "200:bb",
            mtime: 2000,
            size: 200,
            meta: &meta,
            artist_id: &artist_id,
            album_id: &album_id,
        };
        fixture
            .db
            .with_writer(|conn| update_track_meta(conn, FIRST, &content))
            .unwrap();

        let track = fixture.get(FIRST).unwrap();
        assert!(track.updated_at > 10, "{track:?}");
        assert_eq!(
            track,
            Track {
                id: FIRST.to_owned(),
                folder_id: fixture.folders[0],
                rel_path: "a.flac".to_owned(),
                fingerprint: "200:bb".to_owned(),
                title: "ÉTUDE".to_owned(),
                album_id,
                artist_id,
                artist_name: "Tessa Lund".to_owned(),
                album_name: "Open Road".to_owned(),
                track_no: None,
                disc_no: None,
                year: None,
                genre: None,
                duration_ms: 1234,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                size: 200,
                suffix: "mp3".to_owned(),
                content_type: "audio/mpeg".to_owned(),
                lossless: false,
                has_picture: false,
                mtime: 2000,
                created_at: 10,
                updated_at: track.updated_at,
            }
        );
        assert_eq!(fixture.title_lc(FIRST), "étude");
    }

    #[test]
    fn insert_and_update_store_the_album_artist_and_a_relink_keeps_it() {
        let fixture = fixture();
        // The album artist and the track artist of the file differ.
        let mut meta = full_meta();
        meta.artist = "Tessa Lund".to_owned();
        meta.album_artist = "AURORA FIELD".to_owned();
        let content = fixture.content(&meta, "100:aa", 1000, 100);
        fixture
            .db
            .with_writer(|conn| insert_track(conn, FIRST, fixture.folders[0], "a.flac", &content))
            .unwrap();
        assert_eq!(
            fixture.album_artist_name(FIRST).as_deref(),
            Some("AURORA FIELD")
        );
        assert_eq!(fixture.get(FIRST).unwrap().artist_name, "Tessa Lund");

        meta.album_artist = "Aurora Field".to_owned();
        let content = fixture.content(&meta, "100:aa", 1000, 100);
        fixture
            .db
            .with_writer(|conn| update_track_meta(conn, FIRST, &content))
            .unwrap();
        assert_eq!(
            fixture.album_artist_name(FIRST).as_deref(),
            Some("Aurora Field")
        );
        assert_eq!(fixture.get(FIRST).unwrap().artist_name, "Tessa Lund");

        fixture
            .db
            .with_writer(|conn| relink_track(conn, FIRST, fixture.folders[1], "b.flac", 2000, 100))
            .unwrap();
        assert_eq!(
            fixture.album_artist_name(FIRST).as_deref(),
            Some("Aurora Field")
        );
    }

    #[test]
    fn a_row_without_an_album_artist_gets_it_when_its_content_is_stored_again() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        // The row is as the migration leaves a row of schema version 1.
        fixture
            .db
            .with_writer(|conn| {
                conn.execute("UPDATE tracks SET album_artist_name = NULL", [])?;
                Ok(())
            })
            .unwrap();

        // A relink reads no tag and leaves the column without a value.
        fixture
            .db
            .with_writer(|conn| relink_track(conn, FIRST, fixture.folders[1], "b.flac", 2000, 100))
            .unwrap();
        assert_eq!(fixture.album_artist_name(FIRST), None);

        let meta = full_meta();
        let content = fixture.content(&meta, "100:aa", 2000, 100);
        fixture
            .db
            .with_writer(|conn| update_track_meta(conn, FIRST, &content))
            .unwrap();
        assert_eq!(
            fixture.album_artist_name(FIRST).as_deref(),
            Some("Aurora Field")
        );
    }

    #[test]
    fn relink_track_changes_the_locator_and_the_file_state_only() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.age_rows();
        let before = fixture.get(FIRST).unwrap();

        fixture
            .db
            .with_writer(|conn| {
                relink_track(conn, FIRST, fixture.folders[1], "moved/b.flac", 3000, 300)
            })
            .unwrap();

        let after = fixture.get(FIRST).unwrap();
        assert!(after.updated_at > 10, "{after:?}");
        assert_eq!(
            after,
            Track {
                folder_id: fixture.folders[1],
                rel_path: "moved/b.flac".to_owned(),
                mtime: 3000,
                size: 300,
                updated_at: after.updated_at,
                ..before
            }
        );
    }

    #[test]
    fn relink_to_a_locator_that_is_taken_is_an_error() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.insert(SECOND, fixture.folders[0], "b.flac");
        let error = fixture
            .db
            .with_writer(|conn| relink_track(conn, FIRST, fixture.folders[0], "b.flac", 1000, 100))
            .unwrap_err();
        constraint_violation(error);
        assert_eq!(fixture.get(FIRST).unwrap().rel_path, "a.flac");
    }

    #[test]
    fn delete_tracks_removes_the_named_rows_and_counts_them() {
        let fixture = fixture();
        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.insert(SECOND, fixture.folders[0], "b.flac");

        let unknown = ids::new_id(Kind::Track);
        let deleted = fixture
            .db
            .with_writer(|conn| delete_tracks(conn, &[FIRST, unknown.as_str()]))
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(fixture.get(FIRST), None);
        assert!(fixture.get(SECOND).is_some());

        let none: [&str; 0] = [];
        let deleted = fixture
            .db
            .with_writer(|conn| delete_tracks(conn, &none))
            .unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn counts_cover_the_table_and_one_folder() {
        let fixture = fixture();
        let count = |folder: Option<i64>| {
            fixture
                .db
                .with_reader(|conn| match folder {
                    Some(folder_id) => count_tracks_in_folder(conn, folder_id),
                    None => count_tracks(conn),
                })
                .unwrap()
        };
        assert_eq!(count(None), 0);

        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.insert(SECOND, fixture.folders[1], "a.flac");
        fixture.insert(
            "tr-00000000000000000000000000000003",
            fixture.folders[1],
            "b.flac",
        );
        assert_eq!(count(None), 3);
        assert_eq!(count(Some(fixture.folders[0])), 1);
        assert_eq!(count(Some(fixture.folders[1])), 2);
        assert_eq!(count(Some(99)), 0);
    }

    #[test]
    fn load_index_returns_the_locator_and_the_file_state_of_every_track() {
        let fixture = fixture();
        assert_eq!(fixture.db.with_reader(load_index).unwrap(), []);

        fixture.insert(FIRST, fixture.folders[0], "a.flac");
        fixture.insert(SECOND, fixture.folders[1], "sub/b.flac");
        fixture
            .db
            .with_writer(|conn| relink_track(conn, SECOND, fixture.folders[1], "sub/b.flac", 7, 9))
            .unwrap();

        let mut index = fixture.db.with_reader(load_index).unwrap();
        index.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            index,
            [
                IndexEntry {
                    id: FIRST.to_owned(),
                    folder_id: fixture.folders[0],
                    rel_path: "a.flac".to_owned(),
                    mtime: 1000,
                    size: 100,
                    fingerprint: "100:aa".to_owned(),
                },
                IndexEntry {
                    id: SECOND.to_owned(),
                    folder_id: fixture.folders[1],
                    rel_path: "sub/b.flac".to_owned(),
                    mtime: 7,
                    size: 9,
                    fingerprint: "100:aa".to_owned(),
                },
            ]
        );
    }
}
