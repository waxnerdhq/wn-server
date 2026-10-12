//! The queries of `search2` and `search3`: the artists, the albums and the
//! tracks whose names hold every term of a query (`docs/design.md`
//! sections 1.3 and 4).
//!
//! # Terms
//!
//! [`Terms::parse`] splits a query at white space. A track, an album or an
//! artist is a match when every term is part of one of its names. The terms
//! can be parts of different names, and their order in the query has no
//! meaning. A query without a term matches every row, which is how a client
//! reads the whole library.
//!
//! A query has [`MAX_QUERY_CHARS`] characters and [`MAX_TERMS`] different
//! terms at most. The work of a search grows with both, so a longer query is
//! an error and reaches no statement.
//!
//! # The query is a parameter
//!
//! The text of a query never becomes part of a statement. The terms are one
//! parameter: a JSON array of `LIKE` patterns that the statement reads with
//! `json_each`, as the statements of `browse` read a list of IDs. A query
//! with any number of terms is therefore the same statement text. In a pattern
//! the characters `%`, `_` and `\` of a term are escaped with `\`, so they
//! match themselves.
//!
//! # Names and letter case
//!
//! The names are compared in lower case. `artists.key`, `albums.key` and
//! `tracks.title_lc` hold a name in lower case, and a term is lower-cased
//! the same way, so the comparison ignores the case of every letter. The
//! keys are in Unicode form NFC, and so is a term.
//!
//! The track artist, `tracks.artist_name`, is stored as the tag spells it
//! and in no other form. `LIKE` ignores the case of the letters A to Z only,
//! so a letter outside ASCII in the name of a track artist matches a term
//! only when the tag has it in lower case.
//!
//! The title and the track artist of a track are in the Unicode form of the
//! tag. A composed letter of a term does not match the same letter in
//! decomposed form there.
//!
//! A letter with a diacritic and its base letter are different letters:
//! `edith` does not match "Édith".
//!
//! # Order and paging
//!
//! Every list is in the order of the lower-cased names. Rows with the same
//! name are in the order of their rowids, which is the order in which the
//! scanner stored them. The rowid of a row changes only when the database is
//! rebuilt with `VACUUM`, which the server never runs. The order is
//! therefore total and the same for every page, so a client that reads page
//! after page with an offset gets each row once while the library does not
//! change.
//!
//! The indexes `tracks_title_lc` and `albums_key` and the unique index of
//! `artists.key` hold the rows in exactly that order. A statement reads its
//! index from the start and stops when the page is full, and it sorts
//! nothing. The rows before the offset of a query without a term and without
//! a music folder are read from the index alone.
//!
//! # Tracks without a file
//!
//! A track row without a file is a match as every other row
//! (`docs/design.md` section 3.4). It is in no music folder, so a search
//! that names a music folder leaves it out.

use rusqlite::Connection;
use unicode_normalization::UnicodeNormalization;

use super::browse::{self, Album, Artist, Song, json_array};
use super::tracks::HAS_FILE_SQL;
use crate::db::Result;

/// The most characters that a query may have.
///
/// The limit bounds the length of a `LIKE` pattern. SQLite fails a `LIKE`
/// whose pattern has more than 50 000 bytes. The pattern of a term of this
/// length has fewer than 13 000 bytes, whatever its characters are.
pub const MAX_QUERY_CHARS: usize = 1024;

/// The most different terms that a query may have. A term that the query
/// holds twice counts once.
///
/// The limit bounds the work of one search, which grows with the number of
/// terms. The statement of the tracks tests the key of every album and of
/// every artist against every term, and then every track against the terms
/// until one does not match. A query that a person types has fewer terms.
pub const MAX_TERMS: usize = 32;

/// The character that a query may hold and a term does not: the double
/// quote. Some clients send `""` for a query without a term.
const QUOTE: char = '"';

/// The character that a query may end with and that is no part of its last
/// term. The search of the Subsonic API 1.x took it as a wildcard, and some
/// clients still append it to a query.
const WILDCARD: char = '*';

/// The character that escapes `%`, `_` and itself in a pattern. Every `LIKE`
/// of this module names it in its `ESCAPE` clause.
const ESCAPE: char = '\\';

/// The error of a query that is larger than the statements of this module
/// take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    /// The query has more than [`MAX_QUERY_CHARS`] characters.
    #[error("the query has more than {MAX_QUERY_CHARS} characters")]
    TooLong,
    /// The query has more than [`MAX_TERMS`] different terms.
    #[error("the query has more than {MAX_TERMS} terms")]
    TooManyTerms,
}

/// The terms of one search query, as the statements of this module take
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms {
    /// One `LIKE` pattern for each term: the term in lower case between two
    /// `%`, with its `%`, `_` and `\` escaped. No pattern is here twice.
    ///
    /// `None` when a term holds the character U+0000. SQLite reads a pattern
    /// and a name up to that character, so `LIKE` cannot compare such a
    /// term, and the query matches nothing.
    patterns: Option<Vec<String>>,
}

impl Terms {
    /// Returns the terms of `query`.
    ///
    /// - Every double quote is removed, so `""` is a query without a term
    ///   and `"glass harbour"` is the two terms `glass` and `harbour`.
    /// - The text is brought into Unicode form NFC and into lower case, as a
    ///   natural key is (`ids::artist_key`).
    /// - One `*` at the end of the text is removed, so `beat*` is the term
    ///   `beat`. A `*` at any other place is a character of its term.
    /// - The terms are the parts between white space. A term that the query
    ///   holds twice counts once.
    ///
    /// A query of more than [`MAX_QUERY_CHARS`] characters is an error, and
    /// so is a query of more than [`MAX_TERMS`] different terms.
    pub fn parse(query: &str) -> std::result::Result<Self, QueryError> {
        if query.chars().nth(MAX_QUERY_CHARS).is_some() {
            return Err(QueryError::TooLong);
        }
        let text: String = query
            .nfc()
            .filter(|character| *character != QUOTE)
            .collect();
        let text = text.to_lowercase();
        let text = text.trim();
        let text = text.strip_suffix(WILDCARD).unwrap_or(text);

        let mut patterns: Vec<String> = Vec::new();
        let mut comparable = true;
        for term in text.split_whitespace() {
            let pattern = pattern(term);
            if patterns.contains(&pattern) {
                continue;
            }
            if patterns.len() == MAX_TERMS {
                return Err(QueryError::TooManyTerms);
            }
            comparable &= !term.contains('\0');
            patterns.push(pattern);
        }
        Ok(Self {
            patterns: comparable.then_some(patterns),
        })
    }
}

/// Returns the `LIKE` pattern that matches the names which hold `term`: the
/// term between two `%`. A `%`, a `_` and a `\` of the term follow a `\`, so
/// each matches itself and no other character.
fn pattern(term: &str) -> String {
    let mut pattern = String::with_capacity(term.len() + 2);
    pattern.push('%');
    for character in term.chars() {
        if matches!(character, '%' | '_' | ESCAPE) {
            pattern.push(ESCAPE);
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// Returns the artists whose name holds every term of `terms`, in the order
/// of their names: `count` rows at most, after the first `offset` rows.
///
/// With `folder_id` the result holds only the artists that have a track with
/// a file in that music folder, as `browse::list_artists` does.
pub fn search_artists(
    conn: &Connection,
    terms: &Terms,
    count: u32,
    offset: u32,
    folder_id: Option<i64>,
) -> Result<Vec<Artist>> {
    let ids = select_ids(conn, &artists_sql(), terms, count, offset, folder_id)?;
    browse::artists_by_ids(conn, &ids)
}

/// Returns the albums that match `terms`, in the order of their titles:
/// `count` rows at most, after the first `offset` rows. Albums with the same
/// title are in the order in which they were stored.
///
/// An album is a match when each term is part of its title or of the name of
/// its artist.
///
/// With `folder_id` the result holds only the albums that have a track with
/// a file in that music folder.
pub fn search_albums(
    conn: &Connection,
    terms: &Terms,
    count: u32,
    offset: u32,
    folder_id: Option<i64>,
) -> Result<Vec<Album>> {
    let ids = select_ids(conn, &albums_sql(), terms, count, offset, folder_id)?;
    browse::albums_by_ids(conn, &ids)
}

/// Returns the tracks that match `terms`, in the order of their titles:
/// `count` rows at most, after the first `offset` rows. Tracks with the same
/// title are in the order in which they were stored.
///
/// A track is a match when each term is part of its title, of the title of
/// its album, of the name of its track artist or of the name of its album
/// artist. The track artist is compared as the module comment describes.
///
/// A track without a file is a match as every other track. With `folder_id`
/// the result holds only the tracks that have a file in that music folder.
pub fn search_tracks(
    conn: &Connection,
    terms: &Terms,
    count: u32,
    offset: u32,
    folder_id: Option<i64>,
) -> Result<Vec<Song>> {
    let ids = select_ids(conn, &tracks_sql(), terms, count, offset, folder_id)?;
    browse::tracks_by_ids(conn, &ids)
}

/// Runs `sql`, one of the three statements of this module, and returns the
/// IDs it selects, in its order.
///
/// The parameters of the statement are the patterns of `terms` as a JSON
/// array, `count`, `offset` and `folder_id`. A statement is not run for
/// terms that match nothing, nor for a `count` of 0.
fn select_ids(
    conn: &Connection,
    sql: &str,
    terms: &Terms,
    count: u32,
    offset: u32,
    folder_id: Option<i64>,
) -> Result<Vec<String>> {
    let Some(patterns) = &terms.patterns else {
        return Ok(Vec::new());
    };
    if count == 0 {
        return Ok(Vec::new());
    }
    let ids = conn
        .prepare_cached(sql)?
        .query_map((json_array(patterns), count, offset, folder_id), |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The statement of [`search_artists`].
///
/// An artist is left out when one pattern does not match its key. The rows
/// are read in the order of the unique index of `key`, and the music folder
/// is tested with the index `tracks_artist`.
fn artists_sql() -> String {
    format!(
        r"SELECT id FROM artists
          WHERE NOT EXISTS (
                  SELECT 1 FROM json_each(?1) AS term
                  WHERE (artists.key LIKE term.value ESCAPE '\') IS NOT TRUE)
            AND (?4 IS NULL OR EXISTS (
                  SELECT 1 FROM tracks
                  WHERE tracks.artist_id = artists.id AND tracks.folder_id = ?4
                    AND {HAS_FILE_SQL}))
          ORDER BY key
          LIMIT ?2 OFFSET ?3"
    )
}

/// The statement of [`search_albums`].
///
/// An album is left out when one pattern matches neither its key nor the key
/// of its artist. `albums.artist_name` is the name of that artist, and the
/// key is the same name in lower case. The rows are read in the order of the
/// index `albums_key`, and the music folder is tested with the index
/// `tracks_album`.
fn albums_sql() -> String {
    format!(
        r"SELECT id FROM albums
          WHERE NOT EXISTS (
                  SELECT 1 FROM json_each(?1) AS term
                  WHERE (albums.key LIKE term.value ESCAPE '\'
                      OR (SELECT key FROM artists WHERE artists.id = albums.artist_id)
                         LIKE term.value ESCAPE '\') IS NOT TRUE)
            AND (?4 IS NULL OR EXISTS (
                  SELECT 1 FROM tracks
                  WHERE tracks.album_id = albums.id AND tracks.folder_id = ?4
                    AND {HAS_FILE_SQL}))
          ORDER BY key, rowid
          LIMIT ?2 OFFSET ?3"
    )
}

/// The statement of [`search_tracks`].
///
/// A track is left out when one pattern matches none of its four names. The
/// title and the track artist are columns of the track. The title of the
/// album and the name of the album artist are the keys of the album row and
/// of the artist row: `tracks.album_name` has the key of the album row, and
/// a key is the name in lower case.
///
/// `terms` numbers the patterns. `album_hits` and `artist_hits` hold, for
/// each pattern, the IDs of the albums and of the artists whose key it
/// matches. SQLite builds each of the three once for a statement. The test
/// of a track is then a lookup of its `album_id` and of its `artist_id`, and
/// no album row and no artist row is read for a track.
///
/// The rows are read in the order of the index `tracks_title_lc`.
fn tracks_sql() -> String {
    format!(
        r"WITH terms(n, pattern) AS MATERIALIZED (
                 SELECT key, value FROM json_each(?1)),
               album_hits(n, id) AS MATERIALIZED (
                 SELECT terms.n, albums.id FROM terms JOIN albums
                   ON albums.key LIKE terms.pattern ESCAPE '\'),
               artist_hits(n, id) AS MATERIALIZED (
                 SELECT terms.n, artists.id FROM terms JOIN artists
                   ON artists.key LIKE terms.pattern ESCAPE '\')
          SELECT id FROM tracks
          WHERE NOT EXISTS (
                  SELECT 1 FROM terms
                  WHERE (tracks.title_lc LIKE terms.pattern ESCAPE '\'
                      OR tracks.artist_name LIKE terms.pattern ESCAPE '\'
                      OR (terms.n, tracks.album_id) IN (SELECT n, id FROM album_hits)
                      OR (terms.n, tracks.artist_id) IN (SELECT n, id FROM artist_hits))
                    IS NOT TRUE)
            AND (?4 IS NULL OR (tracks.folder_id = ?4 AND {HAS_FILE_SQL}))
          ORDER BY title_lc, rowid
          LIMIT ?2 OFFSET ?3"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::*;
    use crate::db::Db;
    use crate::db::queries::tracks::{self, TrackContent};
    use crate::db::queries::{albums, artists, folders};
    use crate::library::tags::TrackMeta;

    /// A count that is larger than every test library.
    const ALL: u32 = 500;

    /// One track of a test library. Its ID and its path follow from `n`.
    #[derive(Debug, Clone, Copy)]
    struct Spec {
        n: u32,
        folder: &'static str,
        /// The track artist.
        artist: &'static str,
        album_artist: &'static str,
        album: &'static str,
        title: &'static str,
    }

    /// Returns the track `n` in the folder `/music`, with `artist` as its
    /// track artist and as its album artist.
    fn song(n: u32, artist: &'static str, album: &'static str, title: &'static str) -> Spec {
        Spec {
            n,
            folder: "/music",
            artist,
            album_artist: artist,
            album,
            title,
        }
    }

    /// The ID of the track `n`.
    fn track_id(n: u32) -> String {
        format!("tr-{n:032x}")
    }

    /// Stores `specs` as the scanner stores tracks: it inserts the folder,
    /// the artist and the album of each track when they are missing, and
    /// recomputes the counts of the albums and of the artists at the end.
    fn library(specs: &[Spec]) -> Db {
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| {
            for spec in specs {
                let folder = folders::sync_folders(conn, &[PathBuf::from(spec.folder)])?;
                let artist_id = artists::upsert_by_key(conn, spec.album_artist)?;
                let album_id =
                    albums::upsert_by_key(conn, &artist_id, spec.album, spec.album_artist, false)?;
                let meta = TrackMeta {
                    title: spec.title.to_owned(),
                    artist: spec.artist.to_owned(),
                    album_artist: spec.album_artist.to_owned(),
                    album: spec.album.to_owned(),
                    track_no: None,
                    disc_no: None,
                    year: None,
                    genre: None,
                    compilation: false,
                    duration_ms: 1000,
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
                    artist_id: &artist_id,
                    album_id: &album_id,
                };
                tracks::insert_track(
                    conn,
                    &track_id(spec.n),
                    folder[0].id,
                    &format!("{}.flac", spec.n),
                    &content,
                )?;
            }
            albums::recompute_aggregates(conn)?;
            artists::recompute_aggregates(conn)
        })
        .unwrap();
        db
    }

    /// Gives the track `n` its placeholder path, as the scanner does for a
    /// track that it keeps without a file.
    fn remove_file(db: &Db, n: u32) {
        let id = track_id(n);
        db.with_writer(|conn| {
            let track = tracks::get_track(conn, &id)?.expect("the track exists");
            tracks::relink_track(
                conn,
                &id,
                track.folder_id,
                &tracks::placeholder_path(&id),
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

    fn terms(query: &str) -> Terms {
        Terms::parse(query).expect("the query is short enough")
    }

    /// The patterns of `query`.
    fn patterns(query: &str) -> Vec<String> {
        terms(query).patterns.expect("no term holds U+0000")
    }

    /// The names of the artists, the titles of the albums and the titles of
    /// the tracks that match, each list in the order of the search.
    #[derive(Debug, PartialEq, Eq)]
    struct Names {
        artists: Vec<String>,
        albums: Vec<String>,
        tracks: Vec<String>,
    }

    fn names(artists: &[&str], albums: &[&str], tracks: &[&str]) -> Names {
        let owned = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
        Names {
            artists: owned(artists),
            albums: owned(albums),
            tracks: owned(tracks),
        }
    }

    /// Searches `db` for `query` in the music folder `folder`, without an
    /// offset and with a count that holds every match.
    fn find_in(db: &Db, query: &str, folder: Option<i64>) -> Names {
        let terms = terms(query);
        db.with_reader(|conn| {
            Ok(Names {
                artists: search_artists(conn, &terms, ALL, 0, folder)?
                    .into_iter()
                    .map(|artist| artist.name)
                    .collect(),
                albums: search_albums(conn, &terms, ALL, 0, folder)?
                    .into_iter()
                    .map(|album| album.name)
                    .collect(),
                tracks: search_tracks(conn, &terms, ALL, 0, folder)?
                    .into_iter()
                    .map(|song| song.track.title)
                    .collect(),
            })
        })
        .unwrap()
    }

    /// Searches every music folder of `db` for `query`.
    fn find(db: &Db, query: &str) -> Names {
        find_in(db, query, None)
    }

    /// Four artists, five albums and six tracks with plain names.
    fn catalogue() -> Db {
        library(&[
            song(1, "The Beatles", "Help!", "Yesterday"),
            song(2, "The Beatles", "Abbey Road", "Come Together"),
            song(3, "Michael Jackson", "Thriller", "Beat It"),
            song(4, "Aurora Field", "Glass Harbour", "Low Tide"),
            song(5, "Aurora Field", "Glass Harbour", "Breakwater"),
            Spec {
                artist: "Tessa Lund",
                album_artist: "Various Artists",
                ..song(6, "", "Summer Sampler", "Open Road")
            },
        ])
    }

    #[test]
    fn query_without_a_term_has_no_pattern() {
        for query in ["", " ", "\t\n ", "\"\"", "\"", " \"\" \" ", "\u{3000}"] {
            assert_eq!(patterns(query), [] as [&str; 0], "{query:?}");
        }
    }

    #[test]
    fn terms_are_the_parts_between_white_space_in_lower_case() {
        assert_eq!(patterns("Beat"), ["%beat%"]);
        assert_eq!(
            patterns("  Glass\tHARBOUR\u{a0}low\u{3000}tide \n"),
            ["%glass%", "%harbour%", "%low%", "%tide%"]
        );
        // Letters outside ASCII are lower-cased as well.
        assert_eq!(patterns("ÉDITH Кино ÅR"), ["%édith%", "%кино%", "%år%"]);
    }

    #[test]
    fn double_quotes_are_removed_and_other_quotes_stay() {
        assert_eq!(patterns("\"glass harbour\""), ["%glass%", "%harbour%"]);
        assert_eq!(patterns("12\" mix"), ["%12%", "%mix%"]);
        assert_eq!(patterns("gl\"ass"), ["%glass%"]);
        assert_eq!(
            patterns("don't `stop` ’now’"),
            ["%don't%", "%`stop`%", "%’now’%"]
        );
    }

    #[test]
    fn like_wildcards_and_the_escape_character_are_escaped() {
        assert_eq!(patterns("100%"), [r"%100\%%"]);
        assert_eq!(patterns("_"), [r"%\_%"]);
        assert_eq!(patterns(r"\"), [r"%\\%"]);
        assert_eq!(patterns(r"a\%b_c\\"), [r"%a\\\%b\_c\\\\%"]);
        assert_eq!(patterns("% _"), [r"%\%%", r"%\_%"]);
        // Other characters that have a meaning in SQL or in GLOB stay.
        assert_eq!(
            patterns("'; -- * ? [a]"),
            ["%';%", "%--%", "%*%", "%?%", "%[a]%"]
        );
    }

    #[test]
    fn term_is_in_unicode_form_nfc() {
        // `e` and U+0301 compose to U+00E9.
        assert_eq!(patterns("E\u{301}dith"), ["%\u{e9}dith%"]);
        assert_eq!(patterns("\u{c9}dith"), ["%\u{e9}dith%"]);
    }

    #[test]
    fn repeated_term_counts_once() {
        assert_eq!(patterns("a b A a B c"), ["%a%", "%b%", "%c%"]);
    }

    #[test]
    fn term_with_a_nul_character_matches_nothing() {
        for query in ["\0", "a\0b", "glass \0", "\0 glass"] {
            let terms = terms(query);
            assert_eq!(terms.patterns, None, "{query:?}");
        }
        let db = catalogue();
        assert_eq!(find(&db, "a\0b"), names(&[], &[], &[]));
        // `LIKE` would read the pattern of `beat\0zzz` as `%beat`.
        assert_eq!(find(&db, "beat\0zzz"), names(&[], &[], &[]));
        assert_eq!(find(&db, "\0"), names(&[], &[], &[]));
    }

    #[test]
    fn query_of_more_than_the_maximum_length_is_an_error() {
        let longest = "a".repeat(MAX_QUERY_CHARS);
        assert_eq!(patterns(&longest), [format!("%{longest}%")]);
        assert_eq!(
            Terms::parse(&"a".repeat(MAX_QUERY_CHARS + 1)),
            Err(QueryError::TooLong)
        );
        assert_eq!(
            Terms::parse(&" ".repeat(MAX_QUERY_CHARS + 1)),
            Err(QueryError::TooLong)
        );
        assert_eq!(
            Terms::parse(&"é".repeat(1_000_000)),
            Err(QueryError::TooLong)
        );
        // The length is tested before the terms are counted.
        assert_eq!(
            Terms::parse(&"a b ".repeat(MAX_QUERY_CHARS)),
            Err(QueryError::TooLong)
        );

        // The limit counts characters and not bytes.
        let wide = "é".repeat(MAX_QUERY_CHARS);
        assert_eq!(patterns(&wide), [format!("%{wide}%")]);
    }

    /// A query of `count` different terms: the ideographs from U+4E00 on.
    fn different_terms(count: u32) -> String {
        (0..count)
            .map(|n| char::from_u32(0x4e00 + n).unwrap().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn query_of_more_than_the_maximum_number_of_terms_is_an_error() {
        assert_eq!(MAX_TERMS, 32);
        assert_eq!(patterns(&different_terms(32)).len(), 32);
        assert_eq!(
            Terms::parse(&different_terms(33)),
            Err(QueryError::TooManyTerms)
        );
        assert_eq!(
            Terms::parse(&different_terms(500)),
            Err(QueryError::TooManyTerms)
        );

        // A term that the query holds twice counts once, in any letter case
        // and with or without quotes.
        let repeated = format!("{} \u{4e00} A a \"a\"", different_terms(31));
        assert_eq!(patterns(&repeated).len(), 32);
        assert_eq!(patterns(&"a ".repeat(512)), ["%a%"]);
        assert_eq!(
            Terms::parse(&format!("{repeated} b")),
            Err(QueryError::TooManyTerms)
        );

        // A term with a NUL character is a term: 33 terms are an error, and
        // 32 terms match nothing.
        assert_eq!(
            Terms::parse(&format!("a\0b {}", different_terms(32))),
            Err(QueryError::TooManyTerms)
        );
        assert_eq!(
            Terms::parse(&format!("{} a\0b", different_terms(32))),
            Err(QueryError::TooManyTerms)
        );
        assert_eq!(
            terms(&format!("a\0b {}", different_terms(31))).patterns,
            None
        );

        // The statements take the most terms.
        let db = catalogue();
        assert_eq!(find(&db, &different_terms(32)), names(&[], &[], &[]));
        // 32 parts of the names of "Come Together" on "Abbey Road" by The
        // Beatles.
        let parts = "a b c d e g h l m o r s t y ab bb be ey ro oa ad th he ea at tl le es co \
                     om me to";
        assert_eq!(patterns(parts).len(), 32);
        assert_eq!(find(&db, parts), names(&[], &[], &["Come Together"]));
    }

    #[test]
    fn one_wildcard_at_the_end_of_a_query_is_removed() {
        assert_eq!(patterns("beat*"), ["%beat%"]);
        assert_eq!(patterns("BEAT* "), ["%beat%"]);
        assert_eq!(patterns("\"beat*\""), ["%beat%"]);
        assert_eq!(patterns("the beat*"), ["%the%", "%beat%"]);
        assert_eq!(patterns("beat *"), ["%beat%"]);
        assert_eq!(patterns("*"), [] as [&str; 0]);
        assert_eq!(patterns(" \"*\" "), [] as [&str; 0]);
        // One is removed, and only at the end of the query.
        assert_eq!(patterns("beat**"), ["%beat*%"]);
        assert_eq!(patterns("**"), ["%*%"]);
        assert_eq!(patterns("the* beat"), ["%the*%", "%beat%"]);
        assert_eq!(patterns("m*a*s*h"), ["%m*a*s*h%"]);
        assert_eq!(patterns("* beat"), ["%*%", "%beat%"]);

        let db = library(&[
            song(1, "The Beatles", "Help!", "Yesterday"),
            song(2, "Various", "M*A*S*H", "Suicide Is Painless"),
        ]);
        assert_eq!(find(&db, "beat*"), find(&db, "beat"));
        assert_eq!(find(&db, "beat*").artists, ["The Beatles"]);
        assert_eq!(find(&db, "*"), find(&db, ""));
        // A `*` inside a query matches itself.
        assert_eq!(
            find(&db, "m*a*"),
            names(&[], &["M*A*S*H"], &["Suicide Is Painless"])
        );
        assert_eq!(
            find(&db, "**"),
            names(&[], &["M*A*S*H"], &["Suicide Is Painless"])
        );
        assert_eq!(find(&db, "b*t"), names(&[], &[], &[]));
    }

    #[test]
    fn longest_queries_run() {
        let db = catalogue();
        // 1024 characters that grow in lower case, that grow in NFC and that
        // are escaped, and the most terms, each as long as the limit allows.
        let many_terms: String = (0..32u32)
            .map(|n| {
                char::from_u32(0x4e00 + n)
                    .unwrap()
                    .to_string()
                    .repeat(MAX_QUERY_CHARS / 32 - 1)
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(many_terms.chars().count(), MAX_QUERY_CHARS - 1);
        for query in [
            "a".repeat(MAX_QUERY_CHARS),
            "\u{130}".repeat(MAX_QUERY_CHARS),
            "\u{1d160}".repeat(MAX_QUERY_CHARS),
            "%".repeat(MAX_QUERY_CHARS),
            "\u{10ffff}".repeat(MAX_QUERY_CHARS),
            many_terms,
        ] {
            let patterns = patterns(&query);
            assert!(
                patterns.iter().all(|pattern| pattern.len() < 13_000),
                "a pattern of {} bytes",
                patterns.iter().map(String::len).max().unwrap_or(0)
            );
            assert_eq!(find(&db, &query), names(&[], &[], &[]));
        }
        assert_eq!(patterns(&"\u{4e00} ".repeat(512)).len(), 1);
    }

    #[test]
    fn empty_library_has_no_match() {
        let db = Db::open_memory().unwrap();
        assert_eq!(find(&db, ""), names(&[], &[], &[]));
        assert_eq!(find(&db, "a"), names(&[], &[], &[]));
        assert_eq!(find_in(&db, "", Some(1)), names(&[], &[], &[]));
    }

    #[test]
    fn query_without_a_term_returns_every_row_in_name_order() {
        let db = catalogue();
        let everything = names(
            &[
                "Aurora Field",
                "Michael Jackson",
                "The Beatles",
                "Various Artists",
            ],
            &[
                "Abbey Road",
                "Glass Harbour",
                "Help!",
                "Summer Sampler",
                "Thriller",
            ],
            &[
                "Beat It",
                "Breakwater",
                "Come Together",
                "Low Tide",
                "Open Road",
                "Yesterday",
            ],
        );
        assert_eq!(find(&db, ""), everything);
        assert_eq!(find(&db, "\"\""), everything);
        assert_eq!(find(&db, "   "), everything);
    }

    #[test]
    fn term_matches_a_part_of_a_name_without_regard_to_case() {
        let db = catalogue();
        // An artist by its name. An album by its title or by the name of
        // its artist. A track by its title, by the title of its album or by
        // the name of its artist.
        let beat = names(
            &["The Beatles"],
            &["Abbey Road", "Help!"],
            &["Beat It", "Come Together", "Yesterday"],
        );
        assert_eq!(find(&db, "beat"), beat);
        assert_eq!(find(&db, "BEAT"), beat);
        assert_eq!(find(&db, "bEaT"), beat);
        assert_eq!(find(&db, "\"beat\""), beat);

        assert_eq!(
            find(&db, "harbour"),
            names(&[], &["Glass Harbour"], &["Breakwater", "Low Tide"])
        );
        // The end of one word and the start of the next are no match: the
        // term has no space.
        assert_eq!(find(&db, "sharbour"), names(&[], &[], &[]));
        assert_eq!(find(&db, "zzz"), names(&[], &[], &[]));
    }

    #[test]
    fn every_term_must_match_and_each_may_match_another_name() {
        let db = catalogue();
        // No artist name holds both terms. "Abbey Road" holds `road`, and
        // its artist holds `beat`.
        let both = names(&[], &["Abbey Road"], &["Come Together"]);
        assert_eq!(find(&db, "beat road"), both);
        // The order of the terms has no meaning.
        assert_eq!(find(&db, "road beat"), both);
        assert_eq!(find(&db, "  ROAD   Beat "), both);

        assert_eq!(
            find(&db, "the beatles"),
            names(
                &["The Beatles"],
                &["Abbey Road", "Help!"],
                &["Come Together", "Yesterday"]
            )
        );
        // Title and album, title and artist.
        assert_eq!(find(&db, "tide glass"), names(&[], &[], &["Low Tide"]));
        assert_eq!(
            find(&db, "yesterday beatles"),
            names(&[], &[], &["Yesterday"])
        );
        assert_eq!(find(&db, "beat zzz"), names(&[], &[], &[]));
        assert_eq!(find(&db, "yesterday thriller"), names(&[], &[], &[]));
    }

    #[test]
    fn track_matches_by_its_track_artist_and_by_its_album_artist() {
        let db = catalogue();
        // "Open Road" is by Tessa Lund on an album of Various Artists.
        // Tessa Lund has no artist row.
        assert_eq!(find(&db, "tessa"), names(&[], &[], &["Open Road"]));
        assert_eq!(find(&db, "TESSA lund"), names(&[], &[], &["Open Road"]));
        assert_eq!(
            find(&db, "various"),
            names(&["Various Artists"], &["Summer Sampler"], &["Open Road"])
        );
    }

    #[test]
    fn like_wildcards_in_a_query_match_themselves() {
        let db = library(&[
            song(1, "Per%cent", "100% Hits", "100% Pure"),
            song(2, "Percent", "100 Hits", "100 Miles"),
            song(3, "Under_Score", "Snake_Case", "a_b"),
            song(4, "UnderXScore", "SnakeXCase", "axb"),
            song(5, r"Back\Slash", r"C:\Music", r"a\b"),
            song(6, "Backslash", "C Music", "a b"),
        ]);
        assert_eq!(
            find(&db, "100%"),
            names(&[], &["100% Hits"], &["100% Pure"])
        );
        assert_eq!(
            find(&db, "%"),
            names(&["Per%cent"], &["100% Hits"], &["100% Pure"])
        );
        assert_eq!(find(&db, "%%"), names(&[], &[], &[]));
        assert_eq!(
            find(&db, "_"),
            names(&["Under_Score"], &["Snake_Case"], &["a_b"])
        );
        assert_eq!(find(&db, "a_b"), names(&[], &[], &["a_b"]));
        assert_eq!(find(&db, "__"), names(&[], &[], &[]));
        assert_eq!(
            find(&db, r"\"),
            names(&[r"Back\Slash"], &[r"C:\Music"], &[r"a\b"])
        );
        assert_eq!(find(&db, r"a\b"), names(&[], &[], &[r"a\b"]));
        // A backslash does not escape the character after it.
        assert_eq!(find(&db, r"\%"), names(&[], &[], &[]));
        assert_eq!(find(&db, r"\_"), names(&[], &[], &[]));
        assert_eq!(find(&db, r"per\%cent"), names(&[], &[], &[]));
        // A wildcard between two words is no wildcard either.
        assert_eq!(find(&db, "100%pure"), names(&[], &[], &[]));
        assert_eq!(
            find(&db, "snake_case"),
            names(&[], &["Snake_Case"], &["a_b"])
        );
        assert_eq!(
            find(&db, "snakexcase"),
            names(&[], &["SnakeXCase"], &["axb"])
        );
    }

    #[test]
    fn quotes_and_sql_text_in_a_query_are_text() {
        let db = library(&[
            song(1, "Guns N' Roses", "Don't Cry", "It's So Easy"),
            song(2, "The \"Quoted\" Band", "12\" Mixes", "Say \"Hi\""),
            song(
                3,
                "Robert'); DROP TABLE tracks;--",
                "x' OR '1'='1",
                "1; SELECT 1",
            ),
        ]);
        assert_eq!(
            find(&db, "n'"),
            names(&["Guns N' Roses"], &["Don't Cry"], &["It's So Easy"])
        );
        assert_eq!(find(&db, "it's"), names(&[], &[], &["It's So Easy"]));
        assert_eq!(find(&db, "''"), names(&[], &[], &[]));
        // A double quote of a name is not part of a term, and the name is
        // found by the text around it.
        assert_eq!(
            find(&db, "\"quoted\""),
            names(&["The \"Quoted\" Band"], &["12\" Mixes"], &["Say \"Hi\""])
        );
        assert_eq!(
            find(&db, "12\""),
            names(&[], &["12\" Mixes"], &["Say \"Hi\""])
        );

        for query in [
            "'; DROP TABLE tracks;--",
            "' OR '1'='1",
            "x' OR '1'='1",
            "\") OR 1=1 --",
            "'); DELETE FROM artists; --",
            "?1 ?2 :name @x $y",
            "\\' OR 1=1",
            "]}') --",
        ] {
            // The statement runs, and the tables are as they were.
            let found = find(&db, query);
            assert!(found.tracks.len() <= 1, "{query:?}: {found:?}");
            db.with_reader(|conn| {
                assert_eq!(tracks::count_tracks(conn)?, 3, "{query:?}");
                assert_eq!(artists::count(conn)?, 3, "{query:?}");
                assert_eq!(albums::count(conn)?, 3, "{query:?}");
                Ok(())
            })
            .unwrap();
        }
        assert_eq!(
            find(&db, "drop table"),
            names(
                &["Robert'); DROP TABLE tracks;--"],
                &["x' OR '1'='1"],
                &["1; SELECT 1"]
            )
        );
        assert_eq!(
            find(&db, "'1'='1"),
            names(&[], &["x' OR '1'='1"], &["1; SELECT 1"])
        );
        assert_eq!(
            find(&db, "tracks;--"),
            names(
                &["Robert'); DROP TABLE tracks;--"],
                &["x' OR '1'='1"],
                &["1; SELECT 1"]
            )
        );
    }

    #[test]
    fn names_outside_ascii_match_without_regard_to_case() {
        let db = library(&[
            song(
                1,
                "Édith Piaf",
                "La Vie en rose",
                "Non, je ne regrette rien",
            ),
            song(2, "Кино", "Группа крови", "Звезда по имени Солнце"),
            song(3, "坂本龍一", "音楽図鑑", "戦場のメリークリスマス"),
            song(4, "Björk", "Homogenic", "Jóga 🎵"),
        ]);
        let piaf = names(
            &["Édith Piaf"],
            &["La Vie en rose"],
            &["Non, je ne regrette rien"],
        );
        assert_eq!(find(&db, "édith"), piaf);
        assert_eq!(find(&db, "ÉDITH"), piaf);
        assert_eq!(find(&db, "Édith piaf"), piaf);
        // A decomposed spelling of the query is the same term.
        assert_eq!(find(&db, "e\u{301}dith"), piaf);

        let kino = names(&["Кино"], &["Группа крови"], &["Звезда по имени Солнце"]);
        assert_eq!(find(&db, "кино"), kino);
        assert_eq!(find(&db, "КИНО"), kino);
        assert_eq!(find(&db, "ин"), kino);
        assert_eq!(
            find(&db, "группа"),
            names(&[], &["Группа крови"], &["Звезда по имени Солнце"])
        );
        assert_eq!(
            find(&db, "СОЛНЦЕ звезда"),
            names(&[], &[], &["Звезда по имени Солнце"])
        );

        assert_eq!(
            find(&db, "龍一"),
            names(&["坂本龍一"], &["音楽図鑑"], &["戦場のメリークリスマス"])
        );
        assert_eq!(
            find(&db, "メリー"),
            names(&[], &[], &["戦場のメリークリスマス"])
        );
        assert_eq!(find(&db, "🎵"), names(&[], &[], &["Jóga 🎵"]));
        assert_eq!(find(&db, "JÓGA"), names(&[], &[], &["Jóga 🎵"]));
    }

    #[test]
    fn letter_with_a_diacritic_is_not_its_base_letter() {
        let db = library(&[
            song(1, "Édith Piaf", "La Vie en rose", "Milord"),
            song(2, "Edith Day", "Rio Rita", "Following the Sun Around"),
        ]);
        assert_eq!(
            find(&db, "edith"),
            names(&["Edith Day"], &["Rio Rita"], &["Following the Sun Around"])
        );
        assert_eq!(
            find(&db, "édith"),
            names(&["Édith Piaf"], &["La Vie en rose"], &["Milord"])
        );
        assert_eq!(
            find(&db, "dith"),
            names(
                &["Edith Day", "Édith Piaf"],
                &["La Vie en rose", "Rio Rita"],
                &["Following the Sun Around", "Milord"]
            )
        );
    }

    #[test]
    fn track_artist_outside_ascii_matches_in_the_case_of_its_tag() {
        // The track artists have no artist row: the album artist is Various
        // Artists. `tracks.artist_name` holds the spelling of the tag, and
        // `LIKE` ignores the case of ASCII letters only.
        let db = library(&[
            Spec {
                artist: "Ólafur Arnalds",
                album_artist: "Various Artists",
                ..song(1, "", "Northern Lights", "Near Light")
            },
            Spec {
                artist: "sigur rós",
                album_artist: "Various Artists",
                ..song(2, "", "Northern Lights", "Hoppípolla")
            },
        ]);
        assert_eq!(find(&db, "ARNALDS").tracks, ["Near Light"]);
        assert_eq!(find(&db, "lafur").tracks, ["Near Light"]);
        assert_eq!(find(&db, "ólafur").tracks, [] as [&str; 0]);
        assert_eq!(find(&db, "Ólafur").tracks, [] as [&str; 0]);
        // A tag in lower case matches every spelling of the term.
        assert_eq!(find(&db, "rós").tracks, ["Hoppípolla"]);
        assert_eq!(find(&db, "RÓS").tracks, ["Hoppípolla"]);
        // The title is compared in lower case.
        assert_eq!(find(&db, "HOPPÍPOLLA").tracks, ["Hoppípolla"]);
    }

    #[test]
    fn decomposed_spelling_in_a_tag_matches_through_a_key_only() {
        // `e` and U+0301 in the tags, where a term has U+00E9. The keys of
        // the album and of the artist are in NFC. The title and the track
        // artist of a track are in the form of the tag.
        let db = library(&[
            song(1, "Cafe\u{301} Tacvba", "Re\u{301}", "El Baile"),
            Spec {
                artist: "Beyonce\u{301}",
                album_artist: "Various Artists",
                ..song(2, "", "Sampler", "De\u{301}ja\u{300} Vu")
            },
        ]);
        assert_eq!(
            find(&db, "caf\u{e9}"),
            names(&["Cafe\u{301} Tacvba"], &["Re\u{301}"], &["El Baile"])
        );
        assert_eq!(
            find(&db, "r\u{e9}"),
            names(&[], &["Re\u{301}"], &["El Baile"])
        );
        // The title and the track artist of the second track.
        assert_eq!(find(&db, "d\u{e9}j\u{e0}"), names(&[], &[], &[]));
        assert_eq!(find(&db, "beyonc\u{e9}"), names(&[], &[], &[]));
        // The base letters are part of the decomposed text.
        assert_eq!(find(&db, "beyonce").tracks, ["De\u{301}ja\u{300} Vu"]);
        assert_eq!(find(&db, "vu").tracks, ["De\u{301}ja\u{300} Vu"]);
    }

    #[test]
    fn rows_with_one_name_are_in_the_order_in_which_they_were_stored() {
        let db = library(&[
            song(9, "Zed", "Greatest Hits", "Intro"),
            song(3, "Abel", "Greatest Hits", "Intro"),
            song(7, "Mona", "greatest hits", "INTRO"),
            song(1, "Mona", "Aardvark", "intro"),
            song(5, "Abel", "Greatest Hits", "Outro"),
        ]);
        let ids = |query: &str, offset: u32, count: u32| {
            let terms = terms(query);
            db.with_reader(|conn| {
                let albums = search_albums(conn, &terms, count, offset, None)?
                    .into_iter()
                    .map(|album| format!("{} / {}", album.artist_name, album.name))
                    .collect::<Vec<_>>();
                let tracks = search_tracks(conn, &terms, count, offset, None)?
                    .into_iter()
                    .map(|song| song.track.id)
                    .collect::<Vec<_>>();
                Ok((albums, tracks))
            })
            .unwrap()
        };
        let (albums, tracks) = ids("", 0, ALL);
        assert_eq!(
            albums,
            [
                "Mona / Aardvark",
                "Zed / Greatest Hits",
                "Abel / Greatest Hits",
                "Mona / greatest hits",
            ]
        );
        assert_eq!(
            tracks,
            [
                track_id(9),
                track_id(3),
                track_id(7),
                track_id(1),
                track_id(5)
            ]
        );
        // Every page is a part of that order.
        for size in 1..=5 {
            let mut paged_albums = Vec::new();
            let mut paged_tracks = Vec::new();
            for page in 0..5 {
                let (albums, tracks) = ids("", page * size, size);
                paged_albums.extend(albums);
                paged_tracks.extend(tracks);
            }
            assert_eq!(paged_albums, albums, "pages of {size}");
            assert_eq!(paged_tracks, tracks, "pages of {size}");
        }
        // A search with a term has the same order.
        let (albums, tracks) = ids("greatest", 0, ALL);
        assert_eq!(
            albums,
            [
                "Zed / Greatest Hits",
                "Abel / Greatest Hits",
                "Mona / greatest hits",
            ]
        );
        assert_eq!(tracks, [track_id(9), track_id(3), track_id(7), track_id(5)]);
    }

    #[test]
    fn count_and_offset_select_a_part_of_the_matches() {
        let db = catalogue();
        let page = |count: u32, offset: u32| {
            let terms = terms("");
            db.with_reader(|conn| {
                Ok(Names {
                    artists: search_artists(conn, &terms, count, offset, None)?
                        .into_iter()
                        .map(|artist| artist.name)
                        .collect(),
                    albums: search_albums(conn, &terms, count, offset, None)?
                        .into_iter()
                        .map(|album| album.name)
                        .collect(),
                    tracks: search_tracks(conn, &terms, count, offset, None)?
                        .into_iter()
                        .map(|song| song.track.title)
                        .collect(),
                })
            })
            .unwrap()
        };
        assert_eq!(
            page(2, 0),
            names(
                &["Aurora Field", "Michael Jackson"],
                &["Abbey Road", "Glass Harbour"],
                &["Beat It", "Breakwater"]
            )
        );
        assert_eq!(
            page(2, 3),
            names(
                &["Various Artists"],
                &["Summer Sampler", "Thriller"],
                &["Low Tide", "Open Road"]
            )
        );
        assert_eq!(page(0, 0), names(&[], &[], &[]));
        assert_eq!(
            page(ALL, 4),
            names(&[], &["Thriller"], &["Open Road", "Yesterday"])
        );
        assert_eq!(page(ALL, 6), names(&[], &[], &[]));
        assert_eq!(page(u32::MAX, u32::MAX), names(&[], &[], &[]));
        assert_eq!(page(u32::MAX, 0), find(&db, ""));
    }

    #[test]
    fn music_folder_limits_the_matches_to_what_has_a_file_in_it() {
        let other = |spec: Spec| Spec {
            folder: "/other",
            ..spec
        };
        let db = library(&[
            song(1, "Aurora Field", "Glass Harbour", "Low Tide"),
            other(song(2, "Aurora Field", "Glass Harbour", "Breakwater")),
            other(song(3, "Marlow Quartet", "Slow Rivers", "Upstream")),
            song(4, "The Beatles", "Help!", "Yesterday"),
        ]);
        let music = folder_id(&db, "/music");
        let other = folder_id(&db, "/other");
        assert_eq!(
            find_in(&db, "", Some(music)),
            names(
                &["Aurora Field", "The Beatles"],
                &["Glass Harbour", "Help!"],
                &["Low Tide", "Yesterday"]
            )
        );
        assert_eq!(
            find_in(&db, "", Some(other)),
            names(
                &["Aurora Field", "Marlow Quartet"],
                &["Glass Harbour", "Slow Rivers"],
                &["Breakwater", "Upstream"]
            )
        );
        assert_eq!(
            find_in(&db, "aurora", Some(other)),
            names(&["Aurora Field"], &["Glass Harbour"], &["Breakwater"])
        );
        assert_eq!(find_in(&db, "beatles", Some(other)), names(&[], &[], &[]));
        // No folder has these IDs.
        for unknown in [0, -1, other + 1, i64::MAX, i64::MIN] {
            assert_eq!(
                find_in(&db, "", Some(unknown)),
                names(&[], &[], &[]),
                "{unknown}"
            );
        }
    }

    #[test]
    fn track_without_a_file_is_a_match_and_is_in_no_music_folder() {
        let db = library(&[
            song(1, "Aurora Field", "Glass Harbour", "Low Tide"),
            song(2, "Marlow Quartet", "Slow Rivers", "Upstream"),
        ]);
        remove_file(&db, 2);
        let music = folder_id(&db, "/music");

        let everything = names(
            &["Aurora Field", "Marlow Quartet"],
            &["Glass Harbour", "Slow Rivers"],
            &["Low Tide", "Upstream"],
        );
        assert_eq!(find(&db, ""), everything);
        assert_eq!(find(&db, "upstream"), names(&[], &[], &["Upstream"]));
        assert_eq!(
            find(&db, "marlow"),
            names(&["Marlow Quartet"], &["Slow Rivers"], &["Upstream"])
        );
        // With a music folder the track is left out, and so are its album
        // and its artist, which have no other track with a file there.
        assert_eq!(
            find_in(&db, "", Some(music)),
            names(&["Aurora Field"], &["Glass Harbour"], &["Low Tide"])
        );
        assert_eq!(find_in(&db, "marlow", Some(music)), names(&[], &[], &[]));
        // The placeholder path is no name: no term matches it.
        assert_eq!(find(&db, "tr-").tracks, [] as [&str; 0]);
        assert_eq!(find(&db, "/").tracks, [] as [&str; 0]);
    }

    /// The `detail` column of `EXPLAIN QUERY PLAN` for `sql` with the
    /// parameters of a search.
    fn plan(conn: &Connection, sql: &str, patterns: &str, folder: Option<i64>) -> Vec<String> {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map((patterns, 500, 1000, folder), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Checks the plan of each of the three statements, for a query without
    /// a term and with terms, with and without a music folder:
    ///
    /// - The rows of the result are read in the order of the index of the
    ///   name column, and no step sorts.
    /// - `tracks` is read in no other way than the one named here: by the
    ///   index of the titles for the tracks, and by the index of the artist
    ///   or of the album for the test of the music folder.
    /// - The artist of an album is found by its ID.
    fn assert_plans_use_the_indexes(conn: &Connection) {
        let cases = [
            (
                artists_sql(),
                "SCAN artists USING INDEX sqlite_autoindex_artists_2",
                "SEARCH tracks USING INDEX tracks_artist (artist_id=?)",
                None,
            ),
            (
                albums_sql(),
                "SCAN albums USING INDEX albums_key",
                "SEARCH tracks USING INDEX tracks_album (album_id=?)",
                Some("SEARCH artists USING INDEX sqlite_autoindex_artists_1 (id=?)"),
            ),
            (
                tracks_sql(),
                "SCAN tracks USING INDEX tracks_title_lc",
                "SCAN tracks USING INDEX tracks_title_lc",
                None,
            ),
        ];
        for (sql, rows, tracks, artist_of_album) in cases {
            for patterns in ["[]", r#"["%a%"]"#, r#"["%a%","%b%","%c%"]"#] {
                for folder in [None, Some(1)] {
                    let plan = plan(conn, &sql, patterns, folder);
                    let context = format!("{patterns} {folder:?}: {plan:#?}");
                    assert!(plan.iter().any(|step| step == rows), "{context}");
                    assert!(
                        !plan.iter().any(|step| step.contains("B-TREE")),
                        "{context}"
                    );
                    let reads_of_tracks: Vec<&String> = plan
                        .iter()
                        .filter(|step| step.contains(" tracks"))
                        .collect();
                    assert_eq!(reads_of_tracks, [tracks], "{context}");
                    if let Some(lookup) = artist_of_album {
                        assert!(plan.iter().any(|step| step == lookup), "{context}");
                    }
                }
            }
        }
    }

    #[test]
    fn statements_read_the_name_indexes_in_order_and_sort_nothing() {
        let db = catalogue();
        db.with_reader(|conn| {
            assert_plans_use_the_indexes(conn);
            Ok(())
        })
        .unwrap();
    }

    /// Fills a database with 2 100 artists, 8 400 albums and 100 000 tracks
    /// in two music folders, and checks the plans, the paging and the time
    /// of the searches. It prints the times.
    ///
    /// `cargo test --release --lib search::tests::library_of_100_000 -- --ignored --nocapture`
    #[test]
    #[ignore = "builds a library of 100 000 tracks"]
    fn library_of_100_000_tracks_is_paged_through_the_name_indexes() {
        const TRACKS: usize = 100_000;
        let db = Db::open_memory().unwrap();
        db.with_writer(|conn| {
            conn.execute_batch(
                "INSERT INTO folders (id, path, name)
                 VALUES (1, '/music', 'music'), (2, '/other', 'other');
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2100)
                 INSERT INTO artists (id, key, name, sort_name, album_count)
                 SELECT printf('ar-%032x', i), printf('artist x%dx', i), printf('Artist X%dX', i),
                        printf('Artist X%dX', i), 4 FROM n;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 8400)
                 INSERT INTO albums (id, artist_id, key, name, artist_name, song_count, created_at)
                 SELECT printf('al-%032x', i), printf('ar-%032x', (i - 1) / 4 + 1),
                        printf('album y%dy', i % 500), printf('Album Y%dY', i % 500),
                        printf('Artist X%dX', (i - 1) / 4 + 1), 12, 1 FROM n;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
                 INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc,
                   album_id, artist_id, artist_name, album_name, duration_ms, size, suffix,
                   content_type, lossless, mtime, created_at, updated_at)
                 SELECT printf('tr-%032x', i), 1 + (i % 10 = 0), printf('%d.flac', i), '1:aa',
                        printf('Song Z%dZ', i * 7919 % 3000), printf('song z%dz', i * 7919 % 3000),
                        printf('al-%032x', (i - 1) / 12 + 1), printf('ar-%032x', (i - 1) / 48 + 1),
                        printf('Artist X%dX', (i - 1) / 48 + 1),
                        printf('Album Y%dY', ((i - 1) / 12 + 1) % 500),
                        1000, 100, 'flac', 'audio/flac', 1, 1, 1, 1 FROM n;",
            )?;
            Ok(())
        })
        .unwrap();

        db.with_reader(|conn| {
            assert_eq!(tracks::count_tracks(conn)?, TRACKS as u64);
            assert_plans_use_the_indexes(conn);

            // Every track is on one page, whatever the size of the pages.
            let everything = terms("");
            for size in [500, 137] {
                let started = Instant::now();
                let mut ids = HashSet::new();
                let mut titles = Vec::new();
                let mut offset = 0;
                loop {
                    let page = search_tracks(conn, &everything, size, offset, None)?;
                    let full = page.len() == size as usize;
                    for song in page {
                        titles.push(song.track.title.to_lowercase());
                        assert!(ids.insert(song.track.id), "a track is on two pages");
                    }
                    offset += size;
                    if !full {
                        break;
                    }
                }
                assert_eq!(ids.len(), TRACKS);
                assert!(titles.is_sorted(), "the pages are in title order");
                println!(
                    "every track in pages of {size}: {:?} for {} pages",
                    started.elapsed(),
                    offset / size
                );
            }

            let time = |label: &str, query: &str, offset: u32, folder: Option<i64>| {
                let terms = terms(query);
                let started = Instant::now();
                let artists = search_artists(conn, &terms, 500, offset, folder)?.len();
                let artists_time = started.elapsed();
                let started = Instant::now();
                let albums = search_albums(conn, &terms, 500, offset, folder)?.len();
                let albums_time = started.elapsed();
                let started = Instant::now();
                let tracks = search_tracks(conn, &terms, 500, offset, folder)?.len();
                let tracks_time = started.elapsed();
                println!(
                    "{label}: {artists} artists in {artists_time:?}, {albums} albums in \
                     {albums_time:?}, {tracks} tracks in {tracks_time:?}"
                );
                Ok::<_, crate::db::DbError>((artists, albums, tracks))
            };
            assert_eq!(
                time("first page of everything", "", 0, None)?,
                (500, 500, 500)
            );
            assert_eq!(
                time("last page of the tracks", "", 99_500, None)?,
                (0, 0, 500)
            );
            assert_eq!(
                time("last page of folder 1", "", 89_500, Some(1))?,
                (0, 0, 500)
            );
            assert_eq!(
                time("a term that matches nothing", "zzzz", 0, None)?,
                (0, 0, 0)
            );
            assert_eq!(
                time(
                    "three terms that match every track",
                    "song artist album",
                    99_500,
                    None
                )?,
                (0, 0, 500)
            );
            assert_eq!(
                time("two terms that match one artist", "artist x42x", 0, None)?,
                (1, 4, 48)
            );
            let broad: String = "abcdefghijklmnopqrstuvwxyz012345"
                .chars()
                .map(|letter| format!("{letter} "))
                .collect();
            time("32 terms of one character", &broad, 0, None)?;
            // The most terms. 31 are parts of `song`, `artist` and `album`,
            // and each matches every track: 15 of them through the album
            // only. The last term matches none, so every term is tested for
            // every track.
            let every = "s o n g so on ng son ong song z a r t i x l b u m al lb bu um alb lbu \
                         bum albu lbum album y qqq";
            assert_eq!(
                terms(every).patterns.map(|patterns| patterns.len()),
                Some(32)
            );
            assert_eq!(
                time(
                    "31 terms that match every track and one that matches none",
                    every,
                    0,
                    None
                )?,
                (0, 0, 0)
            );
            Ok(())
        })
        .unwrap();
    }
}
