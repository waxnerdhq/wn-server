//! Tests of `library::scanner` on libraries that `LibraryBuilder` writes,
//! with a database in a temp file.
//!
//! A file system stores modification times that the scanner compares in
//! whole seconds, and a test runs in less than one. A test that changes a
//! file in place therefore moves its modification time with
//! [`shift_mtime`], and a test that copies a file gives the copy the time of
//! the original with [`copy_with_mtime`]. [`retag`] moves the time of the
//! file it changes.
//!
//! The first part of the file tests the track rows. The second part, from
//! [`Album`] on, tests the album rows, the artist rows, the annotations and
//! the covers that the scan derives from the tracks.

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
use wn_server::library::tags;

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

/// Returns the summary of a scan that left the four albums and the three
/// artists of the standard catalogue in the database, with no other count.
fn standard() -> ScanSummary {
    ScanSummary {
        albums: 4,
        artists: 3,
        ..ScanSummary::default()
    }
}

/// Returns a summary with `seen` files, the albums and the artists of the
/// standard catalogue, and no other count.
fn seen(seen: u64) -> ScanSummary {
    ScanSummary { seen, ..standard() }
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            albums: 5,
            artists: 4,
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            albums: 5,
            artists: 4,
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

/// Scans the one folder of `scanned` in batches of `batch_size` and returns
/// the summary and the number of batches that were committed.
fn rescan_in_batches_of(scanned: &Scanned, batch_size: usize) -> (ScanSummary, usize) {
    let batches = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&batches);
    let summary = scanned.env.scan_with(
        &[scanned.root()],
        false,
        ScanOptions {
            batch_size,
            on_batch: Some(Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
            })),
            ..ScanOptions::default()
        },
    );
    (summary, batches.load(Ordering::Relaxed))
}

#[test]
fn two_files_that_exchange_paths_keep_their_ids() {
    exchange_two_paths(scanner::DEFAULT_BATCH_SIZE);
}

#[test]
fn two_files_exchange_paths_in_one_transaction_when_a_batch_holds_one_file() {
    // Stored one at a time, the first row would take a path that the
    // second row still has.
    exchange_two_paths(1);
}

fn exchange_two_paths(batch_size: usize) {
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
    let (summary, batches) = rescan_in_batches_of(&scanned, batch_size);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            moved: 2,
            ..standard()
        }
    );
    assert_eq!(batches, 1);
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
    move_over_another_track(scanner::DEFAULT_BATCH_SIZE);
}

#[test]
fn a_file_moves_over_another_track_in_one_transaction_when_a_batch_holds_one_file() {
    move_over_another_track(1);
}

fn move_over_another_track(batch_size: usize) {
    let library = LibraryBuilder::standard().build();
    shift_mtime(&library.files[0], 100);
    shift_mtime(&library.files[1], 200);
    let scanned = Scanned::of(library);
    let overwritten = scanned.id_at(LOW_TIDE);
    let moved = scanned.id_at(BREAKWATER);

    fs::rename(scanned.path(BREAKWATER), scanned.path(LOW_TIDE)).unwrap();
    let (summary, batches) = rescan_in_batches_of(&scanned, batch_size);

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            moved: 1,
            removed: 1,
            ..standard()
        }
    );
    assert_eq!(batches, 1);
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            albums: 1,
            artists: 1,
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
            albums: 1,
            artists: 1,
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
            albums: 1,
            artists: 1,
            ..ScanSummary::default()
        }
    );
    assert_eq!(two.env.index(), kept);
    let folders = two.env.folders();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].id, marlow_id);
}

#[test]
fn a_scan_without_a_configured_folder_deletes_nothing() {
    let scanned = Scanned::new();
    let before = scanned.env.index();

    let summary = scanned.env.scan(&[]);

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("no music folder is configured"));
    assert_eq!(scanned.env.index(), before);
    assert_eq!(scanned.env.folders().len(), 1);
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
            albums: 3,
            artists: 2,
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
    // The directory of one artist is in a directory of the folder that the
    // walk skips, behind a link.
    let store = root.join(".store");
    fs::create_dir(&store).unwrap();
    let target = store.join("Marlow Quartet");
    let link = root.join("Marlow Quartet");
    fs::rename(&link, &target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    // The scan follows the link.
    let scanned = Scanned::of(library);
    assert_eq!(scanned.env.count(), 10);
    let before = scanned.env.index();

    // The directory goes away. The link stays and leads nowhere.
    let unmounted = store.join("unmounted");
    fs::rename(&target, &unmounted).unwrap();
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 8);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Marlow Quartet\""), "{error}");
    assert!(error.contains("cannot be read"), "{error}");
    assert!(error.contains("no track was deleted"), "{error}");
    assert_eq!(scanned.env.index(), before);

    // The directory is back.
    fs::rename(&unmounted, &target).unwrap();
    assert_eq!(scanned.rescan(), seen(10));
    assert_eq!(scanned.env.index(), before);
}

#[test]
fn a_missing_file_keeps_its_row_while_a_folder_cannot_be_read() {
    let two = TwoFolders::new();
    let before = two.env.index();
    let (aurora_id, marlow_id) = (
        two.env.folder_id(&two.aurora),
        two.env.folder_id(&two.marlow),
    );
    let moved_from = "Glass Harbour/01 Low Tide.flac";
    let moved_to = "Imports/01 Low Tide.flac";
    let deleted = "Glass Harbour/02 Breakwater.flac";
    let moved = two.env.id_at(aurora_id, moved_from).unwrap();
    attach(&two.env.db, &moved);

    // One file of the first folder moves to the second folder and another
    // one is deleted. Then the second folder cannot be read. The scan does
    // not know which of the two files is in it.
    move_file(&two.aurora.join(moved_from), &two.marlow.join(moved_to));
    fs::remove_file(two.aurora.join(deleted)).unwrap();
    fs::rename(&two.marlow, two.outside("unmounted")).unwrap();
    let summary = two.rescan();

    assert_eq!(summary.seen, 5);
    assert_eq!((summary.moved, summary.removed), (0, 0));
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Marlow Quartet\""));
    assert!(summary.errors[0].contains("no track was deleted"));
    assert_eq!(
        summary.errors[1],
        "2 tracks whose files are gone were kept, because a path or a file could not be read"
    );
    assert_eq!(two.env.latest_scan().error, Some(summary.errors.join("; ")));
    assert_eq!(two.env.index(), before);
    assert_eq!(
        attached(&two.env.db, &moved),
        Some((aurora_id, moved_from.to_owned()))
    );

    // The second folder is back. The moved file keeps its ID and the row
    // of the deleted file goes.
    fs::rename(two.outside("unmounted"), &two.marlow).unwrap();
    let summary = two.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 8,
            moved: 1,
            removed: 1,
            ..standard()
        }
    );
    assert_eq!(two.env.id_at(marlow_id, moved_to), Some(moved.clone()));
    assert_eq!(two.env.id_at(aurora_id, deleted), None);
    assert_eq!(
        attached(&two.env.db, &moved),
        Some((marlow_id, moved_to.to_owned()))
    );
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege that a test run does not have.
#[cfg(unix)]
#[test]
fn a_file_moved_below_a_link_whose_target_is_gone_keeps_its_id() {
    let scanned = Scanned::new();
    let id = scanned.id_at(LOW_TIDE);
    attach(&scanned.env.db, &id);
    let before = scanned.env.index();

    // The file moves to a directory of the folder that the walk skips and
    // that the library reaches through a link. The directory is not where
    // the link points.
    let mounted = scanned.path(".store/archive");
    let unmounted = scanned.path(".store/unmounted");
    fs::create_dir_all(&unmounted).unwrap();
    fs::rename(scanned.path(LOW_TIDE), unmounted.join("01 Low Tide.flac")).unwrap();
    std::os::unix::fs::symlink(&mounted, scanned.path("Archive")).unwrap();
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 9);
    assert_eq!((summary.moved, summary.removed), (0, 0));
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Archive\""));
    assert!(summary.errors[1].starts_with("1 track whose file is gone was kept"));
    assert_eq!(scanned.env.index(), before);
    assert_eq!(
        attached(&scanned.env.db, &id),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );

    // The directory is where the link points, and the scan reads the file.
    fs::rename(&unmounted, &mounted).unwrap();
    assert_moved(&scanned, &id, LOW_TIDE, "Archive/01 Low Tide.flac");
}

// The tests of the symbolic links that the walk resolves. A symbolic link is
// created with a Unix call. On Windows the call needs a privilege that a
// test run does not have, so these tests run on Unix only. The unit tests in
// `src/library/scanner.rs` compare the target of a link with the folders on
// every OS.
#[cfg(unix)]
mod links {
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::mpsc;

    use super::common::{LogBuffer, captured_log};
    use super::*;

    /// Returns the relative path of every track row, in the order of the
    /// locators.
    fn stored(env: &TestDb) -> Vec<String> {
        env.index()
            .into_keys()
            .map(|(_, rel_path)| rel_path)
            .collect()
    }

    /// Returns the relative paths of the standard catalogue and `more`, in
    /// their order.
    fn standard_paths_and(more: &[&str]) -> Vec<String> {
        let mut paths: Vec<String> = STANDARD
            .iter()
            .chain(more)
            .map(|&path| path.to_owned())
            .collect();
        paths.sort_unstable();
        paths
    }

    /// Writes an audio file with the title `title`. Files with different
    /// titles have different bytes, and each differs from every file of the
    /// standard library.
    fn write_track(to: &Path, title: &str) {
        let other = LibraryBuilder::new()
            .track("Linked Artist", "Linked Album", title, 1, "flac")
            .build();
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(&other.files[0], to).unwrap();
    }

    /// Returns the last component of a relative path.
    fn file_name(rel_path: &str) -> &str {
        rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
    }

    /// Asserts that the latest scan was the first scan of a library whose
    /// rows are the ten standard tracks, and that it recorded no error.
    fn assert_first_scan_stored_the_standard_tracks(env: &TestDb) {
        assert_eq!(stored(env), standard_paths_and(&[]));
        let scan = env.latest_scan();
        assert_eq!(scan.error, None);
        assert_eq!(
            scan.counts,
            ScanCounts {
                seen: 10,
                added: 10,
                ..ScanCounts::default()
            }
        );
    }

    /// Runs a scan that is not full on a thread of its own and returns its
    /// summary. A walk that follows a link out of the folders or around a
    /// loop does not end, and a scan that opens a named pipe waits for a
    /// writer. The test therefore fails when the scan has not ended after
    /// ten seconds. A scan that ends is not waited for.
    fn scan_with_limit(env: &TestDb, folders: &[&Path]) -> ScanSummary {
        let folders: Vec<PathBuf> = folders.iter().map(|path| path.to_path_buf()).collect();
        let db = env.db.clone();
        let cancel = CancellationToken::new();
        let (done, result) = mpsc::channel();
        let thread = std::thread::spawn({
            let cancel = cancel.clone();
            move || {
                let summary = scanner::scan(&db, &folders, &cancel, false, ScanOptions::default());
                // The receiver is gone when the test has failed.
                let _ = done.send(summary);
            }
        });
        match result.recv_timeout(Duration::from_secs(10)) {
            Ok(summary) => summary.expect("the scan returns a summary"),
            // The thread ended without a summary.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread.join().expect("the scan does not panic");
                unreachable!("the scan thread sends its summary");
            }
            // The thread is not joined: it may wait in a call that the
            // cancellation does not end.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancel.cancel();
                panic!("the scan did not end within ten seconds");
            }
        }
    }

    /// The lines of the debug log that say that the scan skipped a link
    /// below `root` whose target is outside the folders.
    fn skipped_links(log: &LogBuffer, root: &Path) -> Vec<String> {
        log.lines_with("the link points outside the music folders")
            .into_iter()
            .filter(|line| line.contains(&format!("link={}/", root.display())))
            .inspect(|line| assert!(line.contains("DEBUG"), "{line}"))
            .collect()
    }

    /// The lines of the info log that count the links of the folder `root`
    /// whose target is outside the folders. A scan with such a link writes
    /// one line.
    fn counted_links(log: &LogBuffer, root: &Path) -> Vec<String> {
        log.lines_with("symbolic links that point outside the music folders were not followed")
            .into_iter()
            .filter(|line| line.contains(&format!("folder={} links=", root.display())))
            .inspect(|line| assert!(line.contains(" INFO "), "{line}"))
            .collect()
    }

    #[test]
    fn a_link_that_points_outside_the_folders_gives_no_row_and_no_error() {
        let log = captured_log();
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // An album and a file outside the music folder.
        let outside = tempfile::tempdir().unwrap();
        write_track(&outside.path().join("Album/01 Elsewhere.flac"), "Elsewhere");
        write_track(&outside.path().join("single.flac"), "Single");
        // A link to the directory, a link to the file, a link to the first
        // link, and a link with the name of a cover image.
        let album = root.join("Aurora Field/Glass Harbour");
        symlink(outside.path().join("Album"), root.join("Linked Album")).unwrap();
        symlink(
            outside.path().join("single.flac"),
            album.join("04 Single.flac"),
        )
        .unwrap();
        symlink("Linked Album", root.join("Second Link")).unwrap();
        symlink(
            outside.path().join("single.flac"),
            root.join("Aurora Field/Paper Maps/cover.jpg"),
        )
        .unwrap();

        let scanned = Scanned::of(library);
        assert_first_scan_stored_the_standard_tracks(&scanned.env);

        // Each link is in the debug log once, and their number is in one
        // line of the info log that names the folder.
        let skipped = skipped_links(&log, &root);
        assert_eq!(skipped.len(), 4, "{skipped:?}");
        for name in ["Linked Album", "04 Single.flac", "Second Link", "cover.jpg"] {
            let named = skipped
                .iter()
                .filter(|line| line.contains(&format!("/{name} target=")));
            assert_eq!(named.count(), 1, "{name}: {skipped:?}");
        }
        let counted = counted_links(&log, &root);
        assert_eq!(counted.len(), 1, "{counted:?}");
        assert!(counted[0].ends_with(" links=4"), "{counted:?}");

        // The links hold no deletion back: a file that is gone loses its
        // row.
        fs::remove_file(scanned.path(BREAKWATER)).unwrap();
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 9,
                removed: 1,
                ..standard()
            }
        );
        assert_eq!(scanned.find(BREAKWATER), None);
        assert_eq!(scanned.env.latest_scan().error, None);
    }

    #[test]
    fn links_to_the_root_directory_and_to_a_device_are_not_followed() {
        let log = captured_log();
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // The root directory, and a device under the name of an audio file.
        symlink("/", root.join("Everything")).unwrap();
        symlink("/dev/null", root.join("Aurora Field/null.flac")).unwrap();

        let env = TestDb::new();
        scan_with_limit(&env, &[&root]);
        assert_first_scan_stored_the_standard_tracks(&env);
        // Both links are skipped as links out of the folders. A walk of the
        // root directory that ends within the limit is therefore no pass.
        let skipped = skipped_links(&log, &root);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped[0].ends_with("/Aurora Field/null.flac target=/dev/null"));
        assert!(skipped[1].ends_with("/Everything target=/"));
        let counted = counted_links(&log, &root);
        assert_eq!(counted.len(), 1, "{counted:?}");
        assert!(counted[0].ends_with(" links=2"), "{counted:?}");

        // The links hold no deletion back.
        fs::remove_file(root.join(BREAKWATER)).unwrap();
        assert_eq!(
            scan_with_limit(&env, &[&root]),
            ScanSummary {
                seen: 9,
                removed: 1,
                ..standard()
            }
        );
    }

    #[test]
    fn links_to_directories_above_the_link_start_no_loop() {
        let log = captured_log();
        // The music folder is one directory of the library, so that the
        // directory above the folder is a directory of the test.
        let library = LibraryBuilder::standard().build();
        let folder = library.root.path().join("Aurora Field");
        // The directory that holds the link, the directory above that one,
        // and the folder itself. The walk reads each of them without the
        // link.
        symlink(".", folder.join("Glass Harbour/here")).unwrap();
        symlink("..", folder.join("Glass Harbour/up")).unwrap();
        symlink(&folder, folder.join("Paper Maps/whole folder")).unwrap();
        // The directory above the folder is outside the folder.
        symlink("..", folder.join("above")).unwrap();

        let env = TestDb::new();
        let first = scan_with_limit(&env, &[&folder]);
        assert_eq!(first.errors, [""; 0]);
        assert_eq!((first.seen, first.added), (7, 7));
        let expected: Vec<String> = standard_paths_and(&[])
            .into_iter()
            .filter_map(|path| path.strip_prefix("Aurora Field/").map(str::to_owned))
            .collect();
        assert_eq!(stored(&env), expected);
        let skipped = skipped_links(&log, &folder);
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("/above target="), "{skipped:?}");

        let before = env.index();
        let second = scan_with_limit(&env, &[&folder]);
        assert_eq!((second.seen, second.added, second.moved), (7, 0, 0));
        assert_eq!(env.index(), before);

        // The links hold no deletion back.
        fs::remove_file(folder.join("Glass Harbour/02 Breakwater.flac")).unwrap();
        let third = scan_with_limit(&env, &[&folder]);
        assert_eq!((third.seen, third.removed), (6, 1));
        assert_eq!(third.errors, [""; 0]);
    }

    #[test]
    fn a_named_pipe_and_a_link_to_one_are_not_opened() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // A named pipe under the name of an audio file. A second pipe is in
        // a directory that the walk skips, behind a link with the name of an
        // audio file. A process that opens a pipe to read it waits for a
        // writer.
        fs::create_dir(root.join(".store")).unwrap();
        for pipe in ["Tessa Lund/pipe.flac", ".store/pipe"] {
            let made = Command::new("mkfifo").arg(root.join(pipe)).status();
            if !made.is_ok_and(|status| status.success()) {
                eprintln!("not checked: mkfifo did not create a named pipe");
                return;
            }
        }
        symlink("../.store/pipe", root.join("Tessa Lund/piped.flac")).unwrap();

        let env = TestDb::new();
        scan_with_limit(&env, &[&root]);
        assert_first_scan_stored_the_standard_tracks(&env);
        assert_eq!(scan_with_limit(&env, &[&root]), seen(10));
    }

    #[test]
    fn a_link_to_a_file_or_a_directory_that_the_walk_reaches_adds_no_row() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // A relative link to a file, and two links to directories.
        fs::create_dir(root.join("Favourites")).unwrap();
        symlink(
            "../Aurora Field/Glass Harbour/01 Low Tide.flac",
            root.join("Favourites/low tide.flac"),
        )
        .unwrap();
        symlink(
            root.join("Aurora Field/Paper Maps"),
            root.join("Favourites/Paper Maps"),
        )
        .unwrap();
        symlink(root.join("Marlow Quartet"), root.join("Linked Artist")).unwrap();

        // Each file has one row, at its own path.
        let scanned = Scanned::of(library);
        assert_first_scan_stored_the_standard_tracks(&scanned.env);

        // The rows stay at these paths from scan to scan.
        let before = scanned.env.index();
        for _ in 0..2 {
            assert_eq!(scanned.rescan(), seen(10));
            assert_eq!(scanned.env.index(), before);
        }

        // The links hold no deletion back.
        fs::remove_file(scanned.path(BREAKWATER)).unwrap();
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 9,
                removed: 1,
                ..standard()
            }
        );
    }

    #[test]
    fn a_link_to_a_file_or_a_directory_that_the_walk_skips_is_followed() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // The directory of one artist and the file of one track are in a
        // directory of the folder that the walk skips. Links are at their
        // paths.
        let store = root.join(".store");
        fs::create_dir(&store).unwrap();
        fs::rename(root.join("Marlow Quartet"), store.join("Marlow Quartet")).unwrap();
        symlink(".store/Marlow Quartet", root.join("Marlow Quartet")).unwrap();
        fs::rename(root.join(LOW_TIDE), store.join("low tide.flac")).unwrap();
        symlink(store.join("low tide.flac"), root.join(LOW_TIDE)).unwrap();

        // Each file has its row at the path through its link.
        let scanned = Scanned::of(library);
        assert_first_scan_stored_the_standard_tracks(&scanned.env);
        let before = scanned.env.index();
        assert_eq!(scanned.rescan(), seen(10));
        assert_eq!(scanned.env.index(), before);

        // A second link to the directory and a second link to the file,
        // both after the first ones in the order of the walk, add no row.
        let album = "Aurora Field/Glass Harbour";
        symlink(store.join("Marlow Quartet"), root.join("Second Link")).unwrap();
        symlink(
            store.join("low tide.flac"),
            scanned.path(&format!("{album}/09 Again.flac")),
        )
        .unwrap();
        for _ in 0..2 {
            assert_eq!(scanned.rescan(), seen(10));
            assert_eq!(scanned.env.index(), before);
        }

        // A link that comes before the first one in the order of the walk
        // takes the file. The row follows with its ID.
        let id = scanned.id_at(LOW_TIDE);
        attach(&scanned.env.db, &id);
        let first = format!("{album}/00 First.flac");
        symlink(store.join("low tide.flac"), scanned.path(&first)).unwrap();
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 10,
                moved: 1,
                ..standard()
            }
        );
        assert_eq!(scanned.find(LOW_TIDE), None);
        assert_eq!(
            attached(&scanned.env.db, &id),
            Some((scanned.folder_id, first))
        );
        assert_eq!(scanned.rescan(), seen(10));
    }

    #[test]
    fn a_link_into_another_folder_adds_no_row_for_a_file_that_the_walk_reaches_there() {
        let two = TwoFolders::new();
        let before = two.env.index();
        let (aurora_id, marlow_id) = (
            two.env.folder_id(&two.aurora),
            two.env.folder_id(&two.marlow),
        );
        // A link to a directory and a link to a file from the first folder
        // into the second one, and a link from the second folder to the
        // first one.
        symlink(two.marlow.join("Slow Rivers"), two.aurora.join("Guests")).unwrap();
        symlink(
            two.marlow.join("Slow Rivers/01 Upstream.m4a"),
            two.aurora.join("Paper Maps/09 Upstream.m4a"),
        )
        .unwrap();
        symlink(&two.aurora, two.marlow.join("Aurora Field")).unwrap();
        for _ in 0..2 {
            assert_eq!(two.rescan(), seen(9));
            assert_eq!(two.env.index(), before);
        }

        // A file of the second folder moves to a directory that the walk of
        // that folder skips. A link in the first folder leads to it, so the
        // row follows the file to the path of the link.
        let from = "Slow Rivers/02 Delta.m4a";
        let to = "Paper Maps/08 Delta.m4a";
        let id = two.env.id_at(marlow_id, from).unwrap();
        attach(&two.env.db, &id);
        move_file(&two.marlow.join(from), &two.marlow.join(".store/delta.m4a"));
        symlink(two.marlow.join(".store/delta.m4a"), two.aurora.join(to)).unwrap();
        assert_eq!(
            two.rescan(),
            ScanSummary {
                seen: 9,
                moved: 1,
                ..standard()
            }
        );
        assert_eq!(attached(&two.env.db, &id), Some((aurora_id, to.to_owned())));
        assert_eq!(two.env.id_at(marlow_id, from), None);
        assert_eq!(two.rescan(), seen(9));
    }

    #[test]
    fn a_row_at_the_path_of_a_link_goes_when_the_walk_reaches_the_file_at_its_own_path() {
        let scanned = Scanned::new();
        // A directory with a copy of each file of one album. Each copy has
        // a row of its own.
        let album = "Aurora Field/Glass Harbour";
        for name in [
            "01 Low Tide.flac",
            "02 Breakwater.flac",
            "03 Night Ferry.flac",
        ] {
            copy_with_mtime(
                &scanned.path(&format!("{album}/{name}")),
                &scanned.path(&format!("Favourites/{name}")),
            );
        }
        assert_eq!(scanned.rescan().added, 3);
        let original = scanned.id_at(LOW_TIDE);
        let copy = scanned.id_at("Favourites/01 Low Tide.flac");
        attach(&scanned.env.db, &original);
        attach(&scanned.env.db, &copy);

        // A link to the album takes the place of the directory. The walk
        // reaches the files of the album at their own paths, so it sees no
        // file at the paths below the link, and their rows go as the rows of
        // deleted files do.
        fs::remove_dir_all(scanned.path("Favourites")).unwrap();
        symlink(scanned.path(album), scanned.path("Favourites")).unwrap();
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 10,
                removed: 3,
                ..standard()
            }
        );
        assert_eq!(stored(&scanned.env), standard_paths_and(&[]));
        assert_eq!(
            attached(&scanned.env.db, &original),
            Some((scanned.folder_id, LOW_TIDE.to_owned()))
        );
        assert_eq!(attached(&scanned.env.db, &copy), None);
        assert_eq!(count_rows(&scanned.env.db, "annotations"), 1);
        assert_eq!(count_rows(&scanned.env.db, "playlist_tracks"), 1);
    }

    #[test]
    fn a_folder_that_is_a_link_is_scanned_as_the_directory_it_points_to() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // The config names a link that leads to the library through a
        // second link.
        let elsewhere = tempfile::tempdir().unwrap();
        symlink(&root, elsewhere.path().join("library")).unwrap();
        let folder = elsewhere.path().join("music");
        symlink("library", &folder).unwrap();
        // The directory of one artist is in a directory that the walk
        // skips, behind a link that names the directory by its own path and
        // not through the folder link.
        fs::create_dir(root.join(".store")).unwrap();
        fs::rename(
            root.join("Marlow Quartet"),
            root.join(".store/Marlow Quartet"),
        )
        .unwrap();
        symlink(
            root.join(".store/Marlow Quartet"),
            root.join("Marlow Quartet"),
        )
        .unwrap();
        // A link that names a directory of the library through the folder
        // link. The walk reaches that directory without it.
        symlink(folder.join("Aurora Field"), root.join("Linked Artist")).unwrap();

        let env = TestDb::new();
        env.scan(&[&folder]);
        assert_first_scan_stored_the_standard_tracks(&env);
        // The folder row has the path of the config.
        assert_eq!(env.folders().len(), 1);
        assert_eq!(env.folders()[0].path, folder);
        let before = env.index();
        assert_eq!(env.scan(&[&folder]), seen(10));
        assert_eq!(env.index(), before);

        fs::remove_file(root.join(BREAKWATER)).unwrap();
        assert_eq!(
            env.scan(&[&folder]),
            ScanSummary {
                seen: 9,
                removed: 1,
                ..standard()
            }
        );
    }

    #[test]
    fn a_row_behind_a_link_that_points_outside_goes_with_the_next_scan_that_reads_everything() {
        let scanned = Scanned::new();
        // The directory of one artist and the file of one track move out of
        // the folder, and links are at their paths. The rows at the paths
        // below the links are rows whose files the walk no longer sees.
        let outside = tempfile::tempdir().unwrap();
        let behind_links = [
            BREAKWATER,
            "Marlow Quartet/Slow Rivers/01 Upstream.m4a",
            "Marlow Quartet/Slow Rivers/02 Delta.m4a",
        ];
        let ids = behind_links.map(|rel_path| scanned.id_at(rel_path));
        for id in &ids {
            attach(&scanned.env.db, id);
        }
        let kept = scanned.id_at(LOW_TIDE);
        attach(&scanned.env.db, &kept);
        for name in [BREAKWATER, "Marlow Quartet"] {
            let target = outside.path().join(file_name(name));
            fs::rename(scanned.path(name), &target).unwrap();
            symlink(&target, scanned.path(name)).unwrap();
        }
        let before = scanned.env.index();

        // While a directory with an ordinary name cannot be listed, the scan
        // deletes no row: the files may be in that directory. A process
        // that runs as root lists the directory, so this part checks nothing
        // for it.
        let audio = scanned.path(LOW_TIDE);
        if let Some(private) = Unlistable::new(scanned.path("Private"), &audio) {
            let summary = scanned.rescan();
            assert_eq!(summary.seen, 7);
            assert_eq!(summary.removed, 0);
            assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
            assert!(summary.errors[0].contains("\"Private\""));
            assert_eq!(
                summary.errors[1],
                "3 tracks whose files are gone were kept, because a path or a file could not be \
                 read"
            );
            assert_eq!(scanned.env.index(), before);
            for (id, rel_path) in ids.iter().zip(behind_links) {
                assert_eq!(
                    attached(&scanned.env.db, id),
                    Some((scanned.folder_id, rel_path.to_owned()))
                );
            }
            drop(private);
        }
        // The directory and the file in it are there in both cases, also
        // for a process that lists the directory. They go before the next
        // scan, which would give the file a row.
        fs::remove_dir_all(scanned.path("Private")).unwrap();

        // The scan that reads everything deletes the three rows with what
        // is attached to them, and the album and the artist that have no
        // other track.
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 7,
                removed: 3,
                albums: 3,
                artists: 2,
                ..ScanSummary::default()
            }
        );
        assert_eq!(scanned.env.latest_scan().error, None);
        for (id, rel_path) in ids.iter().zip(behind_links) {
            assert_eq!(scanned.find(rel_path), None);
            assert_eq!(attached(&scanned.env.db, id), None);
        }
        assert_eq!(
            attached(&scanned.env.db, &kept),
            Some((scanned.folder_id, LOW_TIDE.to_owned()))
        );
        assert_eq!(count_rows(&scanned.env.db, "annotations"), 1);
        assert_eq!(count_rows(&scanned.env.db, "playlist_tracks"), 1);
    }

    #[test]
    fn rows_behind_links_that_point_outside_stay_while_their_folder_has_no_supported_file() {
        let scanned = Scanned::new();
        let before = scanned.env.index();
        // Every directory of the folder moves out of it, and links are at
        // their paths.
        let outside = tempfile::tempdir().unwrap();
        for name in ["Aurora Field", "Marlow Quartet", "Tessa Lund"] {
            fs::rename(scanned.path(name), outside.path().join(name)).unwrap();
            symlink(outside.path().join(name), scanned.path(name)).unwrap();
        }

        // The rule for a folder that had tracks and yields no supported file
        // keeps the rows.
        let summary = scanned.rescan();
        assert_eq!((summary.seen, summary.removed), (0, 0));
        assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("has no supported file"));
        assert_eq!(scanned.env.index(), before);

        // The folder holds a file, and the rows go.
        write_track(&scanned.path("01 Here.flac"), "Here");
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 1,
                added: 1,
                removed: 10,
                tags_read: 1,
                albums: 1,
                artists: 1,
                ..ScanSummary::default()
            }
        );
        assert_eq!(stored(&scanned.env), ["01 Here.flac"]);
    }

    #[test]
    fn rows_behind_a_link_that_points_outside_follow_their_files_into_a_folder_that_is_added() {
        let scanned = Scanned::new();
        let moved = [
            "Marlow Quartet/Slow Rivers/01 Upstream.m4a",
            "Marlow Quartet/Slow Rivers/02 Delta.m4a",
        ];
        let ids = moved.map(|rel_path| scanned.id_at(rel_path));
        for id in &ids {
            attach(&scanned.env.db, id);
        }
        // The directory of one artist is on another volume, and a link is at
        // its path.
        let volume = tempfile::tempdir().unwrap();
        fs::rename(
            scanned.path("Marlow Quartet"),
            volume.path().join("Marlow Quartet"),
        )
        .unwrap();
        symlink(
            volume.path().join("Marlow Quartet"),
            scanned.path("Marlow Quartet"),
        )
        .unwrap();

        // The config names the volume as a second folder before the next
        // scan. The rows follow their files into it with what is attached
        // to them.
        let summary = scanned.env.scan(&[scanned.root(), volume.path()]);
        assert_eq!(
            summary,
            ScanSummary {
                seen: 10,
                moved: 2,
                ..standard()
            }
        );
        let volume_id = scanned.env.folder_id(volume.path());
        for (id, rel_path) in ids.iter().zip(moved) {
            assert_eq!(
                attached(&scanned.env.db, id),
                Some((volume_id, rel_path.to_owned()))
            );
        }
        assert_eq!(scanned.env.scan(&[scanned.root(), volume.path()]), seen(10));
    }

    /// Creates the link `Archive` in a scanned library, in a form that
    /// cannot be resolved.
    type MakeLinks = fn(&Scanned);

    #[test]
    fn a_link_that_cannot_be_resolved_is_an_unread_path() {
        let kinds: [(&str, MakeLinks); 3] = [
            ("a target that is gone", |scanned| {
                symlink(scanned.path("no target"), scanned.path("Archive")).unwrap();
            }),
            ("a link to itself", |scanned| {
                symlink("Archive", scanned.path("Archive")).unwrap();
            }),
            // The second link has a name that the walk skips, so that it is
            // no unread path of its own.
            ("two links that name each other", |scanned| {
                symlink(".archive", scanned.path("Archive")).unwrap();
                symlink("Archive", scanned.path(".archive")).unwrap();
            }),
        ];
        for (kind, make_links) in kinds {
            let scanned = Scanned::new();
            make_links(&scanned);
            fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
            let summary = scanned.rescan();

            // The scan does not know where the link points, so it deletes no
            // track.
            assert_eq!((summary.seen, summary.removed), (9, 0), "{kind}");
            assert_eq!(summary.errors.len(), 2, "{kind}: {:?}", summary.errors);
            let error = &summary.errors[0];
            assert!(error.contains("\"Archive\""), "{kind}: {error}");
            assert!(error.contains("cannot be read"), "{kind}: {error}");
            assert!(error.contains("no track was deleted"), "{kind}: {error}");
            assert_eq!(
                summary.errors[1],
                "1 track whose file is gone was kept, because a path or a file could not be read",
                "{kind}"
            );
            assert!(scanned.find(LOW_TIDE).is_some(), "{kind}");

            // The link is removed, and the row of the deleted file goes.
            fs::remove_file(scanned.path("Archive")).unwrap();
            assert_eq!(
                scanned.rescan(),
                ScanSummary {
                    seen: 9,
                    removed: 1,
                    ..standard()
                },
                "{kind}"
            );
        }
    }

    #[test]
    fn a_link_to_a_target_that_the_process_may_not_reach_is_an_unread_path() {
        let scanned = Scanned::new();
        // A directory without permissions, under a name that the walk skips,
        // so that the directory is no unread path of its own.
        let audio = scanned.path(BREAKWATER);
        let Some(locked) = Unlistable::new(scanned.path(".locked"), &audio) else {
            return;
        };
        symlink(
            scanned.path(".locked/01 Inside.flac"),
            scanned.path("Archive.flac"),
        )
        .unwrap();
        fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
        let summary = scanned.rescan();

        assert_eq!((summary.seen, summary.removed), (9, 0));
        assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("\"Archive.flac\""));
        assert!(summary.errors[0].contains("cannot be read"));
        assert!(summary.errors[1].starts_with("1 track whose file is gone was kept"));
        assert!(scanned.find(LOW_TIDE).is_some());

        // The target can be reached. It is a file of the folder that no walk
        // reaches at its own path, so it gets a row at the path of the link,
        // and the row of the deleted file goes.
        drop(locked);
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 10,
                added: 1,
                removed: 1,
                tags_read: 1,
                ..standard()
            }
        );
        assert!(scanned.find("Archive.flac").is_some());
    }

    #[test]
    fn rows_behind_a_link_into_a_folder_whose_volume_is_away_are_kept() {
        let two = TwoFolders::new();
        let aurora_id = two.env.folder_id(&two.aurora);
        // A directory of the second folder that the walk of that folder
        // skips, and a link to it in the first folder. The file behind the
        // link has its row in the first folder.
        let linked = "Sessions/01 Take.flac";
        write_track(&two.marlow.join(".store/Sessions/01 Take.flac"), "Take");
        symlink(
            two.marlow.join(".store/Sessions"),
            two.aurora.join("Sessions"),
        )
        .unwrap();
        assert_eq!(two.rescan().added, 1);
        let take = two.env.id_at(aurora_id, linked).unwrap();
        attach(&two.env.db, &take);
        // A file of the first folder is deleted.
        let deleted = "Glass Harbour/02 Breakwater.flac";
        fs::remove_file(two.aurora.join(deleted)).unwrap();
        let before = two.env.index();
        let after_each_scan = |summary: &ScanSummary| {
            assert_eq!(summary.removed, 0, "{:?}", summary.errors);
            assert_eq!(two.env.index(), before);
            assert_eq!(
                attached(&two.env.db, &take),
                Some((aurora_id, linked.to_owned()))
            );
        };

        // The volume of the second folder is not mounted, and its mount
        // point is an empty directory. The link leads nowhere. The rule for
        // a folder without a supported file keeps the rows of the second
        // folder only. The link is an unread path, which keeps the row
        // behind it and every other row.
        fs::rename(&two.marlow, two.outside("unmounted")).unwrap();
        fs::create_dir(&two.marlow).unwrap();
        let summary = two.rescan();
        after_each_scan(&summary);
        assert_eq!(summary.errors.len(), 3, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("\"Sessions\" in music folder \"Aurora Field\""));
        assert!(summary.errors[0].contains("cannot be read"));
        assert!(summary.errors[1].contains("\"Marlow Quartet\""));
        assert!(summary.errors[1].contains("has no supported file"));
        assert_eq!(
            summary.errors[2],
            "3 tracks whose files are gone were kept, because a path or a file could not be read"
        );

        // The path of the second folder does not exist. The folder is an
        // unread root as well.
        fs::remove_dir(&two.marlow).unwrap();
        let summary = two.rescan();
        after_each_scan(&summary);
        assert_eq!(summary.errors.len(), 3, "{:?}", summary.errors);

        // The volume is back. The row behind the link has its ID, and the
        // row of the deleted file goes.
        fs::rename(two.outside("unmounted"), &two.marlow).unwrap();
        assert_eq!(
            two.rescan(),
            ScanSummary {
                seen: 9,
                removed: 1,
                albums: 5,
                artists: 4,
                ..ScanSummary::default()
            }
        );
        assert_eq!(two.env.id_at(aurora_id, linked), Some(take.clone()));
        assert_eq!(two.env.id_at(aurora_id, deleted), None);
        assert_eq!(
            attached(&two.env.db, &take),
            Some((aurora_id, linked.to_owned()))
        );
    }

    #[test]
    fn links_that_lead_around_in_a_directory_that_the_walk_skips_are_followed_once() {
        let library = LibraryBuilder::standard().build();
        let root = library.root.path().to_path_buf();
        // Two directories that the walk reaches through a link only. Each
        // has a link to the other one, and one has a link to the directory
        // above both.
        write_track(&root.join(".store/a/01 A.flac"), "A");
        write_track(&root.join(".store/b/01 B.flac"), "B");
        symlink("../b", root.join(".store/a/next")).unwrap();
        symlink("../a", root.join(".store/b/next")).unwrap();
        symlink("..", root.join(".store/b/up")).unwrap();
        symlink(".store/a", root.join("Loop")).unwrap();

        // The walk ends, and each file has one row.
        let env = TestDb::new();
        let summary = scan_with_limit(&env, &[&root]);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!((summary.seen, summary.added), (12, 12));
        assert_eq!(
            stored(&env),
            standard_paths_and(&["Loop/01 A.flac", "Loop/next/01 B.flac"])
        );
        let before = env.index();
        let summary = scan_with_limit(&env, &[&root]);
        assert_eq!((summary.seen, summary.added, summary.moved), (12, 0, 0));
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(env.index(), before);
    }
}

/// Sets the permission bits of a file.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|error| panic!("set the mode of {}: {error}", path.display()));
}

/// Takes the read permission from a file. Returns whether the process can
/// still open it, which is the case for a process that runs as root. A test
/// of an unreadable file then has nothing to check.
#[cfg(unix)]
fn deny_read(path: &Path) -> bool {
    set_mode(path, 0o000);
    let readable = fs::File::open(path).is_ok();
    if readable {
        eprintln!("not checked: the process reads a file that has no read permission");
    }
    !readable
}

// File modes exist on Unix only.
#[cfg(unix)]
#[test]
fn a_moved_file_that_cannot_be_read_keeps_its_row_and_its_id() {
    let scanned = Scanned::new();
    let to = "Aurora Field/Glass Harbour/01 Ebb.flac";
    let before = scanned.track_at(LOW_TIDE);
    attach(&scanned.env.db, &before.id);

    move_file(&scanned.path(LOW_TIDE), &scanned.path(to));
    if !deny_read(&scanned.path(to)) {
        return;
    }
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 10);
    assert_eq!((summary.added, summary.moved, summary.removed), (0, 0, 0));
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains(&format!("\"{to}\"")), "{error}");
    assert!(error.contains("cannot be read"), "{error}");
    assert!(summary.errors[1].starts_with("1 track whose file is gone was kept"));
    assert_eq!(
        scanned.env.latest_scan().error,
        Some(summary.errors.join("; "))
    );
    // The row is where it was, with what the user attached to it.
    assert_eq!(scanned.track_at(LOW_TIDE), before);
    assert_eq!(scanned.find(to), None);
    assert_eq!(scanned.env.count(), 10);
    assert_eq!(
        attached(&scanned.env.db, &before.id),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );

    // The file can be read again and the row follows it.
    set_mode(&scanned.path(to), 0o644);
    assert_moved(&scanned, &before.id, LOW_TIDE, to);
}

// File modes exist on Unix only.
#[cfg(unix)]
#[test]
fn a_changed_file_that_cannot_be_read_keeps_its_row_and_deletes_nothing() {
    let scanned = Scanned::new();
    let before = scanned.env.index();
    let row = scanned.track_at(LOW_TIDE);

    shift_mtime(&scanned.path(LOW_TIDE), 5);
    if !deny_read(&scanned.path(LOW_TIDE)) {
        return;
    }
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 10);
    assert_eq!((summary.changed, summary.removed), (0, 0));
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    assert!(summary.errors[0].contains(&format!("\"{LOW_TIDE}\"")));
    assert_eq!(scanned.env.index(), before);
    assert_eq!(scanned.track_at(LOW_TIDE), row);

    // The file can be read again. Its row takes the new file state.
    set_mode(&scanned.path(LOW_TIDE), 0o644);
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            ..standard()
        }
    );
    assert_eq!(scanned.track_at(LOW_TIDE).id, row.id);
}

#[test]
fn a_track_whose_path_another_track_takes_has_no_file_while_a_folder_cannot_be_read() {
    let library = LibraryBuilder::standard().build();
    shift_mtime(&library.files[0], 100);
    shift_mtime(&library.files[1], 200);
    let aurora = library.root.path().join("Aurora Field");
    let marlow = library.root.path().join("Marlow Quartet");
    let unmounted = library.root.path().join("unmounted");
    let env = TestDb::new();
    env.scan(&[&aurora, &marlow]);
    let aurora_id = env.folder_id(&aurora);
    let first = "Glass Harbour/01 Low Tide.flac";
    let second = "Glass Harbour/02 Breakwater.flac";
    let overwritten = env.id_at(aurora_id, first).unwrap();
    let moved = env.id_at(aurora_id, second).unwrap();
    attach(&env.db, &overwritten);
    attach(&env.db, &moved);

    // One file moves over another one and a third file is deleted, while
    // the second folder cannot be read. The scan does not know whether the
    // bytes of the overwritten track are in that folder.
    fs::rename(aurora.join(second), aurora.join(first)).unwrap();
    fs::remove_file(aurora.join("Glass Harbour/03 Night Ferry.flac")).unwrap();
    fs::rename(&marlow, &unmounted).unwrap();
    let summary = env.scan(&[&aurora, &marlow]);

    // The moved track is at its new path. The overwritten track is kept,
    // and it no longer has the path where the other file is.
    assert_eq!((summary.moved, summary.removed), (1, 0));
    assert_eq!(env.id_at(aurora_id, first), Some(moved.clone()));
    assert_eq!(env.id_at(aurora_id, second), None);
    let placeholder = tracks::placeholder_path(&overwritten);
    assert_eq!(env.track(&overwritten).rel_path, placeholder);
    assert!(!env.track(&overwritten).has_file());
    assert!(env.track(&moved).has_file());
    assert_eq!(
        attached(&env.db, &overwritten),
        Some((aurora_id, placeholder.clone()))
    );
    assert_eq!(
        attached(&env.db, &moved),
        Some((aurora_id, first.to_owned()))
    );
    assert_eq!(summary.errors.len(), 3, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Marlow Quartet\""));
    assert_eq!(
        summary.errors[1],
        format!(
            "track {overwritten} at \"{first}\" has no file until a scan finds it: another \
             track took its path, and a path or a file that could not be read may hold its file"
        )
    );
    assert_eq!(
        summary.errors[2],
        "1 track whose file is gone was kept, because a path or a file could not be read"
    );
    assert_eq!(env.latest_scan().error, Some(summary.errors.join("; ")));

    // The next scan finds the same state and changes nothing. The track
    // without a file is one of the two tracks that are kept.
    let summary = env.scan(&[&aurora, &marlow]);
    assert_eq!((summary.moved, summary.removed), (0, 0));
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert_eq!(
        summary.errors[1],
        "2 tracks whose files are gone were kept, because a path or a file could not be read"
    );
    assert_eq!(env.track(&overwritten).rel_path, placeholder);
    assert_eq!(env.id_at(aurora_id, first), Some(moved.clone()));

    // The second folder is back and does not hold the bytes of the
    // overwritten track. Its row goes, and so does the row of the deleted
    // file.
    fs::rename(&unmounted, &marlow).unwrap();
    let summary = env.scan(&[&aurora, &marlow]);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 7,
            removed: 2,
            ..standard()
        }
    );
    assert!(!env.locators().contains_key(&overwritten));
    assert_eq!(env.id_at(aurora_id, first), Some(moved));
}

#[test]
fn directories_that_a_system_keeps_at_a_volume_root_are_not_walked() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    let audio = &library.files[0];
    for skipped in ["System Volume Information", "$Recycle.Bin", "lost+found"] {
        fs::create_dir(root.join(skipped)).unwrap();
        fs::copy(audio, root.join(skipped).join("01 Inside.flac")).unwrap();
    }
    // The name of the ext directory is matched in its own spelling.
    fs::create_dir(root.join("Lost+Found Sessions")).unwrap();
    fs::copy(audio, root.join("Lost+Found Sessions").join("01 Take.flac")).unwrap();

    let scanned = Scanned::of(library);

    let mut expected: Vec<&str> = STANDARD.to_vec();
    expected.push("Lost+Found Sessions/01 Take.flac");
    expected.sort_unstable();
    let stored: Vec<String> = scanned
        .env
        .index()
        .into_keys()
        .map(|(_, rel_path)| rel_path)
        .collect();
    assert_eq!(stored, expected);
    assert_eq!(scanned.env.latest_scan().counts.seen, 11);
    assert_eq!(scanned.env.latest_scan().error, None);
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege that a test run does not have.
#[cfg(unix)]
#[test]
fn an_entry_that_the_walk_skips_and_cannot_read_does_not_hold_deletions_back() {
    for name in [
        "lost+found",
        "System Volume Information",
        "$RECYCLE.BIN",
        ".Trashes",
    ] {
        let scanned = Scanned::new();
        // A link whose target is gone cannot be read. Under another name it
        // is an unread path.
        std::os::unix::fs::symlink(scanned.path("no target"), scanned.path(name)).unwrap();
        fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
        let summary = scanned.rescan();

        assert_eq!(
            summary,
            ScanSummary {
                seen: 9,
                removed: 1,
                ..standard()
            },
            "{name}"
        );
        assert_eq!(scanned.find(LOW_TIDE), None, "{name}");
    }

    // The same link under an ordinary name holds the deletion back.
    let scanned = Scanned::new();
    std::os::unix::fs::symlink(scanned.path("no target"), scanned.path("Archive")).unwrap();
    fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
    let summary = scanned.rescan();
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Archive\""));
    assert!(scanned.find(LOW_TIDE).is_some());
}

/// A directory that the process cannot list. Dropping the value gives the
/// permission back, so that the directory of the test can be deleted.
#[cfg(unix)]
struct Unlistable(PathBuf);

#[cfg(unix)]
impl Unlistable {
    /// Creates the directory with one audio file in it and takes every
    /// permission from it. Returns `None` when the process can list the
    /// directory anyway, which is the case for a process that runs as root.
    fn new(path: PathBuf, audio: &Path) -> Option<Self> {
        fs::create_dir(&path).unwrap();
        fs::copy(audio, path.join("01 Inside.flac")).unwrap();
        set_mode(&path, 0o000);
        let unlistable = Self(path);
        if fs::read_dir(&unlistable.0).is_ok() {
            eprintln!("not checked: the process lists a directory that has no permission");
            return None;
        }
        Some(unlistable)
    }
}

#[cfg(unix)]
impl Drop for Unlistable {
    fn drop(&mut self) {
        set_mode(&self.0, 0o755);
    }
}

// File modes exist on Unix only.
#[cfg(unix)]
#[test]
fn a_system_directory_that_cannot_be_listed_does_not_hold_deletions_back() {
    let scanned = Scanned::new();
    let audio = scanned.path(BREAKWATER);
    let Some(_system) = Unlistable::new(scanned.path("lost+found"), &audio) else {
        return;
    };
    fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
    let summary = scanned.rescan();

    // The directory is not walked and is no error.
    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            removed: 1,
            ..standard()
        }
    );
    assert_eq!(scanned.find(LOW_TIDE), None);
}

// File modes exist on Unix only.
#[cfg(unix)]
#[test]
fn a_directory_with_an_ordinary_name_that_cannot_be_listed_holds_deletions_back() {
    let scanned = Scanned::new();
    let audio = scanned.path(BREAKWATER);
    let Some(private) = Unlistable::new(scanned.path("Private"), &audio) else {
        return;
    };
    fs::remove_file(scanned.path(LOW_TIDE)).unwrap();
    let summary = scanned.rescan();

    assert_eq!(summary.seen, 9);
    assert_eq!(summary.removed, 0);
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    assert!(summary.errors[0].contains("\"Private\""));
    assert!(summary.errors[0].contains("no track was deleted"));
    assert!(summary.errors[1].starts_with("1 track whose file is gone was kept"));
    assert!(scanned.find(LOW_TIDE).is_some());

    // The directory can be listed again. Its file is new and the missing
    // track goes.
    drop(private);
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 1,
            removed: 1,
            tags_read: 1,
            ..standard()
        }
    );
}

// File modes exist on Unix only.
#[cfg(unix)]
#[test]
fn a_folder_that_cannot_be_listed_deletes_nothing_and_is_named_in_the_error() {
    let two = TwoFolders::new();
    let before = two.env.index();
    let (aurora_id, marlow_id) = (
        two.env.folder_id(&two.aurora),
        two.env.folder_id(&two.marlow),
    );
    for entry in before.values() {
        attach(&two.env.db, &entry.id);
    }

    // A file of the first folder is deleted. The second folder exists and
    // has no permission, which is the state of a network share that refuses
    // the user of the server.
    let deleted = "Glass Harbour/02 Breakwater.flac";
    fs::remove_file(two.aurora.join(deleted)).unwrap();
    set_mode(&two.marlow, 0o000);
    let listed = fs::read_dir(&two.marlow).is_ok();
    let summary = two.rescan();
    // The folder gets its mode back before the assertions, so that the
    // directory of the test can be deleted when one fails.
    set_mode(&two.marlow, 0o755);
    if listed {
        eprintln!("not checked: the process lists a directory that has no permission");
        return;
    }

    assert_eq!(summary.seen, 6);
    assert_eq!((summary.added, summary.moved, summary.removed), (0, 0, 0));
    assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
    let error = &summary.errors[0];
    assert!(error.contains("\"Marlow Quartet\""), "{error}");
    assert!(error.contains(&two.marlow.display().to_string()), "{error}");
    assert!(error.contains("cannot be read"), "{error}");
    assert!(error.contains("no track was deleted"), "{error}");
    assert_eq!(
        summary.errors[1],
        "1 track whose file is gone was kept, because a path or a file could not be read"
    );
    assert_eq!(two.env.latest_scan().error, Some(summary.errors.join("; ")));
    assert_eq!(two.env.latest_scan().counts.removed, 0);

    // Every row is as it was, in both folders, with what was attached to
    // it. The row of the deleted file stays, because its file may be in the
    // folder that the scan could not read.
    assert_eq!(two.env.index(), before);
    for (locator, entry) in &before {
        assert_eq!(attached(&two.env.db, &entry.id).as_ref(), Some(locator));
    }

    // The folder can be listed again. Its tracks are found under their IDs,
    // and the row of the deleted file goes.
    let summary = two.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 8,
            removed: 1,
            ..standard()
        }
    );
    let mut after = before.clone();
    after.remove(&(aurora_id, deleted.to_owned()));
    assert_eq!(two.env.index(), after);
    assert_eq!(
        after
            .keys()
            .filter(|(folder_id, _)| *folder_id == marlow_id)
            .count(),
        2
    );
    for (locator, entry) in &after {
        assert_eq!(attached(&two.env.db, &entry.id).as_ref(), Some(locator));
    }
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
            ..ScanOptions::default()
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
            // The album and the artist of the two tracks of the batch.
            albums: 1,
            artists: 1,
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
            ..standard()
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
            ..standard()
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
            ..standard()
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
            ..standard()
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

/// An album row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Album {
    id: String,
    artist_id: String,
    name: String,
    artist_name: String,
    year: Option<i64>,
    genre: Option<String>,
    song_count: i64,
    duration_ms: i64,
    compilation: bool,
    cover_source: Option<String>,
    cover_mtime: Option<i64>,
}

/// An artist row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Artist {
    id: String,
    name: String,
    album_count: i64,
}

impl TestDb {
    /// Returns every album by `<artist name>/<album name>`.
    fn albums(&self) -> BTreeMap<String, Album> {
        self.db
            .with_reader(|conn| {
                let mut select = conn.prepare(
                    "SELECT id, artist_id, name, artist_name, year, genre, song_count,
                       duration_ms, compilation, cover_source, cover_mtime
                     FROM albums",
                )?;
                let albums = select
                    .query_map([], |row| {
                        Ok(Album {
                            id: row.get(0)?,
                            artist_id: row.get(1)?,
                            name: row.get(2)?,
                            artist_name: row.get(3)?,
                            year: row.get(4)?,
                            genre: row.get(5)?,
                            song_count: row.get(6)?,
                            duration_ms: row.get(7)?,
                            compilation: row.get(8)?,
                            cover_source: row.get(9)?,
                            cover_mtime: row.get(10)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<Album>>>()?;
                Ok(albums)
            })
            .unwrap()
            .into_iter()
            .map(|album| (format!("{}/{}", album.artist_name, album.name), album))
            .collect()
    }

    /// Returns the album `<artist name>/<album name>`.
    fn album(&self, key: &str) -> Album {
        self.albums()
            .remove(key)
            .unwrap_or_else(|| panic!("no album is {key}"))
    }

    /// Returns every artist by name.
    fn artists(&self) -> BTreeMap<String, Artist> {
        self.db
            .with_reader(|conn| {
                let mut select = conn.prepare("SELECT id, name, album_count FROM artists")?;
                let artists = select
                    .query_map([], |row| {
                        Ok(Artist {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            album_count: row.get(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<Artist>>>()?;
                Ok(artists)
            })
            .unwrap()
            .into_iter()
            .map(|artist| (artist.name.clone(), artist))
            .collect()
    }

    /// Returns the name and the album count of every artist in name order.
    fn album_counts(&self) -> Vec<(String, i64)> {
        self.artists()
            .into_values()
            .map(|artist| (artist.name, artist.album_count))
            .collect()
    }

    /// Returns the tracks of the album `album_id` by title.
    fn tracks_of(&self, album_id: &str) -> BTreeMap<String, Track> {
        self.index()
            .into_values()
            .map(|entry| self.track(&entry.id))
            .filter(|track| track.album_id == album_id)
            .map(|track| (track.title.clone(), track))
            .collect()
    }
}

/// Returns the absolute path of `rel_path` below `root`, joined component
/// by component, which is the form of a stored `cover_source`.
fn below(root: &Path, rel_path: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(rel_path.split('/'));
    path
}

/// Returns `path` as the text that the database stores for it.
fn text(path: &Path) -> String {
    path.to_str().expect("a Unicode path").to_owned()
}

/// Changes tags of an audio file: a key with a text gets the text, and a
/// key with `None` is removed. The tags are in the part of the file that the
/// fingerprint covers. The file gets a later modification time than it had,
/// so that the scanner sees the change.
fn retag(path: &Path, changes: &[(ItemKey, Option<&str>)]) {
    let before = fs::metadata(path).unwrap().modified().unwrap();
    let mut file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let tag = file
        .primary_tag_mut()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()));
    for (key, value) in changes {
        match value {
            Some(value) => {
                tag.insert_text(*key, (*value).to_owned());
            }
            None => tag.remove_key(*key),
        }
    }
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
    set_mtime(path, before + Duration::from_secs(5));
}

/// Gives the item `id` what a user attaches to an album or an artist: a
/// star and a rating. `item_type` is `album` or `artist`.
fn star(db: &Db, id: &str, item_type: &str) {
    db.with_writer(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO users (id, name, role, password, created_at, updated_at)
             VALUES (1, 'adnan', 'owner', 'secret', 0, 0)",
            [],
        )?;
        conn.execute(
            "INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating)
             VALUES (1, ?1, ?2, 100, 4)",
            [id, item_type],
        )?;
        Ok(())
    })
    .unwrap();
}

/// Returns whether the item `id` has the star and the rating that [`star`]
/// gave it.
fn starred(db: &Db, id: &str) -> bool {
    db.with_reader(|conn| {
        Ok(conn.query_row(
            "SELECT count(*) FROM annotations
             WHERE item_id = ?1 AND starred_at = 100 AND rating = 4",
            [id],
            |row| row.get::<_, i64>(0),
        )? == 1)
    })
    .unwrap()
}

/// Returns the natural key of the row `id` of `table`, which is `albums` or
/// `artists`.
fn natural_key(db: &Db, table: &str, id: &str) -> String {
    db.with_reader(|conn| {
        Ok(conn.query_row(
            &format!("SELECT key FROM {table} WHERE id = ?1"),
            [id],
            |row| row.get(0),
        )?)
    })
    .unwrap()
}

/// Fails every statement that writes an album row or an artist row or
/// deletes an annotation on the write connection, which the scanner uses.
fn forbid_album_and_artist_writes(db: &Db) {
    db.with_writer(|conn| {
        for table in ["albums", "artists"] {
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
        conn.execute_batch(
            "CREATE TEMP TRIGGER no_annotation_deleted BEFORE DELETE ON annotations
             BEGIN SELECT RAISE(ABORT, 'an annotation was deleted'); END;",
        )?;
        Ok(())
    })
    .unwrap();
}

/// Returns the sum of the durations of the audio files `files`, read from
/// the files.
fn duration_of(files: &[PathBuf]) -> i64 {
    files
        .iter()
        .map(|path| {
            let meta = tags::read(path).unwrap_or_else(|error| panic!("{error}"));
            i64::try_from(meta.duration_ms).unwrap()
        })
        .sum()
}

#[test]
fn the_standard_library_has_three_artists_and_four_albums_with_their_aggregates() {
    let library = LibraryBuilder::standard().build();
    let env = TestDb::new();
    let summary = env.scan(&[library.root.path()]);
    assert_eq!((summary.albums, summary.artists), (4, 3));
    let folder_id = env.folder_id(library.root.path());

    // The album, the files of the album as a range of the standard
    // catalogue, the year, the genre and the compilation flag.
    let expected = [
        (
            "Aurora Field/Glass Harbour",
            0..3,
            2019,
            "Electronic",
            false,
        ),
        ("Aurora Field/Paper Maps", 3..6, 2021, "Electronic", false),
        ("Marlow Quartet/Slow Rivers", 6..8, 2015, "Jazz", false),
        // The compilation has no album-artist tag and two track artists.
        ("Various Artists/Summer Sampler", 8..10, 2020, "Pop", true),
    ];
    let albums = env.albums();
    let artists = env.artists();
    assert_eq!(
        albums.keys().map(String::as_str).collect::<Vec<_>>(),
        expected.iter().map(|album| album.0).collect::<Vec<_>>()
    );
    for (key, files, year, genre, compilation) in expected {
        let album = &albums[key];
        assert_eq!(
            ids::parse(&album.id).map(|(kind, _)| kind),
            Some(Kind::Album)
        );
        let song_count = i64::try_from(files.len()).unwrap();
        let duration_ms = duration_of(&library.files[files.clone()]);
        assert!(duration_ms > 0, "{key}");
        assert_eq!(
            (
                album.song_count,
                album.duration_ms,
                album.year,
                album.genre.as_deref(),
                album.compilation
            ),
            (
                song_count,
                duration_ms,
                Some(year),
                Some(genre),
                compilation
            ),
            "{key}"
        );
        // The album belongs to the row of its artist, and each of its
        // tracks names both rows.
        assert_eq!(album.artist_id, artists[&album.artist_name].id, "{key}");
        for rel_path in &STANDARD[files] {
            let track = env.track(&env.id_at(folder_id, rel_path).unwrap());
            assert_eq!(track.album_id, album.id, "{rel_path}");
            assert_eq!(track.artist_id, album.artist_id, "{rel_path}");
        }
    }

    // A track artist that is no album artist has no row: Tessa Lund sings
    // on the compilation only.
    assert_eq!(
        env.album_counts(),
        [
            ("Aurora Field".to_owned(), 2),
            ("Marlow Quartet".to_owned(), 1),
            ("Various Artists".to_owned(), 1)
        ]
    );
    for artist in artists.values() {
        assert_eq!(
            ids::parse(&artist.id).map(|(kind, _)| kind),
            Some(Kind::Artist)
        );
    }
}

#[test]
fn removing_every_file_of_an_album_removes_the_album_and_its_annotation() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let before = scanned.env.albums();
    let artist = scanned.env.artists()["Aurora Field"].clone();
    let gone = &before["Aurora Field/Paper Maps"];
    let kept = &before["Aurora Field/Glass Harbour"];
    star(db, &gone.id, "album");
    star(db, &kept.id, "album");
    star(db, &artist.id, "artist");

    for rel_path in &STANDARD[3..6] {
        fs::remove_file(scanned.path(rel_path)).unwrap();
    }
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 7,
            removed: 3,
            albums: 3,
            artists: 3,
            ..ScanSummary::default()
        }
    );
    let mut expected = before.clone();
    expected.remove("Aurora Field/Paper Maps");
    // Every other album keeps its row as it was.
    assert_eq!(scanned.env.albums(), expected);
    assert!(!starred(db, &gone.id));
    assert!(starred(db, &kept.id));

    // The artist has another album. It keeps its row and its star.
    assert_eq!(
        scanned.env.artists()["Aurora Field"],
        Artist {
            album_count: 1,
            ..artist.clone()
        }
    );
    assert!(starred(db, &artist.id));
}

#[test]
fn removing_the_last_album_of_an_artist_removes_the_artist_and_its_annotation() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let albums = scanned.env.albums();
    let artists = scanned.env.artists();
    let album = &albums["Marlow Quartet/Slow Rivers"];
    let artist = &artists["Marlow Quartet"];
    let track_id = scanned.id_at(STANDARD[6]);
    star(db, &album.id, "album");
    star(db, &artist.id, "artist");
    attach(db, &track_id);
    star(db, &artists["Aurora Field"].id, "artist");
    assert_eq!(count_rows(db, "annotations"), 4);

    for rel_path in &STANDARD[6..8] {
        fs::remove_file(scanned.path(rel_path)).unwrap();
    }
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 8,
            removed: 2,
            albums: 3,
            artists: 2,
            ..ScanSummary::default()
        }
    );
    assert_eq!(
        scanned.env.album_counts(),
        [
            ("Aurora Field".to_owned(), 2),
            ("Various Artists".to_owned(), 1)
        ]
    );
    // The annotations of the artist, of the album and of the track are
    // gone. The annotation of the other artist stays.
    assert!(!starred(db, &artist.id));
    assert!(!starred(db, &album.id));
    assert_eq!(attached(db, &track_id), None);
    assert!(starred(db, &artists["Aurora Field"].id));
    assert_eq!(count_rows(db, "annotations"), 1);
}

#[test]
fn an_artist_whose_only_tracks_are_on_a_compilation_has_no_row() {
    let scanned = Scanned::new();
    // Aurora Field keeps one track, on the compilation of Various Artists.
    for rel_path in &STANDARD[..6] {
        fs::remove_file(scanned.path(rel_path)).unwrap();
    }
    let summary = scanned.rescan();

    assert_eq!((summary.albums, summary.artists), (2, 2));
    assert_eq!(
        scanned.env.album_counts(),
        [
            ("Marlow Quartet".to_owned(), 1),
            ("Various Artists".to_owned(), 1)
        ]
    );
    let track = scanned.track_at(STANDARD[9]);
    assert_eq!(track.artist_name, "Aurora Field");
    assert_eq!(
        track.album_id,
        scanned.env.album("Various Artists/Summer Sampler").id
    );
}

#[test]
fn retagging_the_title_of_an_album_moves_its_tracks_to_a_new_album() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let old = scanned.env.album("Aurora Field/Glass Harbour");
    let artist = scanned.env.artists()["Aurora Field"].clone();
    let track_ids: Vec<String> = STANDARD[..3]
        .iter()
        .map(|rel_path| scanned.id_at(rel_path))
        .collect();
    attach(db, &track_ids[0]);
    star(db, &old.id, "album");
    star(db, &artist.id, "artist");

    for rel_path in &STANDARD[..3] {
        retag(
            &scanned.path(rel_path),
            &[(ItemKey::AlbumTitle, Some("Glass Harbor"))],
        );
    }
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 3,
            tags_read: 3,
            ..standard()
        }
    );
    let albums = scanned.env.albums();
    assert!(!albums.contains_key("Aurora Field/Glass Harbour"));
    assert!(albums.values().all(|album| album.id != old.id));
    let new = &albums["Aurora Field/Glass Harbor"];
    assert_ne!(new.id, old.id);
    // The new album has the values of the old one. Its cover is the cover
    // file of the directory, which the tracks did not leave.
    assert_eq!(
        *new,
        Album {
            id: new.id.clone(),
            name: "Glass Harbor".to_owned(),
            ..old.clone()
        }
    );
    // The tracks keep their IDs and name the new album.
    for (rel_path, id) in STANDARD[..3].iter().zip(&track_ids) {
        let track = scanned.track_at(rel_path);
        assert_eq!(track.id, *id);
        assert_eq!(track.album_id, new.id);
        assert_eq!(track.artist_id, artist.id);
    }
    // The star of the track survives and the star of the old album is
    // gone. The artist is the same row and keeps its star.
    assert_eq!(
        attached(db, &track_ids[0]),
        Some((scanned.folder_id, LOW_TIDE.to_owned()))
    );
    assert!(!starred(db, &old.id));
    assert!(starred(db, &artist.id));
    assert_eq!(scanned.env.artists()["Aurora Field"], artist);
}

#[test]
fn an_album_keeps_its_row_while_one_track_refers_to_it() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let old = scanned.env.album("Aurora Field/Glass Harbour");
    star(db, &old.id, "album");
    let retitle_album = |rel_path: &str| {
        retag(
            &scanned.path(rel_path),
            &[(ItemKey::AlbumTitle, Some("Glass Harbor"))],
        );
    };

    // Two of the three tracks get another album title.
    retitle_album(STANDARD[1]);
    retitle_album(STANDARD[2]);
    let summary = scanned.rescan();

    assert_eq!((summary.albums, summary.artists), (5, 3));
    let albums = scanned.env.albums();
    let remaining = &albums["Aurora Field/Glass Harbour"];
    let new = albums["Aurora Field/Glass Harbor"].clone();
    assert_eq!(remaining.id, old.id);
    assert_eq!((remaining.song_count, new.song_count), (1, 2));
    assert_eq!(
        remaining.duration_ms + new.duration_ms,
        old.duration_ms,
        "{albums:?}"
    );
    assert!(starred(db, &old.id));
    assert_eq!(
        scanned.env.album_counts()[0],
        ("Aurora Field".to_owned(), 3)
    );
    // Both albums have their first track in the directory of the cover
    // file.
    assert_eq!(remaining.cover_source, old.cover_source);
    assert_eq!(new.cover_source, old.cover_source);

    // The last track follows. No track refers to the old album.
    retitle_album(STANDARD[0]);
    let summary = scanned.rescan();

    assert_eq!((summary.albums, summary.artists), (4, 3));
    let albums = scanned.env.albums();
    assert!(!albums.contains_key("Aurora Field/Glass Harbour"));
    assert!(!starred(db, &old.id));
    // The new album is the row it was, with three tracks.
    assert_eq!(
        albums["Aurora Field/Glass Harbor"],
        Album {
            song_count: 3,
            duration_ms: old.duration_ms,
            ..new
        }
    );
    assert_eq!(
        scanned.env.album_counts()[0],
        ("Aurora Field".to_owned(), 2)
    );
}

#[test]
fn retagging_the_album_artist_makes_a_new_artist_and_a_new_album() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let old_album = scanned.env.album("Marlow Quartet/Slow Rivers");
    let old_artist = scanned.env.artists()["Marlow Quartet"].clone();
    let track_ids: Vec<String> = STANDARD[6..8]
        .iter()
        .map(|rel_path| scanned.id_at(rel_path))
        .collect();
    star(db, &old_album.id, "album");
    star(db, &old_artist.id, "artist");

    for rel_path in &STANDARD[6..8] {
        retag(
            &scanned.path(rel_path),
            &[(ItemKey::AlbumArtist, Some("Marlow Quintet"))],
        );
    }
    let summary = scanned.rescan();

    assert_eq!((summary.albums, summary.artists), (4, 3));
    let artists = scanned.env.artists();
    assert!(!artists.contains_key("Marlow Quartet"));
    let new_artist = &artists["Marlow Quintet"];
    assert_ne!(new_artist.id, old_artist.id);
    assert_eq!(new_artist.album_count, 1);
    let new_album = scanned.env.album("Marlow Quintet/Slow Rivers");
    assert_ne!(new_album.id, old_album.id);
    assert_eq!(new_album.artist_id, new_artist.id);
    assert_eq!(new_album.song_count, 2);
    assert!(!starred(db, &old_album.id));
    assert!(!starred(db, &old_artist.id));
    for (rel_path, id) in STANDARD[6..8].iter().zip(&track_ids) {
        let track = scanned.track_at(rel_path);
        assert_eq!(track.id, *id);
        assert_eq!(track.album_id, new_album.id);
        assert_eq!(track.artist_id, new_artist.id);
        // The track artist tag is as it was.
        assert_eq!(track.artist_name, "Marlow Quartet");
    }
}

#[test]
fn a_rescan_without_changes_keeps_every_album_and_artist_and_writes_no_row() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let albums = scanned.env.albums();
    let artists = scanned.env.artists();
    for album in albums.values() {
        star(db, &album.id, "album");
    }
    for artist in artists.values() {
        star(db, &artist.id, "artist");
    }
    forbid_album_and_artist_writes(db);

    for _ in 0..2 {
        assert_eq!(scanned.rescan(), seen(10));
        assert_eq!(scanned.env.latest_scan().error, None);
    }

    assert_eq!(scanned.env.albums(), albums);
    assert_eq!(scanned.env.artists(), artists);
    assert_eq!(count_rows(db, "annotations"), 7);
    // The triggers are in force: an update of an album row fails.
    let error = db
        .with_writer(|conn| Ok(conn.execute("UPDATE albums SET song_count = 0", [])?))
        .unwrap_err();
    assert!(
        error.to_string().contains("a row of albums was updated"),
        "{error}"
    );
}

#[test]
fn a_full_scan_keeps_every_album_and_artist() {
    let scanned = Scanned::new();
    let albums = scanned.env.albums();
    let artists = scanned.env.artists();

    let summary = scanned
        .env
        .scan_with(&[scanned.root()], true, ScanOptions::default());

    assert_eq!(summary.changed, 10);
    assert_eq!(scanned.env.albums(), albums);
    assert_eq!(scanned.env.artists(), artists);
}

#[test]
fn adding_a_track_to_an_album_keeps_the_album_id() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let before = scanned.env.album("Aurora Field/Glass Harbour");
    let artists = scanned.env.artists();
    star(db, &before.id, "album");

    // A fourth track of the album, with an earlier year and another genre.
    let added = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Undertow", 4, "flac")
        .year(2018)
        .genre("Ambient")
        .build();
    let rel_path = "Aurora Field/Glass Harbour/04 Undertow.flac";
    fs::copy(&added.files[0], scanned.path(rel_path)).unwrap();
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 11,
            added: 1,
            tags_read: 1,
            ..standard()
        }
    );
    let after = scanned.env.album("Aurora Field/Glass Harbour");
    assert_eq!(
        after,
        Album {
            song_count: 4,
            duration_ms: before.duration_ms + duration_of(&added.files),
            // The smallest year of a track. Three of four tracks have the
            // genre that the album had.
            year: Some(2018),
            ..before.clone()
        }
    );
    assert_eq!(scanned.track_at(rel_path).album_id, before.id);
    assert!(starred(db, &before.id));
    assert_eq!(scanned.env.artists(), artists);
}

#[test]
fn a_track_moved_to_another_music_folder_keeps_its_album() {
    let two = TwoFolders::new();
    let db = &two.env.db;
    let (aurora_id, marlow_id) = (
        two.env.folder_id(&two.aurora),
        two.env.folder_id(&two.marlow),
    );
    let albums = two.env.albums();
    let artists = two.env.artists();
    let album = albums["Aurora Field/Glass Harbour"].clone();
    assert_eq!(
        album.cover_source,
        Some(text(&below(&two.aurora, "Glass Harbour/cover.jpg")))
    );
    star(db, &album.id, "album");
    let track_ids: Vec<String> = [
        "01 Low Tide.flac",
        "02 Breakwater.flac",
        "03 Night Ferry.flac",
    ]
    .iter()
    .map(|name| {
        two.env
            .id_at(aurora_id, &format!("Glass Harbour/{name}"))
            .unwrap()
    })
    .collect();

    // The second track of the album moves to the other folder.
    move_file(
        &two.aurora.join("Glass Harbour/02 Breakwater.flac"),
        &two.marlow.join("Imports/02 Breakwater.flac"),
    );
    let summary = two.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            moved: 1,
            ..standard()
        }
    );
    // No album row and no artist row changed. The first track of the album
    // is where it was, so the cover is too.
    assert_eq!(two.env.albums(), albums);
    assert_eq!(two.env.artists(), artists);
    assert_eq!(
        two.env.id_at(marlow_id, "Imports/02 Breakwater.flac"),
        Some(track_ids[1].clone())
    );
    assert_eq!(two.env.track(&track_ids[1]).album_id, album.id);

    // The other tracks and the cover file follow.
    for name in ["01 Low Tide.flac", "03 Night Ferry.flac", "cover.jpg"] {
        move_file(
            &two.aurora.join("Glass Harbour").join(name),
            &two.marlow.join("Imports").join(name),
        );
    }
    let summary = two.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 9,
            moved: 2,
            ..standard()
        }
    );
    // The album is the row it was. Its cover is the file at its new path.
    let mut expected = albums.clone();
    expected.insert(
        "Aurora Field/Glass Harbour".to_owned(),
        Album {
            cover_source: Some(text(&below(&two.marlow, "Imports/cover.jpg"))),
            ..album.clone()
        },
    );
    assert_eq!(two.env.albums(), expected);
    assert_eq!(two.env.artists(), artists);
    for id in &track_ids {
        assert_eq!(two.env.track(id).album_id, album.id);
        assert_eq!(two.env.track(id).folder_id, marlow_id);
    }
    assert!(starred(db, &album.id));
}

#[test]
fn a_changed_folder_path_keeps_every_album_and_artist() {
    let library = LibraryBuilder::standard().build();
    let old = library.root.path();
    let env = TestDb::new();
    env.scan(&[old]);
    let albums = env.albums();
    let artists = env.artists();

    let new = old.join("moved");
    fs::create_dir(&new).unwrap();
    for artist in ["Aurora Field", "Marlow Quartet", "Tessa Lund"] {
        fs::rename(old.join(artist), new.join(artist)).unwrap();
    }
    let summary = env.scan(&[&new]);

    assert_eq!((summary.moved, summary.albums, summary.artists), (10, 4, 3));
    // The cover file is at its path below the new folder path. Every other
    // value is as it was.
    let mut expected = albums.clone();
    let cover = below(&new, "Aurora Field/Glass Harbour/cover.jpg");
    expected
        .get_mut("Aurora Field/Glass Harbour")
        .unwrap()
        .cover_source = Some(text(&cover));
    assert_eq!(env.albums(), expected);
    assert_eq!(env.artists(), artists);
}

#[test]
fn a_compilation_is_one_album_of_various_artists() {
    // The standard compilation has the compilation flag and no album-artist
    // tag. The second one has no flag and an album-artist tag in two
    // spellings.
    let library = LibraryBuilder::standard()
        .track("Nadia Ortiz", "Winter Sampler", "First Snow", 1, "flac")
        .album_artist("VARIOUS ARTISTS")
        .track("Tessa Lund", "Winter Sampler", "Thaw", 2, "flac")
        .album_artist("various artists")
        .build();
    let scanned = Scanned::of(library);

    // The four track artists of the two compilations share one artist row.
    assert_eq!(
        scanned.env.album_counts(),
        [
            ("Aurora Field".to_owned(), 2),
            ("Marlow Quartet".to_owned(), 1),
            ("Various Artists".to_owned(), 2)
        ]
    );
    let various = scanned.env.artists()["Various Artists"].clone();
    let albums = scanned.env.albums();
    for (key, titles, artist_names) in [
        (
            "Various Artists/Summer Sampler",
            ["Last Train", "Open Road"],
            ["Aurora Field", "Tessa Lund"],
        ),
        (
            "Various Artists/Winter Sampler",
            ["First Snow", "Thaw"],
            ["Nadia Ortiz", "Tessa Lund"],
        ),
    ] {
        let album = &albums[key];
        assert!(album.compilation, "{key}");
        assert_eq!(album.song_count, 2, "{key}");
        assert_eq!(album.artist_id, various.id, "{key}");
        let tracks = scanned.env.tracks_of(&album.id);
        assert_eq!(tracks.keys().collect::<Vec<_>>(), titles, "{key}");
        // A track keeps its track artist as text and names the artist row
        // of the album.
        let names: Vec<&str> = tracks
            .values()
            .map(|track| track.artist_name.as_str())
            .collect();
        assert_eq!(names, artist_names, "{key}");
        assert!(tracks.values().all(|track| track.artist_id == various.id));
    }
    // The tracks of the standard compilation are in two directories.
    assert_eq!(
        scanned.track_at(STANDARD[8]).album_id,
        scanned.track_at(STANDARD[9]).album_id
    );
}

#[test]
fn the_discs_of_an_album_are_one_album_whose_first_track_is_on_the_first_disc() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .picture()
        .track("Aurora Field", "Long Play", "Side A", 1, "mp3")
        .disc(1)
        .track("Aurora Field", "Long Play", "Side B", 2, "flac")
        .disc(1)
        .picture()
        .build();
    // Each disc has its directory. The cover file is in the directory of
    // the second disc.
    let album_dir = library.root.path().join("Aurora Field").join("Long Play");
    move_file(
        &album_dir.join("01 Side C.flac"),
        &album_dir.join("CD2").join("01 Side C.flac"),
    );
    for name in ["01 Side A.mp3", "02 Side B.flac"] {
        move_file(&album_dir.join(name), &album_dir.join("CD1").join(name));
    }
    fs::write(album_dir.join("CD2").join("cover.jpg"), b"image").unwrap();
    let scanned = Scanned::of(library);

    let albums = scanned.env.albums();
    assert_eq!(albums.len(), 1);
    let album = &albums["Aurora Field/Long Play"];
    assert_eq!(album.song_count, 3);
    assert_eq!(scanned.env.album_counts(), [("Aurora Field".to_owned(), 1)]);
    // The directory of the album is the one of track 1 of disc 1, which
    // holds no cover file. The first track with a picture is track 2 of
    // disc 1, although track 1 of disc 2 has the lower track number.
    let side_b = scanned.track_at("Aurora Field/Long Play/CD1/02 Side B.flac");
    assert_eq!(album.cover_source, Some(format!("embedded:{}", side_b.id)));
    assert_eq!(album.cover_mtime, Some(side_b.mtime));

    // A cover file in the directory of the first disc is the cover.
    let cover = album_dir.join("CD1").join("folder.png");
    fs::write(&cover, b"image").unwrap();
    assert_eq!(scanned.rescan().seen, 3);
    let after = scanned.env.album("Aurora Field/Long Play");
    assert_eq!(
        after,
        Album {
            cover_source: Some(text(&cover)),
            cover_mtime: Some(mtime(&cover)),
            ..album.clone()
        }
    );
}

#[test]
fn tracks_without_an_album_or_an_artist_tag_are_in_the_rows_of_the_unknown_names() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    // Two files without a tag, in two directories.
    for (fixture, to) in [
        ("a.flac", "Loose/first.flac"),
        ("a.mp3", "Inbox/second.mp3"),
    ] {
        let to = root.join(to);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(common::fixtures_dir().join(fixture), to).unwrap();
    }
    // One file with an artist tag and no album tag, and one with the album
    // tag of a standard album and no artist tag.
    let tagged = LibraryBuilder::new()
        .track("Tessa Lund", "Removed", "Single", 1, "flac")
        .track("Removed", "Glass Harbour", "Stray", 1, "ogg")
        .build();
    retag(&tagged.files[0], &[(ItemKey::AlbumTitle, None)]);
    retag(&tagged.files[1], &[(ItemKey::TrackArtist, None)]);
    fs::copy(&tagged.files[0], root.join("Loose/01 Single.flac")).unwrap();
    fs::copy(&tagged.files[1], root.join("Inbox/01 Stray.ogg")).unwrap();
    let scanned = Scanned::of(library);

    assert_eq!(
        scanned.env.album_counts(),
        [
            ("Aurora Field".to_owned(), 2),
            ("Marlow Quartet".to_owned(), 1),
            ("Tessa Lund".to_owned(), 1),
            ("Various Artists".to_owned(), 1),
            ("[Unknown Artist]".to_owned(), 2),
        ]
    );
    let albums = scanned.env.albums();
    assert_eq!(albums.len(), 7);
    let song_count = |key: &str| albums[key].song_count;
    // The files without a tag are one album, whatever their directories.
    assert_eq!(song_count("[Unknown Artist]/[Unknown Album]"), 2);
    let untagged = scanned
        .env
        .tracks_of(&albums["[Unknown Artist]/[Unknown Album]"].id);
    // The title of a file without a title tag is its file stem.
    assert_eq!(untagged.keys().collect::<Vec<_>>(), ["first", "second"]);
    // The album without a title of an artist is an album of that artist.
    assert_eq!(song_count("Tessa Lund/[Unknown Album]"), 1);
    // An album title belongs to its album artist: the track without an
    // artist tag is not on the album of Aurora Field.
    assert_eq!(song_count("[Unknown Artist]/Glass Harbour"), 1);
    assert_eq!(song_count("Aurora Field/Glass Harbour"), 3);
    assert_ne!(
        albums["[Unknown Artist]/Glass Harbour"].id,
        albums["Aurora Field/Glass Harbour"].id
    );
}

#[test]
fn one_album_title_under_two_album_artists_is_two_albums() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Greatest Hits", "One", 1, "flac")
        .year(2001)
        .track("Aurora Field", "Greatest Hits", "Two", 2, "flac")
        .year(2001)
        .track("Marlow Quartet", "Greatest Hits", "Three", 1, "mp3")
        .year(1999)
        .cover_file("Marlow Quartet", "Greatest Hits", "cover.jpg")
        .build();
    let scanned = Scanned::of(library);
    let db = &scanned.env.db;

    let albums = scanned.env.albums();
    assert_eq!(albums.len(), 2);
    let aurora = albums["Aurora Field/Greatest Hits"].clone();
    let marlow = albums["Marlow Quartet/Greatest Hits"].clone();
    assert_ne!(aurora.id, marlow.id);
    assert_ne!(aurora.artist_id, marlow.artist_id);
    assert_eq!((aurora.song_count, aurora.year), (2, Some(2001)));
    assert_eq!((marlow.song_count, marlow.year), (1, Some(1999)));
    // The cover file is in the directory of one of the two albums.
    assert_eq!(aurora.cover_source, None);
    let cover = below(scanned.root(), "Marlow Quartet/Greatest Hits/cover.jpg");
    assert_eq!(marlow.cover_source, Some(text(&cover)));
    assert_eq!(
        scanned.env.album_counts(),
        [
            ("Aurora Field".to_owned(), 1),
            ("Marlow Quartet".to_owned(), 1)
        ]
    );

    // One of the two albums goes. The other one is the row it was.
    star(db, &aurora.id, "album");
    star(db, &marlow.id, "album");
    fs::remove_file(scanned.path("Marlow Quartet/Greatest Hits/01 Three.mp3")).unwrap();
    let summary = scanned.rescan();
    assert_eq!((summary.albums, summary.artists), (1, 1));
    assert!(starred(db, &aurora.id));
    assert!(!starred(db, &marlow.id));
    assert_eq!(
        scanned.env.albums().into_values().collect::<Vec<_>>(),
        [aurora]
    );
}

#[test]
fn names_that_differ_by_case_and_surrounding_space_are_one_album_and_one_artist() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let albums = scanned.env.albums();
    let artists = scanned.env.artists();
    let album = &albums["Aurora Field/Glass Harbour"];
    star(db, &album.id, "album");
    star(db, &artists["Aurora Field"].id, "artist");
    let respell = |rel_path: &str| {
        retag(
            &scanned.path(rel_path),
            &[
                (ItemKey::AlbumTitle, Some("  GLASS harbour ")),
                (ItemKey::TrackArtist, Some("\taurora FIELD")),
            ],
        );
    };

    // One track of the album gets another spelling of both names.
    respell(STANDARD[1]);
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            ..standard()
        }
    );
    // The rows are as they were: the same IDs and the first spelling.
    assert_eq!(scanned.env.albums(), albums);
    assert_eq!(scanned.env.artists(), artists);
    let track = scanned.track_at(STANDARD[1]);
    assert_eq!(track.album_id, album.id);
    // The track row holds the text of its tags, without the outer space.
    assert_eq!(track.album_name, "GLASS harbour");
    assert_eq!(track.artist_name, "aurora FIELD");

    // Every track of the album has the other spelling. The key of the
    // album is the same, so the row and its star stay. No track carries the
    // first spelling of the title, so the row takes the other one. The
    // tracks of the other album of the artist carry the first spelling of
    // its name, so the artist row keeps it.
    respell(STANDARD[0]);
    respell(STANDARD[2]);
    assert_eq!(scanned.rescan().changed, 2);
    let mut respelled = albums.clone();
    respelled.remove("Aurora Field/Glass Harbour");
    respelled.insert(
        "Aurora Field/GLASS harbour".to_owned(),
        Album {
            name: "GLASS harbour".to_owned(),
            ..album.clone()
        },
    );
    assert_eq!(scanned.env.albums(), respelled);
    assert_eq!(scanned.env.artists(), artists);
    assert!(starred(db, &album.id));
    assert!(starred(db, &artists["Aurora Field"].id));

    // Space inside a name is part of the key.
    retag(
        &scanned.path(STANDARD[0]),
        &[(ItemKey::AlbumTitle, Some("Glass  Harbour"))],
    );
    assert_eq!(scanned.rescan().albums, 5);
}

#[test]
fn names_outside_ascii_are_keyed_in_their_composed_lower_case_form() {
    // "é" as one character and as "e" with a combining accent.
    let composed = "Zo\u{eb} \u{c5}kesson";
    let decomposed = "Zoe\u{308} A\u{30a}kesson";
    assert_ne!(composed, decomposed);
    let library = LibraryBuilder::new()
        .track(composed, "\u{dc}berg\u{e4}nge", "Erster", 1, "flac")
        .track(composed, "\u{dc}berg\u{e4}nge", "Zweiter", 2, "flac")
        .cover_file(composed, "\u{dc}berg\u{e4}nge", "cover.jpg")
        .track(
            "\u{590f}\u{306e}\u{5ead}",
            "\u{96e8}\u{97f3}",
            "\u{671d}",
            1,
            "mp3",
        )
        .build();
    // The second track names the artist in the decomposed form and the
    // album in upper case.
    retag(
        &library.files[1],
        &[
            (ItemKey::TrackArtist, Some(decomposed)),
            (ItemKey::AlbumTitle, Some("\u{dc}BERG\u{c4}NGE")),
        ],
    );
    let scanned = Scanned::of(library);
    let db = &scanned.env.db;

    assert_eq!(
        scanned.env.album_counts(),
        [
            (composed.to_owned(), 1),
            ("\u{590f}\u{306e}\u{5ead}".to_owned(), 1)
        ]
    );
    let albums = scanned.env.albums();
    assert_eq!(albums.len(), 2);
    let album = albums[&format!("{composed}/\u{dc}berg\u{e4}nge")].clone();
    assert_eq!(album.song_count, 2);
    assert_eq!(
        albums["\u{590f}\u{306e}\u{5ead}/\u{96e8}\u{97f3}"].song_count,
        1
    );
    // The cover file is found below directories with these names. A file
    // system may store a name in another form than the one it was given, so
    // the test opens the stored path instead of comparing it.
    let cover = PathBuf::from(album.cover_source.clone().expect("the album has a cover"));
    assert_eq!(
        cover.file_name().and_then(|name| name.to_str()),
        Some("cover.jpg")
    );
    assert_eq!(album.cover_mtime, Some(mtime(&cover)));

    // A rescan and a full scan find the same rows.
    star(db, &album.id, "album");
    assert_eq!(scanned.rescan().albums, 2);
    scanned
        .env
        .scan_with(&[scanned.root()], true, ScanOptions::default());
    assert_eq!(scanned.env.albums(), albums);
    assert!(starred(db, &album.id));
}

/// The library of the respelling tests, scanned once: one artist with an
/// album of three tracks and an album of one track. Every tag names the
/// artist and the albums in lower case.
fn lower_case_library() -> Scanned {
    let library = LibraryBuilder::new()
        .track("aurora field", "glass harbour", "Low Tide", 1, "flac")
        .track("aurora field", "glass harbour", "Breakwater", 2, "flac")
        .track("aurora field", "glass harbour", "Night Ferry", 3, "flac")
        .track("aurora field", "paper maps", "Meridian", 1, "mp3")
        .build();
    Scanned::of(library)
}

/// Returns the one artist row of the library.
fn one_artist(scanned: &Scanned) -> Artist {
    let mut artists = scanned.env.artists();
    assert_eq!(artists.len(), 1, "{artists:?}");
    artists.pop_first().expect("one artist").1
}

/// Gives the album and the artist rows a star and returns the one artist
/// row and the album rows.
fn star_every_row(scanned: &Scanned) -> (Artist, BTreeMap<String, Album>) {
    let db = &scanned.env.db;
    let artist = one_artist(scanned);
    let albums = scanned.env.albums();
    star(db, &artist.id, "artist");
    for album in albums.values() {
        star(db, &album.id, "album");
    }
    (artist, albums)
}

/// Sets the track artist tag of the files `files` to `name`.
fn respell_artist(files: &[PathBuf], name: &str) {
    for file in files {
        retag(file, &[(ItemKey::TrackArtist, Some(name))]);
    }
}

/// Sets the album title tag of the files `files` to `title`.
fn respell_album(files: &[PathBuf], title: &str) {
    for file in files {
        retag(file, &[(ItemKey::AlbumTitle, Some(title))]);
    }
}

#[test]
fn an_artist_whose_tracks_all_get_another_spelling_takes_it_and_keeps_its_id_and_its_star() {
    let scanned = lower_case_library();
    let db = &scanned.env.db;
    let (artist, albums) = star_every_row(&scanned);
    assert_eq!(artist.name, "aurora field");
    let locators = scanned.env.locators();

    respell_artist(&scanned.library.files, "Aurora Field");
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 4,
            changed: 4,
            tags_read: 4,
            albums: 2,
            artists: 1,
            ..ScanSummary::default()
        }
    );
    // The row has the new spelling under its ID and its key.
    assert_eq!(
        one_artist(&scanned),
        Artist {
            name: "Aurora Field".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(natural_key(db, "artists", &artist.id), "aurora field");
    assert!(starred(db, &artist.id));
    // The album rows are the rows they were and show the artist under its
    // new name. Their titles are as they were.
    let after = scanned.env.albums();
    assert_eq!(after.len(), 2);
    for album in albums.values() {
        assert_eq!(
            after[&format!("Aurora Field/{}", album.name)],
            Album {
                artist_name: "Aurora Field".to_owned(),
                ..album.clone()
            }
        );
        assert!(starred(db, &album.id));
    }
    // The tracks are the rows they were and name the artist row.
    assert_eq!(scanned.env.locators(), locators);
    for id in locators.keys() {
        let track = scanned.env.track(id);
        assert_eq!(track.artist_id, artist.id);
        assert_eq!(track.artist_name, "Aurora Field");
    }
    assert_eq!(count_rows(db, "annotations"), 3);
}

#[test]
fn an_artist_keeps_its_spelling_while_one_track_carries_it() {
    let scanned = lower_case_library();
    let db = &scanned.env.db;
    let (artist, albums) = star_every_row(&scanned);
    let files = &scanned.library.files;

    // Three of the four tracks get another spelling. The fourth carries the
    // stored one.
    respell_artist(&files[..3], "Aurora Field");
    assert_eq!(scanned.rescan().changed, 3);
    assert_eq!(one_artist(&scanned), artist);
    assert_eq!(scanned.env.albums(), albums);
    assert!(starred(db, &artist.id));

    // The fourth track gets a third spelling. No track carries the stored
    // one, and the row takes the spelling of the most tracks, which is not
    // the first in byte order.
    respell_artist(&files[3..], "AURORA FIELD");
    assert_eq!(scanned.rescan().changed, 1);
    assert_eq!(
        one_artist(&scanned),
        Artist {
            name: "Aurora Field".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(natural_key(db, "artists", &artist.id), "aurora field");
    assert!(starred(db, &artist.id));
    for album in albums.values() {
        assert_eq!(
            scanned
                .env
                .album(&format!("Aurora Field/{}", album.name))
                .id,
            album.id
        );
        assert!(starred(db, &album.id));
    }
}

#[test]
fn an_album_whose_tracks_all_get_another_spelling_takes_it_and_keeps_its_id_and_its_star() {
    let scanned = lower_case_library();
    let db = &scanned.env.db;
    let (artist, albums) = star_every_row(&scanned);
    let album = albums["aurora field/glass harbour"].clone();
    let locators = scanned.env.locators();

    respell_album(&scanned.library.files[..3], "Glass Harbour");
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 4,
            changed: 3,
            tags_read: 3,
            albums: 2,
            artists: 1,
            ..ScanSummary::default()
        }
    );
    // The row has the new spelling under its ID and its key. Its other
    // values are as they were.
    let after = scanned.env.albums();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after["aurora field/Glass Harbour"],
        Album {
            name: "Glass Harbour".to_owned(),
            ..album.clone()
        }
    );
    assert_eq!(natural_key(db, "albums", &album.id), "glass harbour");
    assert!(starred(db, &album.id));
    // The other album and the artist are as they were.
    assert_eq!(
        after["aurora field/paper maps"],
        albums["aurora field/paper maps"]
    );
    assert_eq!(one_artist(&scanned), artist);
    assert!(starred(db, &artist.id));
    // The tracks are the rows they were and name the album row.
    assert_eq!(scanned.env.locators(), locators);
    let tracks = scanned.env.tracks_of(&album.id);
    assert_eq!(tracks.len(), 3);
    assert!(
        tracks
            .values()
            .all(|track| track.album_name == "Glass Harbour")
    );
    assert_eq!(count_rows(db, "annotations"), 3);
}

#[test]
fn an_album_keeps_its_spelling_while_one_track_carries_it() {
    let scanned = lower_case_library();
    let db = &scanned.env.db;
    let (artist, albums) = star_every_row(&scanned);
    let album = albums["aurora field/glass harbour"].clone();
    let files = &scanned.library.files;

    // Two of the three tracks get another spelling. The third carries the
    // stored one.
    respell_album(&files[..2], "Glass Harbour");
    assert_eq!(scanned.rescan().changed, 2);
    assert_eq!(scanned.env.albums(), albums);
    assert!(starred(db, &album.id));

    // The third track gets a third spelling. No track carries the stored
    // one, and the row takes the spelling of the most tracks, which is not
    // the first in byte order.
    respell_album(&files[2..3], "GLASS HARBOUR");
    assert_eq!(scanned.rescan().changed, 1);
    assert_eq!(
        scanned.env.album("aurora field/Glass Harbour"),
        Album {
            name: "Glass Harbour".to_owned(),
            ..album.clone()
        }
    );
    assert_eq!(natural_key(db, "albums", &album.id), "glass harbour");
    assert!(starred(db, &album.id));
    assert_eq!(scanned.env.albums().len(), 2);
    assert_eq!(one_artist(&scanned), artist);
}

#[test]
fn a_rescan_of_names_in_several_spellings_writes_no_album_row_and_no_artist_row() {
    let scanned = lower_case_library();
    let db = &scanned.env.db;
    let (artist, albums) = star_every_row(&scanned);
    let album = albums["aurora field/glass harbour"].clone();
    let files = &scanned.library.files;

    // No track carries the stored spellings, and two spellings of each name
    // are on equally many tracks. The rows take the first in byte order.
    respell_artist(&files[..2], "Aurora Field");
    respell_artist(&files[2..], "AURORA FIELD");
    respell_album(&files[..1], "Glass Harbour");
    respell_album(&files[1..2], "GLASS HARBOUR");
    respell_album(&files[2..3], "glass Harbour");
    assert_eq!(scanned.rescan().changed, 4);
    let respelled_artists = scanned.env.artists();
    let respelled_albums = scanned.env.albums();
    assert_eq!(
        one_artist(&scanned),
        Artist {
            name: "AURORA FIELD".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(
        respelled_albums["AURORA FIELD/GLASS HARBOUR"],
        Album {
            name: "GLASS HARBOUR".to_owned(),
            artist_name: "AURORA FIELD".to_owned(),
            ..album.clone()
        }
    );

    // No tag changes. The rows have their names, so no scan writes one.
    forbid_album_and_artist_writes(db);
    for _ in 0..2 {
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 4,
                albums: 2,
                artists: 1,
                ..ScanSummary::default()
            }
        );
        assert_eq!(scanned.env.latest_scan().error, None);
    }

    assert_eq!(scanned.env.artists(), respelled_artists);
    assert_eq!(scanned.env.albums(), respelled_albums);
    assert!(starred(db, &artist.id));
    assert!(albums.values().all(|album| starred(db, &album.id)));
    assert_eq!(count_rows(db, "annotations"), 3);
    // The triggers are in force: an update of an artist row fails.
    let error = db
        .with_writer(|conn| Ok(conn.execute("UPDATE artists SET name = name", [])?))
        .unwrap_err();
    assert!(
        error.to_string().contains("a row of artists was updated"),
        "{error}"
    );
}

/// Returns the album artist that the track row `id` holds, which is NULL in
/// a row that no scan has written since schema version 1.
fn album_artist_of(db: &Db, id: &str) -> Option<String> {
    db.with_reader(|conn| {
        Ok(conn.query_row(
            "SELECT album_artist_name FROM tracks WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?)
    })
    .unwrap()
}

/// What identifies the rows of a library and ties them to each other: the
/// ID and the key of every artist, the ID, the artist and the key of every
/// album, and the ID, the artist and the album of every track, each in ID
/// order.
#[derive(Debug, PartialEq, Eq)]
struct Identities {
    artists: Vec<(String, String)>,
    albums: Vec<(String, String, String)>,
    tracks: Vec<(String, String, String)>,
}

fn identities(db: &Db) -> Identities {
    db.with_reader(|conn| {
        let artists = conn
            .prepare("SELECT id, key FROM artists ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let albums = conn
            .prepare("SELECT id, artist_id, key FROM albums ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let tracks = conn
            .prepare("SELECT id, artist_id, album_id FROM tracks ORDER BY id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Identities {
            artists,
            albums,
            tracks,
        })
    })
    .unwrap()
}

/// Fails every statement that writes a track row on the write connection,
/// which the scanner uses.
fn forbid_track_writes(db: &Db) {
    db.with_writer(|conn| {
        for (event, verb) in [
            ("INSERT", "inserted"),
            ("UPDATE", "updated"),
            ("DELETE", "deleted"),
        ] {
            conn.execute_batch(&format!(
                "CREATE TEMP TRIGGER no_tracks_{verb} BEFORE {event} ON tracks
                 BEGIN SELECT RAISE(ABORT, 'a row of tracks was {verb}'); END;"
            ))?;
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_track_row_holds_the_album_artist_of_its_file() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let album_artist = |rel_path: &str| album_artist_of(db, &scanned.id_at(rel_path));

    // A file without an album artist tag: the track artist stands in.
    assert_eq!(album_artist(LOW_TIDE).as_deref(), Some("Aurora Field"));
    // A file with an album artist tag.
    assert_eq!(album_artist(STANDARD[6]).as_deref(), Some("Marlow Quartet"));
    // A file with the compilation flag and without an album artist tag.
    assert_eq!(
        album_artist(STANDARD[8]).as_deref(),
        Some(tags::VARIOUS_ARTISTS)
    );
    assert_eq!(scanned.track_at(STANDARD[8]).artist_name, "Tessa Lund");
    // Every row has a value after the scan that wrote it.
    assert_eq!(
        count_rows(db, "tracks WHERE album_artist_name IS NOT NULL"),
        10
    );

    // A retag of the album artist under the same key reaches the row.
    retag(
        &scanned.path(STANDARD[6]),
        &[(ItemKey::AlbumArtist, Some("marlow QUARTET"))],
    );
    assert_eq!(scanned.rescan().changed, 1);
    assert_eq!(album_artist(STANDARD[6]).as_deref(), Some("marlow QUARTET"));
    assert_eq!(scanned.track_at(STANDARD[6]).artist_name, "Marlow Quartet");
    // A file that loses its album artist tag has its track artist again.
    retag(&scanned.path(STANDARD[6]), &[(ItemKey::AlbumArtist, None)]);
    assert_eq!(scanned.rescan().changed, 1);
    assert_eq!(album_artist(STANDARD[6]).as_deref(), Some("Marlow Quartet"));
}

#[test]
fn album_artist_tags_alone_respell_an_artist_and_keep_its_id_and_its_star() {
    // The two tracks of the artist have an album artist tag and a track
    // artist tag in one spelling.
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let artists = scanned.env.artists();
    let artist = artists["Marlow Quartet"].clone();
    let album = scanned.env.album("Marlow Quartet/Slow Rivers");
    star(db, &artist.id, "artist");
    star(db, &album.id, "album");
    let locators = scanned.env.locators();

    // The album artist tags get another spelling. The track artist tags
    // keep the spelling that the row has.
    for file in &scanned.library.files[6..8] {
        retag(file, &[(ItemKey::AlbumArtist, Some("MARLOW QUARTET"))]);
    }
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 2,
            tags_read: 2,
            ..standard()
        }
    );
    // The row has the new spelling under its ID and its key, with its star.
    let respelled = scanned.env.artists();
    assert_eq!(
        respelled["MARLOW QUARTET"],
        Artist {
            name: "MARLOW QUARTET".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(natural_key(db, "artists", &artist.id), "marlow quartet");
    assert!(starred(db, &artist.id));
    // The new spelling made no row, and the other artists are as they were.
    assert_eq!(respelled.len(), 3);
    assert_eq!(respelled["Aurora Field"], artists["Aurora Field"]);
    assert_eq!(respelled["Various Artists"], artists["Various Artists"]);
    // The album is the row it was and shows the artist under its new name.
    // Its cover is the picture in its first track, whose file the retag
    // wrote, so the time of the cover is the new time of that file.
    assert_eq!(
        scanned.env.album("MARLOW QUARTET/Slow Rivers"),
        Album {
            artist_name: "MARLOW QUARTET".to_owned(),
            cover_mtime: Some(mtime(&scanned.library.files[6])),
            ..album.clone()
        }
    );
    assert!(starred(db, &album.id));
    // The tracks are the rows they were. They name the artist row and hold
    // both tags.
    assert_eq!(scanned.env.locators(), locators);
    for rel_path in &STANDARD[6..8] {
        let track = scanned.track_at(rel_path);
        assert_eq!(track.artist_id, artist.id);
        assert_eq!(track.artist_name, "Marlow Quartet");
        assert_eq!(
            album_artist_of(db, &track.id).as_deref(),
            Some("MARLOW QUARTET")
        );
    }
    assert_eq!(count_rows(db, "annotations"), 2);
}

#[test]
fn a_new_spelling_of_an_artist_changes_no_id_no_key_and_no_artist_of_an_album() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let before = identities(db);
    assert_eq!(
        (
            before.artists.len(),
            before.albums.len(),
            before.tracks.len()
        ),
        (3, 4, 10)
    );

    // One artist gets a new name through its album artist tags and another
    // one through the track artist tags of files without an album artist
    // tag.
    for file in &scanned.library.files[6..8] {
        retag(file, &[(ItemKey::AlbumArtist, Some("MARLOW QUARTET"))]);
    }
    respell_artist(&scanned.library.files[..6], "AURORA FIELD");
    assert_eq!(scanned.rescan().changed, 8);

    // Both names changed.
    assert_eq!(
        scanned.env.artists().into_keys().collect::<Vec<_>>(),
        ["AURORA FIELD", "MARLOW QUARTET", "Various Artists"]
    );
    // No artist row was added or replaced, no key changed, every album has
    // the artist it had, and every track has the artist and the album it
    // had.
    assert_eq!(identities(db), before);

    // A full scan reads every file again and finds the same.
    scanned
        .env
        .scan_with(&[scanned.root()], true, ScanOptions::default());
    assert_eq!(identities(db), before);
}

/// A library of one artist with one album of three tracks, scanned once.
/// Every file names the artist `AURORA FIELD` in its track artist tag. The
/// album artist tags have two other spellings of that name.
fn two_tag_spellings_library() -> Scanned {
    let library = LibraryBuilder::new()
        .track("AURORA FIELD", "Glass Harbour", "Low Tide", 1, "flac")
        .album_artist("aurora field")
        .track("AURORA FIELD", "Glass Harbour", "Breakwater", 2, "flac")
        .album_artist("Aurora Field")
        .track("AURORA FIELD", "Glass Harbour", "Night Ferry", 3, "flac")
        .album_artist("Aurora Field")
        .build();
    Scanned::of(library)
}

#[test]
fn the_album_artist_tags_decide_the_name_when_the_track_artist_tags_spell_it_otherwise() {
    let scanned = two_tag_spellings_library();
    let db = &scanned.env.db;
    let files = &scanned.library.files;

    // The first track made the row with the spelling of its album artist
    // tag. That track carries the name, so the row keeps it. No album
    // artist tag has the spelling of the three track artist tags.
    let artist = one_artist(&scanned);
    assert_eq!(artist.name, "aurora field");
    let album = scanned.env.album("aurora field/Glass Harbour");
    star(db, &artist.id, "artist");

    // No album artist tag carries the name now. The row takes the spelling
    // of the most album artist tags.
    retag(&files[0], &[(ItemKey::AlbumArtist, Some("Aurora Field"))]);
    assert_eq!(scanned.rescan().changed, 1);
    assert_eq!(
        one_artist(&scanned),
        Artist {
            name: "Aurora Field".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(
        scanned.env.album("Aurora Field/Glass Harbour"),
        Album {
            artist_name: "Aurora Field".to_owned(),
            ..album.clone()
        }
    );

    // Another spelling in every track artist tag does not reach the row.
    respell_artist(files, "aurora FIELD");
    assert_eq!(scanned.rescan().changed, 3);
    assert_eq!(
        one_artist(&scanned),
        Artist {
            name: "Aurora Field".to_owned(),
            ..artist.clone()
        }
    );
    assert_eq!(natural_key(db, "artists", &artist.id), "aurora field");
    assert!(starred(db, &artist.id));
    for id in scanned.env.locators().keys() {
        assert_eq!(scanned.env.track(id).artist_name, "aurora FIELD");
        assert_eq!(album_artist_of(db, id).as_deref(), Some("Aurora Field"));
    }
}

#[test]
fn a_rescan_writes_no_row_when_the_album_artist_and_the_track_artist_tags_differ() {
    let scanned = two_tag_spellings_library();
    let db = &scanned.env.db;
    let artists = scanned.env.artists();
    let albums = scanned.env.albums();
    let index = scanned.env.index();
    star(db, &one_artist(&scanned).id, "artist");
    // Every row has its album artist, so a scan has nothing to fill.
    assert_eq!(count_rows(db, "tracks WHERE album_artist_name IS NULL"), 0);

    forbid_track_writes(db);
    forbid_album_and_artist_writes(db);
    for _ in 0..2 {
        assert_eq!(
            scanned.rescan(),
            ScanSummary {
                seen: 3,
                albums: 1,
                artists: 1,
                ..ScanSummary::default()
            }
        );
        assert_eq!(scanned.env.latest_scan().error, None);
    }

    assert_eq!(scanned.env.artists(), artists);
    assert_eq!(scanned.env.albums(), albums);
    assert_eq!(scanned.env.index(), index);
    assert_eq!(count_rows(db, "annotations"), 1);
    // The triggers are in force: a write of a track row and of an artist
    // row fails.
    for (sql, message) in [
        (
            "UPDATE tracks SET mtime = mtime",
            "a row of tracks was updated",
        ),
        (
            "UPDATE artists SET name = name",
            "a row of artists was updated",
        ),
    ] {
        let error = db
            .with_writer(|conn| Ok(conn.execute(sql, [])?))
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
}

/// Scans the one folder of `scanned` with `cache` as the cover cache.
fn rescan_with_cache(scanned: &Scanned, cache: &Path) -> ScanSummary {
    scanned.env.scan_with(
        &[scanned.root()],
        false,
        ScanOptions {
            covers_cache_dir: Some(cache.to_path_buf()),
            ..ScanOptions::default()
        },
    )
}

/// Writes a resized cover of the cover ID `id` to the cache `cache`, as
/// `getCoverArt` names it, and returns its path.
fn cache_cover(cache: &Path, id: &str, size: u32, cover_mtime: i64) -> PathBuf {
    let path = cache.join(format!("{id}-{size}-{cover_mtime}.jpg"));
    fs::write(&path, b"resized").unwrap();
    path
}

#[test]
fn the_cover_of_an_album_is_its_cover_file_or_its_first_embedded_picture() {
    let scanned = Scanned::new();
    let albums = scanned.env.albums();

    // The album with a `cover.jpg` file in its directory.
    let cover = below(scanned.root(), "Aurora Field/Glass Harbour/cover.jpg");
    let with_file = &albums["Aurora Field/Glass Harbour"];
    assert_eq!(with_file.cover_source, Some(text(&cover)));
    assert_eq!(with_file.cover_mtime, Some(mtime(&cover)));

    // The album whose two tracks have an embedded picture: the first one
    // is the source.
    let upstream = scanned.track_at(STANDARD[6]);
    let embedded = &albums["Marlow Quartet/Slow Rivers"];
    assert_eq!(
        embedded.cover_source,
        Some(format!("embedded:{}", upstream.id))
    );
    assert_eq!(embedded.cover_mtime, Some(upstream.mtime));

    for key in ["Aurora Field/Paper Maps", "Various Artists/Summer Sampler"] {
        assert_eq!(
            (&albums[key].cover_source, albums[key].cover_mtime),
            (&None, None),
            "{key}"
        );
    }
}

#[test]
fn replacing_the_cover_file_changes_the_cover_time_and_removes_the_resized_covers() {
    let scanned = Scanned::new();
    let cache = tempfile::tempdir().unwrap();
    let before = scanned.env.albums();
    let album = before["Aurora Field/Glass Harbour"].clone();
    let other = before["Marlow Quartet/Slow Rivers"].clone();
    let old_mtime = album.cover_mtime.expect("the album has a cover");
    let stale = [
        cache_cover(cache.path(), &album.id, 300, old_mtime),
        cache_cover(cache.path(), &album.id, 64, old_mtime),
    ];
    let fresh = [
        cache_cover(cache.path(), &other.id, 300, other.cover_mtime.unwrap()),
        cache_cover(cache.path(), &scanned.id_at(LOW_TIDE), 300, old_mtime),
    ];

    // A scan that finds the cover as it was removes no file.
    assert_eq!(rescan_with_cache(&scanned, cache.path()), seen(10));
    assert!(stale.iter().chain(&fresh).all(|path| path.exists()));

    // The cover file is replaced by a newer file. No track changes.
    let cover = below(scanned.root(), "Aurora Field/Glass Harbour/cover.jpg");
    fs::write(&cover, b"another image").unwrap();
    set_mtime(
        &cover,
        UNIX_EPOCH + Duration::from_secs(u64::try_from(old_mtime).unwrap() + 10),
    );
    let summary = rescan_with_cache(&scanned, cache.path());

    assert_eq!(summary, seen(10));
    let mut expected = before.clone();
    expected.insert(
        "Aurora Field/Glass Harbour".to_owned(),
        Album {
            cover_mtime: Some(old_mtime + 10),
            ..album.clone()
        },
    );
    assert_eq!(scanned.env.albums(), expected);
    for path in &stale {
        assert!(!path.exists(), "{}", path.display());
    }
    for path in &fresh {
        assert!(path.exists(), "{}", path.display());
    }
}

#[test]
fn a_cover_file_that_is_removed_or_added_changes_the_cover_and_removes_the_resized_covers() {
    let scanned = Scanned::new();
    let cache = tempfile::tempdir().unwrap();
    let before = scanned.env.albums();
    let with_file = before["Aurora Field/Glass Harbour"].clone();
    let embedded = before["Marlow Quartet/Slow Rivers"].clone();
    let without = before["Aurora Field/Paper Maps"].clone();
    let cached = [
        cache_cover(
            cache.path(),
            &with_file.id,
            300,
            with_file.cover_mtime.unwrap(),
        ),
        cache_cover(
            cache.path(),
            &embedded.id,
            300,
            embedded.cover_mtime.unwrap(),
        ),
    ];

    // The first album loses its cover file. Its tracks hold no picture.
    fs::remove_file(below(
        scanned.root(),
        "Aurora Field/Glass Harbour/cover.jpg",
    ))
    .unwrap();
    // The second album gets a cover file, which precedes the embedded
    // picture. The file has the modification time of the picture's track,
    // so the name of a resized cover would stay the same.
    let folder_png = below(scanned.root(), "Marlow Quartet/Slow Rivers/Folder.PNG");
    fs::write(&folder_png, b"image").unwrap();
    set_mtime(
        &folder_png,
        UNIX_EPOCH + Duration::from_secs(u64::try_from(embedded.cover_mtime.unwrap()).unwrap()),
    );
    // The third album gets two cover files. `front` precedes `album`.
    let paper_maps = below(scanned.root(), "Aurora Field/Paper Maps");
    fs::write(paper_maps.join("album.jpg"), b"image").unwrap();
    fs::write(paper_maps.join("front.webp"), b"image").unwrap();
    // A file with another name is no cover file.
    fs::write(paper_maps.join("cover.gif"), b"image").unwrap();
    fs::write(paper_maps.join("back.jpg"), b"image").unwrap();
    let summary = rescan_with_cache(&scanned, cache.path());

    assert_eq!(summary, seen(10));
    let after = scanned.env.albums();
    assert_eq!(
        after["Aurora Field/Glass Harbour"],
        Album {
            cover_source: None,
            cover_mtime: None,
            ..with_file
        }
    );
    assert_eq!(
        after["Marlow Quartet/Slow Rivers"],
        Album {
            cover_source: Some(text(&folder_png)),
            ..embedded
        }
    );
    let front = paper_maps.join("front.webp");
    assert_eq!(
        after["Aurora Field/Paper Maps"],
        Album {
            cover_source: Some(text(&front)),
            cover_mtime: Some(mtime(&front)),
            ..without
        }
    );
    // The resized covers of both albums whose cover changed are gone, also
    // where the modification time of the cover is the same.
    for path in &cached {
        assert!(!path.exists(), "{}", path.display());
    }
}

#[test]
fn an_album_that_gets_a_cover_loses_the_resized_covers_that_an_earlier_scan_left() {
    let scanned = Scanned::new();
    let cache = tempfile::tempdir().unwrap();
    let albums = scanned.env.albums();
    let album = albums["Aurora Field/Glass Harbour"].clone();
    let other = &albums["Marlow Quartet/Slow Rivers"];
    let old_mtime = album.cover_mtime.expect("the album has a cover");
    let stale = cache_cover(cache.path(), &album.id, 300, old_mtime);
    let fresh = cache_cover(cache.path(), &other.id, 300, other.cover_mtime.unwrap());

    // The cover file is gone, and a scan without a cache directory stores
    // that the album has no cover. The resized cover stays in the cache.
    let cover = below(scanned.root(), "Aurora Field/Glass Harbour/cover.jpg");
    fs::remove_file(&cover).unwrap();
    assert_eq!(scanned.rescan(), seen(10));
    assert_eq!(
        scanned.env.album("Aurora Field/Glass Harbour"),
        Album {
            cover_source: None,
            cover_mtime: None,
            ..album.clone()
        }
    );
    assert!(stale.exists());

    // Another image with the modification time of the old one is the cover
    // file. Its resized cover has the name of the file in the cache.
    fs::write(&cover, b"another image").unwrap();
    set_mtime(
        &cover,
        UNIX_EPOCH + Duration::from_secs(u64::try_from(old_mtime).unwrap()),
    );
    let summary = rescan_with_cache(&scanned, cache.path());

    assert_eq!(summary, seen(10));
    assert_eq!(scanned.env.album("Aurora Field/Glass Harbour"), album);
    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn a_deleted_album_loses_its_resized_covers() {
    let scanned = Scanned::new();
    let cache = tempfile::tempdir().unwrap();
    let albums = scanned.env.albums();
    let gone = &albums["Marlow Quartet/Slow Rivers"];
    let kept = &albums["Aurora Field/Glass Harbour"];
    let stale = cache_cover(cache.path(), &gone.id, 300, gone.cover_mtime.unwrap());
    let fresh = cache_cover(cache.path(), &kept.id, 300, kept.cover_mtime.unwrap());

    for rel_path in &STANDARD[6..8] {
        fs::remove_file(scanned.path(rel_path)).unwrap();
    }
    let summary = rescan_with_cache(&scanned, cache.path());

    assert_eq!((summary.removed, summary.albums), (2, 3));
    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn a_scan_without_a_cover_cache_sets_the_covers_and_removes_no_file() {
    let scanned = Scanned::new();
    let album = scanned.env.album("Aurora Field/Glass Harbour");
    let cover = below(scanned.root(), "Aurora Field/Glass Harbour/cover.jpg");
    shift_mtime(&cover, 10);

    assert_eq!(scanned.rescan(), seen(10));
    assert_eq!(
        scanned.env.album("Aurora Field/Glass Harbour"),
        Album {
            cover_mtime: Some(album.cover_mtime.unwrap() + 10),
            ..album
        }
    );
}

#[test]
fn an_album_in_a_renamed_directory_keeps_its_id_and_has_the_cover_file_there() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let before = scanned.env.album("Aurora Field/Glass Harbour");
    star(db, &before.id, "album");

    fs::rename(
        scanned.path("Aurora Field/Glass Harbour"),
        scanned.path("Aurora Field/Glass Harbour (2019)"),
    )
    .unwrap();
    let summary = scanned.rescan();

    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            moved: 3,
            ..standard()
        }
    );
    let cover = below(
        scanned.root(),
        "Aurora Field/Glass Harbour (2019)/cover.jpg",
    );
    assert_eq!(
        scanned.env.album("Aurora Field/Glass Harbour"),
        Album {
            cover_source: Some(text(&cover)),
            ..before.clone()
        }
    );
    assert!(starred(db, &before.id));
}

#[test]
fn the_cover_of_an_album_in_a_folder_that_cannot_be_read_stays() {
    let two = TwoFolders::new();
    let cache = tempfile::tempdir().unwrap();
    let albums = two.env.albums();
    let album = &albums["Aurora Field/Glass Harbour"];
    assert!(album.cover_source.is_some());
    let cached = cache_cover(cache.path(), &album.id, 300, album.cover_mtime.unwrap());
    let rescan = || {
        two.env.scan_with(
            &[&two.aurora, &two.marlow],
            false,
            ScanOptions {
                covers_cache_dir: Some(cache.path().to_path_buf()),
                ..ScanOptions::default()
            },
        )
    };

    // The path of the first folder does not exist, which is the state of
    // a volume that is not mounted. The scan does not know which cover
    // files the folder holds.
    fs::rename(&two.aurora, two.outside("unmounted")).unwrap();
    let summary = rescan();
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    assert_eq!((summary.albums, summary.artists), (4, 3));
    assert_eq!(two.env.albums(), albums);
    assert!(cached.exists());

    // The directory of the folder exists and is empty, which is the state
    // of a mount point without its volume.
    fs::create_dir(&two.aurora).unwrap();
    let summary = rescan();
    assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
    assert_eq!(two.env.albums(), albums);
    assert!(cached.exists());

    // The folder is back.
    fs::remove_dir(&two.aurora).unwrap();
    fs::rename(two.outside("unmounted"), &two.aurora).unwrap();
    assert_eq!(rescan(), seen(9));
    assert_eq!(two.env.albums(), albums);
    assert!(cached.exists());
}

/// The key of the album of [`disc_library`] in [`TestDb::albums`], which is
/// also the relative path of its directory.
const LONG_PLAY: &str = "Aurora Field/Long Play";

/// Moves the file `name` of the album directory `album_dir` into the
/// directory `disc` of that directory.
fn move_to_disc(album_dir: &Path, name: &str, disc: &str) {
    move_file(&album_dir.join(name), &album_dir.join(disc).join(name));
}

/// Builds a library of one album, "Long Play" by Aurora Field, whose discs
/// have directories of their own: two tracks of disc 1 in `CD1` and one
/// track of disc 2 in `CD2`. No track holds a picture. Returns the library
/// and the directory of the album, which is the parent directory of `CD1`
/// and `CD2`.
fn disc_library() -> (TestLibrary, PathBuf) {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side A", 1, "flac")
        .disc(1)
        .track("Aurora Field", "Long Play", "Side B", 2, "flac")
        .disc(1)
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .build();
    let album_dir = below(library.root.path(), LONG_PLAY);
    move_to_disc(&album_dir, "01 Side A.flac", "CD1");
    move_to_disc(&album_dir, "02 Side B.flac", "CD1");
    move_to_disc(&album_dir, "01 Side C.flac", "CD2");
    (library, album_dir)
}

/// Returns the stored cover of the album `key`: its source and its
/// modification time.
fn cover_of(env: &TestDb, key: &str) -> (Option<String>, Option<i64>) {
    let album = env.album(key);
    (album.cover_source, album.cover_mtime)
}

/// Returns the cover that a scan stores for the image file at `path`.
fn file_cover(path: &Path) -> (Option<String>, Option<i64>) {
    (Some(text(path)), Some(mtime(path)))
}

#[test]
fn an_album_in_disc_directories_has_the_cover_file_of_their_parent_directory() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side A", 1, "flac")
        .disc(1)
        .picture()
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .picture()
        .build();
    let album_dir = below(library.root.path(), LONG_PLAY);
    move_to_disc(&album_dir, "01 Side A.flac", "CD1");
    move_to_disc(&album_dir, "01 Side C.flac", "CD2");
    let cover = album_dir.join("cover.jpg");
    fs::write(&cover, b"image").unwrap();
    let scanned = Scanned::of(library);

    // The file precedes the embedded pictures of the tracks.
    assert_eq!(scanned.env.albums().len(), 1);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&cover));
}

#[test]
fn a_cover_file_beside_the_first_track_precedes_the_one_of_the_parent_directory() {
    let (library, album_dir) = disc_library();
    let parent = album_dir.join("cover.jpg");
    fs::write(&parent, b"image").unwrap();
    // The directory of the second disc has a cover file and the directory
    // of the first disc has none. The file of the second disc is beside no
    // first track, so the file of the parent directory is the cover.
    let of_second = album_dir.join("CD2").join("cover.jpg");
    fs::write(&of_second, b"image").unwrap();
    let scanned = Scanned::of(library);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&parent));

    // The directory of the first disc has a cover file and the directory
    // of the second disc has none. The name of the file comes after `cover`
    // in the order of the names, and the directory decides first.
    let beside = album_dir.join("CD1").join("album.png");
    fs::rename(&of_second, &beside).unwrap();
    assert_eq!(scanned.rescan().seen, 3);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&beside));

    // No disc directory has a cover file. The cover is the file of the
    // parent directory again.
    fs::remove_file(&beside).unwrap();
    assert_eq!(scanned.rescan().seen, 3);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&parent));
}

#[test]
fn a_parent_directory_that_holds_two_albums_gives_its_cover_file_to_neither() {
    // The directory `Box` holds the two disc directories of "Long Play" and
    // the two disc directories of "Short Play".
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side A", 1, "flac")
        .disc(1)
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .track("Aurora Field", "Short Play", "Front", 1, "mp3")
        .disc(1)
        .track("Aurora Field", "Short Play", "Back", 1, "mp3")
        .disc(2)
        .build();
    let artist_dir = library.root.path().join("Aurora Field");
    let box_dir = artist_dir.join("Box");
    let moves = [
        ("Long Play", "01 Side A.flac", "CD1"),
        ("Long Play", "01 Side C.flac", "CD2"),
        ("Short Play", "01 Front.mp3", "CD3"),
        ("Short Play", "01 Back.mp3", "CD4"),
    ];
    for (album, name, disc) in moves {
        move_file(
            &artist_dir.join(album).join(name),
            &box_dir.join(disc).join(name),
        );
    }
    let cover = box_dir.join("cover.jpg");
    fs::write(&cover, b"image").unwrap();
    let scanned = Scanned::of(library);

    let short_play = "Aurora Field/Short Play";
    assert_eq!(scanned.env.albums().len(), 2);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), (None, None));
    assert_eq!(cover_of(&scanned.env, short_play), (None, None));

    // One track of the other album is enough, in a directory below a disc
    // directory too.
    fs::remove_file(box_dir.join("CD4").join("01 Back.mp3")).unwrap();
    move_file(
        &box_dir.join("CD3").join("01 Front.mp3"),
        &box_dir.join("CD2").join("Extras").join("01 Front.mp3"),
    );
    let summary = scanned.rescan();
    assert_eq!((summary.moved, summary.removed), (1, 1));
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), (None, None));
    assert_eq!(cover_of(&scanned.env, short_play), (None, None));

    // The parent directory holds the tracks of one album when the other
    // album is gone. The file is then the cover of that album.
    fs::remove_file(box_dir.join("CD2").join("Extras").join("01 Front.mp3")).unwrap();
    let summary = scanned.rescan();
    assert_eq!((summary.removed, summary.albums), (1, 1));
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&cover));
}

#[test]
fn an_album_in_one_directory_does_not_take_the_cover_file_of_its_parent_directory() {
    // The directory of an artist with two albums and the directory of an
    // artist with one album get an image file.
    let library = LibraryBuilder::standard().build();
    let root = library.root.path();
    fs::write(below(root, "Aurora Field/folder.jpg"), b"image").unwrap();
    fs::write(below(root, "Marlow Quartet/folder.jpg"), b"image").unwrap();
    let scanned = Scanned::of(library);

    // Every album has the cover that it has in the standard library.
    let own = below(scanned.root(), "Aurora Field/Glass Harbour/cover.jpg");
    assert_eq!(
        cover_of(&scanned.env, "Aurora Field/Glass Harbour"),
        file_cover(&own)
    );
    let upstream = scanned.track_at(STANDARD[6]);
    assert_eq!(
        cover_of(&scanned.env, "Marlow Quartet/Slow Rivers"),
        (
            Some(format!("embedded:{}", upstream.id)),
            Some(upstream.mtime)
        )
    );
    // The two tracks of "Summer Sampler" are in the directories of two
    // artists, which have no common parent directory but the folder.
    for key in ["Aurora Field/Paper Maps", "Various Artists/Summer Sampler"] {
        assert_eq!(cover_of(&scanned.env, key), (None, None), "{key}");
    }
}

#[test]
fn a_cover_file_of_the_parent_directory_that_is_replaced_or_removed_changes_the_cover() {
    let (library, album_dir) = disc_library();
    let cover = album_dir.join("cover.jpg");
    fs::write(&cover, b"image").unwrap();
    // The directory of the artist, one level further up, holds an image
    // too. It is the cover of no album.
    fs::write(
        below(library.root.path(), "Aurora Field/cover.jpg"),
        b"image",
    )
    .unwrap();
    let scanned = Scanned::of(library);
    let cache = tempfile::tempdir().unwrap();
    let album = scanned.env.album(LONG_PLAY);
    let old_mtime = mtime(&cover);
    assert_eq!(
        (album.cover_source.clone(), album.cover_mtime),
        (Some(text(&cover)), Some(old_mtime))
    );
    let unchanged = ScanSummary {
        seen: 3,
        albums: 1,
        artists: 1,
        ..ScanSummary::default()
    };

    // A scan that finds the cover file as it was removes no resized cover.
    let cached = cache_cover(cache.path(), &album.id, 300, old_mtime);
    assert_eq!(rescan_with_cache(&scanned, cache.path()), unchanged);
    assert!(cached.exists());

    // The file is replaced by a newer file. No track changes.
    fs::write(&cover, b"another image").unwrap();
    set_mtime(
        &cover,
        UNIX_EPOCH + Duration::from_secs(u64::try_from(old_mtime).unwrap() + 10),
    );
    assert_eq!(rescan_with_cache(&scanned, cache.path()), unchanged);
    assert_eq!(
        scanned.env.album(LONG_PLAY),
        Album {
            cover_mtime: Some(old_mtime + 10),
            ..album.clone()
        }
    );
    assert!(!cached.exists());

    // The file is removed. The album has no other cover, so the scan
    // clears the source.
    let cached = cache_cover(cache.path(), &album.id, 300, old_mtime + 10);
    fs::remove_file(&cover).unwrap();
    assert_eq!(rescan_with_cache(&scanned, cache.path()), unchanged);
    assert_eq!(
        scanned.env.album(LONG_PLAY),
        Album {
            cover_source: None,
            cover_mtime: None,
            ..album.clone()
        }
    );
    assert!(!cached.exists());

    // A cover file with another name is added to the parent directory.
    let added = album_dir.join("Front.webp");
    fs::write(&added, b"image").unwrap();
    assert_eq!(rescan_with_cache(&scanned, cache.path()), unchanged);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&added));
}

#[test]
fn disc_directories_in_the_music_folder_itself_have_the_cover_file_of_the_folder() {
    // The directory of the album is the music folder, so the parent
    // directory of the disc directories is the folder itself.
    let (_library, album_dir) = disc_library();
    let cover = album_dir.join("cover.jpg");
    fs::write(&cover, b"image").unwrap();
    let env = TestDb::new();
    assert_eq!(env.scan(&[&album_dir]).added, 3);

    assert_eq!(cover_of(&env, LONG_PLAY), file_cover(&cover));
}

#[test]
fn a_cover_file_outside_the_music_folders_is_the_cover_of_no_album() {
    // The cover file is in the directory of the album, and the image of
    // the artist is one directory further up.
    let (library, album_dir) = disc_library();
    fs::write(album_dir.join("cover.jpg"), b"image").unwrap();
    fs::write(
        below(library.root.path(), "Aurora Field/cover.jpg"),
        b"image",
    )
    .unwrap();
    let (first, second) = (album_dir.join("CD1"), album_dir.join("CD2"));

    // Each disc directory is a music folder. The directory of the album is
    // the parent directory of both folders and is outside the library.
    let env = TestDb::new();
    assert_eq!(env.scan(&[&first, &second]).added, 3);
    assert_eq!(cover_of(&env, LONG_PLAY), (None, None));

    // One disc directory is the music folder. Its tracks are in the folder
    // itself, which has no parent directory in the library.
    let env = TestDb::new();
    assert_eq!(env.scan(&[&first]).added, 2);
    assert_eq!(cover_of(&env, LONG_PLAY), (None, None));

    // The directory of the album is the music folder and holds no cover
    // file. The image of the artist is in the parent directory of the
    // folder.
    fs::remove_file(album_dir.join("cover.jpg")).unwrap();
    let env = TestDb::new();
    assert_eq!(env.scan(&[&album_dir]).added, 3);
    assert_eq!(cover_of(&env, LONG_PLAY), (None, None));
}

#[test]
fn tracks_in_the_music_folder_itself_are_in_no_disc_directory() {
    // The directory of the album is the music folder. The tracks of disc 1
    // are in `CD1`, and the track of disc 2 is in the folder itself.
    let (_library, album_dir) = disc_library();
    move_file(
        &album_dir.join("CD2").join("01 Side C.flac"),
        &album_dir.join("01 Side C.flac"),
    );
    let cover = album_dir.join("cover.jpg");
    fs::write(&cover, b"image").unwrap();
    let env = TestDb::new();
    assert_eq!(env.scan(&[&album_dir]).added, 3);

    // The folder itself has no parent directory in the library, so it and
    // `CD1` are not two directories of one parent directory. The cover
    // file of the folder is beside the track of disc 2 and not beside the
    // first track.
    assert_eq!(cover_of(&env, LONG_PLAY), (None, None));

    // With its first track in the folder itself the album has the file.
    move_file(
        &album_dir.join("CD1").join("01 Side A.flac"),
        &album_dir.join("01 Side A.flac"),
    );
    assert_eq!(env.scan(&[&album_dir]).moved, 1);
    assert_eq!(cover_of(&env, LONG_PLAY), file_cover(&cover));
}

#[test]
fn disc_directories_in_two_music_folders_have_no_common_parent_directory() {
    // Two music folders hold a directory `Long Play` each, with one disc
    // directory of the album in each. The two disc directories have the
    // same relative parent path and two parent directories on the disk.
    let (library, album_dir) = disc_library();
    let root = library.root.path();
    let (first, second) = (root.join("first"), root.join("second"));
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(second.join("Long Play")).unwrap();
    fs::rename(album_dir.join("CD2"), second.join("Long Play").join("CD2")).unwrap();
    fs::rename(&album_dir, first.join("Long Play")).unwrap();
    for folder in [&first, &second] {
        fs::write(folder.join("Long Play").join("cover.jpg"), b"image").unwrap();
    }
    let env = TestDb::new();
    assert_eq!(env.scan(&[&first, &second]).added, 3);

    assert_eq!(env.albums().len(), 1);
    assert_eq!(cover_of(&env, LONG_PLAY), (None, None));
}

// A symbolic link is created with a Unix call. On Windows the call needs a
// privilege.
#[cfg(unix)]
#[test]
fn a_cover_file_of_the_parent_directory_that_is_a_symbolic_link_is_no_cover() {
    use std::os::unix::fs::symlink;

    // An image outside the music folder, and an image inside it under a
    // name that is not the name of a cover file.
    let outside = tempfile::tempdir().unwrap();
    let elsewhere = outside.path().join("elsewhere.jpg");
    fs::write(&elsewhere, b"image").unwrap();
    let (library, album_dir) = disc_library();
    let inside = album_dir.join("scan.jpg");
    fs::write(&inside, b"image").unwrap();

    // The link to the file outside has the first name in the order of the
    // cover names.
    let link = album_dir.join("cover.jpg");
    symlink(&elsewhere, &link).unwrap();
    let scanned = Scanned::of(library);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), (None, None));

    // A link to a file inside the folder is no cover either.
    fs::remove_file(&link).unwrap();
    symlink(&inside, &link).unwrap();
    assert_eq!(scanned.rescan().seen, 3);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), (None, None));

    // A regular file with a later name in the order is the cover while the
    // link is there.
    let regular = album_dir.join("folder.jpg");
    fs::write(&regular, b"image").unwrap();
    assert_eq!(scanned.rescan().seen, 3);
    assert_eq!(cover_of(&scanned.env, LONG_PLAY), file_cover(&regular));
}

/// Scans the one folder of `scanned` in batches of `batch_size` and cancels
/// the scan after the first batch.
fn rescan_and_cancel_after_the_first_batch(scanned: &Scanned, batch_size: usize) -> ScanSummary {
    let cancel = CancellationToken::new();
    let hook_token = cancel.clone();
    scanner::scan(
        &scanned.env.db,
        &[scanned.root().to_path_buf()],
        &cancel,
        false,
        ScanOptions {
            batch_size,
            on_batch: Some(Box::new(move || hook_token.cancel())),
            ..ScanOptions::default()
        },
    )
    .expect("the scan returns a summary")
}

#[test]
fn a_cancelled_scan_deletes_no_album_and_the_next_scan_gives_the_album_its_track() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let before = scanned.env.albums();
    let old = before["Marlow Quartet/Slow Rivers"].clone();
    star(db, &old.id, "album");

    // The two tracks of the album get another album title. A track of
    // another album gets the title and the artist of the album. The walk
    // reaches that file last.
    for rel_path in &STANDARD[6..8] {
        retag(
            &scanned.path(rel_path),
            &[(ItemKey::AlbumTitle, Some("Slow Streams"))],
        );
    }
    retag(
        &scanned.path(STANDARD[8]),
        &[
            (ItemKey::AlbumTitle, Some("Slow Rivers")),
            (ItemKey::AlbumArtist, Some("Marlow Quartet")),
        ],
    );

    // The first batch holds the two tracks that leave the album. The scan
    // is cancelled before it stores the track that joins the album.
    let summary = rescan_and_cancel_after_the_first_batch(&scanned, 2);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 2,
            tags_read: 2,
            albums: 5,
            artists: 3,
            errors: vec![CANCELLED.to_owned()],
            ..ScanSummary::default()
        }
    );
    let albums = scanned.env.albums();
    // The album has no track and keeps its row, its ID and its star. Its
    // counts are those of an album without tracks.
    assert_eq!(
        albums["Marlow Quartet/Slow Rivers"],
        Album {
            song_count: 0,
            duration_ms: 0,
            year: None,
            genre: None,
            ..old.clone()
        }
    );
    assert!(starred(db, &old.id));
    // The new album has the counts of its two tracks and no cover yet,
    // although both tracks hold a picture.
    let new = albums["Marlow Quartet/Slow Streams"].clone();
    assert_eq!(
        (new.song_count, new.duration_ms, new.year),
        (2, old.duration_ms, old.year)
    );
    assert_eq!((&new.cover_source, new.cover_mtime), (&None, None));
    // The album of the track that the scan did not store is as it was.
    assert_eq!(
        albums["Various Artists/Summer Sampler"],
        before["Various Artists/Summer Sampler"]
    );

    // The next scan stores the third file. Its album is the row that the
    // cancelled scan kept.
    let summary = scanned.rescan();
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            changed: 1,
            tags_read: 1,
            albums: 5,
            artists: 3,
            ..ScanSummary::default()
        }
    );
    let albums = scanned.env.albums();
    let kept = &albums["Marlow Quartet/Slow Rivers"];
    assert_eq!(kept.id, old.id);
    assert_eq!((kept.song_count, kept.year), (1, Some(2020)));
    assert!(starred(db, &old.id));
    assert_eq!(scanned.track_at(STANDARD[8]).album_id, old.id);
    // The scan completed, so the new album has its cover.
    let upstream = scanned.track_at(STANDARD[6]);
    assert_eq!(
        albums["Marlow Quartet/Slow Streams"],
        Album {
            cover_source: Some(format!("embedded:{}", upstream.id)),
            cover_mtime: Some(upstream.mtime),
            ..new
        }
    );
    assert_eq!(albums["Various Artists/Summer Sampler"].song_count, 1);
}

#[test]
fn a_scan_that_completes_deletes_the_album_that_a_cancelled_scan_left_without_tracks() {
    let scanned = Scanned::new();
    let db = &scanned.env.db;
    let old = scanned.env.album("Marlow Quartet/Slow Rivers");
    star(db, &old.id, "album");
    for rel_path in &STANDARD[6..8] {
        retag(
            &scanned.path(rel_path),
            &[(ItemKey::AlbumTitle, Some("Slow Streams"))],
        );
    }
    // A third change, which the walk reaches last, so that the scan has a
    // second batch.
    retag(
        &scanned.path(STANDARD[8]),
        &[(ItemKey::TrackTitle, Some("Closed Road"))],
    );

    let summary = rescan_and_cancel_after_the_first_batch(&scanned, 2);
    assert_eq!((summary.changed, summary.albums), (2, 5));
    assert_eq!(
        scanned.env.album("Marlow Quartet/Slow Rivers").song_count,
        0
    );
    assert!(starred(db, &old.id));

    let summary = scanned.rescan();
    assert_eq!(
        (summary.changed, summary.albums, summary.artists),
        (1, 4, 3)
    );
    assert!(
        !scanned
            .env
            .albums()
            .contains_key("Marlow Quartet/Slow Rivers")
    );
    assert!(!starred(db, &old.id));
}

#[test]
fn a_scan_sets_the_cover_of_an_album_whose_tracks_a_cancelled_scan_stored() {
    let library = LibraryBuilder::standard().build();
    let root = library.root.path().to_path_buf();
    let env = TestDb::new();
    let cancel = CancellationToken::new();
    let hook_token = cancel.clone();

    // The first batch holds the three tracks of the album with the cover
    // file. The scan is cancelled after it.
    let summary = scanner::scan(
        &env.db,
        std::slice::from_ref(&root),
        &cancel,
        false,
        ScanOptions {
            batch_size: 3,
            on_batch: Some(Box::new(move || hook_token.cancel())),
            ..ScanOptions::default()
        },
    )
    .unwrap();
    assert_eq!((summary.added, summary.albums, summary.artists), (3, 1, 1));
    let stored = env.album("Aurora Field/Glass Harbour");
    assert_eq!(stored.song_count, 3);
    assert_eq!(stored.duration_ms, duration_of(&library.files[..3]));
    assert_eq!((&stored.cover_source, stored.cover_mtime), (&None, None));
    assert_eq!(env.album_counts(), [("Aurora Field".to_owned(), 1)]);

    // The next scan stores no track of the album and sets its cover.
    let summary = env.scan(&[&root]);
    assert_eq!(
        summary,
        ScanSummary {
            seen: 10,
            added: 7,
            tags_read: 7,
            ..standard()
        }
    );
    let cover = below(&root, "Aurora Field/Glass Harbour/cover.jpg");
    assert_eq!(
        env.album("Aurora Field/Glass Harbour"),
        Album {
            cover_source: Some(text(&cover)),
            cover_mtime: Some(mtime(&cover)),
            ..stored
        }
    );
}

#[test]
fn a_scan_that_is_cancelled_before_it_walks_changes_no_album_and_no_artist() {
    let scanned = Scanned::new();
    let albums = scanned.env.albums();
    let artists = scanned.env.artists();
    for rel_path in &STANDARD[6..8] {
        fs::remove_file(scanned.path(rel_path)).unwrap();
    }
    fs::remove_file(below(
        scanned.root(),
        "Aurora Field/Glass Harbour/cover.jpg",
    ))
    .unwrap();
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
    assert_eq!((summary.albums, summary.artists), (4, 3));
    assert_eq!(scanned.env.albums(), albums);
    assert_eq!(scanned.env.artists(), artists);
}
