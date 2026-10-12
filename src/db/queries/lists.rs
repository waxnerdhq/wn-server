//! The queries of the list methods (`docs/design.md` section 4): the album
//! lists of `getAlbumList` and `getAlbumList2`, the random songs of
//! `getRandomSongs` and the songs of a genre of `getSongsByGenre`.
//!
//! Each function selects the IDs of its result with its own SQL, in the
//! order of the result, and reads the rows with `browse::albums_by_ids` and
//! `browse::tracks_by_ids`.
//!
//! # Values of the client
//!
//! A value that a client sent is a parameter of a statement, or it is
//! compared in Rust. The SQL text is built from the constants of this module
//! and from nothing else.
//!
//! # Order
//!
//! Every list that is not random has a total order: the last term of its
//! `ORDER BY` is the ID of the row. Two requests for one list return the
//! rows in the same order, so the pages that `offset` selects neither repeat
//! nor skip a row while the library and the annotations stay as they are.
//!
//! # Random rows
//!
//! `pick` draws random rows. It sorts no table, and it scans no table while
//! many of the rows count. `tracks` and `albums` are rowid tables. The
//! function reads the smallest and the largest rowid, draws distinct rowids
//! of that range in random order, and looks each one up by its rowid. A
//! rowid that no row has, and a row that the filter leaves out, adds
//! nothing. The first rows that are found, in the order of the draw, are the
//! result. Every row that counts is equally likely to be one of them.
//!
//! The number of lookups has a limit that grows with the number of rows
//! asked for (`PROBES_PER_ROW`, `PROBES_BASE`). When the limit is at least
//! the number of rowids, every rowid is looked up and the result is exact.
//! Otherwise a draw that finds too few rows means that few rows count: the
//! filter matches few rows, or the table has lost most of its rows. The
//! function then reads the IDs of the rows that count and keeps a random
//! sample of them, which needs no sort. With a genre it reads the tracks of
//! that genre through the genre index, and without one it reads the table.
//!
//! # Genres
//!
//! A genre matches the stored genres that equal it without regard to case
//! (`genre_spellings`). The stored spellings are read through the genre
//! index, one index lookup for each distinct genre. A statement then takes
//! the matching spellings as a parameter and finds their rows through the
//! same index.
//!
//! # Years
//!
//! An album or a track without a year counts as year 0 in a year range. The
//! tag reader stores no year for the year `0000`, so no item has the year 0
//! for another reason.
//!
//! # Tracks without a file
//!
//! A track row without a file (`docs/design.md` section 3.4) is in no music
//! folder: a list with a music folder leaves it out, and an album is in a
//! music folder when one of its tracks has a file there. [`random_songs`]
//! never returns such a track, because a client plays what it returns.
//! [`songs_by_genre`] without a music folder returns it, as `browse::genres`
//! counts it. The album lists without a music folder return every album row.

use rand::seq::SliceRandom;
use rand::seq::index;
use rand::{Rng, RngExt};
use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use super::browse::{self, Album, Song, json_array};
use super::int;
use super::tracks::HAS_FILE_SQL;
use crate::db::Result;

/// The rowids that [`pick`] looks up for each row it is asked for, before it
/// reads the rows that count instead.
const PROBES_PER_ROW: usize = 16;

/// The rowids that [`pick`] looks up in addition to [`PROBES_PER_ROW`] for
/// each row. A request for one row then still looks up enough rowids to find
/// a row of a filter that one row in ten matches.
const PROBES_BASE: usize = 64;

/// The last terms of the order of every album list: the natural key of the
/// title, the natural key of the artist and the ID. An artist has one album
/// with a key, and an ID is unique, so the order is total.
const ALBUM_TAIL: &str = "albums.key, artists.key, albums.id";

/// The `JOIN` of an album list that needs what the user attached to the
/// album itself: its star and its rating. `?1` is the ID of the user. An
/// album without an annotation row is not part of such a list.
const OWN_ANNOTATION: &str =
    "JOIN annotations AS own ON own.user_id = ?1 AND own.item_id = albums.id";

/// The `JOIN` of an album list that needs the plays of the album. A play is
/// recorded for a track, so the play count of an album is the sum of the
/// play counts of its tracks, and its last play is the latest play of one of
/// its tracks. `?1` is the ID of the user. An album none of whose tracks has
/// an annotation row is not part of such a list.
const PLAYS: &str = "JOIN (
       SELECT tracks.album_id AS album_id, sum(played.play_count) AS play_count,
         max(played.last_played_at) AS last_played_at
       FROM annotations AS played JOIN tracks ON tracks.id = played.item_id
       WHERE played.user_id = ?1
       GROUP BY tracks.album_id
     ) AS plays ON plays.album_id = albums.id";

/// One of the album lists of `getAlbumList` and `getAlbumList2`. The names
/// are the values of the `type` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumList {
    /// Albums drawn at random, anew for every call.
    Random,
    /// By the time the album was first seen, the latest first.
    Newest,
    /// The albums that the user rated, the highest rating first.
    Highest,
    /// The albums that the user played, the most plays first.
    Frequent,
    /// The albums that the user played, the latest play first.
    Recent,
    /// By the title of the album.
    AlphabeticalByName,
    /// By the name of the album artist and then by the title.
    AlphabeticalByArtist,
    /// The albums that the user starred, the latest star first.
    Starred,
    /// The albums of the years between `from` and `to`, both included. The
    /// years ascend when `from` is not greater than `to`, and descend
    /// otherwise. An album without a year counts as year 0.
    ByYear { from: u32, to: u32 },
    /// The albums whose genre is this one, without regard to case, by title.
    ByGenre(String),
}

/// The part of a list that one call returns: at most `size` rows after the
/// first `offset` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub size: u32,
    pub offset: u32,
}

/// What `getRandomSongs` restricts its songs to. A field that is `None`
/// restricts nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SongFilter {
    /// The genre of the songs, without regard to case.
    pub genre: Option<String>,
    /// The first year of the songs.
    pub from_year: Option<u32>,
    /// The last year of the songs.
    pub to_year: Option<u32>,
    /// The music folder of the songs.
    pub folder_id: Option<i64>,
}

/// Returns one page of the album list `list` for the user `user_id`.
///
/// With `folder_id` the list holds only the albums that have a track with a
/// file in that music folder. A folder ID that no folder has gives no album.
///
/// [`AlbumList::Highest`] and [`AlbumList::Starred`] hold the albums that
/// the user rated or starred, and [`AlbumList::Frequent`] and
/// [`AlbumList::Recent`] the albums of which the user played a track. They
/// are empty for a user without such annotations.
///
/// [`AlbumList::Random`] draws its albums with `rng`, anew for every call,
/// so two pages can hold the same album. `page.offset` does not select rows
/// of it. It lowers the number of albums: the call returns at most the
/// number of albums of the list less the offset, so a client that reads
/// pages until one is short comes to an end.
pub fn albums<R: Rng + ?Sized>(
    conn: &Connection,
    rng: &mut R,
    user_id: i64,
    list: &AlbumList,
    folder_id: Option<i64>,
    page: Page,
) -> Result<Vec<Album>> {
    let ordered = match list {
        AlbumList::Random => {
            let ids = random_album_ids(conn, rng, folder_id, page)?;
            return browse::albums_by_ids(conn, &ids);
        }
        AlbumList::Newest => Ordered::new("", "1", "albums.created_at DESC, "),
        AlbumList::Highest => Ordered::new(OWN_ANNOTATION, "own.rating > 0", "own.rating DESC, "),
        AlbumList::Frequent => {
            Ordered::new(PLAYS, "plays.play_count > 0", "plays.play_count DESC, ")
        }
        AlbumList::Recent => Ordered::new(
            PLAYS,
            "plays.last_played_at IS NOT NULL",
            "plays.last_played_at DESC, ",
        ),
        AlbumList::AlphabeticalByName => Ordered::new("", "1", ""),
        AlbumList::AlphabeticalByArtist => Ordered::new("", "1", "artists.key, "),
        AlbumList::Starred => Ordered::new(
            OWN_ANNOTATION,
            "own.starred_at IS NOT NULL",
            "own.starred_at DESC, ",
        ),
        AlbumList::ByYear { from, to } => {
            let order = if from > to {
                "coalesce(albums.year, 0) DESC, "
            } else {
                "coalesce(albums.year, 0), "
            };
            Ordered {
                values: vec![
                    i64::from(*from.min(to)).into(),
                    i64::from(*from.max(to)).into(),
                ],
                ..Ordered::new("", "coalesce(albums.year, 0) BETWEEN ?5 AND ?6", order)
            }
        }
        AlbumList::ByGenre(genre) => {
            let spellings = genre_spellings(conn, Table::Albums, genre)?;
            if spellings.is_empty() {
                return Ok(Vec::new());
            }
            Ordered {
                values: vec![json_array(&spellings).into()],
                ..Ordered::new("", "albums.genre IN (SELECT value FROM json_each(?5))", "")
            }
        }
    };
    let ids = ordered_album_ids(conn, user_id, &ordered, folder_id, page)?;
    browse::albums_by_ids(conn, &ids)
}

/// Returns at most `size` songs that `filter` allows, drawn at random with
/// `rng`. No song is in the result twice. The result holds every song that
/// the filter allows when these are fewer than `size`.
///
/// A track without a file is never part of the result. A track without a
/// year counts as year 0 for `from_year` and `to_year`. A genre that no
/// track has and a folder ID that no folder has give no song.
pub fn random_songs<R: Rng + ?Sized>(
    conn: &Connection,
    rng: &mut R,
    filter: &SongFilter,
    size: u32,
) -> Result<Vec<Song>> {
    let spellings = match &filter.genre {
        Some(genre) => {
            let spellings = genre_spellings(conn, Table::Tracks, genre)?;
            if spellings.is_empty() {
                return Ok(Vec::new());
            }
            Some(spellings)
        }
        None => None,
    };
    let source = Source::tracks(filter, spellings.as_deref());
    let ids = pick(conn, rng, &source, to_usize(size))?;
    browse::tracks_by_ids(conn, &ids)
}

/// Returns one page of the songs whose genre is `genre`, without regard to
/// case: by title in lower case, then by the track artist, the album title
/// and the ID.
///
/// With `folder_id` the list holds only the tracks that have a file in that
/// music folder. Without it the list holds every track row of the genre, a
/// track without a file included, as `browse::genres` counts them.
pub fn songs_by_genre(
    conn: &Connection,
    genre: &str,
    folder_id: Option<i64>,
    page: Page,
) -> Result<Vec<Song>> {
    let spellings = genre_spellings(conn, Table::Tracks, genre)?;
    if spellings.is_empty() {
        return Ok(Vec::new());
    }
    let mut select = conn.prepare_cached(&format!(
        "SELECT tracks.id
         FROM json_each(?1) AS spelling CROSS JOIN tracks ON tracks.genre = spelling.value
         WHERE ?2 IS NULL OR (tracks.folder_id = ?2 AND {HAS_FILE_SQL})
         ORDER BY tracks.title_lc, tracks.artist_name, tracks.album_name, tracks.id
         LIMIT ?3 OFFSET ?4"
    ))?;
    let ids: Vec<String> = select
        .query_map(
            (json_array(&spellings), folder_id, page.size, page.offset),
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?;
    browse::tracks_by_ids(conn, &ids)
}

/// The parts of the statement of an album list that has an order.
struct Ordered {
    /// The `JOIN` that the list adds to `albums` and `artists`. Empty for a
    /// list that needs no other table.
    join: &'static str,
    /// The condition of the albums of the list. It can name `?1`, the ID of
    /// the user, and `?5` and `?6`, the values of `values`.
    condition: &'static str,
    /// The terms of the order that come before [`ALBUM_TAIL`], each with a
    /// comma after it. Empty for a list in the order of the titles.
    order: &'static str,
    /// The values of the parameters `?5` and `?6` of `condition`.
    values: Vec<Value>,
}

impl Ordered {
    /// A list whose condition has no parameter after `?4`.
    fn new(join: &'static str, condition: &'static str, order: &'static str) -> Self {
        Self {
            join,
            condition,
            order,
            values: Vec::new(),
        }
    }
}

/// Returns the IDs of one page of the album list `ordered` for the user
/// `user_id`, in the order of the list.
fn ordered_album_ids(
    conn: &Connection,
    user_id: i64,
    ordered: &Ordered,
    folder_id: Option<i64>,
    page: Page,
) -> Result<Vec<String>> {
    // `?1` to `?4` are the same in every list. The values of a list follow
    // them.
    let params = [
        Value::from(user_id),
        folder_id.into(),
        i64::from(page.size).into(),
        i64::from(page.offset).into(),
    ]
    .into_iter()
    .chain(ordered.values.iter().cloned());
    let mut select = conn.prepare_cached(&format!(
        "SELECT albums.id
         FROM albums JOIN artists ON artists.id = albums.artist_id {join}
         WHERE {condition} AND {in_folder}
         ORDER BY {order}{ALBUM_TAIL}
         LIMIT ?3 OFFSET ?4",
        join = ordered.join,
        condition = ordered.condition,
        order = ordered.order,
        in_folder = album_in_folder(2),
    ))?;
    let ids = select
        .query_map(params_from_iter(params), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Returns the IDs of the albums of one call of [`AlbumList::Random`].
fn random_album_ids<R: Rng + ?Sized>(
    conn: &Connection,
    rng: &mut R,
    folder_id: Option<i64>,
    page: Page,
) -> Result<Vec<String>> {
    let source = Source::albums(folder_id);
    let size = u64::from(page.size);
    let wanted = if page.offset == 0 {
        size
    } else {
        // The count reads every album row. Only a call for a later page
        // needs it.
        let count: u64 = conn
            .prepare_cached(&format!(
                "SELECT count(*) FROM albums WHERE {}",
                source.condition
            ))?
            .query_row(params_from_iter(source.params.iter()), |row| {
                int::get_u64(row, 0)
            })?;
        size.min(count.saturating_sub(u64::from(page.offset)))
    };
    pick(
        conn,
        rng,
        &source,
        usize::try_from(wanted).unwrap_or(usize::MAX),
    )
}

/// The condition of an album that is in the music folder of the parameter
/// `?<parameter>`: one of its tracks has a file in that folder. The
/// condition holds for every album when the parameter is null.
fn album_in_folder(parameter: usize) -> String {
    format!(
        "(?{parameter} IS NULL OR EXISTS (
           SELECT 1 FROM tracks
           WHERE tracks.album_id = albums.id AND tracks.folder_id = ?{parameter}
             AND {HAS_FILE_SQL}))"
    )
}

/// A table whose rows [`pick`] draws and whose genres [`genre_spellings`]
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    Tracks,
    Albums,
}

impl Table {
    /// The name of the table in SQL.
    const fn name(self) -> &'static str {
        match self {
            Table::Tracks => "tracks",
            Table::Albums => "albums",
        }
    }
}

/// The rows that [`pick`] draws from: a table and the condition of the rows
/// that count.
#[derive(Debug)]
struct Source {
    table: Table,
    /// The condition of a row that counts, over the columns of `table`. Its
    /// parameters are `?1` to `?n`, where `n` is the length of `params`.
    condition: String,
    /// The statement that returns the ID of every row that counts. It has
    /// the parameters of `condition`.
    all: String,
    /// The values of the parameters of `condition`.
    params: Vec<Value>,
}

impl Source {
    /// The tracks with a file that `filter` allows. `spellings` are the
    /// stored genres that `filter.genre` matches, and `None` for a filter
    /// without a genre.
    fn tracks(filter: &SongFilter, spellings: Option<&[String]>) -> Self {
        let mut params: Vec<Value> = vec![
            filter.folder_id.into(),
            filter.from_year.map(i64::from).into(),
            filter.to_year.map(i64::from).into(),
        ];
        let condition = format!(
            "{HAS_FILE_SQL}
             AND (?1 IS NULL OR tracks.folder_id = ?1)
             AND (?2 IS NULL OR coalesce(tracks.year, 0) >= ?2)
             AND (?3 IS NULL OR coalesce(tracks.year, 0) <= ?3)"
        );
        match spellings {
            Some(spellings) => {
                params.push(json_array(spellings).into());
                Self {
                    table: Table::Tracks,
                    // The genre index finds the tracks of each spelling.
                    all: format!(
                        "SELECT tracks.id
                         FROM json_each(?4) AS spelling
                         CROSS JOIN tracks ON tracks.genre = spelling.value
                         WHERE {condition}"
                    ),
                    condition: format!(
                        "{condition} AND tracks.genre IN (SELECT value FROM json_each(?4))"
                    ),
                    params,
                }
            }
            None => Self {
                table: Table::Tracks,
                all: format!("SELECT tracks.id FROM tracks WHERE {condition}"),
                condition,
                params,
            },
        }
    }

    /// Every album, or with `folder_id` the albums that have a track with a
    /// file in that music folder.
    fn albums(folder_id: Option<i64>) -> Self {
        let condition = album_in_folder(1);
        Self {
            table: Table::Albums,
            all: format!("SELECT albums.id FROM albums WHERE {condition}"),
            condition,
            params: vec![folder_id.into()],
        }
    }

    /// The statement that looks up the rows of a list of rowids and returns,
    /// for each row that counts, the place of its rowid in the list and its
    /// ID. The list is a JSON array in the parameter after those of
    /// `condition`. The statement reads each rowid of the list from the
    /// table by its key and reads no other row.
    fn probe_sql(&self) -> String {
        let table = self.table.name();
        format!(
            "SELECT pick.key, {table}.id
             FROM json_each(?{candidates}) AS pick
             CROSS JOIN {table} ON {table}.rowid = pick.value
             WHERE {condition}",
            candidates = self.params.len() + 1,
            condition = self.condition,
        )
    }
}

/// The statement that returns the smallest and the largest rowid of
/// `table`, or two nulls for a table without rows. Each of the two is one
/// lookup at an end of the table.
fn rowid_range_sql(table: Table) -> String {
    let table = table.name();
    format!("SELECT (SELECT min(rowid) FROM {table}), (SELECT max(rowid) FROM {table})")
}

/// Returns the IDs of at most `wanted` rows of `source`, drawn at random
/// with `rng`, in random order. No row is in the result twice, and every row
/// of `source` is equally likely to be in it. The result holds every row of
/// `source` when these are fewer than `wanted`.
///
/// The module comment describes how the rows are drawn.
fn pick<R: Rng + ?Sized>(
    conn: &Connection,
    rng: &mut R,
    source: &Source,
    wanted: usize,
) -> Result<Vec<String>> {
    if wanted == 0 {
        return Ok(Vec::new());
    }
    let range: (Option<i64>, Option<i64>) = conn
        .prepare_cached(&rowid_range_sql(source.table))?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let (Some(first), Some(last)) = range else {
        return Ok(Vec::new());
    };
    // The number of rowids from `first` to `last`.
    let span = last
        .checked_sub(first)
        .and_then(|distance| distance.checked_add(1))
        .and_then(|span| usize::try_from(span).ok());
    let Some(span) = span else {
        // The range is too large to draw from. Rowids that the database
        // assigns do not reach such a range.
        return sample_all(conn, rng, source, wanted);
    };

    let budget = wanted
        .saturating_mul(PROBES_PER_ROW)
        .saturating_add(PROBES_BASE)
        .min(span);
    // Distinct rowids of the range, in random order.
    let candidates: Vec<i64> = index::sample(rng, span, budget)
        .iter()
        .filter_map(|offset| first.checked_add(i64::try_from(offset).ok()?))
        .collect();

    // The first lookup takes little more than `wanted` rowids, which is
    // enough when most rowids have a row that counts. Each later lookup
    // takes twice as many as the one before.
    let mut picked: Vec<String> = Vec::new();
    let mut rest = candidates.as_slice();
    let mut take = wanted.saturating_add(wanted / 8).saturating_add(8);
    while !rest.is_empty() && picked.len() < wanted {
        let (chunk, tail) = rest.split_at(take.min(rest.len()));
        picked.extend(probe(conn, source, chunk)?);
        rest = tail;
        take = take.saturating_mul(2);
    }
    if picked.len() >= wanted || budget == span {
        // With `budget == span` every rowid was looked up, so `picked` holds
        // every row that counts.
        picked.truncate(wanted);
        return Ok(picked);
    }
    sample_all(conn, rng, source, wanted)
}

/// Looks up the rowids `candidates` and returns the IDs of the rows of
/// `source` among them, in the order of `candidates`.
fn probe(conn: &Connection, source: &Source, candidates: &[i64]) -> Result<Vec<String>> {
    let list = serde_json::to_string(candidates).expect("a list of integers serialises to JSON");
    let params = source.params.iter().cloned().chain([Value::from(list)]);
    let mut select = conn.prepare_cached(&source.probe_sql())?;
    let mut found: Vec<(i64, String)> = select
        .query_map(params_from_iter(params), |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // The statement returns the rows in no stated order. The place of a
    // rowid in the list restores the order of the draw.
    found.sort_unstable_by_key(|(place, _)| *place);
    Ok(found.into_iter().map(|(_, id)| id).collect())
}

/// Reads the ID of every row of `source` and returns a random sample of at
/// most `wanted` of them, in random order. Every row is equally likely to be
/// in the sample. The function holds at most `wanted` IDs at a time.
fn sample_all<R: Rng + ?Sized>(
    conn: &Connection,
    rng: &mut R,
    source: &Source,
    wanted: usize,
) -> Result<Vec<String>> {
    let mut select = conn.prepare_cached(&source.all)?;
    let mut rows = select.query(params_from_iter(source.params.iter()))?;
    let mut kept: Vec<String> = Vec::new();
    // The number of rows read before the present one.
    let mut seen: usize = 0;
    while let Some(row) = rows.next()? {
        if kept.len() < wanted {
            kept.push(row.get(0)?);
        } else {
            // Row number `seen + 1` replaces a kept row with the
            // probability `wanted / (seen + 1)`.
            let slot = rng.random_range(0..=seen);
            if slot < wanted {
                kept[slot] = row.get(0)?;
            }
        }
        seen += 1;
    }
    kept.shuffle(rng);
    Ok(kept)
}

/// The statement that returns every distinct genre of `table` in byte
/// order. Each step is one lookup in the genre index of the table: the
/// smallest genre, and then the smallest genre above the one before.
fn spellings_sql(table: Table) -> String {
    let table = table.name();
    format!(
        "WITH RECURSIVE spelling(genre) AS (
           SELECT min(genre) FROM {table}
           UNION ALL
           SELECT (SELECT min(genre) FROM {table} WHERE genre > spelling.genre)
           FROM spelling WHERE spelling.genre IS NOT NULL
         )
         SELECT genre FROM spelling WHERE genre IS NOT NULL"
    )
}

/// Returns the genres that the `genre` column of `table` holds and that
/// equal `genre` without regard to case, in byte order. Two texts are equal
/// here when their lower-case forms are, as `str::to_lowercase` gives them.
///
/// `genre` is compared in Rust and is no parameter of a statement.
fn genre_spellings(conn: &Connection, table: Table, genre: &str) -> Result<Vec<String>> {
    let wanted = genre.to_lowercase();
    let mut select = conn.prepare_cached(&spellings_sql(table))?;
    let mut rows = select.query([])?;
    let mut spellings = Vec::new();
    while let Some(row) = rows.next()? {
        let stored: String = row.get(0)?;
        if stored.to_lowercase() == wanted {
            spellings.push(stored);
        }
    }
    Ok(spellings)
}

/// Returns `value` as a `usize`, or the largest `usize` on a platform whose
/// `usize` is smaller than 32 bits.
fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use rusqlite::params;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::{self, TrackContent};
    use crate::db::queries::{albums as album_rows, artists, folders, users};
    use crate::library::tags::TrackMeta;

    /// One track of a test library. [`track`] returns the values that a test
    /// does not set.
    #[derive(Debug, Clone)]
    struct Spec {
        id: String,
        folder: &'static str,
        artist: &'static str,
        album: String,
        title: String,
        year: Option<u32>,
        genre: Option<&'static str>,
    }

    /// Returns track number `number` of "Glass Harbour" by "Aurora Field" in
    /// the folder `/music`, without year and genre. The ID and the title
    /// hold the number.
    fn track(number: usize) -> Spec {
        Spec {
            id: format!("tr-{number:032x}"),
            folder: "/music",
            artist: "Aurora Field",
            album: "Glass Harbour".to_owned(),
            title: format!("Track {number:05}"),
            year: None,
            genre: None,
        }
    }

    /// Stores `specs` as the scanner stores tracks, with the file
    /// `<number>.flac` for each, and recomputes the counts of the albums and
    /// of the artists. The database holds the owner account.
    fn library(specs: &[Spec]) -> Db {
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| {
            users::insert_owner(conn, "owner", "sesame")?.expect("no owner yet");
            for spec in specs {
                let folder = folders::sync_folders(conn, &[PathBuf::from(spec.folder)])?;
                let artist_id = artists::upsert_by_key(conn, spec.artist)?;
                let album_id =
                    album_rows::upsert_by_key(conn, &artist_id, &spec.album, spec.artist, false)?;
                let meta = TrackMeta {
                    title: spec.title.clone(),
                    artist: spec.artist.to_owned(),
                    album_artist: spec.artist.to_owned(),
                    album: spec.album.clone(),
                    track_no: None,
                    disc_no: None,
                    year: spec.year,
                    genre: spec.genre.map(str::to_owned),
                    compilation: false,
                    duration_ms: 1000,
                    bitrate_kbps: Some(900),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    channels: Some(2),
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
                    artist_id: &artist_id,
                    album_id: &album_id,
                };
                let rel_path = format!("{}.flac", spec.id);
                tracks::insert_track(conn, &spec.id, folder[0].id, &rel_path, &content)?;
            }
            album_rows::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .unwrap();
        db
    }

    /// A library of `count` tracks of one album. `change` sets the values of
    /// track `number`.
    fn tracks_of_one_album(count: usize, change: impl Fn(usize, &mut Spec)) -> Db {
        let specs: Vec<Spec> = (0..count)
            .map(|number| {
                let mut spec = track(number);
                change(number, &mut spec);
                spec
            })
            .collect();
        library(&specs)
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

    fn folder_id(db: &Db, path: &str) -> i64 {
        db.with_reader(folders::list)
            .unwrap()
            .into_iter()
            .find(|folder| folder.path == Path::new(path))
            .unwrap_or_else(|| panic!("no folder row has the path {path}"))
            .id
    }

    /// The ID of the owner account of [`library`].
    fn owner(db: &Db) -> i64 {
        db.with_reader(users::get_owner)
            .unwrap()
            .expect("the owner")
            .user
            .id
    }

    /// A generator with a fixed seed. A test that fails then fails in every
    /// run. No assertion depends on the values that the seed gives.
    fn rng() -> StdRng {
        StdRng::seed_from_u64(857)
    }

    fn random(db: &Db, rng: &mut StdRng, filter: &SongFilter, size: u32) -> Vec<Song> {
        db.with_reader(|conn| random_songs(conn, rng, filter, size))
            .unwrap()
    }

    fn song_ids(songs: &[Song]) -> Vec<&str> {
        songs.iter().map(|song| song.track.id.as_str()).collect()
    }

    fn titles(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|album| album.name.as_str()).collect()
    }

    /// Checks that `songs` holds `expected` songs and none of them twice.
    fn assert_distinct(songs: &[Song], expected: usize) {
        let ids: HashSet<&str> = song_ids(songs).into_iter().collect();
        assert_eq!(songs.len(), expected, "{:?}", song_ids(songs));
        assert_eq!(ids.len(), expected, "{:?}", song_ids(songs));
    }

    /// The `detail` column of each row of the query plan of `sql`.
    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut explain = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|error| panic!("{error}: {sql}"));
        let nulls = vec![Value::Null; explain.parameter_count()];
        explain
            .query_map(params_from_iter(nulls), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Checks that no step of the plan of `sql` reads every row of `tracks`
    /// or of `albums`. Such a step starts with `SCAN` and the table name.
    fn assert_no_table_scan(conn: &Connection, sql: &str) -> Vec<String> {
        let plan = plan(conn, sql);
        for step in &plan {
            assert!(
                !step.starts_with("SCAN tracks") && !step.starts_with("SCAN albums"),
                "{step}: {sql}"
            );
        }
        plan
    }

    #[test]
    fn statements_of_a_random_pick_look_rows_up_and_scan_no_table() {
        let db = Db::open_memory().unwrap();
        let spellings = ["Jazz".to_owned()];
        let filter = SongFilter {
            genre: Some("jazz".to_owned()),
            from_year: Some(2000),
            to_year: Some(2020),
            folder_id: Some(1),
        };
        db.with_reader(|conn| {
            for table in [Table::Tracks, Table::Albums] {
                let range = assert_no_table_scan(conn, &rowid_range_sql(table));
                let searches = range.iter().filter(|step| step.starts_with("SEARCH"));
                assert_eq!(searches.count(), 2, "{range:?}");

                // The distinct genres come from the genre index alone.
                let genres = assert_no_table_scan(conn, &spellings_sql(table));
                let index = format!("USING COVERING INDEX {}_genre", table.name());
                let lookups = genres.iter().filter(|step| step.contains(&index));
                assert_eq!(lookups.count(), 2, "{genres:?}");
            }

            let by_rowid = |table: &str| format!("SEARCH {table} USING INTEGER PRIMARY KEY");
            let sources = [
                Source::tracks(&SongFilter::default(), None),
                Source::tracks(&filter, Some(&spellings)),
                Source::albums(None),
                Source::albums(Some(1)),
            ];
            for source in &sources {
                let probe = assert_no_table_scan(conn, &source.probe_sql());
                let lookup = by_rowid(source.table.name());
                assert!(
                    probe.iter().any(|step| step.starts_with(&lookup)),
                    "{probe:?}"
                );
            }

            // With a genre, the statement that reads every row that counts
            // reads the tracks of the genre through the genre index.
            let all = assert_no_table_scan(conn, &sources[1].all);
            assert!(
                all.iter()
                    .any(|step| step.starts_with("SEARCH tracks USING INDEX tracks_genre")),
                "{all:?}"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_empty_library_has_no_lists() {
        let db = Db::open_memory().unwrap();
        let mut rng = rng();
        let page = Page {
            size: 10,
            offset: 0,
        };
        db.with_reader(|conn| {
            for list in [
                AlbumList::Random,
                AlbumList::Newest,
                AlbumList::Frequent,
                AlbumList::ByYear { from: 0, to: 9999 },
                AlbumList::ByGenre("Jazz".to_owned()),
            ] {
                assert_eq!(albums(conn, &mut rng, 1, &list, None, page)?, []);
            }
            assert_eq!(
                random_songs(conn, &mut rng, &SongFilter::default(), 10)?,
                []
            );
            assert_eq!(songs_by_genre(conn, "Jazz", None, page)?, []);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn random_songs_of_a_large_table_are_distinct_rows_of_the_table() {
        // 1500 rowids are more than the 224 that a request for 10 songs
        // looks up, so the songs come from the lookups by rowid.
        let db = tracks_of_one_album(1500, |_, _| {});
        let mut rng = rng();
        let mut drawn = HashSet::new();
        for _ in 0..20 {
            let songs = random(&db, &mut rng, &SongFilter::default(), 10);
            assert_distinct(&songs, 10);
            drawn.extend(songs.into_iter().map(|song| song.track.id));
        }
        // Twenty requests that each returned the same ten songs would not be
        // random. The chance that 200 draws from 1500 rows hit fewer than 50
        // distinct rows is below 1e-200.
        assert!(drawn.len() >= 50, "{}", drawn.len());

        // A request for more songs than the table has returns every song.
        let songs = random(&db, &mut rng, &SongFilter::default(), 2000);
        assert_distinct(&songs, 1500);
        assert_eq!(random(&db, &mut rng, &SongFilter::default(), 0), []);
    }

    #[test]
    fn random_songs_follow_a_filter_that_many_rows_match() {
        // Every second track is Jazz, and every third is from 1990.
        let db = tracks_of_one_album(1500, |number, spec| {
            spec.genre = Some(if number % 2 == 0 { "Jazz" } else { "Pop" });
            spec.year = Some(if number % 3 == 0 { 1990 } else { 2010 });
        });
        let mut rng = rng();
        for _ in 0..10 {
            let filter = SongFilter {
                genre: Some("jazz".to_owned()),
                ..SongFilter::default()
            };
            let songs = random(&db, &mut rng, &filter, 10);
            assert_distinct(&songs, 10);
            for song in &songs {
                assert_eq!(song.track.genre.as_deref(), Some("Jazz"));
            }

            let filter = SongFilter {
                from_year: Some(1980),
                to_year: Some(1999),
                ..SongFilter::default()
            };
            let songs = random(&db, &mut rng, &filter, 10);
            assert_distinct(&songs, 10);
            for song in &songs {
                assert_eq!(song.track.year, Some(1990));
            }
        }
    }

    #[test]
    fn random_songs_of_a_filter_that_few_rows_match_are_all_of_these_rows() {
        // Five tracks of 1500 are Jazz and three are from 1990. The lookups
        // by rowid find too few of them, so the rows that match are read.
        let db = tracks_of_one_album(1500, |number, spec| {
            spec.genre = Some(if number % 300 == 7 { "Jazz" } else { "Pop" });
            spec.year = Some(if number % 500 == 11 { 1990 } else { 2010 });
        });
        let mut rng = rng();
        let jazz = SongFilter {
            genre: Some("JAZZ".to_owned()),
            ..SongFilter::default()
        };
        let old = SongFilter {
            to_year: Some(1990),
            ..SongFilter::default()
        };
        for _ in 0..10 {
            let songs = random(&db, &mut rng, &jazz, 10);
            assert_distinct(&songs, 5);
            for song in &songs {
                assert_eq!(song.track.genre.as_deref(), Some("Jazz"));
            }

            let songs = random(&db, &mut rng, &old, 10);
            assert_distinct(&songs, 3);
            for song in &songs {
                assert_eq!(song.track.year, Some(1990));
            }

            // A request for fewer songs than match returns that many.
            let songs = random(&db, &mut rng, &jazz, 2);
            assert_distinct(&songs, 2);
            for song in &songs {
                assert_eq!(song.track.genre.as_deref(), Some("Jazz"));
            }
        }

        let both = SongFilter {
            genre: Some("Jazz".to_owned()),
            from_year: Some(1990),
            to_year: Some(1990),
            folder_id: None,
        };
        assert_eq!(random(&db, &mut rng, &both, 10), []);
        let unknown = SongFilter {
            genre: Some("Polka".to_owned()),
            ..SongFilter::default()
        };
        assert_eq!(random(&db, &mut rng, &unknown, 10), []);
    }

    #[test]
    fn random_songs_of_a_table_that_lost_most_rows_are_rows_that_exist() {
        let db = tracks_of_one_album(1500, |_, _| {});
        // 30 rows stay, and their rowids are spread over the old range.
        let kept: HashSet<String> = (0..1500)
            .filter(|number| number % 50 == 3)
            .map(|number| track(number).id)
            .collect();
        let deleted: Vec<String> = (0..1500)
            .map(|number| track(number).id)
            .filter(|id| !kept.contains(id))
            .collect();
        db.with_writer(|conn| tracks::delete_tracks(conn, &deleted))
            .unwrap();

        let mut rng = rng();
        for _ in 0..10 {
            let songs = random(&db, &mut rng, &SongFilter::default(), 10);
            assert_distinct(&songs, 10);
            for song in &songs {
                assert!(kept.contains(&song.track.id), "{}", song.track.id);
            }
        }
        let songs = random(&db, &mut rng, &SongFilter::default(), 500);
        assert_distinct(&songs, 30);
    }

    #[test]
    fn every_row_of_a_table_is_drawn() {
        // Four tracks, of which the first and the last have the smallest and
        // the largest rowid. 400 draws of one track miss one of the four
        // with a chance below 1e-49.
        let db = tracks_of_one_album(4, |_, _| {});
        let mut rng = rng();
        let mut drawn = HashSet::new();
        for _ in 0..400 {
            let songs = random(&db, &mut rng, &SongFilter::default(), 1);
            assert_distinct(&songs, 1);
            drawn.insert(songs[0].track.id.clone());
        }
        let all: HashSet<String> = (0..4).map(|number| track(number).id).collect();
        assert_eq!(drawn, all);

        // The same holds for the rows that are read when few rows match:
        // four Jazz tracks among 1500.
        let db = tracks_of_one_album(1500, |number, spec| {
            spec.genre = (number % 400 == 0).then_some("Jazz");
        });
        let jazz = SongFilter {
            genre: Some("Jazz".to_owned()),
            ..SongFilter::default()
        };
        let mut drawn = HashSet::new();
        for _ in 0..400 {
            let songs = random(&db, &mut rng, &jazz, 1);
            assert_distinct(&songs, 1);
            drawn.insert(songs[0].track.id.clone());
        }
        let all: HashSet<String> = [0, 400, 800, 1200]
            .into_iter()
            .map(|number| track(number).id)
            .collect();
        assert_eq!(drawn, all);
    }

    #[test]
    fn random_songs_leave_out_a_track_without_a_file_and_follow_the_folder() {
        let db = library(&[
            track(1),
            track(2),
            Spec {
                folder: "/other",
                ..track(3)
            },
        ]);
        remove_file(&db, &track(2).id);
        let mut rng = rng();
        let (first, third) = (track(1).id, track(3).id);
        for _ in 0..20 {
            let songs = random(&db, &mut rng, &SongFilter::default(), 10);
            let ids: HashSet<&str> = song_ids(&songs).into_iter().collect();
            assert_eq!(ids, HashSet::from([first.as_str(), third.as_str()]));

            let other = SongFilter {
                folder_id: Some(folder_id(&db, "/other")),
                ..SongFilter::default()
            };
            assert_eq!(song_ids(&random(&db, &mut rng, &other, 10)), [&third]);
        }
        let nowhere = SongFilter {
            folder_id: Some(99),
            ..SongFilter::default()
        };
        assert_eq!(random(&db, &mut rng, &nowhere, 10), []);
    }

    #[test]
    fn a_track_without_a_year_counts_as_year_0_in_a_year_range() {
        let db = library(&[
            track(1),
            Spec {
                year: Some(1990),
                ..track(2)
            },
            Spec {
                year: Some(2010),
                ..track(3)
            },
        ]);
        let mut rng = rng();
        let ids = |from_year: Option<u32>, to_year: Option<u32>, rng: &mut StdRng| {
            let filter = SongFilter {
                from_year,
                to_year,
                ..SongFilter::default()
            };
            let mut ids: Vec<String> = random(&db, rng, &filter, 10)
                .into_iter()
                .map(|song| song.track.id)
                .collect();
            ids.sort();
            ids
        };
        let (none, old, new) = (track(1).id, track(2).id, track(3).id);
        let (none, old, new) = (none.as_str(), old.as_str(), new.as_str());
        assert_eq!(ids(Some(0), None, &mut rng), [none, old, new]);
        assert_eq!(ids(Some(1), None, &mut rng), [old, new]);
        assert_eq!(ids(Some(1991), None, &mut rng), [new]);
        assert_eq!(ids(None, Some(2000), &mut rng), [none, old]);
        assert_eq!(ids(None, Some(0), &mut rng), [none]);
        assert_eq!(ids(Some(1990), Some(2010), &mut rng), [old, new]);
        // A range whose first year is after its last year holds no track.
        assert_eq!(ids(Some(2010), Some(1990), &mut rng), [] as [&str; 0]);
    }

    #[test]
    fn genre_spellings_are_the_stored_genres_that_differ_by_case_only() {
        let db = library(&[
            Spec {
                genre: Some("Rock"),
                ..track(1)
            },
            Spec {
                genre: Some("rock"),
                album: "Paper Maps".to_owned(),
                ..track(2)
            },
            Spec {
                genre: Some("Rock & Roll"),
                ..track(3)
            },
            Spec {
                genre: Some("Électro"),
                album: "Slow Rivers".to_owned(),
                ..track(4)
            },
            track(5),
        ]);
        db.with_reader(|conn| {
            let tracks = |genre: &str| genre_spellings(conn, Table::Tracks, genre);
            assert_eq!(tracks("ROCK")?, ["Rock", "rock"]);
            assert_eq!(tracks("rock & roll")?, ["Rock & Roll"]);
            assert_eq!(tracks("électro")?, ["Électro"]);
            assert_eq!(tracks("ÉLECTRO")?, ["Électro"]);
            // The match is of the whole genre: no part, no pattern.
            assert_eq!(tracks("Roc")?, [] as [&str; 0]);
            assert_eq!(tracks("Rock%")?, [] as [&str; 0]);
            assert_eq!(tracks("_ock")?, [] as [&str; 0]);
            assert_eq!(tracks("")?, [] as [&str; 0]);

            // The genre of an album is the genre of most of its tracks.
            let albums = |genre: &str| genre_spellings(conn, Table::Albums, genre);
            assert_eq!(albums("ROCK")?, ["Rock", "rock"]);
            assert_eq!(albums("électro")?, ["Électro"]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn songs_by_genre_are_in_title_order_and_pages_do_not_overlap() {
        // 25 Jazz tracks in the two spellings, with the titles in reverse
        // order of the rows, and 5 tracks of another genre.
        let db = tracks_of_one_album(30, |number, spec| {
            spec.title = format!("Title {:02}", 29 - number);
            spec.genre = Some(match number % 6 {
                0 => "Pop",
                1 => "jazz",
                _ => "Jazz",
            });
        });
        let page = |size, offset| Page { size, offset };
        db.with_reader(|conn| {
            let all = songs_by_genre(conn, "Jazz", None, page(500, 0))?;
            assert_eq!(all.len(), 25);
            let titles: Vec<&str> = all.iter().map(|song| song.track.title.as_str()).collect();
            let mut sorted = titles.clone();
            sorted.sort_unstable();
            assert_eq!(titles, sorted);

            let mut paged = Vec::new();
            for offset in (0..30).step_by(10) {
                paged.extend(songs_by_genre(conn, "JAZZ", None, page(10, offset))?);
            }
            assert_eq!(paged, all);
            assert_eq!(songs_by_genre(conn, "Jazz", None, page(10, 25))?, []);
            assert_eq!(songs_by_genre(conn, "Jazz", None, page(0, 0))?, []);
            assert_eq!(songs_by_genre(conn, "Polka", None, page(10, 0))?, []);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn songs_by_genre_list_a_track_without_a_file_only_without_a_folder() {
        let db = library(&[
            Spec {
                genre: Some("Jazz"),
                ..track(1)
            },
            Spec {
                genre: Some("Jazz"),
                ..track(2)
            },
            Spec {
                genre: Some("Jazz"),
                folder: "/other",
                ..track(3)
            },
        ]);
        remove_file(&db, &track(2).id);
        let page = Page {
            size: 10,
            offset: 0,
        };
        let (music, other) = (folder_id(&db, "/music"), folder_id(&db, "/other"));
        let (first, second, third) = (track(1).id, track(2).id, track(3).id);
        db.with_reader(|conn| {
            let ids = |folder_id: Option<i64>| -> Result<Vec<String>> {
                let songs = songs_by_genre(conn, "Jazz", folder_id, page)?;
                Ok(songs.into_iter().map(|song| song.track.id).collect())
            };
            assert_eq!(ids(None)?, [first.as_str(), &second, &third]);
            assert_eq!(ids(Some(music))?, [first.as_str()]);
            assert_eq!(ids(Some(other))?, [third.as_str()]);
            assert_eq!(ids(Some(99))?, [] as [&str; 0]);
            Ok(())
        })
        .unwrap();
    }

    /// Three albums of two artists: "Glass Harbour" (1990, Jazz) and "Paper
    /// Maps" (no year, Pop) by Aurora Field in `/music`, and "Slow Rivers"
    /// (2010, Jazz) by Marlow Quartet in `/other`. "Paper Maps" has two
    /// tracks.
    fn three_albums() -> Db {
        library(&[
            Spec {
                year: Some(1990),
                genre: Some("Jazz"),
                ..track(1)
            },
            Spec {
                album: "Paper Maps".to_owned(),
                genre: Some("Pop"),
                ..track(2)
            },
            Spec {
                album: "Paper Maps".to_owned(),
                genre: Some("Pop"),
                ..track(3)
            },
            Spec {
                folder: "/other",
                artist: "Marlow Quartet",
                album: "Slow Rivers".to_owned(),
                year: Some(2010),
                genre: Some("Jazz"),
                ..track(4)
            },
        ])
    }

    /// The titles of one page of 10 albums of `list` for the owner.
    fn list_titles(db: &Db, list: &AlbumList, folder_id: Option<i64>) -> Vec<String> {
        let user_id = owner(db);
        let page = Page {
            size: 10,
            offset: 0,
        };
        let albums = db
            .with_reader(|conn| albums(conn, &mut rng(), user_id, list, folder_id, page))
            .unwrap();
        titles(&albums).into_iter().map(str::to_owned).collect()
    }

    #[test]
    fn by_year_counts_an_album_without_a_year_as_year_0() {
        let db = three_albums();
        let by_year = |from, to| list_titles(&db, &AlbumList::ByYear { from, to }, None);
        assert_eq!(
            by_year(0, 2010),
            ["Paper Maps", "Glass Harbour", "Slow Rivers"]
        );
        assert_eq!(
            by_year(2010, 0),
            ["Slow Rivers", "Glass Harbour", "Paper Maps"]
        );
        assert_eq!(by_year(1, 2010), ["Glass Harbour", "Slow Rivers"]);
        assert_eq!(by_year(1990, 1990), ["Glass Harbour"]);
        assert_eq!(by_year(0, 0), ["Paper Maps"]);
        assert_eq!(by_year(1991, 2009), [] as [&str; 0]);
        // A year above every stored year is a bound as every other.
        assert_eq!(by_year(u32::MAX, 2000), ["Slow Rivers"]);
    }

    #[test]
    fn frequent_and_recent_take_the_plays_of_an_album_from_its_tracks() {
        let db = three_albums();
        let user_id = owner(&db);
        assert_eq!(
            list_titles(&db, &AlbumList::Frequent, None),
            [] as [&str; 0]
        );
        assert_eq!(list_titles(&db, &AlbumList::Recent, None), [] as [&str; 0]);

        // "Paper Maps" has 2 + 3 plays, the last at 300. "Glass Harbour" has
        // 4 plays, the last at 900. "Slow Rivers" has a track with a star
        // and no play.
        let rows = [
            (track(1).id, 4, Some(900)),
            (track(2).id, 2, Some(300)),
            (track(3).id, 3, Some(100)),
            (track(4).id, 0, None),
        ];
        db.with_writer(|conn| {
            for (item_id, play_count, last_played_at) in &rows {
                conn.execute(
                    "INSERT INTO annotations (user_id, item_id, item_type, starred_at,
                       play_count, last_played_at)
                     VALUES (?1, ?2, 'track', 50, ?3, ?4)",
                    params![user_id, item_id, play_count, last_played_at],
                )?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            list_titles(&db, &AlbumList::Frequent, None),
            ["Paper Maps", "Glass Harbour"]
        );
        assert_eq!(
            list_titles(&db, &AlbumList::Recent, None),
            ["Glass Harbour", "Paper Maps"]
        );

        // The plays of another user do not count.
        let page = Page {
            size: 10,
            offset: 0,
        };
        db.with_reader(|conn| {
            for list in [AlbumList::Frequent, AlbumList::Recent] {
                assert_eq!(
                    albums(conn, &mut rng(), user_id + 1, &list, None, page)?,
                    []
                );
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn album_lists_with_a_folder_hold_the_albums_with_a_file_in_it() {
        let db = three_albums();
        let (music, other) = (folder_id(&db, "/music"), folder_id(&db, "/other"));
        let by_name = AlbumList::AlphabeticalByName;
        assert_eq!(
            list_titles(&db, &by_name, None),
            ["Glass Harbour", "Paper Maps", "Slow Rivers"]
        );
        assert_eq!(
            list_titles(&db, &by_name, Some(music)),
            ["Glass Harbour", "Paper Maps"]
        );
        assert_eq!(list_titles(&db, &by_name, Some(other)), ["Slow Rivers"]);
        assert_eq!(list_titles(&db, &by_name, Some(99)), [] as [&str; 0]);

        // An album whose tracks all lost their file is in no folder. It
        // stays in the list without a folder.
        remove_file(&db, &track(1).id);
        remove_file(&db, &track(2).id);
        assert_eq!(list_titles(&db, &by_name, Some(music)), ["Paper Maps"]);
        assert_eq!(
            list_titles(&db, &by_name, None),
            ["Glass Harbour", "Paper Maps", "Slow Rivers"]
        );

        let mut random = list_titles(&db, &AlbumList::Random, Some(music));
        assert_eq!(random, ["Paper Maps"]);
        random = list_titles(&db, &AlbumList::Random, Some(99));
        assert_eq!(random, [] as [&str; 0]);
    }

    #[test]
    fn random_albums_are_distinct_and_an_offset_lowers_their_number() {
        // 1200 albums of one track each.
        let specs: Vec<Spec> = (0..1200)
            .map(|number| Spec {
                album: format!("Album {number:04}"),
                ..track(number)
            })
            .collect();
        let db = library(&specs);
        let user_id = owner(&db);
        let mut rng = rng();
        let mut random = |size, offset| {
            let page = Page { size, offset };
            db.with_reader(|conn| albums(conn, &mut rng, user_id, &AlbumList::Random, None, page))
                .unwrap()
        };
        let distinct = |albums: &[Album]| -> usize {
            let ids: HashSet<&str> = albums.iter().map(|album| album.id.as_str()).collect();
            ids.len()
        };

        for _ in 0..10 {
            let page = random(10, 0);
            assert_eq!((page.len(), distinct(&page)), (10, 10));
        }
        let page = random(500, 0);
        assert_eq!((page.len(), distinct(&page)), (500, 500));

        // The offset selects no rows. It leaves `count - offset` albums at
        // most, so the pages of a client end.
        assert_eq!(random(500, 500).len(), 500);
        let page = random(500, 1000);
        assert_eq!((page.len(), distinct(&page)), (200, 200));
        assert_eq!(random(500, 1199).len(), 1);
        assert_eq!(random(500, 1200), []);
        assert_eq!(random(500, u32::MAX), []);
        assert_eq!(random(0, 0), []);

        // A request for more albums than the table has returns every album.
        let all = random(2000, 0);
        assert_eq!((all.len(), distinct(&all)), (1200, 1200));
    }
}
