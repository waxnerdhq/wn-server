//! Tests of `library::scanner` on libraries that `LibraryBuilder` writes,
//! with a database in a temp file.
//!
//! A file system stores modification times that the scanner compares in
//! whole seconds, and a test runs in less than one. A test that changes a
//! file in place therefore moves its modification time with
//! [`shift_mtime`], and a test that copies a file gives the copy the time of
//! the original with [`copy_with_mtime`].

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{LibraryBuilder, TestLibrary};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, TagExt};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wn_server::db::Db;
use wn_server::db::queries::scans::{Scan, ScanCounts};
use wn_server::db::queries::tracks::{IndexEntry, Track};
use wn_server::db::queries::{folders, scans, tracks};
use wn_server::ids::{self, Kind};
use wn_server::library::scanner::{self, CANCELLED, ScanOptions, ScanSummary};

/// The relative paths of the standard catalogue, in the order
/// `LibraryBuilder::standard` adds the tracks.
const STANDARD: [&str; 10] = [
    "Aurora Field/Glass Harbour/01 Low Tide.flac",
    "Aurora Field/Glass Harbour/02 Breakwater.flac",
    "Aurora Field/Glass Harbour/03 Night Ferry.flac",
    "Aurora Field/Paper Maps/01 Meridian.mp3",
    "Aurora Field/Paper Maps/02 Contour.mp3",
    "Aurora Field/Paper Maps/01 Legend.mp3",
    "Marlow Quartet/Slow Rivers/01 Upstream.m4a",
    "Marlow Quartet/Slow Rivers/02 Delta.m4a",
    "Tessa Lund/Summer Sampler/01 Open Road.ogg",
    "Aurora Field/Summer Sampler/02 Last Train.opus",
];

const LOW_TIDE: &str = STANDARD[0];
const BREAKWATER: &str = STANDARD[1];

/// The ID of the playlist that [`attach`] fills.
const PLAYLIST: &str = "pl-00000000000000000000000000000001";

/// A locator: the folder ID and the relative path.
type Locator = (i64, String);

/// A database in a temp file. Dropping the value deletes the file.
struct TestDb {
    db: Db,
    _dir: TempDir,
}

impl TestDb {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create the database directory");
        let db = Db::open(dir.path().join("library.db")).expect("open the database");
        Self { db, _dir: dir }
    }

    /// Runs a scan that is not full, with the default options.
    fn scan(&self, folders: &[&Path]) -> ScanSummary {
        self.scan_with(folders, false, ScanOptions::default())
    }

    fn scan_with(&self, folders: &[&Path], full: bool, options: ScanOptions) -> ScanSummary {
        let folders: Vec<PathBuf> = folders.iter().map(|path| path.to_path_buf()).collect();
        scanner::scan(&self.db, &folders, &CancellationToken::new(), full, options)
            .expect("the scan returns a summary")
    }

    /// Returns the index entry of every track by locator.
    fn index(&self) -> BTreeMap<Locator, IndexEntry> {
        self.db
            .with_reader(tracks::load_index)
            .unwrap()
            .into_iter()
            .map(|entry| ((entry.folder_id, entry.rel_path.clone()), entry))
            .collect()
    }

    /// Returns the locator of every track by ID.
    fn locators(&self) -> BTreeMap<String, Locator> {
        self.index()
            .into_iter()
            .map(|(locator, entry)| (entry.id, locator))
            .collect()
    }

    /// Returns the ID of the track at a locator, or `None` when the locator
    /// has no track.
    fn id_at(&self, folder_id: i64, rel_path: &str) -> Option<String> {
        self.index()
            .remove(&(folder_id, rel_path.to_owned()))
            .map(|entry| entry.id)
    }

    fn track(&self, id: &str) -> Track {
        self.db
            .with_reader(|conn| tracks::get_track(conn, id))
            .unwrap()
            .unwrap_or_else(|| panic!("no track has the ID {id}"))
    }

    fn count(&self) -> u64 {
        self.db.with_reader(tracks::count_tracks).unwrap()
    }

    /// Returns the ID of the folder row of `path`.
    fn folder_id(&self, path: &Path) -> i64 {
        self.folders()
            .into_iter()
            .find(|folder| folder.path == path)
            .unwrap_or_else(|| panic!("no folder row has the path {}", path.display()))
            .id
    }

    fn folders(&self) -> Vec<folders::Folder> {
        self.db.with_reader(folders::list).unwrap()
    }

    fn latest_scan(&self) -> Scan {
        self.db
            .with_reader(scans::latest)
            .unwrap()
            .expect("a scan has run")
    }
}

/// The standard library, scanned once with its root as the one music folder.
struct Scanned {
    library: TestLibrary,
    env: TestDb,
    folder_id: i64,
}

impl Scanned {
    fn new() -> Self {
        Self::of(LibraryBuilder::standard().build())
    }

    fn of(library: TestLibrary) -> Self {
        let env = TestDb::new();
        env.scan(&[library.root.path()]);
        let folder_id = env.folder_id(library.root.path());
        Self {
            library,
            env,
            folder_id,
        }
    }

    fn root(&self) -> &Path {
        self.library.root.path()
    }

    /// Returns the absolute path of a relative path.
    fn path(&self, rel_path: &str) -> PathBuf {
        self.root().join(rel_path)
    }

    fn rescan(&self) -> ScanSummary {
        self.env.scan(&[self.root()])
    }

    /// Returns the ID of the track at `rel_path`.
    fn id_at(&self, rel_path: &str) -> String {
        self.find(rel_path)
            .unwrap_or_else(|| panic!("no track is at {rel_path}"))
    }

    fn find(&self, rel_path: &str) -> Option<String> {
        self.env.id_at(self.folder_id, rel_path)
    }

    fn track_at(&self, rel_path: &str) -> Track {
        self.env.track(&self.id_at(rel_path))
    }
}

/// Returns a summary with `seen` files and no other count.
fn seen(seen: u64) -> ScanSummary {
    ScanSummary {
        seen,
        ..ScanSummary::default()
    }
}

/// Returns the modification time of a file in Unix seconds.
fn mtime(path: &Path) -> i64 {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    i64::try_from(modified.duration_since(UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

fn set_mtime(path: &Path, time: SystemTime) {
    let file = fs::File::options()
        .write(true)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    file.set_modified(time)
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

/// Moves the modification time of a file `seconds` into the future, so that
/// the scanner sees a change.
fn shift_mtime(path: &Path, seconds: u64) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    set_mtime(path, modified + Duration::from_secs(seconds));
}

/// Copies a file with its modification time, as `cp -p` does.
fn copy_with_mtime(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::copy(from, to).unwrap_or_else(|error| panic!("copy to {}: {error}", to.display()));
    set_mtime(to, fs::metadata(from).unwrap().modified().unwrap());
}

/// Moves a file and creates the directory it moves to.
fn move_file(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(from, to).unwrap_or_else(|error| panic!("move to {}: {error}", to.display()));
}

/// Sets the title tag of an audio file. The tag is in the part of the file
/// that the fingerprint covers.
fn retitle(path: &Path, title: &str) {
    let mut file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let tag = file
        .primary_tag_mut()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()));
    tag.insert_text(ItemKey::TrackTitle, title.to_owned());
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
}

/// Writes an audio file whose bytes differ from every file of the standard
/// library.
fn write_other_track(to: &Path) {
    let other = LibraryBuilder::new()
        .track("Other Artist", "Other Album", "Other Title", 9, "flac")
        .build();
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::copy(&other.files[0], to).unwrap();
}

/// Gives the track `id` what a user attaches to a track: a star, a rating
/// and a play count in `annotations`, and an entry in a playlist.
fn attach(db: &Db, id: &str) {
    db.with_writer(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO users (id, name, role, password, created_at, updated_at)
             VALUES (1, 'adnan', 'owner', 'secret', 0, 0)",
            [],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO playlists (id, owner_id, name, created_at, updated_at)
             VALUES (?1, 1, 'Mix', 0, 0)",
            [PLAYLIST],
        )?;
        conn.execute(
            "INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating, play_count)
             VALUES (1, ?1, 'track', 100, 5, 3)",
            [id],
        )?;
        conn.execute(
            "INSERT INTO playlist_tracks (playlist_id, position, track_id)
             SELECT ?1, count(*), ?2 FROM playlist_tracks",
            [PLAYLIST, id],
        )?;
        Ok(())
    })
    .unwrap();
}

/// Returns the locator of the track that has the star, the rating, the play
/// count and the playlist entry that [`attach`] gave the track `id`.
///
/// The playlist entry is deleted with its track row, and the annotation
/// names the track by ID. The result is therefore `None` when the row of
/// `id` was deleted, also when a new row took its place.
fn attached(db: &Db, id: &str) -> Option<Locator> {
    db.with_reader(|conn| {
        let mut select = conn.prepare(
            "SELECT t.folder_id, t.rel_path FROM tracks t
             JOIN annotations a ON a.item_id = t.id AND a.item_type = 'track'
               AND a.starred_at = 100 AND a.rating = 5 AND a.play_count = 3
             JOIN playlist_tracks p ON p.track_id = t.id AND p.playlist_id = ?2
             WHERE t.id = ?1",
        )?;
        let mut rows = select
            .query_map([id, PLAYLIST], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<Locator>>>()?;
        assert!(rows.len() <= 1, "{rows:?}");
        Ok(rows.pop())
    })
    .unwrap()
}

/// Returns the number of rows of a table.
fn count_rows(db: &Db, table: &str) -> i64 {
    db.with_reader(|conn| {
        Ok(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?,
        )
    })
    .unwrap()
}

#[test]
fn first_scan_inserts_the_ten_standard_tracks() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    let env = TestDb::new();
    let progress = Arc::new(AtomicU64::new(0));

    let summary = env.scan_with(
        &[root],
        false,
        ScanOptions {
            progress: Arc::clone(&progress),
            ..ScanOptions::default()
        },
    );
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 10,
            tags_read: 10,
            ..ScanSummary::default()
        }
    );
    assert_eq!(progress.load(Ordering::Relaxed), 10);

    let folder_id = env.folder_id(root);
    assert_eq!(env.count(), 10);
    let mut seen_ids = Vec::new();
    for (path, rel_path) in library.files.iter().zip(STANDARD) {
        // The stored path uses `/` on every OS and names the file.
        assert_eq!(root.join(rel_path), *path);
        let id = env
            .id_at(folder_id, rel_path)
            .unwrap_or_else(|| panic!("no track is at {rel_path}"));
        assert_eq!(ids::parse(&id).map(|(kind, _)| kind), Some(Kind::Track));
        let track = env.track(&id);
        assert_eq!(track.folder_id, folder_id);
        assert_eq!(track.rel_path, rel_path);
        assert_eq!(Some(track.suffix.as_str()), rel_path.rsplit('.').next());
        assert_eq!(track.mtime, mtime(path), "{rel_path}");
        assert_eq!(track.size, fs::metadata(path).unwrap().len(), "{rel_path}");
        assert_eq!(track.fingerprint, ids::fingerprint(path).unwrap());
        seen_ids.push(id);
    }
    seen_ids.sort();
    seen_ids.dedup();
    assert_eq!(seen_ids.len(), 10);

    // The tags fill the row.
    let track = env.track(&env.id_at(folder_id, STANDARD[5]).unwrap());
    assert_eq!(track.title, "Legend");
    assert_eq!(track.artist_name, "Aurora Field");
    assert_eq!(track.album_name, "Paper Maps");
    assert_eq!((track.track_no, track.disc_no), (Some(1), Some(2)));
    assert_eq!(track.year, Some(2021));
    assert_eq!(track.genre.as_deref(), Some("Electronic"));
    assert_eq!(track.content_type, "audio/mpeg");
    assert!(!track.lossless);
    assert_eq!(track.created_at, track.updated_at);

    // Artists and albums exist by natural key: three album artists and four
    // albums, one of them a compilation.
    assert_eq!(count_rows(&env.db, "artists"), 3);
    assert_eq!(count_rows(&env.db, "albums"), 4);
    let compilations: Vec<(String, String)> = env
        .db
        .with_reader(|conn| {
            let mut select =
                conn.prepare("SELECT name, artist_name FROM albums WHERE compilation = 1")?;
            let rows = select
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(
        compilations,
        [("Summer Sampler".to_owned(), "Various Artists".to_owned())]
    );

    let scan = env.latest_scan();
    assert!(scan.finished_at.is_some());
    assert_eq!(
        scan.counts,
        ScanCounts {
            seen: 10,
            added: 10,
            ..ScanCounts::default()
        }
    );
    assert_eq!(scan.error, None);
}

#[test]
fn second_scan_without_changes_reads_no_tag_and_writes_no_track_row() {
    let scanned = Scanned::new();
    let before = scanned.env.index();

    // The triggers fail every statement that writes a track row on the
    // write connection, which the scanner uses.
    scanned
        .env
        .db
        .with_writer(|conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER no_update BEFORE UPDATE ON tracks
                 BEGIN SELECT RAISE(ABORT, 'a track row was updated'); END;
                 CREATE TEMP TRIGGER no_insert BEFORE INSERT ON tracks
                 BEGIN SELECT RAISE(ABORT, 'a track row was inserted'); END;
                 CREATE TEMP TRIGGER no_delete BEFORE DELETE ON tracks
                 BEGIN SELECT RAISE(ABORT, 'a track row was deleted'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    let progress = Arc::new(AtomicU64::new(0));
    let batches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&batches);

    let summary = scanned.env.scan_with(
        &[scanned.root()],
        false,
        ScanOptions {
            on_batch: Some(Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
            })),
            progress: Arc::clone(&progress),
            ..ScanOptions::default()
        },
    );
    assert_eq!(summary, seen(10));
    assert_eq!(summary.tags_read, 0);
    assert_eq!(summary.changed, 0);
    assert_eq!(scanned.env.index(), before);
    assert_eq!(progress.load(Ordering::Relaxed), 10);
    assert_eq!(batches.load(Ordering::Relaxed), 0);

    // The triggers are in force: an update of a track row fails.
    let error = scanned
        .env
        .db
        .with_writer(|conn| Ok(conn.execute("UPDATE tracks SET mtime = mtime", [])?))
        .unwrap_err();
    assert!(
        error.to_string().contains("a track row was updated"),
        "{error}"
    );
}

#[test]
fn a_retagged_file_keeps_its_row_and_its_id() {
    let scanned = Scanned::new();
    let before = scanned.track_at(LOW_TIDE);
    attach(&scanned.env.db, &before.id);

    retitle(&scanned.path(LOW_TIDE), "High Tide");
    shift_mtime(&scanned.path(LOW_TIDE), 5);
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    let after = scanned.track_at(LOW_TIDE);
    assert_eq!(after.id, before.id);
    assert_eq!(after.title, "High Tide");
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.mtime, mtime(&scanned.path(LOW_TIDE)));
    assert!(after.mtime > before.mtime, "{after:?}");
    // The tag edit changed the bytes that the fingerprint covers.
    assert_ne!(after.fingerprint, before.fingerprint);
    assert_eq!(
        after.fingerprint,
        ids::fingerprint(&scanned.path(LOW_TIDE)).unwrap()
    );
    assert_eq!(scanned.env.count(), 10);
    assert_eq!(
        attached(&scanned.env.db, &before.id),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );
}

#[test]
fn a_touched_file_keeps_its_row_and_has_its_tags_read_again() {
    let scanned = Scanned::new();
    let before = scanned.track_at(LOW_TIDE);

    shift_mtime(&scanned.path(LOW_TIDE), 5);
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    let after = scanned.track_at(LOW_TIDE);
    assert_eq!(
        after,
        Track {
            mtime: before.mtime + 5,
            updated_at: after.updated_at,
            ..before
        }
    );
}

#[test]
fn a_file_with_other_content_at_the_same_path_keeps_the_row_when_the_old_bytes_are_gone() {
    // The scanner cannot tell this case from a retag. The path decides.
    let scanned = Scanned::new();
    let before = scanned.track_at(LOW_TIDE);

    write_other_track(&scanned.path(LOW_TIDE));
    shift_mtime(&scanned.path(LOW_TIDE), 5);
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    let after = scanned.track_at(LOW_TIDE);
    assert_eq!(after.id, before.id);
    assert_eq!(after.title, "Other Title");
    assert_eq!(scanned.env.count(), 10);
}

/// Checks that the track `id` moved from `from` to `to` in the one folder
/// of `scanned` and that the scan read no tag.
fn assert_moved(scanned: &Scanned, id: &str, from: &str, to: &str) {
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            moved: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(summary.removed, 0);
    assert_eq!(scanned.find(to).as_deref(), Some(id));
    assert_eq!(scanned.find(from), None);
    assert_eq!(scanned.env.count(), 10);
    assert_eq!(
        attached(&scanned.env.db, id),
        Some((scanned.folder_id, to.to_owned()))
    );
}

#[test]
fn a_renamed_file_keeps_its_id() {
    let scanned = Scanned::new();
    let to = "Aurora Field/Glass Harbour/01 Ebb.flac";
    let before = scanned.track_at(LOW_TIDE);
    attach(&scanned.env.db, &before.id);

    move_file(&scanned.path(LOW_TIDE), &scanned.path(to));
    assert_moved(&scanned, &before.id, LOW_TIDE, to);

    // Only the locator changed.
    let after = scanned.track_at(to);
    assert_eq!(
        after,
        Track {
            rel_path: to.to_owned(),
            updated_at: after.updated_at,
            ..before
        }
    );
}

#[test]
fn a_file_whose_name_changes_case_only_keeps_its_id() {
    let scanned = Scanned::new();
    let to = "Aurora Field/Glass Harbour/01 LOW TIDE.flac";
    let id = scanned.id_at(LOW_TIDE);
    attach(&scanned.env.db, &id);

    fs::rename(scanned.path(LOW_TIDE), scanned.path(to)).unwrap();
    assert_moved(&scanned, &id, LOW_TIDE, to);
}

#[test]
fn a_file_moved_to_another_directory_keeps_its_id() {
    let scanned = Scanned::new();
    let to = "Sorted/By Mood/Calm/01 Low Tide.flac";
    let id = scanned.id_at(LOW_TIDE);
    attach(&scanned.env.db, &id);

    move_file(&scanned.path(LOW_TIDE), &scanned.path(to));
    assert_moved(&scanned, &id, LOW_TIDE, to);
}

#[test]
fn a_file_moved_to_a_second_music_folder_keeps_its_id() {
    let library = LibraryBuilder::standard().build();
    let first = library.root.path().join("Aurora Field");
    let second = library.root.path().join("Marlow Quartet");
    let env = TestDb::new();
    assert_eq!(env.scan(&[&first, &second]).added, 9);
    let (first_id, second_id) = (env.folder_id(&first), env.folder_id(&second));
    assert_ne!(first_id, second_id);

    let from = "Glass Harbour/01 Low Tide.flac";
    let to = "Imports/01 Low Tide.flac";
    let id = env.id_at(first_id, from).unwrap();
    attach(&env.db, &id);
    move_file(&first.join(from), &second.join(to));
    let summary = env.scan(&[&first, &second]);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            moved: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(summary.removed, 0);
    assert_eq!(env.id_at(second_id, to), Some(id.clone()));
    assert_eq!(env.id_at(first_id, from), None);
    assert_eq!(env.count(), 9);
    assert_eq!(attached(&env.db, &id), Some((second_id, to.to_owned())));
}

#[test]
fn a_changed_folder_path_keeps_every_track_id() {
    let library = LibraryBuilder::standard().build();
    let old = library.root.path();
    let env = TestDb::new();
    env.scan(&[old]);
    let old_id = env.folder_id(old);
    let before = env.locators();
    for id in before.keys() {
        attach(&env.db, id);
    }

    // The library moves to another directory and the config names it.
    let new = old.join("moved");
    fs::create_dir(&new).unwrap();
    for artist in ["Aurora Field", "Marlow Quartet", "Tessa Lund"] {
        fs::rename(old.join(artist), new.join(artist)).unwrap();
    }
    let summary = env.scan(&[&new]);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            moved: 10,
            ..ScanSummary::default()
        }
    );
    // The row of the old path is gone and the tracks are in the new row.
    let folders = env.folders();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].path, new);
    let new_id = folders[0].id;
    assert_ne!(new_id, old_id);
    let expected: BTreeMap<String, Locator> = before
        .into_iter()
        .map(|(id, (_, rel_path))| (id, (new_id, rel_path)))
        .collect();
    assert_eq!(env.locators(), expected);
    for (id, locator) in &expected {
        assert_eq!(attached(&env.db, id).as_ref(), Some(locator));
    }
}

#[test]
fn the_row_of_a_deleted_file_is_deleted() {
    let scanned = Scanned::new();
    let id = scanned.id_at(LOW_TIDE);
    let others: Vec<String> = STANDARD[1..]
        .iter()
        .map(|rel_path| scanned.id_at(rel_path))
        .collect();

    fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            removed: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.find(LOW_TIDE), None);
    assert!(!scanned.env.locators().contains_key(&id));
    for (rel_path, id) in STANDARD[1..].iter().zip(&others) {
        assert_eq!(scanned.find(rel_path).as_ref(), Some(id));
    }
    assert_eq!(scanned.env.latest_scan().counts.removed, 1);
}

#[test]
fn a_file_placed_at_the_old_path_of_a_moved_track_gets_a_new_id() {
    let scanned = Scanned::new();
    let old = LOW_TIDE;
    let new = "Aurora Field/Paper Maps/Low Tide.flac";
    let id = scanned.id_at(old);
    let known: Vec<String> = scanned.env.locators().into_keys().collect();
    attach(&scanned.env.db, &id);

    // Track A moves from `old` to `new` and file C takes its place.
    move_file(&scanned.path(old), &scanned.path(new));
    write_other_track(&scanned.path(old));
    shift_mtime(&scanned.path(old), 5);
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            added: 1,
            moved: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    // A keeps its ID at the new path, with what the user attached to it.
    assert_eq!(scanned.id_at(new), id);
    assert_eq!(scanned.track_at(new).title, "Low Tide");
    assert_eq!(
        attached(&scanned.env.db, &id),
        Some((scanned.folder_id, new.to_owned()))
    );
    // C is a new track at the old path.
    let placed = scanned.track_at(old);
    assert!(!known.contains(&placed.id), "{placed:?}");
    assert_eq!(placed.title, "Other Title");
    assert_eq!(attached(&scanned.env.db, &placed.id), None);
    assert_eq!(scanned.env.count(), 11);
}

#[test]
fn two_files_that_exchange_paths_keep_their_ids() {
    let library = LibraryBuilder::standard().build();
    // The two files get times that differ, so that the exchange shows in
    // the file state of both paths.
    shift_mtime(&library.files[0], 100);
    shift_mtime(&library.files[1], 200);
    let scanned = Scanned::of(library);
    let first = scanned.track_at(LOW_TIDE);
    let second = scanned.track_at(BREAKWATER);
    attach(&scanned.env.db, &first.id);
    attach(&scanned.env.db, &second.id);

    let spare = scanned.path("spare.flac");
    fs::rename(scanned.path(LOW_TIDE), &spare).unwrap();
    fs::rename(scanned.path(BREAKWATER), scanned.path(LOW_TIDE)).unwrap();
    fs::rename(&spare, scanned.path(BREAKWATER)).unwrap();
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            moved: 2,
            ..ScanSummary::default()
        }
    );
    let at_first_path = scanned.track_at(LOW_TIDE);
    let at_second_path = scanned.track_at(BREAKWATER);
    assert_eq!(at_first_path.id, second.id);
    assert_eq!(at_first_path.title, "Breakwater");
    assert_eq!(at_first_path.fingerprint, second.fingerprint);
    assert_eq!(at_second_path.id, first.id);
    assert_eq!(at_second_path.title, "Low Tide");
    assert_eq!(at_second_path.fingerprint, first.fingerprint);
    assert_eq!(scanned.env.count(), 10);
    assert_eq!(
        attached(&scanned.env.db, &first.id),
        Some((scanned.folder_id, BREAKWATER.to_owned()))
    );
    assert_eq!(
        attached(&scanned.env.db, &second.id),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );
}

#[test]
fn a_file_moved_over_another_track_keeps_its_id_and_the_other_row_is_deleted() {
    let library = LibraryBuilder::standard().build();
    shift_mtime(&library.files[0], 100);
    shift_mtime(&library.files[1], 200);
    let scanned = Scanned::of(library);
    let overwritten = scanned.id_at(LOW_TIDE);
    let moved = scanned.id_at(BREAKWATER);

    fs::rename(scanned.path(BREAKWATER), scanned.path(LOW_TIDE)).unwrap();
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            moved: 1,
            removed: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.id_at(LOW_TIDE), moved);
    assert_eq!(scanned.find(BREAKWATER), None);
    assert!(!scanned.env.locators().contains_key(&overwritten));
    assert_eq!(scanned.env.count(), 9);
}

#[test]
fn a_copy_gets_a_new_id_and_the_original_keeps_its_id() {
    let scanned = Scanned::new();
    let copy = "Copies/01 Low Tide.flac";
    let original = scanned.track_at(LOW_TIDE);
    attach(&scanned.env.db, &original.id);

    copy_with_mtime(&scanned.path(LOW_TIDE), &scanned.path(copy));
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            added: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.track_at(LOW_TIDE), original);
    let copied = scanned.track_at(copy);
    assert_ne!(copied.id, original.id);
    assert_eq!(copied.fingerprint, original.fingerprint);
    assert_eq!(
        attached(&scanned.env.db, &original.id),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );
    assert_eq!(attached(&scanned.env.db, &copied.id), None);
}

#[test]
fn one_of_two_identical_files_moves_and_keeps_its_own_id() {
    let library = LibraryBuilder::standard().build();
    let copy = "Copies/01 Low Tide.flac";
    copy_with_mtime(&library.files[0], &library.root.path().join(copy));
    let scanned = Scanned::of(library);
    let original = scanned.track_at(LOW_TIDE);
    let copied = scanned.track_at(copy);
    assert_eq!(original.fingerprint, copied.fingerprint);
    assert_ne!(original.id, copied.id);
    attach(&scanned.env.db, &copied.id);

    let to = "Elsewhere/01 Low Tide.flac";
    move_file(&scanned.path(copy), &scanned.path(to));
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            moved: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.id_at(to), copied.id);
    assert_eq!(scanned.id_at(LOW_TIDE), original.id);
    assert_eq!(scanned.find(copy), None);
    assert_eq!(
        attached(&scanned.env.db, &copied.id),
        Some((scanned.folder_id, to.to_owned()))
    );
}

#[test]
fn a_moved_track_with_two_copies_follows_the_copy_with_its_file_name() {
    let scanned = Scanned::new();
    let id = scanned.id_at(LOW_TIDE);
    attach(&scanned.env.db, &id);

    // The file is gone from its path. One copy is in the same directory
    // under another name, the other is far away under the same name.
    let renamed = "Aurora Field/Glass Harbour/renamed.flac";
    let same_name = "Zeta/Deep/01 Low Tide.flac";
    copy_with_mtime(&scanned.path(LOW_TIDE), &scanned.path(same_name));
    move_file(&scanned.path(LOW_TIDE), &scanned.path(renamed));
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            added: 1,
            moved: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.id_at(same_name), id);
    assert_ne!(scanned.id_at(renamed), id);
    assert_eq!(scanned.find(LOW_TIDE), None);
    assert_eq!(
        attached(&scanned.env.db, &id),
        Some((scanned.folder_id, same_name.to_owned()))
    );
}

#[test]
fn a_retag_and_a_move_in_one_interval_make_a_new_track() {
    // `docs/design.md` section 1.3: the fingerprint of the file changed, so
    // nothing connects the file at the new path to the row.
    let scanned = Scanned::new();
    let to = "Aurora Field/Glass Harbour/01 High Tide.flac";
    let id = scanned.id_at(LOW_TIDE);

    retitle(&scanned.path(LOW_TIDE), "High Tide");
    move_file(&scanned.path(LOW_TIDE), &scanned.path(to));
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 1,
            removed: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    assert_ne!(scanned.id_at(to), id);
}

/// Returns a WAV file with `data_len` bytes of samples: mono, 8 kHz, 16-bit
/// PCM. The samples are a fixed sequence in which no 64 KiB window repeats.
fn wav(data_len: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend((36 + data_len).to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes()); // PCM
    bytes.extend(1u16.to_le_bytes()); // channels
    bytes.extend(8000u32.to_le_bytes()); // sample rate
    bytes.extend(16000u32.to_le_bytes()); // bytes per second
    bytes.extend(2u16.to_le_bytes()); // bytes per frame
    bytes.extend(16u16.to_le_bytes()); // bits per sample
    bytes.extend(b"data");
    bytes.extend(data_len.to_le_bytes());
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let end = bytes.len() + data_len as usize;
    while bytes.len() < end {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend(state.to_le_bytes());
    }
    bytes.truncate(end);
    bytes
}

#[test]
fn a_file_larger_than_128_kib_is_identified_by_its_first_and_last_64_kib() {
    const WINDOW: usize = 64 * 1024;
    let library = LibraryBuilder::new().build();
    let root = library.root.path().to_path_buf();
    let from = "Field Recordings/harbour.wav";
    let mut bytes = wav(200 * 1024);
    assert!(bytes.len() > 2 * WINDOW);
    fs::create_dir(root.join("Field Recordings")).unwrap();
    fs::write(root.join(from), &bytes).unwrap();

    let scanned = Scanned::of(library);
    let track = scanned.track_at(from);
    let mut hashed = bytes[..WINDOW].to_vec();
    hashed.extend_from_slice(&bytes[bytes.len() - WINDOW..]);
    let expected = format!(
        "{}:{}",
        bytes.len(),
        hex::encode(&Sha256::digest(&hashed)[..16])
    );
    assert_eq!(track.fingerprint, expected);
    assert_eq!(track.size, bytes.len() as u64);
    assert_eq!(track.title, "harbour");
    assert_eq!(track.duration_ms, 12_800);

    // A byte between the two windows changes. The fingerprint stays, so
    // the file is the same track with other content.
    bytes[WINDOW + 1000] ^= 0xff;
    fs::write(root.join(from), &bytes).unwrap();
    set_mtime(
        &root.join(from),
        UNIX_EPOCH + Duration::from_secs(u64::try_from(track.mtime).unwrap() + 5),
    );
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 1,
            changed: 1,
            tags_read: 1,
            ..ScanSummary::default()
        }
    );
    let changed = scanned.track_at(from);
    assert_eq!(changed.id, track.id);
    assert_eq!(changed.fingerprint, expected);
    assert_eq!(changed.mtime, track.mtime + 5);

    // The file moves and keeps its ID.
    let to = "Archive/2019/harbour.wav";
    attach(&scanned.env.db, &track.id);
    move_file(&root.join(from), &root.join(to));
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 1,
            moved: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.id_at(to), track.id);
    assert_eq!(
        attached(&scanned.env.db, &track.id),
        Some((scanned.folder_id, to.to_owned()))
    );
}

/// Two music folders of the standard library, scanned once: "Aurora Field"
/// with seven tracks and "Marlow Quartet" with two.
struct TwoFolders {
    library: TestLibrary,
    env: TestDb,
    aurora: PathBuf,
    marlow: PathBuf,
}

impl TwoFolders {
    fn new() -> Self {
        let library = LibraryBuilder::standard().build();
        let aurora = library.root.path().join("Aurora Field");
        let marlow = library.root.path().join("Marlow Quartet");
        let env = TestDb::new();
        let summary = env.scan(&[&aurora, &marlow]);
        assert_eq!((summary.seen, summary.added), (9, 9));
        Self {
            library,
            env,
            aurora,
            marlow,
        }
    }

    fn rescan(&self) -> ScanSummary {
        self.env.scan(&[&self.aurora, &self.marlow])
    }

    /// Returns a path next to the music folders, outside both.
    fn outside(&self, name: &str) -> PathBuf {
        self.library.root.path().join(name)
    }
}

#[test]
fn a_folder_that_does_not_exist_deletes_nothing_and_is_named_in_the_error() {
    let two = TwoFolders::new();
    let before = two.env.index();
    let (aurora_id, marlow_id) = (
        two.env.folder_id(&two.aurora),
        two.env.folder_id(&two.marlow),
    );
    for entry in before.values() {
        attach(&two.env.db, &entry.id);
    }

    // A copy of a track of the first folder is in the second folder. The
    // path of the first folder does not exist, which is the state of a
    // volume that is not mounted.
    let copy = "01 Low Tide.flac";
    copy_with_mtime(
        &two.aurora.join("Glass Harbour/01 Low Tide.flac"),
        &two.marlow.join(copy),
    );
    fs::rename(&two.aurora, two.outside("unmounted")).unwrap();
    let summary = two.rescan();

    assert_eq!(summary.seen, 3);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.moved, 0);
    assert_eq!(summary.added, 1);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Aurora Field\""), "{error}");
    assert!(error.contains(&two.aurora.display().to_string()), "{error}");
    assert!(error.contains("cannot be read"), "{error}");
    assert_eq!(two.env.latest_scan().error.as_ref(), Some(error));
    assert_eq!(two.env.latest_scan().counts.removed, 0);

    // Every row is as it was. The copy did not take the row of its
    // original: it is a new row.
    let mut after = two.env.index();
    let copied = after.remove(&(marlow_id, copy.to_owned())).unwrap();
    assert_eq!(after, before);
    assert!(before.values().all(|entry| entry.id != copied.id));
    assert_eq!(
        after
            .keys()
            .filter(|(folder_id, _)| *folder_id == aurora_id)
            .count(),
        7
    );
    for (locator, entry) in &before {
        assert_eq!(attached(&two.env.db, &entry.id).as_ref(), Some(locator));
    }

    // The folder is back. The next scan finds every file where its row is.
    fs::rename(two.outside("unmounted"), &two.aurora).unwrap();
    assert_eq!(two.rescan(), seen(10));
    assert_eq!(two.env.latest_scan().error, None);
}

#[test]
fn a_folder_path_that_is_a_file_deletes_nothing_and_is_named_in_the_error() {
    let two = TwoFolders::new();
    let before = two.env.index();

    fs::rename(&two.marlow, two.outside("unmounted")).unwrap();
    fs::write(&two.marlow, "not a directory").unwrap();
    let summary = two.rescan();

    assert_eq!(summary.seen, 7);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Marlow Quartet\""), "{error}");
    assert!(error.contains("is not a directory"), "{error}");
    assert_eq!(two.env.latest_scan().error.as_ref(), Some(error));
    assert_eq!(two.env.index(), before);
}

#[test]
fn a_folder_that_had_tracks_and_is_empty_deletes_nothing_and_is_named_in_the_error() {
    let two = TwoFolders::new();
    let before = two.env.index();

    // The directory of the second folder stays and its content is gone,
    // which is the state of a mount point whose volume is not mounted.
    fs::rename(two.marlow.join("Slow Rivers"), two.outside("elsewhere")).unwrap();
    let summary = two.rescan();

    assert_eq!(summary.seen, 7);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Marlow Quartet\""), "{error}");
    assert!(error.contains(&two.marlow.display().to_string()), "{error}");
    assert!(error.contains("has no supported file"), "{error}");
    assert_eq!(two.env.latest_scan().error.as_ref(), Some(error));
    assert_eq!(two.env.index(), before);

    // The content is back.
    fs::rename(two.outside("elsewhere"), two.marlow.join("Slow Rivers")).unwrap();
    assert_eq!(two.rescan(), seen(9));
    assert_eq!(two.env.index(), before);
}

#[test]
fn an_empty_folder_that_never_had_tracks_is_no_error() {
    let library = LibraryBuilder::standard().build();
    let empty = library.root.path().join("Empty");
    fs::create_dir(&empty).unwrap();
    let env = TestDb::new();
    let summary = env.scan(&[&empty]);
    assert_eq!(summary, ScanSummary::default());
    assert_eq!(env.latest_scan().error, None);
}

#[test]
fn the_tracks_and_the_row_of_a_folder_removed_from_the_config_are_deleted() {
    let two = TwoFolders::new();
    let marlow_id = two.env.folder_id(&two.marlow);
    let kept: BTreeMap<Locator, IndexEntry> = two
        .env
        .index()
        .into_iter()
        .filter(|((folder_id, _), _)| *folder_id == marlow_id)
        .collect();

    let summary = two.env.scan(&[&two.marlow]);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 2,
            removed: 7,
            ..ScanSummary::default()
        }
    );
    assert_eq!(two.env.index(), kept);
    let folders = two.env.folders();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].id, marlow_id);
}

#[test]
fn tracks_of_a_removed_folder_are_kept_while_a_configured_folder_has_no_supported_file() {
    let library = LibraryBuilder::standard().build();
    let old = library.root.path().join("Aurora Field");
    let env = TestDb::new();
    assert_eq!(env.scan(&[&old]).added, 7);
    let before = env.locators();

    // The config names a new path whose volume is not mounted yet: the
    // directory exists and is empty.
    let new = library.root.path().join("mount point");
    fs::create_dir(&new).unwrap();
    let summary = env.scan(&[&new]);

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Aurora Field\""), "{error}");
    assert!(error.contains("were kept"), "{error}");
    assert_eq!(env.latest_scan().error.as_ref(), Some(error));
    assert_eq!(env.locators(), before);
    assert_eq!(env.folders().len(), 2);

    // The files appear at the new path. Every track keeps its ID, and the
    // row of the old path goes.
    for album in ["Glass Harbour", "Paper Maps", "Summer Sampler"] {
        fs::rename(old.join(album), new.join(album)).unwrap();
    }
    let summary = env.scan(&[&new]);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 7,
            moved: 7,
            ..ScanSummary::default()
        }
    );
    let folders = env.folders();
    assert_eq!(folders.len(), 1);
    let expected: BTreeMap<String, Locator> = before
        .into_iter()
        .map(|(id, (_, rel_path))| (id, (folders[0].id, rel_path)))
        .collect();
    assert_eq!(env.locators(), expected);
}

#[test]
fn tracks_of_a_removed_folder_are_kept_while_a_configured_folder_does_not_exist() {
    let two = TwoFolders::new();
    let before = two.env.index();

    // The config drops the first folder, and the second one is gone.
    fs::rename(&two.marlow, two.outside("unmounted")).unwrap();
    let summary = two.env.scan(&[&two.marlow]);

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Marlow Quartet\""));
    assert!(summary.errors[1].contains("\"Aurora Field\""));
    assert_eq!(two.env.latest_scan().error, Some(summary.errors.join("; ")));
    assert_eq!(two.env.index(), before);
    assert_eq!(two.env.folders().len(), 2);
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege that a test run does not have.
#[cfg(unix)]
#[test]
fn tracks_below_a_link_whose_target_is_gone_are_kept() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    // The directory of one artist is on another volume, behind a link.
    let volume = tempfile::tempdir().unwrap();
    let target = volume.path().join("Marlow Quartet");
    let link = root.join("Marlow Quartet");
    fs::rename(&link, &target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    // The scan follows the link.
    let scanned = Scanned::of(library);
    assert_eq!(scanned.env.count(), 10);
    let before = scanned.env.index();

    // The volume goes away. The link stays and leads nowhere.
    let unmounted = volume.path().join("unmounted");
    fs::rename(&target, &unmounted).unwrap();
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 8);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Marlow Quartet\""), "{error}");
    assert!(error.contains("were kept"), "{error}");
    assert_eq!(scanned.env.index(), before);

    // The volume is back.
    fs::rename(&unmounted, &target).unwrap();
    assert_eq!(scanned.rescan(), seen(10));
    assert_eq!(scanned.env.index(), before);
}

#[test]
fn cancelling_after_the_first_batch_keeps_the_rows_of_that_batch_only() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path().to_path_buf();
    let env = TestDb::new();
    let cancel = CancellationToken::new();
    let hook_token = cancel.clone();
    let batches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&batches);
    let progress = Arc::new(AtomicU64::new(0));

    let summary = scanner::scan(
        &env.db,
        std::slice::from_ref(&root),
        &cancel,
        false,
        ScanOptions {
            batch_size: 2,
            on_batch: Some(Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                hook_token.cancel();
            })),
            progress: Arc::clone(&progress),
        },
    )
    .unwrap();

    assert_eq!(batches.load(Ordering::Relaxed), 1);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 2,
            tags_read: 2,
            errors: vec![CANCELLED.to_owned()],
            ..ScanSummary::default()
        }
    );
    assert_eq!(progress.load(Ordering::Relaxed), 2);
    // The walk yields the entries of a directory in name order, so the
    // first batch holds the first two tracks.
    let folder_id = env.folder_id(&root);
    let committed = env.index();
    assert_eq!(
        committed.keys().cloned().collect::<Vec<_>>(),
        [
            (folder_id, LOW_TIDE.to_owned()),
            (folder_id, BREAKWATER.to_owned())
        ]
    );
    let scan = env.latest_scan();
    assert_eq!(scan.error.as_deref(), Some("cancelled"));
    assert!(scan.finished_at.is_some());
    assert_eq!(
        scan.counts,
        ScanCounts {
            seen: 10,
            added: 2,
            ..ScanCounts::default()
        }
    );

    // The next scan adds the rest and keeps the two rows.
    let summary = env.scan(&[&root]);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 8,
            tags_read: 8,
            ..ScanSummary::default()
        }
    );
    let complete = env.index();
    assert_eq!(complete.len(), 10);
    for (locator, entry) in &committed {
        assert_eq!(complete.get(locator), Some(entry));
    }
}

#[test]
fn a_scan_that_is_cancelled_before_it_starts_writes_no_track() {
    let scanned = Scanned::new();
    let before = scanned.env.index();
    fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
    write_other_track(&scanned.path("New/01 Other Title.flac"));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let summary = scanner::scan(
        &scanned.env.db,
        &[scanned.root().to_path_buf()],
        &cancel,
        false,
        ScanOptions::default(),
    )
    .unwrap();

    assert_eq!(summary.errors, [CANCELLED]);
    assert_eq!((summary.added, summary.removed), (0, 0));
    assert_eq!(scanned.env.index(), before);
    assert_eq!(
        scanned.env.latest_scan().error.as_deref(),
        Some("cancelled")
    );
}

#[test]
fn each_batch_holds_batch_size_files_and_runs_the_hook_once() {
    let library = LibraryBuilder::standard().build();
    for (batch_size, expected) in [(3, 4), (10, 1), (0, 10)] {
        let env = TestDb::new();
        let batches = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&batches);
        let summary = env.scan_with(
            &[library.root.path()],
            false,
            ScanOptions {
                batch_size,
                on_batch: Some(Box::new(move || {
                    counted.fetch_add(1, Ordering::Relaxed);
                })),
                ..ScanOptions::default()
            },
        );
        assert_eq!(summary.added, 10);
        assert_eq!(
            batches.load(Ordering::Relaxed),
            expected,
            "batch size {batch_size}"
        );
    }
}

#[test]
fn a_full_scan_reads_every_file_again_and_keeps_every_id() {
    let scanned = Scanned::new();
    let before = scanned.env.index();

    let summary = scanned
        .env
        .scan_with(&[scanned.root()], true, ScanOptions::default());

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 10,
            tags_read: 10,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.env.index(), before);
}

#[test]
fn unsupported_extensions_and_names_that_start_with_a_dot_are_skipped() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    let audio = &library.files[0];
    fs::write(root.join("notes.txt"), "not audio").unwrap();
    fs::copy(audio, root.join("track.wma")).unwrap();
    fs::copy(audio, root.join("no extension")).unwrap();
    fs::copy(audio, root.join(".hidden.flac")).unwrap();
    fs::create_dir(root.join(".hidden")).unwrap();
    fs::copy(audio, root.join(".hidden").join("01 Inside.flac")).unwrap();
    fs::create_dir(root.join("Aurora Field").join(".trash")).unwrap();
    fs::copy(
        audio,
        root.join("Aurora Field")
            .join(".trash")
            .join("02 Inside.flac"),
    )
    .unwrap();
    // The extension is matched without regard to case.
    fs::create_dir(root.join("Upper")).unwrap();
    fs::copy(audio, root.join("Upper").join("SHOUT.FLAC")).unwrap();

    let scanned = Scanned::of(library);

    let mut expected: Vec<&str> = STANDARD.to_vec();
    expected.push("Upper/SHOUT.FLAC");
    expected.sort_unstable();
    let stored: Vec<String> = scanned
        .env
        .index()
        .into_keys()
        .map(|(_, rel_path)| rel_path)
        .collect();
    assert_eq!(stored, expected);
    assert_eq!(scanned.track_at("Upper/SHOUT.FLAC").suffix, "flac");
    // The scan counts the supported files only.
    assert_eq!(scanned.env.latest_scan().counts.seen, 11);
    assert_eq!(scanned.rescan(), seen(11));
}

// A name that is not valid Unicode is built from raw bytes, which only Unix
// allows.
#[cfg(unix)]
#[test]
fn names_that_are_not_valid_unicode_are_skipped() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    let audio = &library.files[0];
    let file = root.join(OsStr::from_bytes(b"bad\xff.flac"));
    // APFS and HFS+ refuse a name that is not valid UTF-8, so on macOS no
    // such file exists and the test has nothing to check. The file systems
    // of Linux accept the name.
    if let Err(error) = fs::copy(audio, &file) {
        eprintln!("not checked: the file system refuses the name: {error}");
        return;
    }
    let dir = root.join(OsStr::from_bytes(b"dir\xff"));
    fs::create_dir(&dir).unwrap();
    fs::copy(audio, dir.join("01 Inside.flac")).unwrap();

    let env = TestDb::new();
    let summary = env.scan(&[root]);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 10,
            tags_read: 10,
            ..ScanSummary::default()
        }
    );
    assert_eq!(env.count(), 10);
}

#[test]
fn a_file_that_cannot_be_parsed_is_skipped_and_the_row_at_its_path_stays() {
    let scanned = Scanned::new();
    let before = scanned.track_at(LOW_TIDE);

    // One file that is not audio is new, another replaces a track.
    let noise = "Broken/noise.flac";
    fs::create_dir(scanned.path("Broken")).unwrap();
    fs::write(scanned.path(noise), b"not audio").unwrap();
    fs::write(scanned.path(LOW_TIDE), b"not audio either").unwrap();
    shift_mtime(&scanned.path(LOW_TIDE), 5);
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            tags_read: 2,
            ..ScanSummary::default()
        }
    );
    assert_eq!(scanned.track_at(LOW_TIDE), before);
    assert_eq!(scanned.find(noise), None);
    assert_eq!(scanned.env.count(), 10);
    assert_eq!(scanned.env.latest_scan().error, None);
}

#[tokio::test]
async fn scan_runs_on_a_blocking_thread_of_the_runtime() {
    let library = LibraryBuilder::standard().build();
    let env = TestDb::new();
    let folders = vec![library.root.path().to_path_buf()];
    let db = env.db.clone();

    let summary = tokio::task::spawn_blocking(move || {
        scanner::scan(
            &db,
            &folders,
            &CancellationToken::new(),
            false,
            ScanOptions::default(),
        )
    })
    .await
    .unwrap()
    .unwrap();

    assert_eq!(summary.added, 10);
    assert_eq!(env.db.read(tracks::count_tracks).await.unwrap(), 10);
}
