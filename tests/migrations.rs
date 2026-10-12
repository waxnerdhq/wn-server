//! Tests of the schema migrations through the public interface. A library
//! that a scan stored at schema version 1 is migrated by `Db::open` and
//! scanned again. The server and a command report a database that they
//! cannot migrate and a database of a newer schema version. The times of
//! the scans of a database of schema version 2 are read over the router
//! after the upgrade.
//!
//! The unit tests in `src/db/mod.rs` cover the migration runner with rows
//! that the tests write.
//!
//! A database of schema version 1 is made with the text of the first
//! migration, and one of schema version 2 with the texts of the first two.
//! Those files do not change, so these databases are the ones that a build
//! with one migration and a build with two migrations left on disk.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::{LibraryBuilder, TestServer};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, TagExt};
use rusqlite::Connection;
use rusqlite::types::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wn_server::cli::PASSWORD_ENV;
use wn_server::config::{Config, DATA_DIR_ENV, DataDir};
use wn_server::db::Db;
use wn_server::db::queries::scans::{self, ScanCounts};
use wn_server::library::scanner::{self, ScanOptions, ScanSummary};
use wn_server::library::tags::VARIOUS_ARTISTS;
use wn_server::server::{ServerError, ServerOptions, run_server};

/// The schema of version 1: the text of the first migration.
const V1_SCHEMA: &str = include_str!("../migrations/0001_init.sql");

/// The text of the second migration, which brings a database to schema
/// version 2.
const V2_MIGRATION: &str = include_str!("../migrations/0002_tracks_album_artist_name.sql");

/// The names of the files of the second and the third migration.
const SECOND: &str = "0002_tracks_album_artist_name.sql";
const THIRD: &str = "0003_scans_milliseconds_and_drop_has_lyrics.sql";

/// The name that [`dump`] gives the rowid of a row.
const ROWID: &str = "_rowid";

/// The tables whose rows a user or a scan attaches to the tracks, the albums
/// and the artists.
const ATTACHED: [&str; 6] = [
    "annotations",
    "play_queue",
    "play_queue_tracks",
    "playlist_tracks",
    "playlists",
    "users",
];

/// The rows of every table by the name of the table: for each row in rowid
/// order, the rowid under the name [`ROWID`], and the name and the value of
/// every column.
type Dump = BTreeMap<String, Vec<Vec<(String, Value)>>>;

fn table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    conn.prepare(
        "SELECT name FROM main.sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?
    .query_map([], |row| row.get(0))?
    .collect()
}

fn dump(conn: &Connection) -> rusqlite::Result<Dump> {
    let mut tables = Dump::new();
    for table in table_names(conn)? {
        let mut select = conn.prepare(&format!(
            "SELECT rowid AS {ROWID}, * FROM main.{table} ORDER BY rowid"
        ))?;
        let names: Vec<String> = select
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let rows = select
            .query_map([], |row| {
                names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| Ok((name.clone(), row.get(index)?)))
                    .collect()
            })?
            .collect::<rusqlite::Result<_>>()?;
        tables.insert(table, rows);
    }
    Ok(tables)
}

fn dump_of(db: &Db) -> Dump {
    db.with_reader(|conn| Ok(dump(conn)?)).unwrap()
}

fn user_version(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

/// Returns the schema version that this build gives a new database.
fn latest_version() -> i64 {
    Db::open_memory()
        .unwrap()
        .with_reader(|conn| Ok(user_version(conn)))
        .unwrap()
}

/// Opens a new database file at `path` in WAL mode and gives it the schema
/// of version 1, as `Db::open` of a build with one migration did.
fn create_at_version_1(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    conn.execute_batch(V1_SCHEMA).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn
}

/// Opens a new database file at `path` in WAL mode and gives it the schema
/// of version 2, as `Db::open` of a build with two migrations did.
fn create_at_version_2(path: &Path) -> Connection {
    let conn = create_at_version_1(path);
    conn.execute_batch(V2_MIGRATION).unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    conn
}

/// Returns the values of the columns `columns` of every row of `table`.
fn values(dump: &Dump, table: &str, columns: &[&str]) -> Vec<Vec<Value>> {
    dump[table]
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| {
                    let found = row.iter().find(|(name, _)| name == column);
                    found
                        .unwrap_or_else(|| panic!("{table} has no column {column}"))
                        .1
                        .clone()
                })
                .collect()
        })
        .collect()
}

/// Returns `dump` with the columns that `reference` has, in the order of
/// `reference`. A table or a column that `dump` lacks is a failure.
fn columns_of(dump: &Dump, reference: &Dump) -> Dump {
    reference
        .iter()
        .map(|(table, rows)| {
            let columns: Vec<&str> = rows[0].iter().map(|(name, _)| name.as_str()).collect();
            let kept = values(dump, table, &columns)
                .into_iter()
                .map(|row| {
                    let names = columns.iter().map(|name| (*name).to_owned());
                    names.zip(row).collect()
                })
                .collect();
            (table.clone(), kept)
        })
        .collect()
}

/// What identifies the rows of the library and ties them to each other and
/// to their files.
fn identities(dump: &Dump) -> [Vec<Vec<Value>>; 3] {
    [
        values(dump, "artists", &["id", "key"]),
        values(dump, "albums", &["id", "artist_id", "key"]),
        values(
            dump,
            "tracks",
            &["id", "artist_id", "album_id", "folder_id", "rel_path"],
        ),
    ]
}

/// The rows of the tables of [`ATTACHED`].
fn attached(dump: &Dump) -> Vec<&Vec<Vec<(String, Value)>>> {
    ATTACHED.iter().map(|table| &dump[*table]).collect()
}

/// Returns `dump` without the table `scans`, which gets a row with every
/// scan.
fn without_scans(dump: &Dump) -> Dump {
    let mut dump = dump.clone();
    dump.remove("scans");
    dump
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.clone()),
        Value::Null => None,
        other => panic!("{other:?} is no text"),
    }
}

/// Returns the album artist that each track row holds, by the relative path
/// of the track.
fn album_artists(dump: &Dump) -> BTreeMap<String, Option<String>> {
    values(dump, "tracks", &["rel_path", "album_artist_name"])
        .iter()
        .map(|row| (text(&row[0]).expect("a path"), text(&row[1])))
        .collect()
}

/// Returns the name of every artist, in the order of the keys.
fn artist_names(dump: &Dump) -> Vec<String> {
    let mut rows = values(dump, "artists", &["key", "name"]);
    rows.sort_by_key(|row| text(&row[0]));
    rows.iter().filter_map(|row| text(&row[1])).collect()
}

/// Returns the artist name that every album row shows, in the order of the
/// album keys.
fn album_artist_names(dump: &Dump) -> Vec<String> {
    let mut rows = values(dump, "albums", &["key", "artist_name"]);
    rows.sort_by_key(|row| text(&row[0]));
    rows.iter().filter_map(|row| text(&row[1])).collect()
}

fn scan(db: &Db, root: &Path, full: bool) -> ScanSummary {
    scanner::scan(
        db,
        &[root.to_path_buf()],
        &CancellationToken::new(),
        full,
        ScanOptions::default(),
    )
    .expect("the scan returns a summary")
}

/// Gives the library what a user attaches to it: an owner account, a star,
/// a rating and a play count on the first track, a star and a rating on its
/// album, a star on its artist, a playlist of three entries and a play
/// queue of two.
fn attach(db: &Db) {
    db.with_writer(|conn| {
        conn.execute_batch(
            "INSERT INTO users (id, name, role, password, created_at, updated_at)
             VALUES (1, 'owner', 'owner', 'sesame', 10, 10);
             INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating,
               play_count, last_played_at)
             SELECT 1, id, 'track', 100, 5, 7, 200 FROM tracks ORDER BY rel_path LIMIT 1;
             INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating)
             SELECT 1, album_id, 'album', 101, 4 FROM tracks ORDER BY rel_path LIMIT 1;
             INSERT INTO annotations (user_id, item_id, item_type, starred_at)
             SELECT 1, artist_id, 'artist', 102 FROM tracks ORDER BY rel_path LIMIT 1;
             INSERT INTO playlists (id, owner_id, name, comment, public, created_at, updated_at)
             VALUES ('pl-00000000000000000000000000000001', 1, 'Mix', 'for the road', 1, 20, 30);
             INSERT INTO playlist_tracks (playlist_id, position, track_id)
             SELECT 'pl-00000000000000000000000000000001',
               row_number() OVER (ORDER BY rel_path DESC) - 1, id
             FROM tracks ORDER BY rel_path DESC LIMIT 3;
             INSERT INTO play_queue (user_id, current_id, position_ms, changed_at, changed_by)
             SELECT 1, id, 1500, 40, 'a client' FROM tracks ORDER BY rel_path LIMIT 1;
             INSERT INTO play_queue_tracks (user_id, position, track_id)
             SELECT 1, row_number() OVER (ORDER BY rel_path) - 1, id
             FROM tracks ORDER BY rel_path LIMIT 2;",
        )?;
        Ok(())
    })
    .unwrap();
}

/// Copies every row of the tables `tables` of the attached database `from`
/// into the tables of the same names of the main database of `conn`, in the
/// columns that both databases have. A column that only the main database
/// has gets its default.
fn copy_rows(conn: &Connection, from: &str, tables: &[String]) {
    // A row can be stored before the row that it refers to. The references
    // are checked when the transaction commits.
    conn.execute_batch("BEGIN; PRAGMA defer_foreign_keys = ON")
        .unwrap();
    for table in tables {
        let columns: Vec<String> = conn
            .prepare(
                "SELECT name FROM pragma_table_info(?1, 'main')
                 WHERE name IN (SELECT name FROM pragma_table_info(?1, ?2))",
            )
            .unwrap()
            .query_map([table.as_str(), from], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let columns = columns.join(", ");
        conn.execute(
            &format!("INSERT INTO main.{table} ({columns}) SELECT {columns} FROM {from}.{table}"),
            [],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
}

/// Writes a database of schema version 1 to `target` that holds the rows of
/// the database at `scanned`, which this build wrote, in the columns of
/// version 1. The times of the scans are stored in whole seconds, which is
/// their unit at that version.
fn store_at_version_1(scanned: &Path, target: &Path) {
    let conn = create_at_version_1(target);
    conn.execute(
        "ATTACH DATABASE ?1 AS scanned",
        [scanned.to_str().expect("a Unicode path")],
    )
    .unwrap();
    copy_rows(&conn, "scanned", &table_names(&conn).unwrap());
    conn.execute_batch(
        "DETACH DATABASE scanned;
         UPDATE scans SET started_at = started_at / 1000, finished_at = finished_at / 1000;",
    )
    .unwrap();
}

/// Returns what the migrations after the first one make of the rows `v1` of
/// a database of schema version 1, in the columns of that version that the
/// latest schema has: a track row is without `has_lyrics`, and the times of
/// a scan are multiplied by 1000. Every other value is the one of `v1`.
fn kept_from_version_1(v1: &Dump) -> Dump {
    let mut kept = v1.clone();
    for row in kept.get_mut("tracks").expect("the table of the tracks") {
        row.retain(|(name, _)| name != "has_lyrics");
    }
    for row in kept.get_mut("scans").expect("the table of the scans") {
        for (name, value) in row {
            if let ("started_at" | "finished_at", Value::Integer(time)) = (name.as_str(), value) {
                *time *= 1000;
            }
        }
    }
    kept
}

/// Sets the title tag of an audio file and gives the file a later
/// modification time than it had, so that a scan sees the change.
fn retitle(path: &Path, title: &str) {
    let before = fs::metadata(path).unwrap().modified().unwrap();
    let mut file = lofty::read_from_path(path).unwrap();
    let tag = file.primary_tag_mut().expect("a primary tag");
    tag.insert_text(ItemKey::TrackTitle, title.to_owned());
    tag.save_to_path(path, WriteOptions::default()).unwrap();
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(before + Duration::from_secs(5)).unwrap();
}

/// Fails every statement on the write connection, which the scanner uses,
/// that writes a row of the library or a row that is attached to it.
fn forbid_writes(db: &Db) {
    db.with_writer(|conn| {
        for table in ["tracks", "albums", "artists"].iter().chain(&ATTACHED) {
            for (event, verb) in [
                ("INSERT", "inserted"),
                ("UPDATE", "updated"),
                ("DELETE", "deleted"),
            ] {
                conn.execute_batch(&format!(
                    "CREATE TEMP TRIGGER no_{table}_{verb} BEFORE {event} ON {table}
                     BEGIN SELECT RAISE(ABORT, 'a row of {table} was {verb}'); END;"
                ))?;
            }
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_library_scanned_at_schema_version_1_keeps_its_ids_and_what_is_attached_to_them() {
    // The first artist is spelled in one way in the album artist tags and in
    // another way in the track artist tags. The fourth file has no album
    // artist tag, and the fifth has the compilation flag.
    let library = LibraryBuilder::new()
        .track("AURORA FIELD", "Glass Harbour", "Low Tide", 1, "flac")
        .album_artist("Aurora Field")
        .track("AURORA FIELD", "Glass Harbour", "Breakwater", 2, "flac")
        .album_artist("Aurora Field")
        .track("AURORA FIELD", "Paper Maps", "Meridian", 1, "mp3")
        .album_artist("Aurora Field")
        .track("Tessa Lund", "Open Road", "Etude", 1, "ogg")
        .track("Nadia Ortiz", "Summer Sampler", "Last Train", 1, "opus")
        .compilation()
        .build();
    let root = library.root.path();
    let dir = TempDir::new().unwrap();

    // This build scans the library and a user attaches rows to it. The rows
    // are then stored in a database of schema version 1.
    let scanned_path = dir.path().join("scanned.db");
    let db = Db::open(&scanned_path).unwrap();
    assert_eq!(scan(&db, root, false).added, 5);
    attach(&db);
    drop(db);
    let path = dir.path().join("library.db");
    store_at_version_1(&scanned_path, &path);
    let v1 = Connection::open(&path).unwrap();
    // At schema version 1 a scan took the name of an artist from the track
    // artist tags of its files.
    v1.execute_batch(
        "UPDATE artists SET name = 'AURORA FIELD', sort_name = 'AURORA FIELD'
         WHERE key = 'aurora field';
         UPDATE albums SET artist_name = 'AURORA FIELD' WHERE artist_name = 'Aurora Field';",
    )
    .unwrap();
    // A build of schema version 1 could store 1 in the column that the
    // third migration removes.
    v1.execute_batch(
        "UPDATE tracks SET has_lyrics = 1
         WHERE rel_path = 'AURORA FIELD/Glass Harbour/01 Low Tide.flac';",
    )
    .unwrap();
    assert_eq!(user_version(&v1), 1);
    let before = dump(&v1).unwrap();
    drop(v1);
    assert_eq!(before.len(), 11);
    for (table, rows) in &before {
        assert!(!rows.is_empty(), "{table} has no row");
    }
    let lyrics = values(&before, "tracks", &["has_lyrics"]);
    assert_eq!(
        lyrics
            .concat()
            .iter()
            .filter(|value| **value == Value::Integer(1))
            .count(),
        1
    );
    // The scan of this build ended at a time of this century, which the
    // file of schema version 1 holds in seconds.
    let ends = values(&before, "scans", &["started_at", "finished_at"]);
    for end in ends.concat() {
        let Value::Integer(seconds) = end else {
            panic!("{end:?} is no time");
        };
        assert!(
            (1_700_000_000..4_000_000_000).contains(&seconds),
            "{seconds}"
        );
    }
    assert!(
        before["tracks"][0]
            .iter()
            .all(|(name, _)| name != "album_artist_name")
    );
    assert_eq!(
        artist_names(&before),
        ["AURORA FIELD", "Tessa Lund", VARIOUS_ARTISTS]
    );

    // `Db::open` migrates the file. Every row has its rowid and the values
    // it had, except that the times of the scan are in milliseconds and no
    // track row has `has_lyrics`. No row has an album artist, and the scan
    // counts as one that changed the library.
    let db = Db::open(&path).unwrap();
    let migrated = dump_of(&db);
    let version = db.with_reader(|conn| Ok(user_version(conn))).unwrap();
    assert_eq!(version, latest_version());
    assert!(version > 1);
    let kept = kept_from_version_1(&before);
    assert_ne!(kept, before);
    assert_eq!(columns_of(&migrated, &kept), kept);
    assert!(
        migrated["tracks"][0]
            .iter()
            .all(|(name, _)| name != "has_lyrics")
    );
    assert_eq!(
        values(&migrated, "scans", &["changed_library"]),
        [[Value::Integer(1)]]
    );
    assert_eq!(album_artists(&migrated).len(), 5);
    assert!(album_artists(&migrated).values().all(Option::is_none));

    // A scan that finds no file changed reads no file. No row changes: the
    // rows without an album artist carry their track artist, which is the
    // name that the artist row has.
    let unchanged = ScanSummary {
        seen: 5,
        albums: 4,
        artists: 3,
        ..ScanSummary::default()
    };
    assert_eq!(scan(&db, root, false), unchanged);
    assert_eq!(without_scans(&dump_of(&db)), without_scans(&migrated));

    // A file changes. The scan reads it, and its row gets its album artist.
    // The other rows of the artist carry the name of the row, so it stays.
    retitle(&library.files[0], "High Tide");
    let summary = scan(&db, root, false);
    assert_eq!((summary.changed, summary.tags_read), (1, 1));
    let one_read = dump_of(&db);
    let low_tide = "AURORA FIELD/Glass Harbour/01 Low Tide.flac";
    for (rel_path, album_artist) in album_artists(&one_read) {
        let expected = (rel_path == low_tide).then(|| "Aurora Field".to_owned());
        assert_eq!(album_artist, expected, "{rel_path}");
    }
    assert_eq!(artist_names(&one_read), artist_names(&before));
    assert_eq!(identities(&one_read), identities(&migrated));
    assert_eq!(attached(&one_read), attached(&migrated));

    // A full scan reads every file, and every row gets its album artist: the
    // album artist tag, the track artist of the file without that tag, and
    // the name of the compilations. No track carries the name of the first
    // artist row now, so the row takes the spelling of the album artist
    // tags. Every row keeps its ID, and the attached rows are as they were.
    let summary = scan(&db, root, true);
    assert_eq!((summary.changed, summary.tags_read), (5, 5));
    let filled = dump_of(&db);
    let expected = [
        (
            "AURORA FIELD/Glass Harbour/01 Low Tide.flac",
            "Aurora Field",
        ),
        (
            "AURORA FIELD/Glass Harbour/02 Breakwater.flac",
            "Aurora Field",
        ),
        ("AURORA FIELD/Paper Maps/01 Meridian.mp3", "Aurora Field"),
        (
            "Nadia Ortiz/Summer Sampler/01 Last Train.opus",
            VARIOUS_ARTISTS,
        ),
        ("Tessa Lund/Open Road/01 Etude.ogg", "Tessa Lund"),
    ]
    .map(|(rel_path, album_artist)| (rel_path.to_owned(), Some(album_artist.to_owned())));
    assert_eq!(album_artists(&filled), BTreeMap::from(expected));
    assert_eq!(
        artist_names(&filled),
        ["Aurora Field", "Tessa Lund", VARIOUS_ARTISTS]
    );
    assert_eq!(
        album_artist_names(&filled),
        [
            "Aurora Field",
            "Tessa Lund",
            "Aurora Field",
            VARIOUS_ARTISTS
        ]
    );
    assert_eq!(identities(&filled), identities(&migrated));
    assert_eq!(attached(&filled), attached(&migrated));
    assert_eq!(attached(&filled), attached(&before));

    // Every row has its album artist. A scan that finds no file changed
    // writes no row.
    forbid_writes(&db);
    for _ in 0..2 {
        assert_eq!(scan(&db, root, false), unchanged);
    }
    assert_eq!(without_scans(&dump_of(&db)), without_scans(&filled));
    // The triggers are in force: a write of a track row fails.
    let error = db
        .with_writer(|conn| Ok(conn.execute("UPDATE tracks SET mtime = mtime", [])?))
        .unwrap_err();
    assert!(
        error.to_string().contains("a row of tracks was updated"),
        "{error}"
    );
}

/// Returns a data directory in a new temporary directory. The guard keeps
/// the directory for the life of the test.
fn data_dir() -> (TempDir, DataDir) {
    let dir = TempDir::new().unwrap();
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
    (dir, data_dir)
}

/// Runs the server on `data_dir` with a token that is already cancelled and
/// returns the error that ends the start. The server opens the database
/// before it binds an address. Its address is `127.0.0.1` with a port that
/// the system chooses.
async fn start_error(data_dir: &DataDir) -> ServerError {
    let mut config = Config::default();
    config.server.bind = "127.0.0.1".parse().expect("an address");
    config.server.port = 0;
    config.library.scan_on_start = false;
    let cancel = CancellationToken::new();
    cancel.cancel();
    run_server(config, data_dir, cancel, ServerOptions::default())
        .await
        .expect_err("the server does not start")
}

/// Runs `wn-server user show` on `data_dir` and returns the exit code, what
/// the command printed to stdout and what it printed to stderr.
fn user_show(data_dir: &DataDir) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_wn-server"))
        .env_remove(DATA_DIR_ENV)
        .env_remove(PASSWORD_ENV)
        .arg("--data-dir")
        .arg(data_dir.path())
        .args(["user", "show"])
        .output()
        .expect("run wn-server");
    (
        output.status.code(),
        String::from_utf8(output.stdout).expect("stdout is UTF-8"),
        String::from_utf8(output.stderr).expect("stderr is UTF-8"),
    )
}

const OWNER: &str = "INSERT INTO users (id, name, role, password, created_at, updated_at)
                     VALUES (1, 'owner', 'owner', 'sesame', 10, 10);
                     INSERT INTO folders (id, path, name) VALUES (1, '/music', 'music');";

#[tokio::test]
async fn the_server_and_a_command_refuse_a_database_of_a_newer_schema_version() {
    // The database has the version that a build with one more migration
    // gives it. This build is the older one.
    let (_dir, data_dir) = data_dir();
    let path = data_dir.db_path();
    drop(Db::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    let latest = user_version(&conn);
    let newer = latest + 1;
    conn.execute_batch(OWNER).unwrap();
    conn.pragma_update(None, "user_version", newer).unwrap();
    let before = dump(&conn).unwrap();
    drop(conn);
    let message = format!(
        "the database has schema version {newer}; this build supports versions 0 to {latest}"
    );

    let error = start_error(&data_dir).await;
    assert!(
        matches!(&error, ServerError::Database { path: failed, .. } if *failed == path),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("cannot open the database {}: {message}", path.display())
    );
    assert_eq!(
        user_show(&data_dir),
        (Some(1), String::new(), format!("wn-server: {message}\n"))
    );

    // Neither changed the version or a row.
    let conn = Connection::open(&path).unwrap();
    assert_eq!(user_version(&conn), newer);
    assert_eq!(dump(&conn).unwrap(), before);
}

/// Checks that the server and a command fail on `data_dir` with the error
/// of a migration that failed: `message` names the migration, the version
/// that the database was left at and the cause. Checks that the database
/// has the version `version` and the rows `before` after both attempts.
async fn assert_migration_fails(data_dir: &DataDir, message: &str, version: i64, before: &Dump) {
    let path = data_dir.db_path();
    let error = start_error(data_dir).await;
    assert!(
        matches!(&error, ServerError::Database { path: failed, .. } if *failed == path),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("cannot open the database {}: {message}", path.display())
    );
    assert_eq!(
        user_show(data_dir),
        (Some(1), String::new(), format!("wn-server: {message}\n"))
    );

    let conn = Connection::open(&path).unwrap();
    assert_eq!(user_version(&conn), version);
    assert_eq!(&dump(&conn).unwrap(), before);
}

#[tokio::test]
async fn the_server_and_a_command_report_a_migration_that_fails_and_the_version_stays() {
    // The table of this database of schema version 1 has the column that
    // the second migration adds, so the first statement of that migration
    // fails.
    let (_dir, data_dir) = data_dir();
    let conn = create_at_version_1(&data_dir.db_path());
    conn.execute_batch(OWNER).unwrap();
    conn.execute_batch("ALTER TABLE tracks ADD COLUMN album_artist_name TEXT")
        .unwrap();
    let before = dump(&conn).unwrap();
    drop(conn);

    let message = format!(
        "migration {SECOND} failed and the database was left at schema version 1: duplicate \
         column name: album_artist_name"
    );
    assert_migration_fails(&data_dir, &message, 1, &before).await;
}

#[tokio::test]
async fn the_server_and_a_command_report_the_third_migration_that_fails_at_its_last_statement() {
    // An index of this database of schema version 2 names the column that
    // the last statement of the third migration removes, so that statement
    // fails after the three statements before it ran.
    let (_dir, data_dir) = data_dir();
    let conn = create_at_version_2(&data_dir.db_path());
    conn.execute_batch(OWNER).unwrap();
    conn.execute_batch(
        "INSERT INTO scans (id, started_at, finished_at, tracks_seen, tracks_added)
         VALUES (1, 1791680500, 1791680523, 2, 2);
         CREATE INDEX tracks_lyrics ON tracks(has_lyrics);",
    )
    .unwrap();
    let before = dump(&conn).unwrap();
    let columns = |conn: &Connection, table: &str| -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap()
            .query_map([table], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let scan_columns = columns(&conn, "scans");
    drop(conn);

    let message = format!(
        "migration {THIRD} failed and the database was left at schema version 2: error in index \
         tracks_lyrics after drop column: no such column: has_lyrics"
    );
    assert_migration_fails(&data_dir, &message, 2, &before).await;

    // The scan has its times in seconds, and the table has neither the
    // column nor the index that the first statements of the migration add.
    let conn = Connection::open(data_dir.db_path()).unwrap();
    let times: (i64, i64) = conn
        .query_row("SELECT started_at, finished_at FROM scans", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(times, (1_791_680_500, 1_791_680_523));
    assert_eq!(columns(&conn, "scans"), scan_columns);
    assert!(!scan_columns.contains(&"changed_library".to_owned()));
    let indexes: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'scans_changed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 0);
    assert!(columns(&conn, "tracks").contains(&"has_lyrics".to_owned()));
}

/// The statement with which the build at schema version 2 read `lastScan`
/// of `getScanStatus`.
const V2_LAST_SCAN_SQL: &str =
    "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', max(finished_at), 'unixepoch') FROM scans";

/// The statement with which the build at schema version 2 read
/// `lastModified` of `getIndexes`: the end time in seconds of the last scan
/// that counted by the rule of that build, and the number of scans before
/// it that ended in the same second. The build reported the first value
/// times 1000 plus the second.
const V2_LAST_MODIFIED_SQL: &str = "SELECT scans.finished_at,
       (SELECT count(*) FROM scans AS earlier
        WHERE earlier.id < scans.id AND earlier.id > scans.id - 1000
          AND earlier.finished_at = scans.finished_at)
     FROM scans
     WHERE scans.finished_at IS NOT NULL
       AND (scans.tracks_added > 0 OR scans.tracks_changed > 0
            OR scans.tracks_moved > 0 OR scans.tracks_removed > 0
            OR scans.error IS NOT NULL
            OR NOT coalesce(
                 (SELECT previous.finished_at IS NOT NULL AND previous.error IS NULL
                  FROM scans AS previous
                  WHERE previous.id < scans.id
                  ORDER BY previous.id DESC LIMIT 1),
                 0))
     ORDER BY scans.id DESC
     LIMIT 1";

/// A library of one artist with one album and two tracks, in the columns of
/// schema version 2. One track holds 1 in `has_lyrics`.
const V2_LIBRARY: &str = "
    INSERT INTO folders (id, path, name) VALUES (1, '/music', 'music');
    INSERT INTO artists (id, key, name, sort_name, album_count) VALUES
      ('ar-000000000000000000000000000000a1', 'aurora field', 'Aurora Field',
       'Aurora Field', 1);
    INSERT INTO albums (id, artist_id, key, name, artist_name, song_count, duration_ms,
      created_at) VALUES
      ('al-000000000000000000000000000000b1', 'ar-000000000000000000000000000000a1',
       'glass harbour', 'Glass Harbour', 'Aurora Field', 2, 1000, 500);
    INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc, album_id,
      artist_id, artist_name, album_name, track_no, duration_ms, size, suffix, content_type,
      lossless, has_lyrics, mtime, created_at, updated_at, album_artist_name) VALUES
      ('tr-000000000000000000000000000000c1', 1, 'Glass Harbour/01 Low Tide.flac', '100:aa',
       'Low Tide', 'low tide', 'al-000000000000000000000000000000b1',
       'ar-000000000000000000000000000000a1', 'Aurora Field', 'Glass Harbour', 1, 500, 100,
       'flac', 'audio/flac', 1, 1, 700, 800, 900, 'Aurora Field'),
      ('tr-000000000000000000000000000000c2', 1, 'Glass Harbour/02 Breakwater.flac',
       '200:bb', 'Breakwater', 'breakwater', 'al-000000000000000000000000000000b1',
       'ar-000000000000000000000000000000a1', 'Aurora Field', 'Glass Harbour', 2, 500, 200,
       'flac', 'audio/flac', 1, 0, 701, 801, 901, NULL);
";

/// What a build reports of the times of the scans.
#[derive(Debug, PartialEq, Eq)]
struct Reported {
    /// `lastScan` of `getScanStatus`.
    last_scan: &'static str,
    /// `lastModified` of `getIndexes`.
    last_modified: i64,
}

/// Stores [`V2_LIBRARY`] and the scans `scans` in a database of schema
/// version 2, checks that the build at that version reported `v2` of them,
/// and lets this build upgrade the file. Returns a server whose database
/// holds the rows that the upgrade left.
async fn upgraded_server(scans: &str, v2: &Reported) -> TestServer {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("library.db");
    let conn = create_at_version_2(&path);
    conn.execute_batch(V2_LIBRARY).unwrap();
    conn.execute_batch(scans).unwrap();

    // What the build at schema version 2 answered from this file.
    let last_scan: String = conn
        .query_row(V2_LAST_SCAN_SQL, [], |row| row.get(0))
        .unwrap();
    let (seconds, earlier): (i64, i64) = conn
        .query_row(V2_LAST_MODIFIED_SQL, [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (last_scan.as_str(), seconds * 1000 + earlier),
        (v2.last_scan, v2.last_modified)
    );
    drop(conn);

    // This build upgrades the file in place.
    let db = Db::open(&path).unwrap();
    let version = db.read(|conn| Ok(user_version(conn))).await.unwrap();
    assert!(version >= 3, "{version}");
    db.close().await.unwrap();

    // The server of the tests has a database in memory, which holds the
    // owner account. It gets the rows of the library and of the scans that
    // the upgrade left in the file.
    let server = TestServer::bare(Config::default()).await;
    let file = path.to_str().expect("a Unicode path").to_owned();
    server
        .db()
        .write(move |conn| {
            conn.execute("ATTACH DATABASE ?1 AS upgraded", [file])?;
            let tables = ["folders", "artists", "albums", "tracks", "scans"];
            copy_rows(conn, "upgraded", &tables.map(str::to_owned));
            conn.execute_batch("DETACH DATABASE upgraded")?;
            Ok(())
        })
        .await
        .unwrap();
    server
}

/// What `server` reports of the times of the scans.
async fn reported(server: &TestServer) -> (String, i64) {
    let status = server.get_json("getScanStatus", &[]).await;
    let last_scan = status["scanStatus"]["lastScan"]
        .as_str()
        .unwrap_or_else(|| panic!("no lastScan: {status}"));
    let indexes = server.get_json("getIndexes", &[]).await;
    let last_modified = indexes["indexes"]["lastModified"]
        .as_i64()
        .unwrap_or_else(|| panic!("no lastModified: {indexes}"));
    (last_scan.to_owned(), last_modified)
}

/// Returns the names of the artists that `getIndexes` with the
/// `ifModifiedSince` value `since` lists, or `None` for an answer without
/// `index`.
async fn artists_since(server: &TestServer, since: i64) -> Option<Vec<String>> {
    let since = since.to_string();
    let response = server
        .get_json("getIndexes", &[("ifModifiedSince", &since)])
        .await;
    assert_eq!(response["status"], "ok", "{response}");
    let index = response["indexes"].get("index")?;
    let names = index.as_array().expect("a list of indexes").iter();
    Some(
        names
            .flat_map(|index| index["artist"].as_array().expect("a list of artists"))
            .map(|artist| artist["name"].as_str().expect("a name").to_owned())
            .collect(),
    )
}

/// Ends a new scan in the database of `server` now, with the flag
/// `changed_library`, and returns the `lastModified` after it.
async fn scan_now(server: &TestServer, changed_library: bool) -> i64 {
    server
        .db()
        .write(move |conn| {
            let id = scans::insert(conn)?;
            scans::finish(conn, id, &ScanCounts::default(), changed_library, None)
        })
        .await
        .unwrap();
    reported(server).await.1
}

#[tokio::test]
async fn the_scan_times_of_schema_version_2_are_reported_in_the_same_units_after_the_upgrade() {
    // The scans ended on 2025-06-15, before the time of every clock that a
    // test runs with. The first scan ended at 01:02:03 and added two
    // tracks. The second one ended at 02:03:04 and found nothing changed, so
    // the build at schema version 2 kept the `lastModified` of the first
    // one.
    let scans = "INSERT INTO scans (id, started_at, finished_at, tracks_seen, tracks_added)
                 VALUES (1, 1749949300, 1749949323, 2, 2), (2, 1749952980, 1749952984, 2, 0);";
    let v2 = Reported {
        last_scan: "2025-06-15T02:03:04Z",
        last_modified: 1_749_949_323_000,
    };
    let server = upgraded_server(scans, &v2).await;

    // `lastScan` is the same text. `lastModified` is the end of the latest
    // scan in milliseconds: the migration cannot tell which scans changed
    // the library, so each of them counts.
    let upgraded = Reported {
        last_scan: "2025-06-15T02:03:04Z",
        last_modified: 1_749_952_984_000,
    };
    let (last_scan, last_modified) = reported(&server).await;
    assert_eq!(
        (last_scan.as_str(), last_modified),
        (upgraded.last_scan, upgraded.last_modified)
    );
    let status = server.get_json("getScanStatus", &[]).await;
    assert_eq!(
        status["scanStatus"],
        json!({ "scanning": false, "count": 0, "lastScan": "2025-06-15T02:03:04Z" })
    );

    // A client that holds the time of the build at schema version 2 gets
    // the artists once more. A client that holds the new time gets none.
    let artists = Some(vec!["Aurora Field".to_owned()]);
    assert_eq!(artists_since(&server, v2.last_modified).await, artists);
    assert_eq!(
        artists_since(&server, upgraded.last_modified - 1).await,
        artists
    );
    assert_eq!(artists_since(&server, upgraded.last_modified).await, None);
    assert_eq!(
        artists_since(&server, upgraded.last_modified + 1).await,
        None
    );
    // The times are milliseconds: the end of the latest scan in seconds is
    // a time long before it.
    assert_eq!(artists_since(&server, 1_749_952_984).await, artists);

    // A scan of this build that changes nothing keeps the time, and one
    // that changes the library has a later one.
    assert_eq!(scan_now(&server, false).await, upgraded.last_modified);
    let next = scan_now(&server, true).await;
    assert!(next > upgraded.last_modified, "{next}");
    assert_eq!(next.to_string().len(), 13, "{next}");
    assert_eq!(
        artists_since(&server, upgraded.last_modified).await,
        artists
    );
    assert_eq!(artists_since(&server, next).await, None);
}

#[tokio::test]
async fn two_scans_of_schema_version_2_that_ended_in_one_second_have_one_time_after_the_upgrade() {
    // Both scans ended at 03:04:05 and stored a change. The build at schema
    // version 2 told them apart by one millisecond for the second scan.
    let scans = "INSERT INTO scans (id, started_at, finished_at, tracks_seen, tracks_added,
                   tracks_changed)
                 VALUES (1, 1749956640, 1749956645, 2, 2, 0), (2, 1749956645, 1749956645, 2, 0, 1);";
    let v2 = Reported {
        last_scan: "2025-06-15T03:04:05Z",
        last_modified: 1_749_956_645_001,
    };
    let server = upgraded_server(scans, &v2).await;

    // The migration multiplies the seconds by 1000, so both scans have the
    // start of that second as their end time. `lastModified` is one
    // millisecond before the value of the build at schema version 2, and
    // in the same second.
    let (last_scan, last_modified) = reported(&server).await;
    assert_eq!(
        (last_scan.as_str(), last_modified),
        ("2025-06-15T03:04:05Z", 1_749_956_645_000)
    );
    let ends: Vec<Option<i64>> = server
        .db()
        .read(scans::list)
        .await
        .unwrap()
        .iter()
        .map(|scan| scan.finished_at)
        .collect();
    assert_eq!(ends, [Some(1_749_956_645_000), Some(1_749_956_645_000)]);

    // Nothing changed since, and a client that holds either time gets no
    // artists.
    let artists = Some(vec!["Aurora Field".to_owned()]);
    assert_eq!(artists_since(&server, v2.last_modified).await, None);
    assert_eq!(artists_since(&server, last_modified).await, None);
    assert_eq!(artists_since(&server, last_modified - 1).await, artists);

    // The next scan that changes the library ends later than both times,
    // so both clients get the artists.
    let next = scan_now(&server, true).await;
    assert!(next > v2.last_modified, "{next}");
    assert_eq!(artists_since(&server, v2.last_modified).await, artists);
    assert_eq!(artists_since(&server, last_modified).await, artists);
}
