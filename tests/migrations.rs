//! Tests of the schema migrations through the public interface. A library
//! that a scan stored at schema version 1 is migrated by `Db::open` and
//! scanned again. The server and a command report a database that they
//! cannot migrate and a database of a newer schema version.
//!
//! The unit tests in `src/db/mod.rs` cover the migration runner with rows
//! that the tests write.
//!
//! A database of schema version 1 is made with the text of the first
//! migration. That file does not change, so these databases are the ones
//! that a build with one migration left on disk.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::LibraryBuilder;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, TagExt};
use rusqlite::Connection;
use rusqlite::types::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wn_server::cli::PASSWORD_ENV;
use wn_server::config::{Config, DATA_DIR_ENV, DataDir};
use wn_server::db::Db;
use wn_server::library::scanner::{self, ScanOptions, ScanSummary};
use wn_server::library::tags::VARIOUS_ARTISTS;
use wn_server::server::{ServerError, ServerOptions, run_server};

/// The schema of version 1: the text of the first migration.
const V1_SCHEMA: &str = include_str!("../migrations/0001_init.sql");

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
/// order, the name and the value of every column.
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
        let mut select = conn.prepare(&format!("SELECT * FROM main.{table} ORDER BY rowid"))?;
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

/// Writes a database of schema version 1 to `target` that holds the rows of
/// the database at `scanned`, which this build wrote, in the columns of
/// version 1.
fn store_at_version_1(scanned: &Path, target: &Path) {
    let conn = create_at_version_1(target);
    conn.execute(
        "ATTACH DATABASE ?1 AS scanned",
        [scanned.to_str().expect("a Unicode path")],
    )
    .unwrap();
    // The tables are copied in the order of their names, so a row is stored
    // before the row that it refers to. The references are checked when the
    // transaction commits.
    conn.execute_batch("BEGIN; PRAGMA defer_foreign_keys = ON")
        .unwrap();
    for table in table_names(&conn).unwrap() {
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info(?1, 'main')")
            .unwrap()
            .query_map([&table], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let columns = columns.join(", ");
        conn.execute(
            &format!("INSERT INTO main.{table} ({columns}) SELECT {columns} FROM scanned.{table}"),
            [],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT; DETACH DATABASE scanned")
        .unwrap();
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
    assert_eq!(user_version(&v1), 1);
    let before = dump(&v1).unwrap();
    drop(v1);
    assert_eq!(before.len(), 11);
    for (table, rows) in &before {
        assert!(!rows.is_empty(), "{table} has no row");
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

    // `Db::open` migrates the file. Every row has the values it had, and no
    // row has an album artist.
    let db = Db::open(&path).unwrap();
    let migrated = dump_of(&db);
    let version = db.with_reader(|conn| Ok(user_version(conn))).unwrap();
    assert_eq!(version, latest_version());
    assert!(version > 1);
    assert_eq!(columns_of(&migrated, &before), before);
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

#[tokio::test]
async fn the_server_and_a_command_report_a_migration_that_fails_and_the_version_stays() {
    // The table of this database of schema version 1 has the column that
    // the second migration adds, so the first statement of that migration
    // fails.
    let (_dir, data_dir) = data_dir();
    let path = data_dir.db_path();
    let conn = create_at_version_1(&path);
    conn.execute_batch(OWNER).unwrap();
    conn.execute_batch("ALTER TABLE tracks ADD COLUMN album_artist_name TEXT")
        .unwrap();
    let before = dump(&conn).unwrap();
    drop(conn);
    let cause = "duplicate column name: album_artist_name";

    let error = start_error(&data_dir).await;
    assert!(
        matches!(&error, ServerError::Database { path: failed, .. } if *failed == path),
        "{error:?}"
    );
    let prefix = format!("cannot open the database {}: ", path.display());
    assert!(error.to_string().starts_with(&prefix), "{error}");
    assert!(error.to_string().contains(cause), "{error}");
    let (code, stdout, stderr) = user_show(&data_dir);
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert!(stderr.starts_with("wn-server: "), "{stderr}");
    assert!(stderr.contains(cause), "{stderr}");

    // The database is at its version with its rows, after two attempts.
    let conn = Connection::open(&path).unwrap();
    assert_eq!(user_version(&conn), 1);
    assert_eq!(dump(&conn).unwrap(), before);
}
