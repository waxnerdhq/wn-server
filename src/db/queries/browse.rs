//! The queries that read the library for the API: artists, albums, songs,
//! genres, music folders, the path of the file of a track and the time of
//! the last scan.
//!
//! The rows of this module are what the response models are built from
//! (`api::model`). A handler reads rows here, reads the annotations of the
//! user for their IDs with `annotations::get_many`, and passes both to a
//! model constructor.
//!
//! # Lists of other modules
//!
//! A query module of a group of API methods (lists, search, playlists, play
//! queue) selects the IDs of its result in the order of the result and reads
//! the rows with [`artists_by_ids`], [`albums_by_ids`] and
//! [`tracks_by_ids`]. It needs no column list and no row reader of its own.
//!
//! # Tracks without a file
//!
//! A track row whose `rel_path` is a placeholder path has no file
//! (`docs/design.md` section 3.4). The functions here return such a row as
//! every other row: it keeps its tags, its ID and its annotations, and the
//! counts of its album and of its genre include it. The functions that ask
//! where a file is leave it out: [`list_artists`] with a folder, and
//! [`file_path`]. A caller that needs the file of a [`Song`] asks
//! `Track::has_file` or [`file_path`] and never reads `rel_path` for that.

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Row};

use super::albums::TRACK_ORDER;
use super::int;
use super::tracks::{self, HAS_FILE_SQL, Track};
use crate::db::Result;

pub use super::folders::Folder;
/// Returns every music folder row in ID order.
pub use super::folders::list as folders;

/// The columns of [`Artist`] in the order [`artist_from_row`] reads them.
const ARTIST_COLUMNS: &str = "id, name, sort_name, album_count";

/// The columns of [`Album`] in the order [`album_from_row`] reads them.
const ALBUM_COLUMNS: &str = "id, artist_id, name, artist_name, year, genre, song_count, \
     duration_ms, compilation, cover_source, cover_mtime, created_at";

/// The column that follows `tracks::TRACK_COLUMNS` in a statement that reads
/// a [`Song`]: whether the album of the track has a cover.
const ALBUM_HAS_COVER: &str = "EXISTS (SELECT 1 FROM albums \
     WHERE albums.id = tracks.album_id AND albums.cover_source IS NOT NULL)";

/// A row of the `artists` table: one album artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub id: String,
    /// The display name: a spelling of the name that the tags carry
    /// (`docs/design.md` section 3.3).
    pub name: String,
    /// The same value as `name`.
    pub sort_name: String,
    /// The number of albums of the artist, as the last scan counted them.
    pub album_count: u32,
}

/// A row of the `albums` table: one album of one album artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: String,
    /// The ID of the album artist.
    pub artist_id: String,
    /// The display name: a spelling of the title that the tags carry
    /// (`docs/design.md` section 3.3).
    pub name: String,
    /// The display name of the album artist.
    pub artist_name: String,
    /// The smallest year of a track of the album.
    pub year: Option<u32>,
    /// The genre that most tracks of the album have.
    pub genre: Option<String>,
    /// The number of track rows of the album. A track without a file counts.
    pub song_count: u32,
    /// The sum of the durations of the tracks in milliseconds.
    pub duration_ms: u64,
    pub compilation: bool,
    /// The absolute path of the cover image file, or `embedded:<track id>`.
    /// `None` when the album has no cover.
    pub cover_source: Option<String>,
    /// The modification time of the cover image file, or of the track that
    /// holds the embedded picture, in Unix seconds.
    pub cover_mtime: Option<i64>,
    /// The time of the first scan that saw the album, in Unix seconds.
    pub created_at: i64,
}

/// A track with the one value of its album row that the response model of a
/// song needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Song {
    /// The row of the `tracks` table.
    pub track: Track,
    /// Whether the album of the track has a cover, which is the case when
    /// `albums.cover_source` is set.
    pub album_has_cover: bool,
}

/// A genre with the number of tracks and albums that have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genre {
    /// The genre as the tags spell it.
    pub name: String,
    /// The number of tracks whose genre tag is `name`. A track without a
    /// file counts.
    pub song_count: u64,
    /// The number of albums whose genre is `name`. The genre of an album is
    /// the genre that most of its tracks have, so an album counts for one
    /// genre.
    pub album_count: u64,
}

/// Returns the artists in the order of their natural keys, which is the
/// order of their lower-cased names.
///
/// With `folder_id` the result holds the artists that have a track with a
/// file in that music folder. A track without a file is in no folder. The
/// album count of an artist is the stored one in both cases: it counts the
/// albums of the artist in every folder.
pub fn list_artists(conn: &Connection, folder_id: Option<i64>) -> Result<Vec<Artist>> {
    let mut select = conn.prepare_cached(&format!(
        "SELECT {ARTIST_COLUMNS} FROM artists
         WHERE ?1 IS NULL OR EXISTS (
           SELECT 1 FROM tracks
           WHERE tracks.artist_id = artists.id AND tracks.folder_id = ?1 AND {HAS_FILE_SQL})
         ORDER BY key"
    ))?;
    let artists = select
        .query_map([folder_id], artist_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(artists)
}

/// Returns the artist `id`, or `None` when no artist has the ID.
pub fn artist(conn: &Connection, id: &str) -> Result<Option<Artist>> {
    let artist = conn
        .prepare_cached(&format!(
            "SELECT {ARTIST_COLUMNS} FROM artists WHERE id = ?1"
        ))?
        .query_row([id], artist_from_row)
        .optional()?;
    Ok(artist)
}

/// Returns the artists with the IDs `ids`, in the order of `ids`. An ID that
/// no artist has adds nothing. An ID that `ids` holds twice adds its artist
/// twice.
pub fn artists_by_ids<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<Vec<Artist>> {
    let sql = format!(
        "SELECT {ARTIST_COLUMNS} FROM artists WHERE id IN (SELECT value FROM json_each(?1))"
    );
    let rows = rows_with_ids(conn, &sql, ids, artist_from_row)?;
    Ok(in_order_of(ids, rows, |artist| &artist.id))
}

/// Returns the albums of the artist `artist_id`: by year, where an album
/// without one follows the others, then by the natural key of the title. An
/// ID that no artist has gives no album.
pub fn albums_of_artist(conn: &Connection, artist_id: &str) -> Result<Vec<Album>> {
    let mut select = conn.prepare_cached(&format!(
        "SELECT {ALBUM_COLUMNS} FROM albums WHERE artist_id = ?1
         ORDER BY year IS NULL, year, key, id"
    ))?;
    let albums = select
        .query_map([artist_id], album_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(albums)
}

/// Returns the album `id`, or `None` when no album has the ID.
pub fn album(conn: &Connection, id: &str) -> Result<Option<Album>> {
    let album = conn
        .prepare_cached(&format!("SELECT {ALBUM_COLUMNS} FROM albums WHERE id = ?1"))?
        .query_row([id], album_from_row)
        .optional()?;
    Ok(album)
}

/// Returns the albums with the IDs `ids`, in the order of `ids`. An ID that
/// no album has adds nothing. An ID that `ids` holds twice adds its album
/// twice.
pub fn albums_by_ids<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<Vec<Album>> {
    let sql =
        format!("SELECT {ALBUM_COLUMNS} FROM albums WHERE id IN (SELECT value FROM json_each(?1))");
    let rows = rows_with_ids(conn, &sql, ids, album_from_row)?;
    Ok(in_order_of(ids, rows, |album| &album.id))
}

/// Returns the tracks of the album `album_id`: by disc number, where a track
/// without one counts as disc 1, then by track number, where a track without
/// one follows the numbered tracks, then by path. The order is
/// `albums::TRACK_ORDER`, in which the scanner picks the first track of an
/// album. An ID that no album has gives no track.
///
/// The result holds every track row of the album, so its length is the
/// `song_count` of the album after a scan. A track without a file is one of
/// them.
pub fn tracks_of_album(conn: &Connection, album_id: &str) -> Result<Vec<Song>> {
    let mut select = conn.prepare_cached(&format!(
        "SELECT {}, {ALBUM_HAS_COVER} FROM tracks WHERE album_id = ?1 ORDER BY {TRACK_ORDER}",
        tracks::TRACK_COLUMNS
    ))?;
    let songs = select
        .query_map([album_id], song_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(songs)
}

/// Returns the track `id`, or `None` when no track has the ID.
pub fn track(conn: &Connection, id: &str) -> Result<Option<Song>> {
    let song = conn
        .prepare_cached(&format!(
            "SELECT {}, {ALBUM_HAS_COVER} FROM tracks WHERE id = ?1",
            tracks::TRACK_COLUMNS
        ))?
        .query_row([id], song_from_row)
        .optional()?;
    Ok(song)
}

/// Returns the tracks with the IDs `ids`, in the order of `ids`. An ID that
/// no track has adds nothing. An ID that `ids` holds twice adds its track
/// twice, as a playlist that holds a track twice needs it.
pub fn tracks_by_ids<S: AsRef<str>>(conn: &Connection, ids: &[S]) -> Result<Vec<Song>> {
    let sql = format!(
        "SELECT {}, {ALBUM_HAS_COVER} FROM tracks
         WHERE id IN (SELECT value FROM json_each(?1))",
        tracks::TRACK_COLUMNS
    );
    let rows = rows_with_ids(conn, &sql, ids, song_from_row)?;
    Ok(in_order_of(ids, rows, |song| &song.track.id))
}

/// Returns the genres of the tracks in the order of their names, without
/// regard to ASCII case. A track without a genre tag adds nothing.
///
/// Two spellings of a genre that differ by case are two genres, because the
/// tracks are grouped by the tag as it is stored.
pub fn genres(conn: &Connection) -> Result<Vec<Genre>> {
    let mut select = conn.prepare_cached(
        "SELECT tracks.genre, count(*),
           (SELECT count(*) FROM albums WHERE albums.genre = tracks.genre)
         FROM tracks WHERE tracks.genre IS NOT NULL
         GROUP BY tracks.genre
         ORDER BY tracks.genre COLLATE NOCASE, tracks.genre",
    )?;
    let genres = select
        .query_map([], |row| {
            Ok(Genre {
                name: row.get(0)?,
                song_count: int::get_u64(row, 1)?,
                album_count: int::get_u64(row, 2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(genres)
}

/// Returns the absolute path of the file of `track`: the path of its music
/// folder joined with the components of its relative path.
///
/// Returns `None` for a track without a file. Its `rel_path` is a
/// placeholder path, which is never joined to a folder path
/// (`docs/design.md` section 3.4). Returns `None` as well when the folder row
/// of the track is gone.
///
/// The path is where the last scan saw the file. The file can be gone or
/// replaced since then.
pub fn file_path(conn: &Connection, track: &Track) -> Result<Option<PathBuf>> {
    if !track.has_file() {
        return Ok(None);
    }
    let folder: Option<String> = conn
        .prepare_cached("SELECT path FROM folders WHERE id = ?1")?
        .query_row([track.folder_id], |row| row.get(0))
        .optional()?;
    Ok(folder.map(|folder| {
        let mut path = PathBuf::from(folder);
        path.extend(track.rel_path.split('/'));
        path
    }))
}

/// Runs `sql`, whose one parameter is a JSON array of IDs, with `ids` and
/// reads every row with `read`.
///
/// The IDs are one parameter, so a list of any length is one statement and
/// no ID is part of the SQL text.
fn rows_with_ids<T, S: AsRef<str>>(
    conn: &Connection,
    sql: &str,
    ids: &[S],
    read: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = conn
        .prepare_cached(sql)?
        .query_map([json_array(ids)], read)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Returns `ids` as the text of a JSON array of strings, which is the
/// parameter of `json_each` in the statements that take a list of IDs.
pub(super) fn json_array<S: AsRef<str>>(ids: &[S]) -> String {
    let ids: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
    serde_json::to_string(&ids).expect("a list of strings serialises to JSON")
}

/// Returns the rows of `rows` in the order of `ids`. `id_of` returns the ID
/// of a row. An ID without a row adds nothing, and an ID that `ids` holds
/// twice adds its row twice.
///
/// A row moves into the result at the last place at which `ids` names it,
/// and it is cloned for each earlier place. A list that names no ID twice
/// clones no row.
fn in_order_of<T: Clone, S: AsRef<str>>(
    ids: &[S],
    rows: Vec<T>,
    id_of: fn(&T) -> &String,
) -> Vec<T> {
    // The last place of each ID in `ids`. A later place replaces an earlier
    // one of the same ID.
    let last_place: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(place, id)| (id.as_ref(), place))
        .collect();
    // The rows by ID. The keys are the texts of `ids`, so no ID is copied.
    let mut by_id: HashMap<&str, T> = HashMap::with_capacity(rows.len());
    for row in rows {
        if let Some((id, _)) = last_place.get_key_value(id_of(&row).as_str()) {
            by_id.insert(*id, row);
        }
    }
    ids.iter()
        .enumerate()
        .filter_map(|(place, id)| {
            let id = id.as_ref();
            if last_place.get(id) == Some(&place) {
                by_id.remove(id)
            } else {
                by_id.get(id).cloned()
            }
        })
        .collect()
}

fn artist_from_row(row: &Row<'_>) -> rusqlite::Result<Artist> {
    Ok(Artist {
        id: row.get(0)?,
        name: row.get(1)?,
        sort_name: row.get(2)?,
        album_count: row.get(3)?,
    })
}

fn album_from_row(row: &Row<'_>) -> rusqlite::Result<Album> {
    Ok(Album {
        id: row.get(0)?,
        artist_id: row.get(1)?,
        name: row.get(2)?,
        artist_name: row.get(3)?,
        year: row.get(4)?,
        genre: row.get(5)?,
        song_count: row.get(6)?,
        duration_ms: int::get_u64(row, 7)?,
        compilation: row.get(8)?,
        cover_source: row.get(9)?,
        cover_mtime: row.get(10)?,
        created_at: row.get(11)?,
    })
}

/// Reads a [`Song`] from a row that holds `tracks::TRACK_COLUMNS` and then
/// [`ALBUM_HAS_COVER`] as its last column.
fn song_from_row(row: &Row<'_>) -> rusqlite::Result<Song> {
    let last = row.as_ref().column_count() - 1;
    Ok(Song {
        track: tracks::track_from_row(row)?,
        album_has_cover: row.get(last)?,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;
    use std::rc::Rc;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::TrackContent;
    use crate::db::queries::{albums, artists, folders as folder_rows};
    use crate::library::tags::TrackMeta;

    /// One track of a test library. [`song`] returns the values that a test
    /// does not set.
    #[derive(Debug, Clone)]
    struct Spec {
        id: &'static str,
        rel_path: &'static str,
        folder: &'static str,
        artist: &'static str,
        album_artist: &'static str,
        album: &'static str,
        title: &'static str,
        disc_no: Option<u32>,
        track_no: Option<u32>,
        year: Option<u32>,
        genre: Option<&'static str>,
        duration_ms: u64,
    }

    /// Returns a track of "Aurora Field" on "Glass Harbour" in the folder
    /// `/music`, without numbers, year and genre, that lasts one second.
    fn song(id: &'static str, rel_path: &'static str) -> Spec {
        Spec {
            id,
            rel_path,
            folder: "/music",
            artist: "Aurora Field",
            album_artist: "Aurora Field",
            album: "Glass Harbour",
            title: "Low Tide",
            disc_no: None,
            track_no: None,
            year: None,
            genre: None,
            duration_ms: 1000,
        }
    }

    const T1: &str = "tr-00000000000000000000000000000001";
    const T2: &str = "tr-00000000000000000000000000000002";
    const T3: &str = "tr-00000000000000000000000000000003";
    const T4: &str = "tr-00000000000000000000000000000004";
    const T5: &str = "tr-00000000000000000000000000000005";
    const UNKNOWN_TRACK: &str = "tr-ffffffffffffffffffffffffffffffff";

    /// Stores `specs` as the scanner stores tracks: it inserts the folder,
    /// the artist and the album of each track when they are missing, and
    /// recomputes the counts of the albums and of the artists at the end.
    fn library(specs: &[Spec]) -> Db {
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| {
            for spec in specs {
                let folder = folder_rows::sync_folders(conn, &[PathBuf::from(spec.folder)])?;
                let artist_id = artists::upsert_by_key(conn, spec.album_artist)?;
                let album_id =
                    albums::upsert_by_key(conn, &artist_id, spec.album, spec.album_artist, false)?;
                let meta = TrackMeta {
                    title: spec.title.to_owned(),
                    artist: spec.artist.to_owned(),
                    album_artist: spec.album_artist.to_owned(),
                    album: spec.album.to_owned(),
                    track_no: spec.track_no,
                    disc_no: spec.disc_no,
                    year: spec.year,
                    genre: spec.genre.map(str::to_owned),
                    compilation: false,
                    duration_ms: spec.duration_ms,
                    bitrate_kbps: Some(900),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    channels: Some(2),
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
                    artist_id: &artist_id,
                    album_id: &album_id,
                };
                tracks::insert_track(conn, spec.id, folder[0].id, spec.rel_path, &content)?;
            }
            albums::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .unwrap();
        db
    }

    /// Gives the track `id` its placeholder path, as the scanner does for a
    /// track that it keeps without a file.
    fn remove_file(db: &Db, id: &str) {
        db.with_writer(|conn| {
            let track = tracks::get_track(conn, id)?.expect("the track exists");
            tracks::relink_track(
                conn,
                id,
                track.folder_id,
                &tracks::placeholder_path(id),
                track.mtime,
                track.size,
            )
        })
        .unwrap();
    }

    fn artist_id(db: &Db, name: &str) -> String {
        db.with_writer(|conn| artists::upsert_by_key(conn, name))
            .unwrap()
    }

    fn album_id(db: &Db, artist: &str, title: &str) -> String {
        let artist_id = artist_id(db, artist);
        db.with_writer(|conn| albums::upsert_by_key(conn, &artist_id, title, artist, false))
            .unwrap()
    }

    fn folder_id(db: &Db, path: &str) -> i64 {
        db.with_reader(folders)
            .unwrap()
            .into_iter()
            .find(|folder| folder.path == Path::new(path))
            .unwrap_or_else(|| panic!("no folder row has the path {path}"))
            .id
    }

    fn names(artists: &[Artist]) -> Vec<&str> {
        artists.iter().map(|artist| artist.name.as_str()).collect()
    }

    fn titles(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|album| album.name.as_str()).collect()
    }

    fn track_ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|song| song.track.id.as_str()).collect()
    }

    /// Two artists in two folders: Aurora Field with two albums in `/music`
    /// and Marlow Quartet with one album in `/other`.
    fn two_artists() -> Db {
        library(&[
            Spec {
                year: Some(2019),
                genre: Some("Electronic"),
                ..song(T1, "a/1.flac")
            },
            Spec {
                album: "Paper Maps",
                year: Some(2021),
                genre: Some("Electronic"),
                ..song(T2, "b/1.flac")
            },
            Spec {
                folder: "/other",
                artist: "Marlow Quartet",
                album_artist: "Marlow Quartet",
                album: "Slow Rivers",
                year: Some(2015),
                genre: Some("Jazz"),
                ..song(T3, "c/1.flac")
            },
        ])
    }

    /// A row that counts how often it is cloned. The clones share the count.
    #[derive(Debug)]
    struct Counted {
        id: String,
        clones: Rc<Cell<usize>>,
    }

    impl Clone for Counted {
        fn clone(&self) -> Self {
            self.clones.set(self.clones.get() + 1);
            Self {
                id: self.id.clone(),
                clones: Rc::clone(&self.clones),
            }
        }
    }

    /// Orders one row for each of `row_ids` by `ids` and returns the IDs of
    /// the result and the number of clones that were made.
    fn ordered(row_ids: &[&str], ids: &[&str]) -> (Vec<String>, usize) {
        let clones = Rc::new(Cell::new(0));
        let rows = row_ids
            .iter()
            .map(|id| Counted {
                id: (*id).to_owned(),
                clones: Rc::clone(&clones),
            })
            .collect();
        let result = in_order_of(ids, rows, |row: &Counted| &row.id);
        let result_ids = result.into_iter().map(|row| row.id).collect();
        (result_ids, clones.get())
    }

    #[test]
    fn rows_in_the_order_of_ids_are_cloned_only_for_a_repeated_id() {
        // The rows arrive in the order of the table and not of the list.
        let (result, clones) = ordered(&["a", "b", "c"], &["c", "x", "a", "b"]);
        assert_eq!(result, ["c", "a", "b"]);
        assert_eq!(clones, 0);

        // `a` is named three times: two clones and the row itself.
        let (result, clones) = ordered(&["a", "b"], &["a", "b", "a", "x", "a"]);
        assert_eq!(result, ["a", "b", "a", "a"]);
        assert_eq!(clones, 2);

        let (result, clones) = ordered(&["a"], &[]);
        assert_eq!(result, [] as [&str; 0]);
        assert_eq!(clones, 0);
        let (result, clones) = ordered(&[], &["a", "a"]);
        assert_eq!(result, [] as [&str; 0]);
        assert_eq!(clones, 0);
    }

    #[test]
    fn an_empty_library_has_no_rows() {
        let db = Db::open_memory().unwrap();
        db.with_reader(|conn| {
            assert_eq!(list_artists(conn, None)?, []);
            assert_eq!(list_artists(conn, Some(1))?, []);
            assert_eq!(artist(conn, "ar-00000000000000000000000000000001")?, None);
            assert_eq!(
                albums_of_artist(conn, "ar-00000000000000000000000000000001")?,
                []
            );
            assert_eq!(album(conn, "al-00000000000000000000000000000001")?, None);
            assert_eq!(
                tracks_of_album(conn, "al-00000000000000000000000000000001")?,
                []
            );
            assert_eq!(track(conn, T1)?, None);
            assert_eq!(genres(conn)?, []);
            assert_eq!(folders(conn)?, []);
            assert_eq!(artists_by_ids(conn, &[T1])?, []);
            assert_eq!(albums_by_ids(conn, &[T1])?, []);
            assert_eq!(tracks_by_ids(conn, &[T1])?, []);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn list_artists_returns_every_artist_with_its_album_count_in_name_order() {
        let db = two_artists();
        let artists = db.with_reader(|conn| list_artists(conn, None)).unwrap();
        assert_eq!(
            artists,
            [
                Artist {
                    id: artist_id(&db, "Aurora Field"),
                    name: "Aurora Field".to_owned(),
                    sort_name: "Aurora Field".to_owned(),
                    album_count: 2,
                },
                Artist {
                    id: artist_id(&db, "Marlow Quartet"),
                    name: "Marlow Quartet".to_owned(),
                    sort_name: "Marlow Quartet".to_owned(),
                    album_count: 1,
                },
            ]
        );
    }

    #[test]
    fn list_artists_orders_names_without_regard_to_case() {
        let db = library(&[
            Spec {
                album_artist: "beta",
                ..song(T1, "1.flac")
            },
            Spec {
                album_artist: "Alpha",
                ..song(T2, "2.flac")
            },
            Spec {
                album_artist: "Gamma",
                ..song(T3, "3.flac")
            },
        ]);
        let artists = db.with_reader(|conn| list_artists(conn, None)).unwrap();
        assert_eq!(names(&artists), ["Alpha", "beta", "Gamma"]);
    }

    #[test]
    fn list_artists_with_a_folder_returns_the_artists_with_a_file_in_it() {
        let db = two_artists();
        let music = folder_id(&db, "/music");
        let other = folder_id(&db, "/other");
        let of = |folder: i64| {
            db.with_reader(|conn| list_artists(conn, Some(folder)))
                .unwrap()
        };
        assert_eq!(names(&of(music)), ["Aurora Field"]);
        assert_eq!(names(&of(other)), ["Marlow Quartet"]);
        assert_eq!(names(&of(music + other + 1)), [] as [&str; 0]);
        // The album count is the stored one, whatever the folder.
        assert_eq!(of(music)[0].album_count, 2);
    }

    #[test]
    fn list_artists_with_a_folder_leaves_out_an_artist_whose_tracks_have_no_file() {
        let db = two_artists();
        let other = folder_id(&db, "/other");
        remove_file(&db, T3);
        // The row of the track still names the folder.
        let track = db.with_reader(|conn| tracks::get_track(conn, T3)).unwrap();
        assert_eq!(track.unwrap().folder_id, other);

        let in_folder = db
            .with_reader(|conn| list_artists(conn, Some(other)))
            .unwrap();
        assert_eq!(names(&in_folder), [] as [&str; 0]);
        // Without a folder the artist is listed, because its row exists.
        let all = db.with_reader(|conn| list_artists(conn, None)).unwrap();
        assert_eq!(names(&all), ["Aurora Field", "Marlow Quartet"]);
    }

    #[test]
    fn artist_returns_the_row_of_the_id() {
        let db = two_artists();
        let id = artist_id(&db, "Marlow Quartet");
        let found = db.with_reader(|conn| artist(conn, &id)).unwrap().unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.name, "Marlow Quartet");
        assert_eq!(found.album_count, 1);
        let missing = db
            .with_reader(|conn| artist(conn, "ar-ffffffffffffffffffffffffffffffff"))
            .unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn albums_of_artist_orders_by_year_then_title_with_no_year_last() {
        let db = library(&[
            Spec {
                album: "Zenith",
                year: Some(2001),
                ..song(T1, "1.flac")
            },
            Spec {
                album: "undated",
                ..song(T2, "2.flac")
            },
            Spec {
                album: "beta",
                year: Some(2005),
                ..song(T3, "3.flac")
            },
            Spec {
                album: "Alpha",
                year: Some(2005),
                ..song(T4, "4.flac")
            },
            Spec {
                album_artist: "Marlow Quartet",
                album: "Slow Rivers",
                year: Some(1990),
                ..song(T5, "5.flac")
            },
        ]);
        let id = artist_id(&db, "Aurora Field");
        let albums = db.with_reader(|conn| albums_of_artist(conn, &id)).unwrap();
        assert_eq!(titles(&albums), ["Zenith", "Alpha", "beta", "undated"]);
        assert!(albums.iter().all(|album| album.artist_id == id));
    }

    #[test]
    fn album_returns_the_row_with_its_aggregates_and_its_cover() {
        let db = library(&[
            Spec {
                year: Some(2019),
                genre: Some("Electronic"),
                duration_ms: 1500,
                ..song(T1, "a/1.flac")
            },
            Spec {
                year: Some(2018),
                genre: Some("Electronic"),
                duration_ms: 2000,
                ..song(T2, "a/2.flac")
            },
        ]);
        let id = album_id(&db, "Aurora Field", "Glass Harbour");
        let read = |db: &Db| db.with_reader(|conn| album(conn, &id)).unwrap().unwrap();
        let created_at = read(&db).created_at;
        assert!(created_at > 0);
        assert_eq!(
            read(&db),
            Album {
                id: id.clone(),
                artist_id: artist_id(&db, "Aurora Field"),
                name: "Glass Harbour".to_owned(),
                artist_name: "Aurora Field".to_owned(),
                year: Some(2018),
                genre: Some("Electronic".to_owned()),
                song_count: 2,
                duration_ms: 3500,
                compilation: false,
                cover_source: None,
                cover_mtime: None,
                created_at,
            }
        );

        db.with_writer(|conn| albums::set_cover(conn, &id, Some("/music/a/cover.jpg"), Some(77)))
            .unwrap();
        let with_cover = read(&db);
        assert_eq!(
            with_cover.cover_source.as_deref(),
            Some("/music/a/cover.jpg")
        );
        assert_eq!(with_cover.cover_mtime, Some(77));

        let missing = db
            .with_reader(|conn| album(conn, "al-ffffffffffffffffffffffffffffffff"))
            .unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn tracks_of_album_orders_by_disc_then_track_then_path() {
        let db = library(&[
            Spec {
                disc_no: Some(2),
                track_no: Some(1),
                ..song(T1, "a/d2-1.flac")
            },
            // No disc number counts as disc 1. No track number follows the
            // numbered tracks of the disc.
            Spec {
                ..song(T2, "a/unnumbered.flac")
            },
            Spec {
                track_no: Some(2),
                ..song(T3, "a/2.flac")
            },
            Spec {
                disc_no: Some(1),
                track_no: Some(1),
                ..song(T4, "a/1.flac")
            },
            // The same disc and track as the track before: the path decides.
            Spec {
                disc_no: Some(1),
                track_no: Some(1),
                ..song(T5, "a/0.flac")
            },
        ]);
        let id = album_id(&db, "Aurora Field", "Glass Harbour");
        let songs = db.with_reader(|conn| tracks_of_album(conn, &id)).unwrap();
        assert_eq!(track_ids(&songs), [T5, T4, T3, T2, T1]);
        // Every song holds the row that `tracks::get_track` returns.
        for song in &songs {
            let row = db
                .with_reader(|conn| tracks::get_track(conn, &song.track.id))
                .unwrap();
            assert_eq!(Some(&song.track), row.as_ref());
        }
    }

    #[test]
    fn tracks_of_album_lists_a_track_without_a_file() {
        let db = library(&[
            Spec {
                track_no: Some(1),
                ..song(T1, "a/1.flac")
            },
            Spec {
                track_no: Some(2),
                ..song(T2, "a/2.flac")
            },
        ]);
        remove_file(&db, T1);
        let id = album_id(&db, "Aurora Field", "Glass Harbour");
        let songs = db.with_reader(|conn| tracks_of_album(conn, &id)).unwrap();
        assert_eq!(track_ids(&songs), [T1, T2]);
        assert!(!songs[0].track.has_file());
        assert!(songs[1].track.has_file());
        // The count of the album agrees with the rows.
        let album = db.with_reader(|conn| album(conn, &id)).unwrap().unwrap();
        assert_eq!(album.song_count, 2);
    }

    #[test]
    fn a_song_reports_whether_its_album_has_a_cover() {
        let db = two_artists();
        let with_cover = album_id(&db, "Aurora Field", "Glass Harbour");
        db.with_writer(|conn| albums::set_cover(conn, &with_cover, Some("embedded:tr-1"), Some(5)))
            .unwrap();
        let read = |id: &'static str| db.with_reader(|conn| track(conn, id)).unwrap().unwrap();
        assert!(read(T1).album_has_cover);
        assert!(!read(T2).album_has_cover);
        assert!(!read(T3).album_has_cover);

        let songs = db
            .with_reader(|conn| tracks_of_album(conn, &with_cover))
            .unwrap();
        assert_eq!(songs.len(), 1);
        assert!(songs[0].album_has_cover);
        let by_ids = db
            .with_reader(|conn| tracks_by_ids(conn, &[T2, T1]))
            .unwrap();
        assert_eq!(
            by_ids
                .iter()
                .map(|song| song.album_has_cover)
                .collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn track_returns_the_row_of_the_id() {
        let db = two_artists();
        let song = db.with_reader(|conn| track(conn, T3)).unwrap().unwrap();
        let row = db.with_reader(|conn| tracks::get_track(conn, T3)).unwrap();
        assert_eq!(Some(song.track), row);
        let missing = db.with_reader(|conn| track(conn, UNKNOWN_TRACK)).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn rows_by_ids_keep_the_order_and_the_repeats_of_the_ids() {
        let db = two_artists();
        let songs = db
            .with_reader(|conn| tracks_by_ids(conn, &[T3, UNKNOWN_TRACK, T1, T3, "x", ""]))
            .unwrap();
        assert_eq!(track_ids(&songs), [T3, T1, T3]);
        let none: [&str; 0] = [];
        assert_eq!(
            db.with_reader(|conn| tracks_by_ids(conn, &none)).unwrap(),
            []
        );

        let glass = album_id(&db, "Aurora Field", "Glass Harbour");
        let paper = album_id(&db, "Aurora Field", "Paper Maps");
        let rivers = album_id(&db, "Marlow Quartet", "Slow Rivers");
        let wanted = [rivers.clone(), paper.clone(), glass.clone(), paper.clone()];
        let albums = db.with_reader(|conn| albums_by_ids(conn, &wanted)).unwrap();
        assert_eq!(
            titles(&albums),
            ["Slow Rivers", "Paper Maps", "Glass Harbour", "Paper Maps"]
        );
        // An ID of another kind names no album.
        let albums = db
            .with_reader(|conn| albums_by_ids(conn, &[T1.to_owned(), glass]))
            .unwrap();
        assert_eq!(titles(&albums), ["Glass Harbour"]);

        let aurora = artist_id(&db, "Aurora Field");
        let marlow = artist_id(&db, "Marlow Quartet");
        let wanted = [marlow.clone(), aurora, marlow];
        let artists = db
            .with_reader(|conn| artists_by_ids(conn, &wanted))
            .unwrap();
        assert_eq!(
            names(&artists),
            ["Marlow Quartet", "Aurora Field", "Marlow Quartet"]
        );
    }

    #[test]
    fn rows_by_ids_read_a_list_longer_than_the_parameter_limit_of_sqlite() {
        // SQLite allows 32 766 parameters in one statement. The IDs are one
        // parameter, so a longer list is read.
        let db = two_artists();
        let ids: Vec<&str> = [T1, UNKNOWN_TRACK, T2].repeat(20_000);
        let songs = db.with_reader(|conn| tracks_by_ids(conn, &ids)).unwrap();
        assert_eq!(songs.len(), 40_000);
        assert_eq!(track_ids(&songs[..4]), [T1, T2, T1, T2]);
    }

    #[test]
    fn rows_by_ids_take_an_id_as_text_and_not_as_sql() {
        let db = two_artists();
        let hostile = [
            "tr-1' OR '1'='1",
            "\"]) OR 1=1 --",
            "%",
            "[\"tr-00000000000000000000000000000001\"]",
        ];
        let songs = db
            .with_reader(|conn| tracks_by_ids(conn, &hostile))
            .unwrap();
        assert_eq!(songs, []);
        assert_eq!(db.with_reader(tracks::count_tracks).unwrap(), 3);
    }

    #[test]
    fn genres_count_the_songs_and_the_albums_of_each_genre() {
        let db = library(&[
            Spec {
                genre: Some("Electronic"),
                ..song(T1, "a/1.flac")
            },
            Spec {
                genre: Some("Electronic"),
                ..song(T2, "a/2.flac")
            },
            // The one track of "Glass Harbour" with another genre. The album
            // counts for the genre of most of its tracks.
            Spec {
                genre: Some("ambient"),
                ..song(T3, "a/3.flac")
            },
            Spec {
                album: "Paper Maps",
                genre: Some("Electronic"),
                ..song(T4, "b/1.flac")
            },
            // A track without a genre adds no genre.
            Spec {
                album: "Untagged",
                ..song(T5, "c/1.flac")
            },
        ]);
        let genre = |name: &str, song_count, album_count| Genre {
            name: name.to_owned(),
            song_count,
            album_count,
        };
        assert_eq!(
            db.with_reader(genres).unwrap(),
            [genre("ambient", 1, 0), genre("Electronic", 3, 2)]
        );
    }

    #[test]
    fn genres_count_a_track_without_a_file() {
        let db = library(&[Spec {
            genre: Some("Jazz"),
            ..song(T1, "a/1.flac")
        }]);
        remove_file(&db, T1);
        let genres = db.with_reader(genres).unwrap();
        assert_eq!(genres.len(), 1);
        assert_eq!((genres[0].song_count, genres[0].album_count), (1, 1));
    }

    #[test]
    fn folders_returns_the_folder_rows_in_id_order() {
        let db = two_artists();
        let rows = db.with_reader(folders).unwrap();
        assert_eq!(rows, db.with_reader(folder_rows::list).unwrap());
        let paths: Vec<&Path> = rows.iter().map(|folder| folder.path.as_path()).collect();
        assert_eq!(paths, [Path::new("/music"), Path::new("/other")]);
        assert!(rows[0].id < rows[1].id);
    }

    #[test]
    fn file_path_joins_the_folder_and_the_components_of_the_relative_path() {
        let db = library(&[song(T1, "Aurora Field/Glass Harbour/01 Low Tide.flac")]);
        let track = db
            .with_reader(|conn| tracks::get_track(conn, T1))
            .unwrap()
            .unwrap();
        let path = db.with_reader(|conn| file_path(conn, &track)).unwrap();
        let expected: PathBuf = [
            "/music",
            "Aurora Field",
            "Glass Harbour",
            "01 Low Tide.flac",
        ]
        .iter()
        .collect();
        assert_eq!(path, Some(expected));
    }

    #[test]
    fn file_path_of_a_track_without_a_file_is_none() {
        let db = library(&[song(T1, "a/1.flac")]);
        remove_file(&db, T1);
        let track = db
            .with_reader(|conn| tracks::get_track(conn, T1))
            .unwrap()
            .unwrap();
        assert!(!track.has_file());
        assert_eq!(
            db.with_reader(|conn| file_path(conn, &track)).unwrap(),
            None
        );
    }

    #[test]
    fn file_path_of_a_track_whose_folder_row_is_gone_is_none() {
        let db = library(&[song(T1, "a/1.flac")]);
        let mut track = db
            .with_reader(|conn| tracks::get_track(conn, T1))
            .unwrap()
            .unwrap();
        track.folder_id += 1;
        assert_eq!(
            db.with_reader(|conn| file_path(conn, &track)).unwrap(),
            None
        );
    }
}
