//! The scanner: brings the `tracks` table in line with the files in the
//! music folders (`docs/design.md` section 3.10).
//!
//! A track keeps its ID while the scanner can tell where its file is. A
//! file that is unchanged at its path, a file that changed at its path, and a
//! file that moved, alone or with its folder, all keep the row.
//!
//! A row is deleted only when the scanner knows that its file is gone. That
//! needs two things. The scanner read the directory of the file and the file
//! is not there, or the folder was removed from the configuration. And the
//! scanner read every file that could be the file of the row: a path that
//! could not be read may hold any file, and a file whose content could not
//! be read may be any file of its size.
//!
//! When the scanner finds that the file at the path of a row is the file of
//! another track or a new file, the row leaves the path. It follows its own
//! file, or it is deleted. When the scanner can do neither in this scan, the
//! row is kept without a file, at a placeholder path, so that its ID does
//! not name the other file.
//!
//! The scanner gives a row only to a file inside the configured folders. A
//! symbolic link whose target is outside them is not followed. A link whose
//! target is inside them is followed only when no walk reaches the target at
//! its own path, so that a file has one row.
//!
//! After the track pass the scanner brings the album rows and the artist
//! rows in line with the tracks: it deletes the rows that no track refers to,
//! recomputes the counts and sets the cover of each album. An album row and
//! an artist row keep their ID for as long as a track refers to them.
//!
//! [`scan`] blocks its thread on file and database work. Files are read
//! outside the write transactions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::db::queries::albums::{self, AlbumTrack, CoverInputs};
use crate::db::queries::folders::{self, Folder};
use crate::db::queries::scans::{self, ScanCounts};
use crate::db::queries::tracks::{self, IndexEntry, TrackContent};
use crate::db::queries::{annotations, artists};
use crate::db::{Db, Result};
use crate::ids::{self, Kind};
use crate::library::tags::{self, TagError, TrackMeta};

/// The number of files in a batch when [`ScanOptions`] has its defaults.
pub const DEFAULT_BATCH_SIZE: usize = 500;

/// The error of a cancelled scan in [`ScanSummary::errors`] and in the
/// `scans` row.
pub const CANCELLED: &str = "cancelled";

/// The options of one scan.
pub struct ScanOptions {
    /// The number of files whose changes one write transaction stores. The
    /// value 0 counts as 1.
    pub batch_size: usize,
    /// Runs after each batch is committed.
    pub on_batch: Option<Box<dyn Fn() + Send>>,
    /// The scan adds 1 for each supported file it has finished with: a file
    /// that is unchanged, a file whose changes are committed, a file that
    /// cannot be read, and a file whose changes wait for the next scan. A
    /// scan that completes adds [`ScanSummary::seen`] in total.
    pub progress: Arc<AtomicU64>,
    /// The directory of the resized covers, `cache/covers` in the data
    /// directory (`DataDir::covers_cache_dir`). The scan removes the files of
    /// an album there when the cover of the album changes and when the album
    /// is deleted, and the files of a track when the track has no row. With
    /// `None` the scan removes no file, and a resized cover of an old image
    /// can be served for a new image with the same modification time. Every
    /// caller that scans the library of a data directory sets it. The scan
    /// writes no file there and removes no file anywhere else.
    pub covers_cache_dir: Option<PathBuf>,
}

impl Default for ScanOptions {
    /// Returns a batch size of [`DEFAULT_BATCH_SIZE`], no hook, a new
    /// counter at 0 and no cover cache.
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            on_batch: None,
            progress: Arc::default(),
            covers_cache_dir: None,
        }
    }
}

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// The supported files the walk saw.
    pub seen: u64,
    /// The rows inserted with a new ID.
    pub added: u64,
    /// The rows updated in place from a file that changed at its path.
    pub changed: u64,
    /// The rows that followed their file to another path or folder.
    pub moved: u64,
    /// The rows deleted.
    pub removed: u64,
    /// The calls of the tag reader, the failed ones included.
    pub tags_read: u64,
    /// The album rows in the database when the scan ended.
    pub albums: u64,
    /// The artist rows in the database when the scan ended.
    pub artists: u64,
    /// One message for each problem that kept the scan from finishing its
    /// work: a folder, a path or a file that cannot be read, the number of
    /// tracks that were kept for that reason, a folder that had tracks and
    /// has no supported file, a removed folder whose tracks were kept, and a
    /// track that is kept without a file. A cancelled scan has
    /// [`CANCELLED`] as its last message.
    pub errors: Vec<String>,
}

/// Scans `folders` and stores the result in `db`.
///
/// The folder rows are synchronised with `folders` first. A track whose file
/// is unchanged is not read. A track whose file changed at its path keeps
/// its row and has its tags read again. A track whose file is gone is
/// matched by fingerprint to a file the database does not know and follows
/// it. A track without a match is deleted, unless a file that the scan could
/// not read may be its file, or its folder holds no supported file any more.
/// The tracks of a folder that is no longer in `folders` are deleted when
/// every folder in `folders` was read in full and holds a supported file.
/// `full` reads the fingerprint and the tags of every file.
///
/// A file gets a row only when it is inside `folders` after every symbolic
/// link of its path is resolved. A link whose target is outside `folders` is
/// not followed. A link whose target is inside `folders` is followed when no
/// walk reaches the target at its own path, so that a file has one row. A
/// link that cannot be resolved is a path that cannot be read.
///
/// After the tracks, the scan brings the albums and the artists in line
/// with them. It deletes every album that no track refers to, then every
/// artist that no album and no track refers to, and the annotations of items
/// that no longer exist. It recomputes the song count, the duration, the
/// year and the genre of every album and the album count of every artist.
/// It sets the cover of every album whose directory it saw, and removes the
/// resized covers of an album from [`ScanOptions::covers_cache_dir`] when
/// the cover of the album changed or the album was deleted. It removes the
/// resized covers of every track that has no row from the same directory.
/// An album and an artist keep their ID while a track refers to them.
///
/// The `scans` row of the scan stores whether the scan changed the library:
/// it wrote a track row, an album row or an artist row, or it follows a scan
/// that has no end time. A scan that found nothing changed, a cancellation
/// and a path or a file that cannot be read do not count as a change.
/// `getIndexes` reports the end time of the last scan that changed the
/// library as `lastModified`.
///
/// The function blocks. An async caller runs it with
/// `tokio::task::spawn_blocking`. `cancel` is checked during the walk,
/// before each read of a fingerprint or of the tags of a file, and between
/// batches. After the cancellation the scan finishes at most the file read
/// that is in progress. A cancelled scan keeps the batches it committed,
/// stores nothing of the batch it was reading, deletes nothing and returns a
/// summary whose last error is [`CANCELLED`]. It deletes no album, no artist
/// and no annotation, sets no cover and removes no resized cover. It
/// recomputes the counts, so that they describe the tracks it stored.
///
/// # Errors
///
/// Returns a database error, and the error of a folder path that is not
/// valid Unicode. A file or a folder that cannot be read does not end the
/// scan. It is a message in [`ScanSummary::errors`].
///
/// # Panics
///
/// Panics when called from an async task, as [`Db::with_writer`] does.
pub fn scan(
    db: &Db,
    folders: &[PathBuf],
    cancel: &CancellationToken,
    full: bool,
    options: ScanOptions,
) -> Result<ScanSummary> {
    scan_with(db, folders, cancel, full, options, &FileReader::default())
}

/// The two reads of the content of a file. [`scan`] reads the files on disk.
/// The tests of this module replace the reads to make them fail.
struct FileReader {
    fingerprint: Box<ReadFingerprint>,
    tags: Box<ReadTags>,
}

/// Reads the fingerprint of the file at a path.
type ReadFingerprint = dyn Fn(&Path) -> io::Result<String>;

/// Reads the tags of the file at a path.
type ReadTags = dyn Fn(&Path) -> std::result::Result<TrackMeta, TagError>;

impl Default for FileReader {
    /// Returns the reader of the files on disk: `ids::fingerprint` and
    /// `tags::read`.
    fn default() -> Self {
        Self {
            fingerprint: Box::new(ids::fingerprint),
            tags: Box::new(tags::read),
        }
    }
}

/// Runs [`scan`] with `reader` as the reader of file content.
fn scan_with(
    db: &Db,
    folders: &[PathBuf],
    cancel: &CancellationToken,
    full: bool,
    options: ScanOptions,
    reader: &FileReader,
) -> Result<ScanSummary> {
    let Start {
        scan_id,
        configured,
        unconfigured,
        index,
        after_open_scan,
    } = db.with_writer(|conn| begin(conn, folders))?;
    let mut scanner = Scanner {
        db,
        cancel,
        full,
        options,
        reader,
        folders: configured,
        links: Links::default(),
        unread: Unread::default(),
        summary: ScanSummary::default(),
        // The rows that a scan without an end time stored are in the
        // database, and no `scans` row with an end time covers them.
        changed_library: after_open_scan,
    };
    let outcome = scanner.run(&unconfigured, &index);
    let changed_library = scanner.changed_library;
    let mut summary = scanner.summary;

    let error = match &outcome {
        Ok(Outcome::Completed) => (!summary.errors.is_empty()).then(|| summary.errors.join("; ")),
        Ok(Outcome::Cancelled) => {
            summary.errors.push(CANCELLED.to_owned());
            Some(CANCELLED.to_owned())
        }
        Err(error) => Some(error.to_string()),
    };
    let counts = ScanCounts {
        seen: summary.seen,
        added: summary.added,
        changed: summary.changed,
        moved: summary.moved,
        removed: summary.removed,
    };
    // The flag covers what the scan committed, also when the scan was
    // cancelled or a database error ended it. A cancellation, an unread
    // path, an unread file and an error do not set it by themselves.
    let finished = db.with_writer(|conn| {
        scans::finish(conn, scan_id, &counts, changed_library, error.as_deref())
    });
    // When the scan failed, its error is the one to return, also when the
    // `scans` row cannot be closed.
    outcome?;
    finished?;
    tracing::info!(
        seen = summary.seen,
        added = summary.added,
        changed = summary.changed,
        moved = summary.moved,
        removed = summary.removed,
        albums = summary.albums,
        artists = summary.artists,
        errors = summary.errors.len(),
        "the scan finished"
    );
    Ok(summary)
}

/// What the scan reads from the database before it walks.
struct Start {
    scan_id: i64,
    /// The folder rows of the configured folders.
    configured: Vec<Folder>,
    /// The folder rows that no configured folder owns.
    unconfigured: Vec<Folder>,
    index: Vec<IndexEntry>,
    /// Whether the scan before this one has no end time
    /// (`scans::previous_is_open`).
    after_open_scan: bool,
}

/// Inserts the `scans` row, synchronises the folder rows with `paths` and
/// loads the index, in one transaction.
fn begin(conn: &mut Connection, paths: &[PathBuf]) -> Result<Start> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let scan_id = scans::insert(&tx)?;
    let after_open_scan = scans::previous_is_open(&tx, scan_id)?;
    let configured = folders::sync_folders(&tx, paths)?;
    let unconfigured = folders::list(&tx)?
        .into_iter()
        .filter(|folder| configured.iter().all(|kept| kept.id != folder.id))
        .collect();
    let index = tracks::load_index(&tx)?;
    tx.commit()?;
    Ok(Start {
        scan_id,
        configured,
        unconfigured,
        index,
        after_open_scan,
    })
}

/// How a scan ended when no database error stopped it.
enum Outcome {
    Completed,
    Cancelled,
}

/// What [`Scanner::apply`] did with a batch.
#[must_use]
enum Applied {
    /// The batch is committed.
    Stored,
    /// The scan was cancelled while the files of the batch were read.
    /// Nothing of the batch is stored.
    Cancelled,
}

/// What the walk and the fingerprints say about one row of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    /// The walk saw no file at the path of the row.
    Unseen,
    /// A file is at the path of the row. The scan has not found another
    /// fingerprint in it than the one of the row.
    Present,
    /// The file at the path of the row has another fingerprint than the row.
    Displaced,
}

/// A supported file that the walk saw. The path that opens the file is the
/// path of its folder joined with the components of `rel_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundFile {
    folder_id: i64,
    /// The path below the folder, components joined with `/`.
    rel_path: String,
    /// The modification time in Unix seconds.
    mtime: i64,
    size: u64,
}

/// The files of the walk that need work.
#[derive(Default)]
struct Found {
    /// The files at the path of a row whose modification time or size
    /// differs from the row, with the index of the row. A full scan puts
    /// every file at the path of a row here.
    changed: Vec<(usize, FoundFile)>,
    /// The files at a path that no row has.
    new: Vec<FoundFile>,
}

/// What the walk of one folder found out about the folder.
#[derive(Debug, Default)]
struct FolderWalk {
    /// The relative paths that could not be read. The empty path stands for
    /// the folder itself.
    unread: Vec<String>,
    /// The number of supported files the walk saw.
    supported: u64,
    /// The directories in which the walk saw a supported file, as relative
    /// paths. The empty path stands for the folder itself.
    audio_directories: HashSet<String>,
    /// The cover image of each directory that holds one, by the relative
    /// path of the directory.
    covers: HashMap<String, CoverFile>,
    /// The number of symbolic links whose target is outside every configured
    /// folder. The walk followed none of them.
    outside_links: u64,
}

/// A cover image file that the walk saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoverFile {
    /// The file name.
    name: String,
    /// The place of the name in the order of the cover names
    /// ([`cover_rank`]).
    rank: usize,
    /// The modification time in Unix seconds.
    mtime: i64,
}

impl FolderWalk {
    /// Records that the walk saw a supported file at `rel_path`.
    fn see_audio(&mut self, rel_path: &str) {
        let directory = directory(rel_path);
        if !self.audio_directories.contains(directory) {
            self.audio_directories.insert(directory.to_owned());
        }
    }

    /// Records that the walk saw the cover image file at `rel_path`, whose
    /// name has the place `rank`. A directory keeps the image with the
    /// smallest rank, and the first one the walk saw among images of the same
    /// rank, which is the first in the order of the file names.
    fn see_cover(&mut self, rel_path: &str, rank: usize, mtime: i64) {
        let directory = directory(rel_path);
        if self
            .covers
            .get(directory)
            .is_some_and(|known| known.rank <= rank)
        {
            return;
        }
        self.covers.insert(
            directory.to_owned(),
            CoverFile {
                name: file_name(rel_path).to_owned(),
                rank,
                mtime,
            },
        );
    }

    /// Returns whether the walk listed `directory` and saw a supported file
    /// in it. The cover image of such a directory, or the lack of one, is
    /// known.
    fn saw_directory(&self, directory: &str) -> bool {
        self.audio_directories.contains(directory) && !self.is_unread(directory)
    }

    /// Returns whether the walk could not read `rel_path` or a directory
    /// above it. The scan does not know whether such a file exists.
    fn is_unread(&self, rel_path: &str) -> bool {
        self.unread.iter().any(|unread| {
            unread.is_empty()
                || rel_path
                    .strip_prefix(unread.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }

    /// Returns whether the walk read the whole folder and saw a supported
    /// file.
    fn holds_files(&self) -> bool {
        self.unread.is_empty() && self.supported > 0
    }
}

/// What the scan could not read. A row whose file is gone is deleted only
/// when nothing here may be its file.
#[derive(Debug, Default)]
struct Unread {
    /// Whether the walk could not read a path in a configured folder. A file
    /// of any size may be at or below such a path.
    paths: bool,
    /// The sizes of the files whose fingerprint could not be read.
    sizes: HashSet<u64>,
}

impl Unread {
    /// Returns whether a file that the scan could not read may be the file
    /// of `entry`. A fingerprint starts with the size of its file, so a file
    /// of another size cannot have the fingerprint of the row.
    fn may_be(&self, entry: &IndexEntry) -> bool {
        self.paths
            || (!self.sizes.is_empty()
                && fingerprint_size(&entry.fingerprint)
                    .is_none_or(|size| self.sizes.contains(&size)))
    }
}

/// What the walks of one scan know about the symbolic links in the folders.
///
/// A path here is a canonical path: the result of `fs::canonicalize`, which
/// resolves every link of a path, or such a result joined with names that
/// the walk read from a directory without following a link.
#[derive(Debug, Default)]
struct Links {
    /// The canonical path of each configured folder whose path resolves. A
    /// folder whose path does not resolve is an unread root and has no path
    /// here.
    roots: Vec<PathBuf>,
    /// The targets of the directory links that the walks followed. A
    /// directory is walked through one link, so that the walk ends for every
    /// arrangement of links.
    followed: HashSet<PathBuf>,
    /// The files that the walks saw through a link.
    files: HashSet<PathBuf>,
}

impl Links {
    /// Returns whether `file`, which a walk reached through a link, gets a
    /// row at the path through that link. It gets none when a walk reaches
    /// the file at its own path, and when a walk saw it through another link
    /// before. The first call for a file records it.
    fn takes(&mut self, file: PathBuf) -> bool {
        !walks_reach(&file, &self.roots, false) && self.files.insert(file)
    }

    /// Returns the tree that the walk reads for the link at `path`, an entry
    /// of `tree`, whose target is the directory `target` inside a configured
    /// folder. Returns `None` for a link that the walk does not follow.
    ///
    /// - A target that is the path of the entry itself is no link to another
    ///   place. Windows reports a volume that is mounted at a directory as a
    ///   link, and such an entry resolves to its own path. It is a directory
    ///   of `tree`.
    /// - A target that a walk reaches at its own path is read there. A link
    ///   to a directory above the link is such a link, so it starts no loop.
    /// - Any other target is followed through the first link that leads to
    ///   it. The first call for a target records it.
    fn tree_behind(&mut self, tree: &Tree, path: &Path, target: PathBuf) -> Option<Tree> {
        let direct = if target == tree.canonical.join(below(tree, path)) {
            tree.direct
        } else if walks_reach(&target, &self.roots, true) || !self.followed.insert(target.clone()) {
            return None;
        } else {
            false
        };
        Some(Tree {
            path: path.to_path_buf(),
            canonical: target,
            direct,
        })
    }
}

/// A directory that the walk of a folder reads together with the directories
/// below it. The walk follows no link while it reads a tree. The target of a
/// link that the walk follows is a tree of its own.
#[derive(Debug)]
struct Tree {
    /// The path at which the walk opens the directory: the path of the
    /// folder, or a path below it.
    path: PathBuf,
    /// The canonical path of the directory.
    canonical: PathBuf,
    /// Whether the walk reached the directory from the folder through no
    /// link. A file in such a tree is at its own path.
    direct: bool,
}

/// What the walk does with a symbolic link.
enum Link {
    /// The walk leaves the link: its target is outside the configured
    /// folders, cannot be resolved, is no file and no directory, or is a
    /// directory that a walk reads without this link.
    Skipped,
    /// The target is a directory that the walk reads as a tree.
    Directory(Tree),
    /// The target is a regular file inside a configured folder.
    File {
        /// The canonical path of the file.
        target: PathBuf,
        state: fs::Metadata,
    },
}

/// A file that may be the new place of a missing track.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    file: FoundFile,
    fingerprint: String,
    /// The index of the row at the path of the file, when the path has one.
    displaces: Option<usize>,
}

/// One change that the scan decided on. `row` is an index into the index of
/// the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    /// The row stays at its path and takes the content of the file there.
    Update {
        row: usize,
        file: FoundFile,
        fingerprint: String,
    },
    /// The row moves to the path of the file. `reread` says whether the tags
    /// are read again.
    Relink {
        row: usize,
        file: FoundFile,
        reread: bool,
    },
    /// The file becomes a new row. The fingerprint is `None` when it has not
    /// been read.
    Insert {
        file: FoundFile,
        fingerprint: Option<String>,
    },
    /// The row is deleted because another track takes its path.
    Delete { row: usize },
    /// Another track takes the path of the row, and the row is not deleted,
    /// because a path or a file that the scan could not read may hold its
    /// file. The row gets its placeholder path and has no file until a scan
    /// finds its file or deletes the row.
    Detach { row: usize },
}

/// The changes of one scan.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// The changes that vacate or take the path of a displaced row. They
    /// are stored in one transaction whatever their number, because one
    /// row takes the path that another row leaves.
    exchange: Vec<Op>,
    /// The other changes. They are stored after `exchange`, in batches.
    rest: Vec<Op>,
    /// The rows whose file is gone and that no candidate matches.
    unmatched: Vec<usize>,
}

/// A change with everything that was read for it from the file.
enum Write<'a> {
    Delete {
        id: &'a str,
    },
    /// The row gets its placeholder path and keeps every other value.
    Detach {
        id: &'a str,
        folder_id: i64,
        mtime: i64,
        size: u64,
    },
    Relink {
        id: &'a str,
        /// Whether the row moves to a temporary path before any row takes
        /// its new path.
        via_temporary: bool,
        file: FoundFile,
        /// The fingerprint of the row, which is the one of the file.
        fingerprint: &'a str,
        /// The tags, when they were read again.
        meta: Option<TrackMeta>,
    },
    Update {
        id: &'a str,
        file: FoundFile,
        fingerprint: String,
        meta: TrackMeta,
    },
    Insert {
        file: FoundFile,
        fingerprint: String,
        meta: TrackMeta,
    },
}

impl<'a> Write<'a> {
    /// Returns the change that gives the row `known` its placeholder path.
    fn detach(known: &'a IndexEntry) -> Self {
        Write::Detach {
            id: &known.id,
            folder_id: known.folder_id,
            mtime: known.mtime,
            size: known.size,
        }
    }
}

/// The state of one scan.
struct Scanner<'a> {
    db: &'a Db,
    cancel: &'a CancellationToken,
    full: bool,
    options: ScanOptions,
    reader: &'a FileReader,
    /// The folder rows of the configured folders.
    folders: Vec<Folder>,
    links: Links,
    unread: Unread,
    summary: ScanSummary,
    /// Whether the scan changed the library, which the `scans` row of the
    /// scan stores (`docs/design.md` section 3.10 step 7). The scan sets it
    /// when a transaction of the track pass wrote a row, and when the
    /// aggregation deleted or wrote an album row or an artist row. It starts
    /// as `true` in a scan that follows a scan without an end time.
    changed_library: bool,
}

impl Scanner<'_> {
    /// Runs the track pass and then the aggregation of albums and artists.
    fn run(&mut self, unconfigured: &[Folder], index: &[IndexEntry]) -> Result<Outcome> {
        let mut walks: HashMap<i64, FolderWalk> = HashMap::with_capacity(self.folders.len());
        let outcome = self.tracks(unconfigured, index, &mut walks)?;
        self.aggregate(&outcome, &walks)?;
        Ok(outcome)
    }

    /// Steps 3 to 5 of the scan: walks the folders, matches the rows to the
    /// files, stores the changes and deletes the rows whose file is gone.
    /// `walks` receives what the walk of each folder found out. It holds
    /// every configured folder when the scan was not cancelled during the
    /// walk.
    fn tracks(
        &mut self,
        unconfigured: &[Folder],
        index: &[IndexEntry],
        walks: &mut HashMap<i64, FolderWalk>,
    ) -> Result<Outcome> {
        let mut states = vec![RowState::Unseen; index.len()];
        let with_tracks: HashSet<i64> = index.iter().map(|entry| entry.folder_id).collect();

        // The walk reads directories and file states only.
        let mut found = Found::default();
        {
            let lookup: HashMap<(i64, &str), usize> = index
                .iter()
                .enumerate()
                .map(|(row, entry)| (locator(entry), row))
                .collect();
            // Every folder is resolved before the first walk, because a
            // link in one folder can point into another one.
            let roots: Vec<io::Result<PathBuf>> = self
                .folders
                .iter()
                .map(|folder| fs::canonicalize(&folder.path))
                .collect();
            self.links.roots = roots
                .iter()
                .filter_map(|root| root.as_ref().ok().cloned())
                .collect();
            for (folder, root) in self.folders.clone().into_iter().zip(roots) {
                let walk = self.walk(&folder, root, &lookup, index, &mut states, &mut found);
                if walk.outside_links > 0 {
                    // Each link is in the debug log of the walk.
                    tracing::info!(
                        folder = %folder.path.display(),
                        links = walk.outside_links,
                        "symbolic links that point outside the music folders were not followed"
                    );
                }
                if self.cancel.is_cancelled() {
                    return Ok(Outcome::Cancelled);
                }
                if walk.unread.is_empty() && walk.supported == 0 && with_tracks.contains(&folder.id)
                {
                    self.problem(format!(
                        "music folder {} has no supported file; none of its tracks was deleted",
                        describe(&folder)
                    ));
                }
                walks.insert(folder.id, walk);
            }
        }
        let walks = &*walks;
        self.unread.paths = walks.values().any(|walk| !walk.unread.is_empty());

        let any_gone = (0..index.len()).any(|row| is_gone(&index[row], states[row], walks));
        let plan = if found.changed.is_empty() && !any_gone {
            // No row can be matched, so every file at a new path is a new
            // track. Its fingerprint is read with its tags.
            Plan {
                rest: found
                    .new
                    .into_iter()
                    .map(|file| Op::Insert {
                        file,
                        fingerprint: None,
                    })
                    .collect(),
                ..Plan::default()
            }
        } else {
            let Some((updates, candidates)) = self.fingerprints(found, index, &mut states) else {
                return Ok(Outcome::Cancelled);
            };
            let mut missing: Vec<usize> = (0..index.len())
                .filter(|&row| {
                    states[row] == RowState::Displaced || is_gone(&index[row], states[row], walks)
                })
                .collect();
            missing.sort_by(|&a, &b| locator(&index[a]).cmp(&locator(&index[b])));
            plan(index, &states, &missing, candidates, updates, &self.unread)
        };

        let Plan {
            exchange,
            rest,
            unmatched,
        } = plan;
        if !exchange.is_empty() {
            if self.cancel.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            if let Applied::Cancelled = self.apply(index, &states, exchange)? {
                return Ok(Outcome::Cancelled);
            }
        }
        let batch_size = self.options.batch_size.max(1);
        let mut rest = rest.into_iter();
        loop {
            let batch: Vec<Op> = rest.by_ref().take(batch_size).collect();
            if batch.is_empty() {
                break;
            }
            if self.cancel.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            if let Applied::Cancelled = self.apply(index, &states, batch)? {
                return Ok(Outcome::Cancelled);
            }
        }
        if self.cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }

        self.remove(index, walks, unconfigured, &unmatched)?;
        Ok(Outcome::Completed)
    }

    /// Step 6 of the scan: brings the albums, the artists and the
    /// annotations in line with the tracks ([`aggregate`]), stores the
    /// numbers of albums and artists in the summary and removes the resized
    /// covers that are stale.
    ///
    /// A scan that was cancelled deletes no row and sets no cover, because
    /// it did not store every change it found. `walks` is read for a scan
    /// that completed only. A scan that was cancelled removes no resized
    /// cover either: a server that stops waits for its scan, and the next
    /// scan that completes removes the files.
    fn aggregate(&mut self, outcome: &Outcome, walks: &HashMap<i64, FolderWalk>) -> Result<()> {
        let seen = match outcome {
            Outcome::Completed => Some(Seen {
                folders: &self.folders,
                walks,
            }),
            Outcome::Cancelled => None,
        };
        let aggregated = self.db.with_writer(|conn| aggregate(conn, seen.as_ref()))?;
        self.changed_library |= aggregated.changed_library;
        self.summary.albums = aggregated.albums;
        self.summary.artists = aggregated.artists;
        // The transaction is committed and the write connection is free.
        if let (Some(cache_dir), Outcome::Completed) = (&self.options.covers_cache_dir, outcome) {
            remove_cached_covers(self.db, cache_dir, &aggregated.stale_covers);
        }
        Ok(())
    }

    /// Walks `folder`, whose canonical path is `root`. Marks the rows whose
    /// path holds a file and sorts the supported files into unchanged,
    /// changed and new.
    ///
    /// The walk reads the folder as a tree and follows no symbolic link
    /// while it reads a tree. It resolves each link it sees ([`Self::link`])
    /// and reads the target of a link it follows as a further tree, whose
    /// files have their paths through the link. A file that the walk reaches
    /// through a link gets a row only when no walk reaches it at its own
    /// path and no walk saw it through another link before
    /// ([`Links::takes`]).
    fn walk(
        &mut self,
        folder: &Folder,
        root: io::Result<PathBuf>,
        lookup: &HashMap<(i64, &str), usize>,
        index: &[IndexEntry],
        states: &mut [RowState],
        found: &mut Found,
    ) -> FolderWalk {
        let mut walk = FolderWalk::default();
        // A folder whose path does not resolve is an unread root, so that a
        // scan that compares no link with the folder deletes nothing.
        let directory = root.and_then(|root| Ok((fs::metadata(&root)?.is_dir(), root)));
        let root = match directory {
            Ok((true, root)) => root,
            Ok((false, _)) => {
                self.unread_root(folder, &mut walk, "it is not a directory");
                return walk;
            }
            Err(error) => {
                self.unread_root(folder, &mut walk, &error.to_string());
                return walk;
            }
        };
        let mut trees = VecDeque::from([Tree {
            path: folder.path.clone(),
            canonical: root,
            direct: true,
        }]);
        while let Some(tree) = trees.pop_front() {
            // A link at the path of the tree itself is followed: it is the
            // folder, or a link that `link` resolved.
            let mut entries = WalkDir::new(&tree.path)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter();
            while let Some(entry) = entries.next() {
                if self.cancel.is_cancelled() {
                    return walk;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.walk_error(folder, &mut walk, &error);
                        continue;
                    }
                };
                if entry.depth() == 0 {
                    continue;
                }
                let name = entry.file_name();
                let is_dir = entry.file_type().is_dir();
                let is_link = entry.file_type().is_symlink();
                // A link has the type of its target, which is not known
                // before the link is resolved. Its name alone decides, as for
                // an entry that cannot be read.
                if is_hidden(name) || ((is_dir || is_link) && is_system_directory(name)) {
                    // The walk has opened the directory. Leaving it here also
                    // drops the error of a directory that cannot be listed.
                    if is_dir {
                        entries.skip_current_dir();
                    }
                    continue;
                }
                let relative = entry.path().strip_prefix(&folder.path).ok();
                let Some(rel_path) = relative.and_then(rel_path) else {
                    tracing::warn!(
                        path = %entry.path().display(),
                        "the name is not valid Unicode; the entry is skipped"
                    );
                    if is_dir {
                        entries.skip_current_dir();
                    }
                    continue;
                };
                // The file behind a link, or `None` for a file that is the
                // entry itself.
                let linked = if is_link {
                    match self.link(folder, &mut walk, &tree, entry.path(), &rel_path) {
                        Link::Skipped => continue,
                        Link::Directory(target) => {
                            trees.push_back(target);
                            continue;
                        }
                        Link::File { target, state } => Some((target, state)),
                    }
                } else if entry.file_type().is_file() {
                    None
                } else {
                    // A directory, which the walk enters by itself, or a
                    // device, a pipe or a socket, which is not opened.
                    continue;
                };
                if let Some(rank) = cover_rank(file_name(&rel_path)) {
                    // `library::covers` reads an image file only when its
                    // path names a regular file, so a symbolic link with the
                    // name of a cover image is no cover. A link could name
                    // any file that the server can read. The directory of
                    // the image can be a link that the walk followed.
                    if linked.is_some() {
                        tracing::debug!(
                            path = %entry.path().display(),
                            "the cover image is a symbolic link; the image is skipped"
                        );
                        continue;
                    }
                    match file_state(&entry) {
                        Ok((mtime, _)) => walk.see_cover(&rel_path, rank, mtime),
                        // An image file is the file of no track, so it holds
                        // no deletion back.
                        Err(cause) => tracing::warn!(
                            path = %entry.path().display(),
                            cause,
                            "the state of the cover image cannot be read; the image is skipped"
                        ),
                    }
                    continue;
                }
                if !tags::is_supported(entry.path()) {
                    continue;
                }
                // The canonical path of a file that the walk reached through
                // a link: the target of the entry, or a file in the target
                // of a link above it.
                let through_link = match &linked {
                    Some((target, _)) => Some(target.clone()),
                    None if tree.direct => None,
                    None => Some(tree.canonical.join(below(&tree, entry.path()))),
                };
                if through_link.is_some_and(|file| !self.links.takes(file)) {
                    continue;
                }
                let state = match &linked {
                    Some((_, state)) => state_of(state),
                    None => file_state(&entry),
                };
                let (mtime, size) = match state {
                    Ok(state) => state,
                    Err(cause) => {
                        self.unread_path(folder, &mut walk, rel_path, &cause);
                        continue;
                    }
                };
                walk.supported += 1;
                walk.see_audio(&rel_path);
                self.summary.seen += 1;
                let file = FoundFile {
                    folder_id: folder.id,
                    rel_path,
                    mtime,
                    size,
                };
                match lookup.get(&(folder.id, file.rel_path.as_str())) {
                    Some(&row) => {
                        states[row] = RowState::Present;
                        let known = &index[row];
                        if !self.full && known.mtime == mtime && known.size == size {
                            self.advance(1);
                        } else {
                            found.changed.push((row, file));
                        }
                    }
                    None => found.new.push(file),
                }
            }
        }
        walk
    }

    /// Resolves the symbolic link at `path`, an entry of `tree` with the
    /// relative path `rel_path`, and decides what the walk does with it.
    ///
    /// - A link that cannot be resolved is an unread path. Its target is
    ///   gone, a directory on the way to the target cannot be searched, or
    ///   the link leads back to itself. The scan does not know where such a
    ///   link points, so the files of rows may be behind it.
    /// - A link whose target is outside every configured folder is left
    ///   alone and counted. It is no unread path: no file behind it gets a
    ///   row. Only the path of the target is read, not the target.
    /// - A link to a regular file inside a configured folder is returned as
    ///   that file.
    /// - A link to a directory inside a configured folder is followed when
    ///   no walk reaches the directory at its own path and no walk followed
    ///   another link to it ([`Links::tree_behind`]).
    /// - A link to anything else, such as a device or a pipe, is left alone.
    fn link(
        &mut self,
        folder: &Folder,
        walk: &mut FolderWalk,
        tree: &Tree,
        path: &Path,
        rel_path: &str,
    ) -> Link {
        let target = match fs::canonicalize(path) {
            Ok(target) => target,
            Err(error) => {
                self.unread_path(folder, walk, rel_path.to_owned(), &error.to_string());
                return Link::Skipped;
            }
        };
        if !inside(&target, &self.links.roots) {
            walk.outside_links += 1;
            tracing::debug!(
                link = %path.display(),
                target = %target.display(),
                "the link points outside the music folders; it is not followed"
            );
            return Link::Skipped;
        }
        let state = match fs::metadata(&target) {
            Ok(state) => state,
            Err(error) => {
                self.unread_path(folder, walk, rel_path.to_owned(), &error.to_string());
                return Link::Skipped;
            }
        };
        if state.is_file() {
            return Link::File { target, state };
        }
        if !state.is_dir() {
            // A device, a pipe or a socket. Nothing opens it.
            return Link::Skipped;
        }
        match self.links.tree_behind(tree, path, target) {
            Some(target) => Link::Directory(target),
            None => {
                tracing::debug!(
                    link = %path.display(),
                    "the walk reads the target of the link without this link; it is not followed"
                );
                Link::Skipped
            }
        }
    }

    /// Records what the walk of `folder` could not read.
    fn walk_error(&mut self, folder: &Folder, walk: &mut FolderWalk, error: &walkdir::Error) {
        let cause = error
            .io_error()
            .map_or_else(|| error.to_string(), ToString::to_string);
        if error.path() == Some(folder.path.as_path()) {
            self.unread_root(folder, walk, &cause);
            return;
        }
        let relative = error
            .path()
            .and_then(|path| path.strip_prefix(&folder.path).ok());
        let Some(relative) = relative else {
            // The error does not say which directory it is about.
            walk.unread.push(String::new());
            self.problem(format!(
                "music folder {} cannot be read in full: {cause}; no track was deleted",
                describe(folder)
            ));
            return;
        };
        // The walk skips these names, so an entry with one of them that
        // cannot be read is no unread path. The walk skips a system name
        // only for a directory. Here the type of the entry is unknown,
        // because the entry could not be read, so the name alone decides.
        let skipped = |name: &OsStr| is_hidden(name) || is_system_directory(name);
        if relative.file_name().is_some_and(skipped) {
            return;
        }
        match rel_path(relative) {
            Some(rel_path) => self.unread_path(folder, walk, rel_path, &cause),
            // No track is stored below a name that is not valid Unicode.
            None => tracing::warn!(
                path = %relative.display(),
                cause,
                "the entry cannot be read; it is skipped"
            ),
        }
    }

    /// Records that the walk could not read `folder` itself.
    fn unread_root(&mut self, folder: &Folder, walk: &mut FolderWalk, cause: &str) {
        walk.unread.push(String::new());
        self.problem(format!(
            "music folder {} cannot be read: {cause}; no track was deleted",
            describe(folder)
        ));
    }

    /// Records that the walk could not read `rel_path` in `folder`.
    fn unread_path(
        &mut self,
        folder: &Folder,
        walk: &mut FolderWalk,
        rel_path: String,
        cause: &str,
    ) {
        self.problem(format!(
            "\"{rel_path}\" in music folder {} cannot be read: {cause}; no track was deleted",
            describe(folder)
        ));
        walk.unread.push(rel_path);
    }

    /// Reads the fingerprint of every changed and every new file. Returns
    /// the updates of the rows whose file kept its fingerprint, and the
    /// candidates. Marks the rows whose file has another fingerprint as
    /// displaced. The token is checked before each read. Returns `None`
    /// when the scan is cancelled.
    fn fingerprints(
        &mut self,
        found: Found,
        index: &[IndexEntry],
        states: &mut [RowState],
    ) -> Option<(Vec<Op>, Vec<Candidate>)> {
        let mut updates = Vec::new();
        let mut candidates = Vec::new();
        let changed = found
            .changed
            .into_iter()
            .map(|(row, file)| (Some(row), file));
        let new = found.new.into_iter().map(|file| (None, file));
        for (row, file) in changed.chain(new) {
            if self.cancel.is_cancelled() {
                return None;
            }
            let Some(fingerprint) = self.read_fingerprint(&file) else {
                // The row of a file that cannot be read stays as it is.
                self.advance(1);
                continue;
            };
            match row {
                Some(row) if index[row].fingerprint == fingerprint => updates.push(Op::Update {
                    row,
                    file,
                    fingerprint,
                }),
                Some(row) => {
                    states[row] = RowState::Displaced;
                    candidates.push(Candidate {
                        file,
                        fingerprint,
                        displaces: Some(row),
                    });
                }
                None => candidates.push(Candidate {
                    file,
                    fingerprint,
                    displaces: None,
                }),
            }
        }
        Some((updates, candidates))
    }

    /// Reads what the changes of one batch need from the files, stores the
    /// changes in one transaction and runs the batch hook.
    ///
    /// The token is checked before each read of a fingerprint and before
    /// each read of tags. When it is cancelled, the function returns
    /// [`Applied::Cancelled`]: it reads no further file, stores no change
    /// of the batch and does not run the hook. A batch is therefore stored
    /// whole or not at all, which the exchange batch needs, and the next
    /// scan finds the changes of the batch again.
    ///
    /// A change whose tags cannot be read is left out, and the next scan
    /// finds it again: a new file gets no row, a changed file leaves its row
    /// as it is, and a moved file does not get its row. The row of the moved
    /// file stays at its old path when no file is there. When another file
    /// is there, which is the case for a displaced row, the row gets its
    /// placeholder path and the scan records an error.
    fn apply(
        &mut self,
        index: &[IndexEntry],
        states: &[RowState],
        batch: Vec<Op>,
    ) -> Result<Applied> {
        let mut writes = Vec::with_capacity(batch.len());
        let mut files = 0;
        // The messages about the rows that get their placeholder path.
        let mut detached = Vec::new();
        for op in batch {
            match op {
                Op::Delete { row } => writes.push(Write::Delete { id: &index[row].id }),
                Op::Detach { row } => {
                    let known = &index[row];
                    detached.push(format!(
                        "track {} at \"{}\" has no file until a scan finds it: another track \
                         took its path, and a path or a file that could not be read may hold its \
                         file",
                        known.id, known.rel_path
                    ));
                    writes.push(Write::detach(known));
                }
                Op::Update {
                    row,
                    file,
                    fingerprint,
                } => {
                    if self.cancel.is_cancelled() {
                        return Ok(Applied::Cancelled);
                    }
                    files += 1;
                    if let Some(meta) = self.read_tags(&file) {
                        writes.push(Write::Update {
                            id: &index[row].id,
                            file,
                            fingerprint,
                            meta,
                        });
                    }
                }
                Op::Relink { row, file, reread } => {
                    if reread && self.cancel.is_cancelled() {
                        return Ok(Applied::Cancelled);
                    }
                    files += 1;
                    let known = &index[row];
                    let meta = if reread { self.read_tags(&file) } else { None };
                    if reread && meta.is_none() {
                        // The row does not move in this scan. A displaced
                        // row leaves its path all the same, because another
                        // file is there.
                        if states[row] == RowState::Displaced {
                            detached.push(format!(
                                "track {} at \"{}\" has no file until a scan reads the tags of \
                                 its file at \"{}\": another file is at its path",
                                known.id, known.rel_path, file.rel_path
                            ));
                            writes.push(Write::detach(known));
                        }
                        continue;
                    }
                    writes.push(Write::Relink {
                        id: &known.id,
                        via_temporary: states[row] == RowState::Displaced,
                        file,
                        fingerprint: &known.fingerprint,
                        meta,
                    });
                }
                Op::Insert { file, fingerprint } => {
                    if self.cancel.is_cancelled() {
                        return Ok(Applied::Cancelled);
                    }
                    files += 1;
                    let fingerprint = match fingerprint {
                        Some(fingerprint) => fingerprint,
                        None => {
                            let Some(fingerprint) = self.read_fingerprint(&file) else {
                                continue;
                            };
                            // The fingerprint read is a file read of its
                            // own, so the tag read has a check of its own.
                            if self.cancel.is_cancelled() {
                                return Ok(Applied::Cancelled);
                            }
                            fingerprint
                        }
                    };
                    if let Some(meta) = self.read_tags(&file) {
                        writes.push(Write::Insert {
                            file,
                            fingerprint,
                            meta,
                        });
                    }
                }
            }
        }

        let Stored {
            counts,
            changed_library,
        } = self.db.with_writer(|conn| store(conn, &writes))?;
        self.changed_library |= changed_library;
        self.summary.added += counts.added;
        self.summary.changed += counts.changed;
        self.summary.moved += counts.moved;
        self.summary.removed += counts.removed;
        self.advance(files);
        for message in detached {
            self.problem(message);
        }
        if let Some(on_batch) = &self.options.on_batch {
            on_batch();
        }
        Ok(Applied::Stored)
    }

    /// Deletes the rows whose file is gone, and the folder rows that are no
    /// longer configured.
    ///
    /// A row stays when a file that the scan could not read may be its file.
    /// A configured folder that had tracks and holds no supported file keeps
    /// its rows. A folder that is no longer configured keeps its rows unless
    /// at least one folder is configured and every configured folder was
    /// read in full and holds a supported file. Otherwise its files may be
    /// in a folder that the scan could not read.
    fn remove(
        &mut self,
        index: &[IndexEntry],
        walks: &HashMap<i64, FolderWalk>,
        unconfigured: &[Folder],
        unmatched: &[usize],
    ) -> Result<()> {
        let all_hold_files =
            !self.folders.is_empty() && walks.values().all(FolderWalk::holds_files);
        let mut deleted_ids: Vec<&str> = Vec::new();
        // The rows in configured folders that an unread path or file keeps.
        let mut held_back: u64 = 0;
        // The rows that stay in each folder that is no longer configured.
        let mut kept: HashMap<i64, u64> = HashMap::new();
        for &row in unmatched {
            let entry = &index[row];
            let may_be_unread = self.unread.may_be(entry);
            match walks.get(&entry.folder_id) {
                Some(_) if may_be_unread => held_back += 1,
                Some(walk) if walk.supported > 0 => deleted_ids.push(&entry.id),
                // The folder had tracks and holds no supported file. The
                // walk recorded the error.
                Some(_) => {}
                None if !may_be_unread && all_hold_files => deleted_ids.push(&entry.id),
                None => *kept.entry(entry.folder_id).or_default() += 1,
            }
        }
        if held_back > 0 {
            let kept = if held_back == 1 {
                "1 track whose file is gone was kept".to_owned()
            } else {
                format!("{held_back} tracks whose files are gone were kept")
            };
            self.problem(format!(
                "{kept}, because a path or a file could not be read"
            ));
        }
        let reason = if self.folders.is_empty() {
            "no music folder is configured"
        } else {
            "a configured music folder could not be read in full or has no supported file"
        };
        for folder in unconfigured {
            if let Some(&count) = kept.get(&folder.id) {
                let (tracks, were) = if count == 1 {
                    ("1 track".to_owned(), "was")
                } else {
                    (format!("{count} tracks"), "were")
                };
                self.problem(format!(
                    "{tracks} of the removed music folder {} {were} kept, because {reason}",
                    describe(folder)
                ));
            }
        }

        let removed = self.db.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let removed = tracks::delete_tracks(&tx, &deleted_ids)?;
            for folder in unconfigured {
                if tracks::count_tracks_in_folder(&tx, folder.id)? == 0 {
                    folders::delete(&tx, folder.id)?;
                }
            }
            tx.commit()?;
            Ok(removed)
        })?;
        // The deletion of a folder row that has no track changes no artist.
        self.changed_library |= removed > 0;
        self.summary.removed += removed;
        Ok(())
    }

    /// Reads the fingerprint of `file`. When the file cannot be read, records
    /// an error and the size of the file, and returns `None`. No missing row
    /// of that size is deleted in this scan, because the file may be the
    /// file of the row.
    fn read_fingerprint(&mut self, file: &FoundFile) -> Option<String> {
        match (self.reader.fingerprint)(&self.path_of(file)) {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                self.unread.sizes.insert(file.size);
                let folder = self.folder_of(file).map(describe).unwrap_or_default();
                self.problem(format!(
                    "\"{}\" in music folder {folder} cannot be read: {error}; no track of {} \
                     bytes was deleted",
                    file.rel_path, file.size
                ));
                None
            }
        }
    }

    /// Reads the tags of `file`. Logs a warning and returns `None` when the
    /// tags cannot be read.
    fn read_tags(&mut self, file: &FoundFile) -> Option<TrackMeta> {
        self.summary.tags_read += 1;
        match (self.reader.tags)(&self.path_of(file)) {
            Ok(meta) => Some(meta),
            Err(error) => {
                tracing::warn!(%error, "the file is left for the next scan");
                None
            }
        }
    }

    /// Returns the folder row of `file`.
    fn folder_of(&self, file: &FoundFile) -> Option<&Folder> {
        self.folders
            .iter()
            .find(|folder| folder.id == file.folder_id)
    }

    /// Returns the path that opens `file`: the path of its folder joined
    /// with the components of its relative path.
    fn path_of(&self, file: &FoundFile) -> PathBuf {
        let mut path = self
            .folder_of(file)
            .map(|folder| folder.path.clone())
            .unwrap_or_default();
        path.extend(file.rel_path.split('/'));
        path
    }

    /// Logs `message` as a warning and adds it to the errors of the scan.
    fn problem(&mut self, message: String) {
        tracing::warn!("{message}");
        self.summary.errors.push(message);
    }

    /// Adds `files` to the progress counter.
    fn advance(&self, files: u64) {
        self.options.progress.fetch_add(files, Ordering::Relaxed);
    }
}

/// Returns whether the scan knows that the file of a row is gone, so that
/// the row can follow a file elsewhere or be deleted. That is the case when
/// the walk saw no file at the path of the row and the folder of the row is
/// no longer configured, or the walk read the directory of the path.
fn is_gone(entry: &IndexEntry, state: RowState, walks: &HashMap<i64, FolderWalk>) -> bool {
    state == RowState::Unseen
        && walks
            .get(&entry.folder_id)
            .is_none_or(|walk| !walk.is_unread(&entry.rel_path))
}

/// Matches the missing rows to the candidates and returns the changes.
///
/// `missing` holds the rows whose file is gone and the displaced rows, in
/// the order in which they choose. A row chooses among the candidates that
/// have its fingerprint and that no earlier row chose. `updates` holds the
/// updates of the rows whose changed file kept its fingerprint.
///
/// A displaced row without a match stays at its path and takes the content
/// of the file there, unless another row chose that file. That is the case
/// of a file that was retagged in place. When another row chose the file,
/// the displaced row leaves its path: it is deleted, or it is kept without a
/// file when a path or a file in `unread` may hold its file.
fn plan(
    index: &[IndexEntry],
    states: &[RowState],
    missing: &[usize],
    candidates: Vec<Candidate>,
    updates: Vec<Op>,
    unread: &Unread,
) -> Plan {
    let mut by_fingerprint: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, candidate) in candidates.iter().enumerate() {
        by_fingerprint
            .entry(candidate.fingerprint.as_str())
            .or_default()
            .push(position);
    }
    let mut chosen_by: Vec<Option<usize>> = vec![None; candidates.len()];
    let mut matched = vec![false; index.len()];
    for &row in missing {
        let entry = &index[row];
        let Some(same) = by_fingerprint.get(entry.fingerprint.as_str()) else {
            continue;
        };
        let best = same
            .iter()
            .copied()
            .filter(|&position| chosen_by[position].is_none())
            .min_by_key(|&position| rank(entry, &candidates[position].file));
        if let Some(position) = best {
            chosen_by[position] = Some(row);
            matched[row] = true;
        }
    }
    drop(by_fingerprint);

    let mut plan = Plan {
        rest: updates,
        unmatched: missing
            .iter()
            .copied()
            .filter(|&row| !matched[row] && states[row] == RowState::Unseen)
            .collect(),
        ..Plan::default()
    };
    for (candidate, chosen_by) in candidates.into_iter().zip(chosen_by) {
        let Candidate {
            file,
            fingerprint,
            displaces,
        } = candidate;
        // The row at the path of the file, when it follows no other file.
        let stays = displaces.filter(|&row| !matched[row]);
        match (chosen_by, stays) {
            (Some(row), stays) => {
                // The row at the path of the file has no file there any more.
                if let Some(replaced) = stays {
                    plan.exchange.push(if unread.may_be(&index[replaced]) {
                        Op::Detach { row: replaced }
                    } else {
                        Op::Delete { row: replaced }
                    });
                }
                let known = &index[row];
                let reread = known.mtime != file.mtime
                    || known.size != file.size
                    || !same_extension(&known.rel_path, &file.rel_path);
                let op = Op::Relink { row, file, reread };
                if displaces.is_some() || states[row] == RowState::Displaced {
                    plan.exchange.push(op);
                } else {
                    plan.rest.push(op);
                }
            }
            (None, Some(row)) => plan.rest.push(Op::Update {
                row,
                file,
                fingerprint,
            }),
            (None, None) => {
                let op = Op::Insert {
                    file,
                    fingerprint: Some(fingerprint),
                };
                // The row at the path of the file leaves it in the exchange.
                if displaces.is_some() {
                    plan.exchange.push(op);
                } else {
                    plan.rest.push(op);
                }
            }
        }
    }
    plan
}

/// Returns the key by which a missing row orders the candidates that have
/// its fingerprint. The smallest key wins: the same file name first, then
/// the shortest path distance, then the order of the relative paths, then
/// the order of the folder IDs.
fn rank<'a>(missing: &IndexEntry, file: &'a FoundFile) -> (bool, usize, &'a str, i64) {
    (
        file_name(&file.rel_path) != file_name(&missing.rel_path),
        path_distance(&missing.rel_path, &file.rel_path),
        &file.rel_path,
        file.folder_id,
    )
}

/// What [`store`] did with one batch.
struct Stored {
    /// The rows that were inserted, updated in place, relinked and deleted.
    /// A row that was given its placeholder path and kept is in no count.
    counts: ScanCounts,
    /// Whether the transaction wrote a row: a track row that was inserted,
    /// updated, relinked, deleted or given its placeholder path, and the
    /// album rows and the artist rows that were made for a track.
    changed_library: bool,
}

/// Stores the changes of one batch in one transaction and returns what it
/// changed.
///
/// The deletions run first. Then every displaced row that moves or that is
/// kept without a file is given its placeholder path, which no file can
/// have. Then the rows that move are given their new paths. In this order
/// two rows can exchange paths, although SQLite checks the unique index on
/// the locator after every statement.
fn store(conn: &mut Connection, writes: &[Write<'_>]) -> Result<Stored> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // SQLite counts every row that a statement of the connection inserts,
    // updates or deletes. The statements of this function write rows of the
    // library only.
    let rows_written = tx.total_changes();
    let mut counts = ScanCounts::default();

    let deleted: Vec<&str> = writes
        .iter()
        .filter_map(|write| match write {
            Write::Delete { id } => Some(*id),
            _ => None,
        })
        .collect();
    counts.removed = tracks::delete_tracks(&tx, &deleted)?;
    for write in writes {
        match write {
            Write::Detach {
                id,
                folder_id,
                mtime,
                size,
            } => {
                let placeholder = tracks::placeholder_path(id);
                tracks::relink_track(&tx, id, *folder_id, &placeholder, *mtime, *size)?;
            }
            Write::Relink {
                id,
                via_temporary: true,
                file,
                ..
            } => {
                let (folder_id, mtime) = (file.folder_id, file.mtime);
                let placeholder = tracks::placeholder_path(id);
                tracks::relink_track(&tx, id, folder_id, &placeholder, mtime, file.size)?;
            }
            _ => {}
        }
    }

    for write in writes {
        match write {
            Write::Delete { .. } | Write::Detach { .. } => {}
            Write::Relink {
                id,
                file,
                fingerprint,
                meta,
                ..
            } => {
                let (folder_id, mtime) = (file.folder_id, file.mtime);
                tracks::relink_track(&tx, id, folder_id, &file.rel_path, mtime, file.size)?;
                if let Some(meta) = meta {
                    with_content(&tx, file, fingerprint, meta, |content| {
                        tracks::update_track_meta(&tx, id, content)
                    })?;
                }
                counts.moved += 1;
            }
            Write::Update {
                id,
                file,
                fingerprint,
                meta,
            } => {
                with_content(&tx, file, fingerprint, meta, |content| {
                    tracks::update_track_meta(&tx, id, content)
                })?;
                counts.changed += 1;
            }
            Write::Insert {
                file,
                fingerprint,
                meta,
            } => {
                let id = ids::new_id(Kind::Track);
                with_content(&tx, file, fingerprint, meta, |content| {
                    tracks::insert_track(&tx, &id, file.folder_id, &file.rel_path, content)
                })?;
                counts.added += 1;
            }
        }
    }
    let changed_library = tx.total_changes() != rows_written;
    tx.commit()?;
    Ok(Stored {
        counts,
        changed_library,
    })
}

/// Upserts the artist row and the album row of `meta` and runs `write` with
/// the content of `file`.
fn with_content(
    conn: &Connection,
    file: &FoundFile,
    fingerprint: &str,
    meta: &TrackMeta,
    write: impl FnOnce(&TrackContent<'_>) -> Result<()>,
) -> Result<()> {
    let artist_id = artists::upsert_by_key(conn, &meta.album_artist)?;
    let album_id = albums::upsert_by_key(
        conn,
        &artist_id,
        &meta.album,
        &meta.album_artist,
        meta.compilation,
    )?;
    write(&TrackContent {
        fingerprint,
        mtime: file.mtime,
        size: file.size,
        meta,
        artist_id: &artist_id,
        album_id: &album_id,
    })
}

/// What a scan that completed saw of the configured folders.
struct Seen<'a> {
    /// The folder rows of the configured folders.
    folders: &'a [Folder],
    /// What the walk of each configured folder found out.
    walks: &'a HashMap<i64, FolderWalk>,
}

/// What [`aggregate`] did.
struct Aggregated {
    /// The number of album rows.
    albums: u64,
    /// The number of artist rows.
    artists: u64,
    /// The albums whose resized covers in the cache are stale: the deleted
    /// albums and the albums whose cover changed.
    stale_covers: Vec<String>,
    /// Whether the step deleted or wrote an album row or an artist row. The
    /// deletion of annotations does not count: it removes what was attached
    /// to rows that are gone, and it changes no row of the library.
    changed_library: bool,
}

/// Step 6 of the scan, in one transaction. It reads no file: the covers come
/// from what the walk saw.
///
/// With `seen`, which a scan that completed passes:
///
/// 1. Every album that no track refers to is deleted, then every artist that
///    no album and no track refers to. These are the only deletions of album
///    rows and artist rows. An album or an artist that a track refers to
///    keeps its row and its ID, also when the track has no file.
/// 2. The song count, the duration, the year and the genre of every album
///    and the album count of every artist are recomputed. An artist row and
///    an album row whose name no track carries take the spelling that the
///    most tracks carry, and an album row takes the name of its artist row
///    (`docs/design.md` section 3.3).
/// 3. The cover of every album whose directory the walk saw is set
///    ([`resolve_cover`]).
/// 4. Every annotation of an item that no longer exists is deleted.
///
/// Without `seen`, which is the case of a cancelled scan, only step 2 runs.
/// A cancelled scan has not stored every change it found. An album without a
/// track may get a track from a file that the scan has not stored, and the
/// rows may name paths that the walk saw no file at.
///
/// A row that already has its values is not written. The result says
/// whether an album row or an artist row was deleted or written, which is
/// the number of rows that SQLite counts for the statements before the
/// deletion of annotations.
fn aggregate(conn: &mut Connection, seen: Option<&Seen<'_>>) -> Result<Aggregated> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows_written = tx.total_changes();
    let mut stale_covers = Vec::new();
    if seen.is_some() {
        stale_covers = albums::delete_orphans(&tx)?;
        artists::delete_orphans(&tx)?;
    }
    albums::recompute_aggregates(&tx)?;
    artists::recompute_aggregates(&tx)?;
    // The artists come first: an album row takes the name of its artist row.
    artists::update_names(&tx)?;
    albums::update_names(&tx)?;
    if let Some(seen) = seen {
        let tracks = albums::tracks_with_file(&tx)?;
        let directories = TrackDirectories::of(&tracks);
        for inputs in albums::cover_inputs(&tx)? {
            let Cover::Known(cover) = resolve_cover(&inputs, seen, &directories) else {
                continue;
            };
            let source = cover.as_ref().map(|(source, _)| source.as_str());
            let mtime = cover.as_ref().map(|(_, mtime)| *mtime);
            if inputs.cover_source.as_deref() == source && inputs.cover_mtime == mtime {
                continue;
            }
            albums::set_cover(&tx, &inputs.album_id, source, mtime)?;
            // The name of a cached file holds the ID of the album and the
            // modification time of its cover, and not the source. A new
            // source with the old time would be served from the old files.
            // That holds for an album without a stored cover too: the cache
            // holds files of the album when a scan without a cache directory
            // cleared its cover, and when a process ended between the commit
            // and the removal.
            stale_covers.push(inputs.album_id);
        }
    }
    // Every statement so far writes album rows and artist rows only.
    let changed_library = tx.total_changes() != rows_written;
    if seen.is_some() {
        annotations::delete_orphans(&tx)?;
    }
    let aggregated = Aggregated {
        albums: albums::count(&tx)?,
        artists: artists::count(&tx)?,
        stale_covers,
        changed_library,
    };
    tx.commit()?;
    Ok(aggregated)
}

/// The cover that a scan found for one album.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cover {
    /// The walk did not see the directory of the album, so the scan does not
    /// know which image files it holds. The stored cover stays.
    Unknown,
    /// The source and the modification time of the cover, or `None` for an
    /// album without a cover.
    Known(Option<(String, i64)>),
}

/// The start of the `cover_source` of an embedded picture. The ID of the
/// track follows it. `library::covers` reads the value.
pub(super) const EMBEDDED: &str = "embedded:";

/// Returns the cover of an album from its tracks and from what the walk saw
/// (`docs/design.md` section 1.3).
///
/// The directory of an album is the directory of its first track that has a
/// file. The cover is, in this order:
///
/// 1. The cover image file of that directory ([`cover_rank`]), with the
///    absolute path of the file as the source and its modification time.
/// 2. The cover image file of the parent directory of the disc directories
///    of the album ([`TrackDirectories::parent_of_discs`]), with the same
///    source and time. An album has that directory when its tracks are in
///    two or more directories of one parent directory, and no track of
///    another album is at or below the parent directory.
/// 3. The embedded picture of the first track that has a file and a picture,
///    with `embedded:<track id>` as the source and the modification time of
///    the track.
/// 4. No cover.
///
/// A track without a file gives no directory and no picture. An album whose
/// tracks all have no file has no cover.
///
/// The walk knows the image files of a directory when it listed the
/// directory and saw a supported file in it. That is not the case for a
/// directory in a folder that is not configured, for a directory at or below
/// a path that the walk could not read, and for the directory of a row whose
/// file is gone and that the scan kept. The result is then
/// [`Cover::Unknown`]. A walk that listed a directory listed its parent
/// directory before, so it knows the image files of both.
///
/// The walk records the image files inside a folder only, and the parent
/// directory is a path inside the folder of the tracks. No file outside the
/// configured folders is the cover of an album.
fn resolve_cover(
    inputs: &CoverInputs,
    seen: &Seen<'_>,
    directories: &TrackDirectories<'_>,
) -> Cover {
    let Some((folder_id, rel_path)) = &inputs.first_track else {
        return Cover::Known(None);
    };
    let directory = directory(rel_path);
    let folder = seen.folders.iter().find(|folder| folder.id == *folder_id);
    let (Some(folder), Some(walk)) = (folder, seen.walks.get(folder_id)) else {
        return Cover::Unknown;
    };
    if !walk.saw_directory(directory) {
        return Cover::Unknown;
    }
    let beside = walk.covers.get(directory).map(|image| (directory, image));
    let image = beside.or_else(|| {
        // The disc directories of an album are in one folder, which is the
        // folder of its first track.
        let (_, parent) = directories
            .parent_of_discs(&inputs.album_id)
            .filter(|(parent_folder, _)| parent_folder == folder_id)?;
        Some((parent, walk.covers.get(parent)?))
    });
    if let Some((directory, image)) = image {
        let mut path = folder.path.clone();
        path.extend(directory.split('/').filter(|name| !name.is_empty()));
        path.push(&image.name);
        return match path.to_str() {
            Some(path) => Cover::Known(Some((path.to_owned(), image.mtime))),
            // The path of a folder row and the names of the walk are valid
            // Unicode, so this path is too.
            None => Cover::Unknown,
        };
    }
    Cover::Known(
        inputs
            .first_picture
            .as_ref()
            .map(|(track_id, mtime)| (format!("{EMBEDDED}{track_id}"), *mtime)),
    )
}

/// The directories that hold the track rows, by album and by folder. The
/// scan reads from them whether the discs of an album have directories of
/// their own ([`TrackDirectories::parent_of_discs`]).
///
/// A track row counts for as long as it exists, also a row that the scan
/// kept although its file is gone. A track without a file is in no directory
/// and is not part of the value.
#[derive(Debug, Default)]
struct TrackDirectories<'a> {
    /// The directories of the tracks of each album, by album ID.
    albums: HashMap<&'a str, Placement<'a>>,
    /// The album of the tracks at and below a directory, by folder ID and by
    /// the relative path of the directory. The value is `None` when the
    /// tracks there belong to more than one album. A folder has an entry for
    /// every directory that holds a track and for every directory above
    /// such a directory, up to the folder itself, which is the empty path.
    below: HashMap<i64, HashMap<&'a str, Option<&'a str>>>,
}

/// The directories that hold the tracks of one album.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement<'a> {
    /// One directory.
    One { folder_id: i64, directory: &'a str },
    /// Two or more directories of one folder that have the same parent
    /// directory.
    Siblings { folder_id: i64, parent: &'a str },
    /// Directories that have no common parent directory: they are in two
    /// folders, at different depths or below different directories, or one
    /// of them is the folder itself.
    Apart,
}

impl<'a> Placement<'a> {
    /// Adds the directory of one more track of the album.
    fn add(&mut self, folder_id: i64, directory: &'a str) {
        *self = match *self {
            Self::One {
                folder_id: known,
                directory: only,
            } if known == folder_id => {
                if only == directory {
                    return;
                }
                match (parent(only), parent(directory)) {
                    (Some(first), Some(second)) if first == second => Self::Siblings {
                        folder_id,
                        parent: first,
                    },
                    _ => Self::Apart,
                }
            }
            Self::Siblings {
                folder_id: known,
                parent: shared,
            } if known == folder_id && parent(directory) == Some(shared) => return,
            _ => Self::Apart,
        };
    }
}

impl<'a> TrackDirectories<'a> {
    /// Returns the directories of `tracks`, the tracks that have a file.
    fn of(tracks: &'a [AlbumTrack]) -> Self {
        let mut directories = Self::default();
        for track in tracks {
            let album = track.album_id.as_str();
            let directory = directory(&track.rel_path);
            directories
                .albums
                .entry(album)
                .and_modify(|placement| placement.add(track.folder_id, directory))
                .or_insert(Placement::One {
                    folder_id: track.folder_id,
                    directory,
                });
            let below = directories.below.entry(track.folder_id).or_default();
            let mut next = Some(directory);
            while let Some(at) = next {
                below
                    .entry(at)
                    .and_modify(|held| {
                        if *held != Some(album) {
                            *held = None;
                        }
                    })
                    .or_insert(Some(album));
                next = parent(at);
            }
        }
        directories
    }

    /// Returns the parent directory of the disc directories of the album
    /// `album_id`, as the ID of its folder and its relative path
    /// (`docs/design.md` section 1.3).
    ///
    /// An album has that directory when both of these hold:
    ///
    /// - Its tracks are in two or more directories, and all of these
    ///   directories have the same parent directory in one folder. An album
    ///   in one directory has none. In a library of `<artist>/<album>`
    ///   directories the parent directory of such an album is the directory
    ///   of its artist, and an image there is not the cover of the album.
    /// - No track of another album is at or below the parent directory.
    ///
    /// The parent directory is a directory of the folder or the folder
    /// itself. The folder itself has no parent directory ([`parent`]), so
    /// tracks in the folder itself are in no disc directory.
    fn parent_of_discs(&self, album_id: &str) -> Option<(i64, &'a str)> {
        let Some(Placement::Siblings { folder_id, parent }) = self.albums.get(album_id) else {
            return None;
        };
        let held = self.below.get(folder_id)?.get(parent)?;
        (*held == Some(album_id)).then_some((*folder_id, *parent))
    }
}

/// The file stems of a cover image, in the order in which they are chosen.
const COVER_STEMS: [&str; 4] = ["cover", "folder", "front", "album"];

/// The extensions of a cover image, in the order in which they are chosen.
const COVER_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// Returns the place of the file name `name` in the order of the cover
/// image names, or `None` when the name is not one of them.
///
/// A cover image is named `cover`, `folder`, `front` or `album` with the
/// extension `jpg`, `jpeg`, `png` or `webp`, without regard to case. The
/// stem decides the place first, in that order, and then the extension:
/// `cover.png` precedes `folder.jpg`, and `cover.jpg` precedes `cover.png`.
fn cover_rank(name: &str) -> Option<usize> {
    let (stem, extension) = name.rsplit_once('.')?;
    let place = |known: &[&str], text: &str| {
        known
            .iter()
            .position(|known| known.eq_ignore_ascii_case(text))
    };
    let stem = place(&COVER_STEMS, stem)?;
    let extension = place(&COVER_EXTENSIONS, extension)?;
    Some(stem * COVER_EXTENSIONS.len() + extension)
}

/// The number of track IDs that [`remove_cached_covers`] looks up while it
/// holds one read connection.
const TRACK_LOOKUPS: usize = 500;

/// Removes the stale resized covers from the cover cache `cache_dir`: the
/// files of the albums `album_ids`, and the files of every track that has no
/// row in `db`. A file of the cache is named
/// `<cover ID>-<size>-<cover mtime>-<version>.jpg`, and a cover ID is the ID
/// of an album or of a track (`docs/design.md` sections 3.3 and 3.5).
///
/// The files of a track are removed when no row has the ID of the track, so
/// the function needs no list of the tracks that this scan deleted. It also
/// removes the files of a track that an earlier scan deleted, and the files
/// that the cover service wrote after such a scan listed the directory. A
/// file of an album is removed for the albums in `album_ids` and for no other
/// album.
///
/// The directory is listed first, and the database is asked afterwards. The
/// cover service writes a file for a track that has a row, and an ID is
/// given once. A track that has no row after its file was listed is
/// therefore a deleted track, and never a new one.
///
/// Every path that the function removes is the path of an entry that the
/// listing of `cache_dir` returned: a name of the directory, joined to the
/// path of the directory. No other path is built, so no file outside the
/// directory is removed. An entry that is a symbolic link is removed as a
/// link, and its target stays.
///
/// The function lists the directory once for a scan, on the thread of the
/// scan, and holds no write connection. The lookups of
/// [`TRACK_LOOKUPS`] tracks share one read connection, which is free between
/// two such groups.
///
/// A file that cannot be removed stays, with a warning. A cache directory
/// that does not exist holds no file. When the database cannot be read, the
/// files of the tracks stay, with a warning, and the next scan removes them.
fn remove_cached_covers(db: &Db, cache_dir: &Path, album_ids: &[String]) {
    let stale_albums: HashSet<&str> = album_ids.iter().map(String::as_str).collect();
    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                path = %cache_dir.display(),
                %error,
                "the cover cache cannot be read; its stale files stay"
            );
            return;
        }
    };
    // The files whose name starts with the ID of a track, by that ID.
    let mut of_tracks: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    path = %cache_dir.display(),
                    %error,
                    "an entry of the cover cache cannot be read"
                );
                continue;
            }
        };
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(cached_cover_id) else {
            continue;
        };
        if stale_albums.contains(id) {
            remove_cached_cover(&entry.path());
        } else if matches!(ids::parse(id), Some((Kind::Track, _))) {
            of_tracks
                .entry(id.to_owned())
                .or_default()
                .push(entry.path());
        }
    }

    let track_ids: Vec<&String> = of_tracks.keys().collect();
    for group in track_ids.chunks(TRACK_LOOKUPS) {
        let without_row = db.with_reader(|conn| {
            let mut without_row = Vec::new();
            for id in group {
                if tracks::get_track(conn, id)?.is_none() {
                    without_row.push(*id);
                }
            }
            Ok(without_row)
        });
        match without_row {
            Ok(without_row) => {
                for path in without_row.into_iter().flat_map(|id| &of_tracks[id]) {
                    remove_cached_cover(path);
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the tracks of the cover cache cannot be read; the files of deleted tracks stay"
                );
                return;
            }
        }
    }
}

/// Removes the file `path` of the cover cache. A file that cannot be removed
/// stays, with a warning.
fn remove_cached_cover(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "the stale cover cannot be removed"
        );
    }
}

/// Returns the cover ID at the start of the name of a cached cover: the
/// prefix, the hyphen and the hex characters of
/// `<prefix>-<hex>-<size>-<cover mtime>-<version>.jpg`. The parts after the
/// ID are not read, so the name of a file that another version of the cover
/// service wrote, which has no version, starts with its cover ID as well.
fn cached_cover_id(name: &str) -> Option<&str> {
    let (prefix, rest) = name.split_once('-')?;
    let (hex, _) = rest.split_once('-')?;
    name.get(..prefix.len() + 1 + hex.len())
}

/// Returns the modification time in Unix seconds and the size of the file
/// of `entry`, or the reason why they cannot be read.
fn file_state(entry: &walkdir::DirEntry) -> std::result::Result<(i64, u64), String> {
    let metadata = entry.metadata().map_err(|error| error.to_string())?;
    state_of(&metadata)
}

/// Returns the modification time in Unix seconds and the size that
/// `metadata` holds, or the reason why the time cannot be read.
fn state_of(metadata: &fs::Metadata) -> std::result::Result<(i64, u64), String> {
    let modified = metadata.modified().map_err(|error| error.to_string())?;
    Ok((unix_seconds(modified), metadata.len()))
}

/// Returns the part of `path` below the path of `tree`. `path` is the path
/// of an entry that the walk of the tree returned.
fn below<'a>(tree: &Tree, path: &'a Path) -> &'a Path {
    path.strip_prefix(&tree.path).unwrap_or(path)
}

/// Returns whether `target` is one of `roots` or below one of them. Both are
/// canonical paths.
///
/// The paths are compared component by component and in the letter case that
/// they have. On macOS and on Windows, whose file systems ignore the case by
/// default, `fs::canonicalize` returns each name as the file system stores
/// it, so two paths of one file do not differ in case. On Windows it returns
/// both paths in the form that starts with `\\?\`. `media::stream` compares
/// the same paths in the same way before it serves a file, so a file that
/// has a row here passes the check there.
fn inside(target: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| target.starts_with(root))
}

/// Returns whether the walk of a configured folder reaches `target` at its
/// own path, which is through no link. `target` is a canonical path, a
/// directory when `is_dir` is set and else a file. `roots` holds the
/// canonical paths of the configured folders.
///
/// A walk reaches a path below its folder when it skips no name of the
/// path: no name starts with `.`, no directory has the name of a system
/// directory, and every name is valid Unicode. A file is reached when its
/// extension is a supported one as well. The folder itself is reached
/// whatever its name is.
fn walks_reach(target: &Path, roots: &[PathBuf], is_dir: bool) -> bool {
    if !is_dir && !tags::is_supported(target) {
        return false;
    }
    roots.iter().any(|root| {
        let Ok(relative) = target.strip_prefix(root) else {
            return false;
        };
        let mut names = relative.components().peekable();
        while let Some(component) = names.next() {
            let Component::Normal(name) = component else {
                return false;
            };
            let directory = is_dir || names.peek().is_some();
            if name.to_str().is_none()
                || is_hidden(name)
                || (directory && is_system_directory(name))
            {
                return false;
            }
        }
        true
    })
}

/// Returns `time` in whole Unix seconds, rounded towards the past.
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(error) => {
            let before = error.duration();
            let seconds = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            if before.subsec_nanos() == 0 {
                -seconds
            } else {
                -seconds - 1
            }
        }
    }
}

/// Returns the relative path that the database stores for `relative`: the
/// components joined with `/`, on every OS.
///
/// Returns `None` for an empty path, for a component that is not a plain
/// name, and for a name that is not valid Unicode.
fn rel_path(relative: &Path) -> Option<String> {
    join_components(relative.components())
}

/// Joins the components of a relative path with `/`. The components of
/// `Artist\Album\01.flac` on Windows and of `Artist/Album/01.flac` on Unix
/// are the same three names, so both give the same result.
fn join_components<'a>(components: impl IntoIterator<Item = Component<'a>>) -> Option<String> {
    let mut joined = String::new();
    for component in components {
        let Component::Normal(name) = component else {
            return None;
        };
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(name.to_str()?);
    }
    (!joined.is_empty()).then_some(joined)
}

/// Returns whether `name` starts with `.`. The scan skips such an entry.
fn is_hidden(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// The directories that Windows keeps for itself at the root of a volume.
/// Their names are matched without regard to case.
const WINDOWS_SYSTEM_DIRECTORIES: [&str; 2] = ["System Volume Information", "$RECYCLE.BIN"];

/// The directory at the root of an ext file system that holds recovered
/// files. Its name is matched in this spelling.
const LOST_AND_FOUND: &str = "lost+found";

/// Returns whether `name` is the name of a directory that an operating
/// system keeps at the root of a volume. The scan skips a directory with
/// such a name at every depth, because a volume can be mounted below a
/// music folder. The directory holds no music, and the user of the server
/// cannot list it.
fn is_system_directory(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name == LOST_AND_FOUND
            || WINDOWS_SYSTEM_DIRECTORIES
                .iter()
                .any(|system| system.eq_ignore_ascii_case(name))
    })
}

/// Returns the name and the path of `folder` for a message.
fn describe(folder: &Folder) -> String {
    format!("\"{}\" ({})", folder.name, folder.path.display())
}

/// Returns the order of a row: its folder, then its relative path.
fn locator(entry: &IndexEntry) -> (i64, &str) {
    (entry.folder_id, &entry.rel_path)
}

/// Returns the last component of a relative path.
fn file_name(rel_path: &str) -> &str {
    rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
}

/// Returns the directory of a relative path: the path without its last
/// component. The directory of a file in the folder itself is the empty
/// path.
fn directory(rel_path: &str) -> &str {
    rel_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory)
}

/// Returns the parent directory of the directory `path` of a folder, as a
/// relative path. The folder itself, which is the empty path, has none: its
/// parent directory is outside the library, and the scan reads nothing
/// there.
fn parent(path: &str) -> Option<&str> {
    (!path.is_empty()).then(|| directory(path))
}

/// Returns the directory components of a relative path.
fn directories(rel_path: &str) -> impl Iterator<Item = &str> {
    directory(rel_path)
        .split('/')
        .filter(|name| !name.is_empty())
}

/// Returns the number of directory steps between the directories of two
/// relative paths: the steps up from the first to their common directory
/// plus the steps down to the second. The folders of the paths do not count.
fn path_distance(from: &str, to: &str) -> usize {
    let common = directories(from)
        .zip(directories(to))
        .take_while(|(a, b)| a == b)
        .count();
    directories(from).count() + directories(to).count() - 2 * common
}

/// Returns the size at the start of a fingerprint, which has the form
/// `size:hash`.
fn fingerprint_size(fingerprint: &str) -> Option<u64> {
    fingerprint.split_once(':')?.0.parse().ok()
}

/// Returns whether two relative paths have the same extension without
/// regard to case. The extension decides the suffix and the content type of
/// a track.
fn same_extension(a: &str, b: &str) -> bool {
    let extension = |rel_path: &str| {
        file_name(rel_path)
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
    };
    extension(a) == extension(b)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::fs;
    use std::rc::Rc;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;
    use crate::db::queries::tracks::Track;

    fn entry(id: &str, folder_id: i64, rel_path: &str, fingerprint: &str) -> IndexEntry {
        IndexEntry {
            id: id.to_owned(),
            folder_id,
            rel_path: rel_path.to_owned(),
            mtime: 100,
            size: 10,
            fingerprint: fingerprint.to_owned(),
        }
    }

    fn file(folder_id: i64, rel_path: &str) -> FoundFile {
        FoundFile {
            folder_id,
            rel_path: rel_path.to_owned(),
            mtime: 100,
            size: 10,
        }
    }

    fn candidate(rel_path: &str, fingerprint: &str, displaces: Option<usize>) -> Candidate {
        Candidate {
            file: file(1, rel_path),
            fingerprint: fingerprint.to_owned(),
            displaces,
        }
    }

    fn relink(row: usize, rel_path: &str) -> Op {
        Op::Relink {
            row,
            file: file(1, rel_path),
            reread: false,
        }
    }

    fn insert(rel_path: &str, fingerprint: &str) -> Op {
        Op::Insert {
            file: file(1, rel_path),
            fingerprint: Some(fingerprint.to_owned()),
        }
    }

    /// Plans with every row in `missing`, in the order of the rows, for a
    /// scan that read every file.
    fn plan_of(index: &[IndexEntry], states: &[RowState], candidates: Vec<Candidate>) -> Plan {
        plan_with(index, states, candidates, &Unread::default())
    }

    /// Plans with every row in `missing`, in the order of the rows.
    fn plan_with(
        index: &[IndexEntry],
        states: &[RowState],
        candidates: Vec<Candidate>,
        unread: &Unread,
    ) -> Plan {
        let mut missing: Vec<usize> = (0..index.len())
            .filter(|&row| states[row] != RowState::Present)
            .collect();
        missing.sort_by(|&a, &b| locator(&index[a]).cmp(&locator(&index[b])));
        plan(index, states, &missing, candidates, Vec::new(), unread)
    }

    /// Returns what the scan could not read when it failed to read the
    /// fingerprint of files of these sizes.
    fn unread_sizes(sizes: &[u64]) -> Unread {
        Unread {
            paths: false,
            sizes: sizes.iter().copied().collect(),
        }
    }

    #[test]
    fn rel_path_joins_the_components_with_a_slash_on_every_os() {
        // The path is built with the separator of the OS, which is `\` on
        // Windows.
        let native: PathBuf = ["Aurora Field", "Glass Harbour", "01 Low Tide.flac"]
            .iter()
            .collect();
        assert_eq!(
            rel_path(&native).as_deref(),
            Some("Aurora Field/Glass Harbour/01 Low Tide.flac")
        );
        assert_eq!(
            rel_path(Path::new("01 Low Tide.flac")).as_deref(),
            Some("01 Low Tide.flac")
        );
    }

    // A backslash separates components on Windows only.
    #[cfg(windows)]
    #[test]
    fn rel_path_of_a_windows_path_equals_the_unix_one() {
        let unix = rel_path(Path::new("Aurora Field/Glass Harbour/01 Low Tide.flac"));
        assert_eq!(
            rel_path(Path::new(r"Aurora Field\Glass Harbour\01 Low Tide.flac")),
            unix
        );
        assert_eq!(
            rel_path(Path::new(r"Aurora Field\Glass Harbour/01 Low Tide.flac")),
            unix
        );
        assert_eq!(rel_path(Path::new(r"C:\Music\a.flac")), None);
    }

    #[test]
    fn rel_path_of_windows_style_components_equals_the_unix_one() {
        // `Path::components` yields these values for the Windows path
        // `Aurora Field\Glass Harbour\01 Low Tide.flac`.
        let windows = ["Aurora Field", "Glass Harbour", "01 Low Tide.flac"]
            .map(|name| Component::Normal(OsStr::new(name)));
        let unix = rel_path(Path::new("Aurora Field/Glass Harbour/01 Low Tide.flac"));
        assert_eq!(join_components(windows), unix);
        assert_eq!(
            unix.as_deref(),
            Some("Aurora Field/Glass Harbour/01 Low Tide.flac")
        );

        // An absolute Windows path starts with a prefix and a root. It is
        // not a relative path.
        let absolute = [Component::RootDir, Component::Normal(OsStr::new("a.flac"))];
        assert_eq!(join_components(absolute), None);
    }

    #[test]
    fn rel_path_rejects_paths_that_are_not_plain_names() {
        for rejected in ["", "/a.flac", "../a.flac", "a/../b.flac"] {
            assert_eq!(rel_path(Path::new(rejected)), None, "{rejected:?}");
        }
    }

    // A name that is not valid Unicode is built from raw bytes, which only
    // Unix allows.
    #[cfg(unix)]
    #[test]
    fn rel_path_rejects_a_name_that_is_not_unicode() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new("Artist").join(OsStr::from_bytes(b"bad\xff.flac"));
        assert_eq!(rel_path(&path), None);
    }

    #[test]
    fn is_hidden_is_true_for_a_leading_dot_only() {
        assert!(is_hidden(OsStr::new(".hidden.flac")));
        assert!(is_hidden(OsStr::new(".")));
        assert!(!is_hidden(OsStr::new("a.flac")));
        assert!(!is_hidden(OsStr::new("")));
    }

    #[test]
    fn unix_seconds_rounds_towards_the_past() {
        assert_eq!(unix_seconds(UNIX_EPOCH), 0);
        assert_eq!(unix_seconds(UNIX_EPOCH + Duration::from_millis(1999)), 1);
        assert_eq!(unix_seconds(UNIX_EPOCH - Duration::from_secs(2)), -2);
        assert_eq!(unix_seconds(UNIX_EPOCH - Duration::from_millis(1500)), -2);
    }

    #[test]
    fn path_distance_counts_the_directory_steps() {
        assert_eq!(path_distance("a.flac", "b.flac"), 0);
        assert_eq!(path_distance("x/a.flac", "x/b.flac"), 0);
        assert_eq!(path_distance("a.flac", "x/a.flac"), 1);
        assert_eq!(path_distance("x/y/a.flac", "x/a.flac"), 1);
        assert_eq!(path_distance("x/y/a.flac", "x/z/a.flac"), 2);
        assert_eq!(path_distance("x/y/a.flac", "p/q/r/a.flac"), 5);
    }

    #[test]
    fn same_extension_ignores_case_and_directories() {
        assert!(same_extension("a/b.flac", "c.FLAC"));
        assert!(!same_extension("a.ogg", "a.opus"));
        assert!(!same_extension("a.b/c", "a.b"));
        assert!(same_extension("x", "y/z"));
    }

    #[test]
    fn an_unread_path_covers_itself_and_what_is_below_it() {
        let walk = FolderWalk {
            unread: vec!["Artist/Album".to_owned()],
            supported: 1,
            ..FolderWalk::default()
        };
        assert!(walk.is_unread("Artist/Album"));
        assert!(walk.is_unread("Artist/Album/01.flac"));
        assert!(walk.is_unread("Artist/Album/disc 1/01.flac"));
        assert!(!walk.is_unread("Artist/Album 2/01.flac"));
        assert!(!walk.is_unread("Artist/01.flac"));
        assert!(!walk.holds_files());

        let failed = FolderWalk {
            unread: vec![String::new()],
            supported: 0,
            ..FolderWalk::default()
        };
        assert!(failed.is_unread("Artist/Album/01.flac"));

        let read = FolderWalk {
            unread: Vec::new(),
            supported: 1,
            ..FolderWalk::default()
        };
        assert!(!read.is_unread("Artist/Album/01.flac"));
        assert!(read.holds_files());
        assert!(!FolderWalk::default().holds_files());
    }

    #[test]
    fn a_missing_row_follows_the_file_with_its_fingerprint() {
        let index = [entry("tr-a", 1, "old/a.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Unseen],
            vec![
                candidate("new/b.flac", "10:bb", None),
                candidate("new/a.flac", "10:aa", None),
            ],
        );
        assert_eq!(
            plan,
            Plan {
                exchange: Vec::new(),
                rest: vec![insert("new/b.flac", "10:bb"), relink(0, "new/a.flac")],
                unmatched: Vec::new(),
            }
        );
    }

    #[test]
    fn a_missing_row_without_a_candidate_is_unmatched() {
        let index = [entry("tr-a", 1, "a.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Unseen],
            vec![candidate("b.flac", "10:bb", None)],
        );
        assert_eq!(plan.rest, [insert("b.flac", "10:bb")]);
        assert_eq!(plan.unmatched, [0]);
    }

    #[test]
    fn a_row_that_is_present_is_not_matched_to_a_copy_of_its_file() {
        let index = [entry("tr-a", 1, "a.flac", "10:aa")];
        let plan = plan(
            &index,
            &[RowState::Present],
            &[],
            vec![candidate("copy/a.flac", "10:aa", None)],
            Vec::new(),
            &Unread::default(),
        );
        assert_eq!(plan.rest, [insert("copy/a.flac", "10:aa")]);
        assert_eq!(plan.unmatched, [0usize; 0]);
    }

    #[test]
    fn the_same_file_name_wins_over_a_shorter_distance() {
        let index = [entry("tr-a", 1, "x/y/song.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Unseen],
            vec![
                candidate("x/y/renamed.flac", "10:aa", None),
                candidate("p/q/r/song.flac", "10:aa", None),
            ],
        );
        assert_eq!(
            plan.rest,
            [
                insert("x/y/renamed.flac", "10:aa"),
                relink(0, "p/q/r/song.flac")
            ]
        );
    }

    #[test]
    fn the_shorter_distance_wins_over_the_order_of_the_paths() {
        let index = [entry("tr-a", 1, "x/y/song.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Unseen],
            vec![
                candidate("a/song.flac", "10:aa", None),
                candidate("x/song.flac", "10:aa", None),
            ],
        );
        assert_eq!(
            plan.rest,
            [insert("a/song.flac", "10:aa"), relink(0, "x/song.flac")]
        );
    }

    #[test]
    fn the_order_of_the_paths_decides_between_equal_candidates() {
        let index = [entry("tr-a", 1, "song.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Unseen],
            vec![
                candidate("b/song.flac", "10:aa", None),
                candidate("a/song.flac", "10:aa", None),
            ],
        );
        assert_eq!(
            plan.rest,
            [insert("b/song.flac", "10:aa"), relink(0, "a/song.flac")]
        );
    }

    #[test]
    fn each_candidate_is_chosen_by_one_row() {
        // Three rows hold copies of one file. Two copies are found again.
        let index = [
            entry("tr-a", 1, "a/song.flac", "10:aa"),
            entry("tr-b", 1, "b/song.flac", "10:aa"),
            entry("tr-c", 1, "c/song.flac", "10:aa"),
        ];
        let plan = plan_of(
            &index,
            &[RowState::Unseen; 3],
            vec![
                candidate("new/b/song.flac", "10:aa", None),
                candidate("new/a/song.flac", "10:aa", None),
            ],
        );
        // The rows choose in the order of their paths, and each takes the
        // first free candidate in the order of the candidate paths.
        assert_eq!(
            plan.rest,
            [relink(1, "new/b/song.flac"), relink(0, "new/a/song.flac")]
        );
        assert_eq!(plan.unmatched, [2]);
    }

    #[test]
    fn a_relink_reads_the_tags_when_the_file_state_or_the_extension_differs() {
        let index = [
            entry("tr-a", 1, "a.ogg", "10:aa"),
            entry("tr-b", 1, "b.flac", "10:bb"),
            entry("tr-c", 1, "c.flac", "10:cc"),
        ];
        let mut touched = candidate("moved/b.flac", "10:bb", None);
        touched.file.mtime = 101;
        let plan = plan_of(
            &index,
            &[RowState::Unseen; 3],
            vec![
                candidate("a.opus", "10:aa", None),
                touched.clone(),
                candidate("moved/c.FLAC", "10:cc", None),
            ],
        );
        assert_eq!(
            plan.rest,
            [
                Op::Relink {
                    row: 0,
                    file: file(1, "a.opus"),
                    reread: true,
                },
                Op::Relink {
                    row: 1,
                    file: touched.file,
                    reread: true,
                },
                relink(2, "moved/c.FLAC"),
            ]
        );
    }

    #[test]
    fn a_displaced_row_without_a_match_takes_the_file_at_its_path() {
        // The file was retagged in place: no file has the old fingerprint.
        let index = [entry("tr-a", 1, "a.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Displaced],
            vec![candidate("a.flac", "12:a2", Some(0))],
        );
        assert_eq!(
            plan,
            Plan {
                exchange: Vec::new(),
                rest: vec![Op::Update {
                    row: 0,
                    file: file(1, "a.flac"),
                    fingerprint: "12:a2".to_owned(),
                }],
                unmatched: Vec::new(),
            }
        );
    }

    #[test]
    fn a_displaced_row_follows_its_file_and_the_file_at_its_path_is_new() {
        // Track A moved from `p.flac` to `q.flac` and file C was placed at
        // `p.flac`.
        let index = [entry("tr-a", 1, "p.flac", "10:aa")];
        let plan = plan_of(
            &index,
            &[RowState::Displaced],
            vec![
                candidate("p.flac", "10:cc", Some(0)),
                candidate("q.flac", "10:aa", None),
            ],
        );
        assert_eq!(
            plan,
            Plan {
                exchange: vec![insert("p.flac", "10:cc"), relink(0, "q.flac")],
                rest: Vec::new(),
                unmatched: Vec::new(),
            }
        );
    }

    #[test]
    fn two_rows_that_exchange_paths_are_one_exchange() {
        let index = [
            entry("tr-a", 1, "p.flac", "10:aa"),
            entry("tr-b", 1, "q.flac", "10:bb"),
        ];
        let plan = plan_of(
            &index,
            &[RowState::Displaced; 2],
            vec![
                candidate("p.flac", "10:bb", Some(0)),
                candidate("q.flac", "10:aa", Some(1)),
            ],
        );
        assert_eq!(
            plan,
            Plan {
                exchange: vec![relink(1, "p.flac"), relink(0, "q.flac")],
                rest: Vec::new(),
                unmatched: Vec::new(),
            }
        );
    }

    #[test]
    fn a_displaced_row_is_deleted_when_another_row_takes_its_path() {
        // The file of B was moved over the file of A. The bytes of A are
        // gone.
        let index = [
            entry("tr-a", 1, "p.flac", "10:aa"),
            entry("tr-b", 1, "q.flac", "10:bb"),
        ];
        let plan = plan_of(
            &index,
            &[RowState::Displaced, RowState::Unseen],
            vec![candidate("p.flac", "10:bb", Some(0))],
        );
        assert_eq!(
            plan,
            Plan {
                exchange: vec![Op::Delete { row: 0 }, relink(1, "p.flac")],
                rest: Vec::new(),
                unmatched: Vec::new(),
            }
        );
    }

    #[test]
    fn updates_of_unchanged_fingerprints_stay_in_the_plan() {
        let index = [entry("tr-a", 1, "a.flac", "10:aa")];
        let update = Op::Update {
            row: 0,
            file: file(1, "a.flac"),
            fingerprint: "10:aa".to_owned(),
        };
        let plan = plan(
            &index,
            &[RowState::Present],
            &[],
            Vec::new(),
            vec![update.clone()],
            &Unread::default(),
        );
        assert_eq!(plan.rest, [update]);
    }

    #[test]
    fn fingerprint_size_is_the_number_before_the_colon() {
        assert_eq!(fingerprint_size("244:0123abcd"), Some(244));
        assert_eq!(fingerprint_size("0:e3b0"), Some(0));
        assert_eq!(fingerprint_size("0123abcd"), None);
        assert_eq!(fingerprint_size("x:0123abcd"), None);
    }

    #[test]
    fn an_unread_file_may_be_a_row_of_its_size_and_an_unread_path_any_row() {
        let row = entry("tr-a", 1, "a.flac", "10:aa");
        assert!(!Unread::default().may_be(&row));
        assert!(unread_sizes(&[10]).may_be(&row));
        assert!(unread_sizes(&[9, 10]).may_be(&row));
        assert!(!unread_sizes(&[9, 11]).may_be(&row));
        let paths = Unread {
            paths: true,
            sizes: HashSet::new(),
        };
        assert!(paths.may_be(&row));

        // A fingerprint without a size may belong to a file of any size.
        let odd = entry("tr-b", 1, "b.flac", "aa");
        assert!(unread_sizes(&[9]).may_be(&odd));
        assert!(!Unread::default().may_be(&odd));
    }

    #[test]
    fn a_row_whose_path_is_taken_is_kept_without_a_file_while_an_unread_file_may_be_its_file() {
        // The file of B was moved over the file of A. A file of the size of
        // A could not be read, so the bytes of A may be in it.
        let index = [
            entry("tr-a", 1, "p.flac", "10:aa"),
            entry("tr-b", 1, "q.flac", "10:bb"),
        ];
        let states = [RowState::Displaced, RowState::Unseen];
        let candidates = || vec![candidate("p.flac", "10:bb", Some(0))];
        let detaches = [Op::Detach { row: 0 }, relink(1, "p.flac")];

        let plan = plan_with(&index, &states, candidates(), &unread_sizes(&[10]));
        assert_eq!(plan.exchange, detaches);
        assert_eq!(plan.unmatched, [0usize; 0]);
        let paths = Unread {
            paths: true,
            sizes: HashSet::new(),
        };
        let plan = plan_with(&index, &states, candidates(), &paths);
        assert_eq!(plan.exchange, detaches);

        // An unread file of another size cannot be the file of A.
        let plan = plan_with(&index, &states, candidates(), &unread_sizes(&[11]));
        assert_eq!(plan.exchange, [Op::Delete { row: 0 }, relink(1, "p.flac")]);
    }

    #[test]
    fn an_exchange_of_two_paths_is_planned_the_same_while_a_file_is_unread() {
        let index = [
            entry("tr-a", 1, "p.flac", "10:aa"),
            entry("tr-b", 1, "q.flac", "10:bb"),
        ];
        let plan = plan_with(
            &index,
            &[RowState::Displaced; 2],
            vec![
                candidate("p.flac", "10:bb", Some(0)),
                candidate("q.flac", "10:aa", Some(1)),
            ],
            &unread_sizes(&[10]),
        );
        assert_eq!(plan.exchange, [relink(1, "p.flac"), relink(0, "q.flac")]);
    }

    #[test]
    fn the_directories_that_a_system_keeps_at_a_volume_root_are_named() {
        for name in [
            "System Volume Information",
            "system volume information",
            "$RECYCLE.BIN",
            "$Recycle.Bin",
            "lost+found",
        ] {
            assert!(is_system_directory(OsStr::new(name)), "{name}");
        }
        for name in ["Lost+Found", "lost+found 2", "RECYCLE.BIN", "Music", ""] {
            assert!(!is_system_directory(OsStr::new(name)), "{name}");
        }
    }

    /// Returns an absolute path of `names` in the form that
    /// `fs::canonicalize` returns on the OS of the test run.
    fn canonical(names: &[&str]) -> PathBuf {
        let mut path = PathBuf::from(if cfg!(windows) { r"\\?\C:\" } else { "/" });
        path.extend(names);
        path
    }

    #[test]
    fn a_target_is_inside_a_folder_when_it_is_the_folder_or_below_it() {
        let roots = [canonical(&["srv", "music"]), canonical(&["mnt", "more"])];
        for names in [
            &["srv", "music"][..],
            &["srv", "music", "Artist"],
            &["srv", "music", "Artist", "Album", "01.flac"],
            &["srv", "music", ".hidden", "01.flac"],
            &["mnt", "more"],
            &["mnt", "more", "01.flac"],
        ] {
            assert!(inside(&canonical(names), &roots), "{names:?}");
        }
        for names in [
            // The directories above a folder, and the root of the volume.
            &[][..],
            &["srv"],
            &["mnt"],
            // A directory next to a folder whose name starts with the name
            // of the folder.
            &["srv", "music2"],
            &["srv", "music2", "01.flac"],
            &["srv", "musi"],
            &["srv", "other", "music"],
            &["music"],
            &["dev", "null"],
        ] {
            assert!(!inside(&canonical(names), &roots), "{names:?}");
        }
        assert!(!inside(&canonical(&["srv", "music"]), &[]));
    }

    #[test]
    fn a_target_is_compared_with_the_folders_in_the_letter_case_it_has() {
        // `fs::canonicalize` returns both paths in the case of the file
        // system, so a path of the folder in another case is another
        // directory, which a file system that tells the cases apart can hold.
        let roots = [canonical(&["srv", "Music"])];
        assert!(inside(&canonical(&["srv", "Music", "a.flac"]), &roots));
        assert!(!inside(&canonical(&["srv", "music", "a.flac"]), &roots));
        assert!(!inside(&canonical(&["SRV", "Music", "a.flac"]), &roots));
    }

    // The forms of a canonical path that only Windows has: a drive and a
    // network share, both with the `\\?\` prefix.
    #[cfg(windows)]
    #[test]
    fn a_windows_target_is_inside_a_folder_of_its_drive_or_its_share_only() {
        let roots = [
            PathBuf::from(r"\\?\C:\Music"),
            PathBuf::from(r"\\?\UNC\nas\media\Music"),
        ];
        for target in [
            r"\\?\C:\Music",
            r"\\?\C:\Music\Artist\01.flac",
            r"\\?\UNC\nas\media\Music\Artist\01.flac",
        ] {
            assert!(inside(Path::new(target), &roots), "{target}");
        }
        for target in [
            r"\\?\C:\",
            r"\\?\C:\Music2\01.flac",
            r"\\?\D:\Music\01.flac",
            r"\\?\UNC\nas\media\01.flac",
            r"\\?\UNC\nas\other\Music\01.flac",
            r"\\?\UNC\other\media\Music\01.flac",
            // A path without the prefix is no result of `fs::canonicalize`.
            r"C:\Music\01.flac",
        ] {
            assert!(!inside(Path::new(target), &roots), "{target}");
        }
        assert!(walks_reach(
            Path::new(r"\\?\C:\Music\Artist\01.flac"),
            &roots,
            false
        ));
        assert!(!walks_reach(
            Path::new(r"\\?\C:\Music\$RECYCLE.BIN\01.flac"),
            &roots,
            false
        ));
    }

    #[test]
    fn a_walk_reaches_the_paths_in_a_folder_that_have_no_skipped_name() {
        let roots = [canonical(&["srv", "music"])];
        let reaches = |names: &[&str], is_dir: bool| {
            let mut path = roots[0].clone();
            path.extend(names);
            walks_reach(&path, &roots, is_dir)
        };

        // The folder itself, directories and supported files.
        assert!(reaches(&[], true));
        assert!(reaches(&["Artist"], true));
        assert!(reaches(&["Artist", "Album"], true));
        assert!(reaches(&["Artist", "Album", "01.flac"], false));
        assert!(reaches(&["01.MP3"], false));
        // A directory with the name of an audio file, and a file with the
        // name of a system directory and a supported extension.
        assert!(reaches(&["Album.flac"], true));
        assert!(reaches(&["Album.flac", "01.flac"], false));
        assert!(reaches(&["Artist", "lost+found.flac"], false));

        // A name that starts with a dot, at every place of the path.
        assert!(!reaches(&[".store"], true));
        assert!(!reaches(&[".store", "Album"], true));
        assert!(!reaches(&[".store", "Album", "01.flac"], false));
        assert!(!reaches(&["Artist", ".hidden.flac"], false));
        // A system directory, at every depth.
        assert!(!reaches(&["lost+found"], true));
        assert!(!reaches(&["Artist", "$RECYCLE.BIN", "01.flac"], false));
        assert!(!reaches(&["System Volume Information", "Album"], true));
        // A file that the walk does not take: another extension, or none.
        assert!(!reaches(&["Artist", "notes.txt"], false));
        assert!(!reaches(&["Artist", "cover.jpg"], false));
        assert!(!reaches(&["Artist", "Album"], false));
        // A file is not below the folder when it is the folder.
        assert!(!reaches(&[], false));
    }

    #[test]
    fn a_walk_reaches_no_path_outside_the_folders() {
        let roots = [canonical(&["srv", "music"])];
        for names in [
            &[][..],
            &["srv"],
            &["srv", "music2", "01.flac"],
            &["mnt", "more", "01.flac"],
        ] {
            assert!(!walks_reach(&canonical(names), &roots, false), "{names:?}");
            assert!(!walks_reach(&canonical(names), &roots, true), "{names:?}");
        }
        assert!(!walks_reach(&canonical(&["srv", "music"]), &[], true));
    }

    #[test]
    fn the_walk_of_a_folder_below_a_skipped_name_reaches_its_own_paths() {
        // The name of a folder itself is exempt from the rules of the walk,
        // so a folder can be a directory that the walk of another folder
        // skips.
        let roots = [
            canonical(&["srv", "music"]),
            canonical(&["srv", "music", ".store"]),
        ];
        let file = canonical(&["srv", "music", ".store", "Album", "01.flac"]);
        assert!(walks_reach(&file, &roots, false));
        assert!(!walks_reach(&file, &roots[..1], false));
        assert!(walks_reach(&roots[1], &roots, true));
    }

    // A name that is not valid Unicode is built from raw bytes, which only
    // Unix allows.
    #[cfg(unix)]
    #[test]
    fn a_walk_reaches_no_path_with_a_name_that_is_not_unicode() {
        use std::os::unix::ffi::OsStrExt;

        let roots = [canonical(&["srv", "music"])];
        let directory = roots[0].join(OsStr::from_bytes(b"bad\xff"));
        assert!(!walks_reach(&directory, &roots, true));
        assert!(!walks_reach(&directory.join("01.flac"), &roots, false));
        // The file is inside the folder all the same.
        assert!(inside(&directory.join("01.flac"), &roots));
    }

    #[test]
    fn a_file_behind_a_link_gets_a_row_once_and_only_when_no_walk_reaches_it() {
        let mut links = Links {
            roots: vec![canonical(&["srv", "music"])],
            ..Links::default()
        };
        let stored = canonical(&["srv", "music", ".store", "01.flac"]);
        // The first link to a file that no walk reaches takes it.
        assert!(links.takes(stored.clone()));
        assert!(!links.takes(stored.clone()));
        assert!(links.takes(canonical(&["srv", "music", ".store", "02.flac"])));
        // A file at a path that the walk reaches has its row there.
        let reached = canonical(&["srv", "music", "Artist", "01.flac"]);
        assert!(!links.takes(reached.clone()));
        assert!(!links.takes(reached));
        assert_eq!(links.files.len(), 2);
    }

    #[test]
    fn a_directory_behind_a_link_is_walked_once_and_only_when_no_walk_reaches_it() {
        let root = canonical(&["srv", "music"]);
        let mut links = Links {
            roots: vec![root.clone()],
            ..Links::default()
        };
        // The folder is walked at a path that is not its canonical path.
        let folder = Tree {
            path: canonical(&["links", "music"]),
            canonical: root.clone(),
            direct: true,
        };
        let link = folder.path.join("Artist").join("link");

        // A target that the walk of the folder reaches: the directory of the
        // link, a directory above it, the folder itself and another
        // directory of the folder.
        for names in [&["Artist"][..], &[], &["Other", "Album"]] {
            let mut target = root.clone();
            target.extend(names);
            assert!(links.tree_behind(&folder, &link, target).is_none());
        }
        assert!(links.followed.is_empty());

        // A target that no walk reaches is a tree whose files are behind a
        // link. A second link to it is not followed.
        let stored = root.join(".store").join("Album");
        let behind = links
            .tree_behind(&folder, &link, stored.clone())
            .expect("the first link is followed");
        assert_eq!(behind.path, link);
        assert_eq!(behind.canonical, stored);
        assert!(!behind.direct);
        let second = folder.path.join("second link");
        assert!(
            links
                .tree_behind(&folder, &second, stored.clone())
                .is_none()
        );
        // A link in that tree that leads back to the tree is not followed.
        let back = behind.path.join("Disc 1").join("back");
        assert!(links.tree_behind(&behind, &back, stored).is_none());
    }

    #[test]
    fn an_entry_that_resolves_to_its_own_path_is_a_directory_of_its_tree() {
        // Windows reports a volume that is mounted at a directory as a link
        // that resolves to the path of the directory.
        let root = canonical(&["srv", "music"]);
        let mut links = Links {
            roots: vec![root.clone()],
            ..Links::default()
        };
        let folder = Tree {
            path: canonical(&["links", "music"]),
            canonical: root.clone(),
            direct: true,
        };
        let mounted = folder.path.join("Volume");
        for _ in 0..2 {
            let tree = links
                .tree_behind(&folder, &mounted, root.join("Volume"))
                .expect("the directory is walked on every scan of the tree");
            assert_eq!(tree.path, mounted);
            assert_eq!(tree.canonical, root.join("Volume"));
            assert!(tree.direct);
        }
        assert!(links.followed.is_empty());

        // The same entry in a tree behind a link is a directory of that
        // tree, whose files are behind the link.
        let behind = Tree {
            path: folder.path.join("link"),
            canonical: root.join(".store"),
            direct: false,
        };
        let mounted = behind.path.join("Volume");
        let tree = links
            .tree_behind(&behind, &mounted, root.join(".store").join("Volume"))
            .expect("the directory is walked");
        assert!(!tree.direct);
    }

    #[test]
    fn no_walked_file_has_a_placeholder_path() {
        // The walk builds every relative path with `rel_path`.
        let placeholder = tracks::placeholder_path("tr-0123");
        assert!(!tracks::has_file(&placeholder));
        assert_eq!(rel_path(Path::new(&placeholder)), None);
        let walked = rel_path(Path::new("tr-0123")).unwrap();
        assert!(tracks::has_file(&walked));
    }

    #[test]
    fn a_cover_image_has_one_of_four_stems_and_one_of_four_extensions() {
        for name in [
            "cover.jpg",
            "folder.jpeg",
            "front.png",
            "album.webp",
            "Cover.JPG",
            "FOLDER.Png",
        ] {
            assert!(cover_rank(name).is_some(), "{name}");
        }
        for name in [
            "cover",
            "cover.gif",
            "cover.jpg.bak",
            "back.jpg",
            "cover art.jpg",
            "my cover.jpg",
            "covers.jpg",
            ".jpg",
            "jpg",
            "",
        ] {
            assert_eq!(cover_rank(name), None, "{name:?}");
        }
    }

    #[test]
    fn cover_images_are_ordered_by_stem_and_then_by_extension() {
        let ordered = [
            "cover.jpg",
            "cover.jpeg",
            "cover.png",
            "cover.webp",
            "folder.jpg",
            "folder.webp",
            "front.jpg",
            "album.jpg",
            "album.webp",
        ];
        let ranks: Vec<usize> = ordered
            .iter()
            .map(|name| cover_rank(name).unwrap_or_else(|| panic!("{name}")))
            .collect();
        assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]), "{ranks:?}");
        // The case of a name does not change its place.
        assert_eq!(cover_rank("COVER.PNG"), cover_rank("cover.png"));
    }

    /// Returns the walk of a folder in which the walk saw the supported
    /// files `audio` and the cover images `covers`, each given with its
    /// modification time.
    fn walk_of(audio: &[&str], covers: &[(&str, i64)]) -> FolderWalk {
        let mut walk = FolderWalk::default();
        for rel_path in audio {
            walk.supported += 1;
            walk.see_audio(rel_path);
        }
        for (rel_path, mtime) in covers {
            let rank = cover_rank(file_name(rel_path)).expect("the name of a cover image");
            walk.see_cover(rel_path, rank, *mtime);
        }
        walk
    }

    #[test]
    fn a_directory_keeps_the_cover_image_that_comes_first_in_the_order() {
        // The walk sees the files of a directory in the order of their
        // names.
        let walk = walk_of(
            &[],
            &[
                ("a/album.jpg", 1),
                ("a/cover.png", 2),
                ("a/folder.jpg", 3),
                ("b/Cover.jpg", 4),
                ("b/cover.jpg", 5),
                ("front.webp", 6),
            ],
        );
        let kept = |directory: &str| {
            let cover = &walk.covers[directory];
            (cover.name.as_str(), cover.mtime)
        };
        assert_eq!(kept("a"), ("cover.png", 2));
        // Two names of one rank: the first one the walk saw stays.
        assert_eq!(kept("b"), ("Cover.jpg", 4));
        // The empty path is the folder itself.
        assert_eq!(kept(""), ("front.webp", 6));
        assert_eq!(walk.covers.len(), 3);
    }

    #[test]
    fn the_walk_saw_a_directory_that_holds_a_supported_file_and_is_not_unread() {
        let mut walk = walk_of(&["Artist/Album/01.flac", "loose.flac"], &[]);
        assert!(walk.saw_directory("Artist/Album"));
        assert!(walk.saw_directory(""));
        // A directory without a supported file of its own.
        assert!(!walk.saw_directory("Artist"));
        assert!(!walk.saw_directory("Artist/Album/Scans"));

        // A file of the directory that cannot be read leaves the directory
        // listed. A directory that cannot be read is not listed in full.
        walk.unread.push("Artist/Album/02.flac".to_owned());
        assert!(walk.saw_directory("Artist/Album"));
        walk.unread.push("Artist".to_owned());
        assert!(!walk.saw_directory("Artist/Album"));
        assert!(walk.saw_directory(""));
        walk.unread.push(String::new());
        assert!(!walk.saw_directory(""));
    }

    /// The path of the folder of [`seen_of`].
    const MUSIC: &str = "music";

    /// Returns the folder rows and the walks of a scan of one folder, which
    /// has the ID 1.
    fn seen_of(walk: FolderWalk) -> (Vec<Folder>, HashMap<i64, FolderWalk>) {
        let folder = Folder {
            id: 1,
            path: PathBuf::from(MUSIC),
            name: MUSIC.to_owned(),
        };
        (vec![folder], HashMap::from([(1, walk)]))
    }

    /// Returns the cover inputs of an album in the folder 1 whose first
    /// track is at `first_track` and whose first track with a picture has
    /// the ID and the modification time `first_picture`.
    fn inputs_of(first_track: Option<&str>, first_picture: Option<(&str, i64)>) -> CoverInputs {
        CoverInputs {
            album_id: "al-1".to_owned(),
            cover_source: None,
            cover_mtime: None,
            first_track: first_track.map(|rel_path| (1, rel_path.to_owned())),
            first_picture: first_picture.map(|(id, mtime)| (id.to_owned(), mtime)),
        }
    }

    /// Returns the tracks at the relative paths `rel_paths` in the folder 1
    /// as tracks of the album `album_id`.
    fn tracks_in(album_id: &str, rel_paths: &[&str]) -> Vec<AlbumTrack> {
        rel_paths
            .iter()
            .map(|rel_path| AlbumTrack {
                album_id: album_id.to_owned(),
                folder_id: 1,
                rel_path: (*rel_path).to_owned(),
            })
            .collect()
    }

    /// Returns the cover of an album whose one track with a file is its
    /// first track.
    fn resolve(inputs: &CoverInputs, walk: FolderWalk) -> Cover {
        let first_track = inputs.first_track.iter();
        let tracks: Vec<AlbumTrack> = first_track
            .map(|(folder_id, rel_path)| AlbumTrack {
                album_id: inputs.album_id.clone(),
                folder_id: *folder_id,
                rel_path: rel_path.clone(),
            })
            .collect();
        resolve_among(inputs, walk, &tracks)
    }

    /// Returns the cover of an album in a library whose tracks with a file
    /// are `tracks`.
    fn resolve_among(inputs: &CoverInputs, walk: FolderWalk, tracks: &[AlbumTrack]) -> Cover {
        let (folders, walks) = seen_of(walk);
        resolve_cover(
            inputs,
            &Seen {
                folders: &folders,
                walks: &walks,
            },
            &TrackDirectories::of(tracks),
        )
    }

    /// Returns the cover whose source is the image file at `components`
    /// below the folder of [`seen_of`].
    fn image(components: &[&str], mtime: i64) -> Cover {
        let mut path = PathBuf::from(MUSIC);
        path.extend(components);
        let path = path.to_str().expect("a Unicode path").to_owned();
        Cover::Known(Some((path, mtime)))
    }

    #[test]
    fn the_cover_image_in_the_directory_of_the_first_track_is_the_cover() {
        let inputs = inputs_of(Some("Artist/Album/01.flac"), Some(("tr-1", 100)));
        let walk = walk_of(
            &["Artist/Album/01.flac"],
            &[("Artist/Album/Folder.JPG", 500)],
        );
        // The image file precedes the embedded picture.
        assert_eq!(
            resolve(&inputs, walk),
            image(&["Artist", "Album", "Folder.JPG"], 500)
        );
    }

    #[test]
    fn a_cover_image_in_the_folder_itself_is_the_cover_of_the_tracks_there() {
        let inputs = inputs_of(Some("01.flac"), None);
        let walk = walk_of(&["01.flac"], &[("cover.jpg", 500)]);
        assert_eq!(resolve(&inputs, walk), image(&["cover.jpg"], 500));
    }

    #[test]
    fn a_cover_image_in_another_directory_is_not_the_cover() {
        // Every track of the album is in `cd1`.
        let inputs = inputs_of(Some("Artist/Album/cd1/01.flac"), None);
        let walk = walk_of(
            &["Artist/Album/cd1/01.flac", "Artist/Album/cd2/01.flac"],
            &[
                ("Artist/Album/cover.jpg", 1),
                ("Artist/Album/cd2/cover.jpg", 2),
                ("Artist/cover.jpg", 3),
                ("cover.jpg", 4),
            ],
        );
        assert_eq!(resolve(&inputs, walk), Cover::Known(None));
    }

    /// The relative paths of the tracks of an album with two disc
    /// directories.
    const DISCS: [&str; 3] = [
        "Artist/Album/cd1/01.flac",
        "Artist/Album/cd1/02.flac",
        "Artist/Album/cd2/01.flac",
    ];

    #[test]
    fn the_cover_image_of_the_parent_directory_is_the_cover_of_an_album_in_disc_directories() {
        let inputs = inputs_of(Some(DISCS[0]), Some(("tr-1", 100)));
        let tracks = tracks_in("al-1", &DISCS);
        let walk = || {
            walk_of(
                &DISCS,
                &[
                    ("Artist/Album/Folder.PNG", 500),
                    ("Artist/Album/cd2/cover.jpg", 2),
                    ("Artist/cover.jpg", 3),
                    ("cover.jpg", 4),
                ],
            )
        };
        // The image file precedes the embedded picture. The image of the
        // second disc and the images further up are not the cover.
        let parent = image(&["Artist", "Album", "Folder.PNG"], 500);
        assert_eq!(resolve_among(&inputs, walk(), &tracks), parent);

        // A third disc directory, and a track of another album outside the
        // parent directory.
        let mut more = tracks.clone();
        more.extend(tracks_in("al-1", &["Artist/Album/cd3/01.flac"]));
        more.extend(tracks_in("al-2", &["Artist/Other/01.flac", "loose.flac"]));
        assert_eq!(resolve_among(&inputs, walk(), &more), parent);
    }

    #[test]
    fn the_cover_image_beside_the_first_track_precedes_the_one_of_the_parent_directory() {
        let inputs = inputs_of(Some(DISCS[0]), Some(("tr-1", 100)));
        let tracks = tracks_in("al-1", &DISCS);
        let walk = walk_of(
            &DISCS,
            &[
                ("Artist/Album/cd1/album.webp", 1),
                ("Artist/Album/cover.jpg", 500),
            ],
        );
        assert_eq!(
            resolve_among(&inputs, walk, &tracks),
            image(&["Artist", "Album", "cd1", "album.webp"], 1)
        );
    }

    #[test]
    fn an_album_in_disc_directories_without_an_image_in_both_places_has_its_embedded_picture() {
        let tracks = tracks_in("al-1", &DISCS);
        // The image of the second disc is beside no first track.
        let covers = [("Artist/Album/cd2/cover.jpg", 2)];
        let pictured = inputs_of(Some(DISCS[0]), Some(("tr-7", 700)));
        assert_eq!(
            resolve_among(&pictured, walk_of(&DISCS, &covers), &tracks),
            Cover::Known(Some(("embedded:tr-7".to_owned(), 700)))
        );
        let plain = inputs_of(Some(DISCS[0]), None);
        assert_eq!(
            resolve_among(&plain, walk_of(&DISCS, &covers), &tracks),
            Cover::Known(None)
        );
    }

    #[test]
    fn a_parent_directory_with_tracks_of_another_album_gives_its_cover_image_to_no_album() {
        let inputs = inputs_of(Some(DISCS[0]), None);
        // The track of the other album is in the parent directory itself, in
        // a directory beside the discs, and deeper below the parent
        // directory.
        for other in [
            "Artist/Album/bonus.flac",
            "Artist/Album/cd3/01.flac",
            "Artist/Album/cd2/extra/01.flac",
        ] {
            let mut audio = DISCS.to_vec();
            audio.push(other);
            let mut tracks = tracks_in("al-1", &DISCS);
            tracks.extend(tracks_in("al-2", &[other]));
            let walk = walk_of(&audio, &[("Artist/Album/cover.jpg", 500)]);
            assert_eq!(
                resolve_among(&inputs, walk, &tracks),
                Cover::Known(None),
                "{other}"
            );
        }

        // Two albums with two disc directories each, in one parent
        // directory.
        let second = ["Artist/Album/cd3/01.flac", "Artist/Album/cd4/01.flac"];
        let mut audio = DISCS.to_vec();
        audio.extend(second);
        let mut tracks = tracks_in("al-1", &DISCS);
        tracks.extend(tracks_in("al-2", &second));
        let mut other = inputs_of(Some(second[0]), None);
        other.album_id = "al-2".to_owned();
        for inputs in [&inputs, &other] {
            let walk = walk_of(&audio, &[("Artist/Album/cover.jpg", 500)]);
            assert_eq!(
                resolve_among(inputs, walk, &tracks),
                Cover::Known(None),
                "{}",
                inputs.album_id
            );
        }
    }

    #[test]
    fn disc_directories_in_the_folder_itself_have_the_cover_image_of_the_folder() {
        let audio = ["CD1/01.flac", "CD2/01.flac"];
        let inputs = inputs_of(Some(audio[0]), None);
        let walk = walk_of(&audio, &[("cover.jpg", 500)]);
        assert_eq!(
            resolve_among(&inputs, walk, &tracks_in("al-1", &audio)),
            image(&["cover.jpg"], 500)
        );
    }

    #[test]
    fn tracks_in_the_folder_itself_are_in_no_disc_directory() {
        // The folder itself has no parent directory in the library. Its
        // image is beside the tracks in the folder, and it is not the image
        // of a parent directory of the folder and of `CD2`.
        let audio = ["01.flac", "CD2/01.flac"];
        let inputs = inputs_of(Some("CD2/01.flac"), None);
        let walk = walk_of(&audio, &[("cover.jpg", 500)]);
        assert_eq!(
            resolve_among(&inputs, walk, &tracks_in("al-1", &audio)),
            Cover::Known(None)
        );
    }

    #[test]
    fn tracks_in_two_folders_are_in_no_disc_directories_of_one_parent_directory() {
        // The two directories have the same relative parent path in two
        // folders, which are two directories on the disk.
        let inputs = inputs_of(Some(DISCS[0]), None);
        let mut tracks = tracks_in("al-1", &DISCS[..2]);
        tracks.push(AlbumTrack {
            album_id: "al-1".to_owned(),
            folder_id: 2,
            rel_path: DISCS[2].to_owned(),
        });
        let walk = walk_of(&DISCS[..2], &[("Artist/Album/cover.jpg", 500)]);
        assert_eq!(resolve_among(&inputs, walk, &tracks), Cover::Known(None));
    }

    #[test]
    fn directories_without_a_common_parent_directory_are_no_disc_directories() {
        let cases = [
            // At different depths.
            ["Artist/Album/cd1/01.flac", "Artist/Album/cd2/part/01.flac"],
            // The parent directory holds a track itself.
            ["Artist/Album/cd1/01.flac", "Artist/Album/02.flac"],
            // Below different directories.
            ["Artist/Album/cd1/01.flac", "Artist/Other/cd2/01.flac"],
        ];
        for audio in cases {
            let inputs = inputs_of(Some(audio[0]), None);
            let walk = walk_of(
                &audio,
                &[("Artist/Album/cover.jpg", 500), ("Artist/cover.jpg", 3)],
            );
            assert_eq!(
                resolve_among(&inputs, walk, &tracks_in("al-1", &audio)),
                Cover::Known(None),
                "{audio:?}"
            );
        }
    }

    #[test]
    fn the_cover_of_an_album_in_disc_directories_is_unknown_when_the_walk_did_not_see_the_first() {
        let inputs = inputs_of(Some(DISCS[0]), None);
        let tracks = tracks_in("al-1", &DISCS);
        // The walk saw the parent directory and its image, and no supported
        // file in the directory of the first track.
        let walk = walk_of(&DISCS[2..], &[("Artist/Album/cover.jpg", 500)]);
        assert_eq!(resolve_among(&inputs, walk, &tracks), Cover::Unknown);
    }

    #[test]
    fn the_parent_of_a_directory_is_inside_the_folder() {
        assert_eq!(parent("Artist/Album/cd1"), Some("Artist/Album"));
        assert_eq!(parent("Artist"), Some(""));
        // The folder itself.
        assert_eq!(parent(""), None);
    }

    #[test]
    fn the_placement_of_an_album_follows_the_directories_of_its_tracks() {
        let one = Placement::One {
            folder_id: 1,
            directory: "Album/cd1",
        };
        let siblings = Placement::Siblings {
            folder_id: 1,
            parent: "Album",
        };
        let after = |mut placement: Placement<'static>, folder_id, directory| {
            placement.add(folder_id, directory);
            placement
        };
        assert_eq!(after(one, 1, "Album/cd1"), one);
        assert_eq!(after(one, 1, "Album/cd2"), siblings);
        assert_eq!(after(one, 2, "Album/cd1"), Placement::Apart);
        assert_eq!(after(one, 2, "Album/cd2"), Placement::Apart);
        assert_eq!(after(one, 1, "Album"), Placement::Apart);
        assert_eq!(after(siblings, 1, "Album/cd3"), siblings);
        assert_eq!(after(siblings, 2, "Album/cd3"), Placement::Apart);
        assert_eq!(after(siblings, 1, "Other/cd3"), Placement::Apart);
        assert_eq!(after(siblings, 1, "Album/cd3/part"), Placement::Apart);
        assert_eq!(after(Placement::Apart, 1, "Album/cd1"), Placement::Apart);

        // Two directories of the folder itself have the folder as their
        // parent directory. The folder itself has none.
        let top = Placement::One {
            folder_id: 1,
            directory: "cd1",
        };
        let in_folder = Placement::Siblings {
            folder_id: 1,
            parent: "",
        };
        assert_eq!(after(top, 1, "cd2"), in_folder);
        assert_eq!(after(top, 1, ""), Placement::Apart);
        assert_eq!(after(in_folder, 1, ""), Placement::Apart);
        let root = Placement::One {
            folder_id: 1,
            directory: "",
        };
        assert_eq!(after(root, 1, ""), root);
        assert_eq!(after(root, 1, "cd2"), Placement::Apart);
    }

    #[test]
    fn the_embedded_picture_is_the_cover_of_an_album_without_a_cover_image() {
        let inputs = inputs_of(Some("Artist/Album/01.flac"), Some(("tr-7", 700)));
        let walk = walk_of(&["Artist/Album/01.flac"], &[]);
        assert_eq!(
            resolve(&inputs, walk),
            Cover::Known(Some(("embedded:tr-7".to_owned(), 700)))
        );
    }

    #[test]
    fn an_album_without_a_cover_image_and_without_a_picture_has_no_cover() {
        let inputs = inputs_of(Some("Artist/Album/01.flac"), None);
        let walk = walk_of(&["Artist/Album/01.flac"], &[]);
        assert_eq!(resolve(&inputs, walk), Cover::Known(None));
    }

    #[test]
    fn an_album_whose_tracks_have_no_file_has_no_cover() {
        // `cover_inputs` names no track without a file, so the album has no
        // directory. The image files of the folder are not its cover.
        let inputs = inputs_of(None, None);
        let walk = walk_of(&["01.flac"], &[("cover.jpg", 500)]);
        assert_eq!(resolve(&inputs, walk), Cover::Known(None));
    }

    #[test]
    fn the_cover_is_unknown_when_the_walk_did_not_see_the_directory_of_the_album() {
        let inputs = inputs_of(Some("Artist/Album/01.flac"), Some(("tr-1", 100)));

        // The walk saw no supported file in the directory: the row is kept
        // although its file is gone.
        let elsewhere = walk_of(&["Other/01.flac"], &[("Artist/Album/cover.jpg", 500)]);
        assert_eq!(resolve(&inputs, elsewhere), Cover::Unknown);

        // The directory is below a path that the walk could not read.
        let mut unread = walk_of(&["Artist/Album/01.flac"], &[]);
        unread.unread.push("Artist".to_owned());
        assert_eq!(resolve(&inputs, unread), Cover::Unknown);

        // The folder could not be read.
        let mut failed = FolderWalk::default();
        failed.unread.push(String::new());
        assert_eq!(resolve(&inputs, failed), Cover::Unknown);

        // The folder of the track is not configured, so it was not walked.
        let (folders, walks) = seen_of(walk_of(&["Artist/Album/01.flac"], &[]));
        let mut other_folder = inputs.clone();
        other_folder.first_track = Some((2, "Artist/Album/01.flac".to_owned()));
        let seen = Seen {
            folders: &folders,
            walks: &walks,
        };
        let directories = TrackDirectories::default();
        assert_eq!(
            resolve_cover(&other_folder, &seen, &directories),
            Cover::Unknown
        );
    }

    #[test]
    fn the_cover_id_of_a_cached_cover_is_the_start_of_its_name() {
        let id = "al-0123456789abcdef0123456789abcdef";
        assert_eq!(
            cached_cover_id(&format!("{id}-300-1700000000.jpg")),
            Some(id)
        );
        assert_eq!(
            cached_cover_id(&format!("{id}-300-1700000000.jpg.tmp")),
            Some(id)
        );
        let track = "tr-0123456789abcdef0123456789abcdef";
        assert_eq!(cached_cover_id(&format!("{track}-64--5.jpg")), Some(track));
        assert_eq!(cached_cover_id(id), None);
        assert_eq!(cached_cover_id("readme.txt"), None);
        assert_eq!(cached_cover_id(""), None);
    }

    /// Writes a file with each of `names` to the directory `dir`.
    fn write_files(dir: &Path, names: &[String]) {
        for name in names {
            fs::write(dir.join(name), b"jpeg").unwrap();
        }
    }

    /// Returns the names of the entries of the directory `dir`, in order.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn remove_cached_covers_removes_the_files_of_the_named_albums_only() {
        let library = Library::new(&[]);
        let dir = tempfile::tempdir().unwrap();
        let stale = "al-00000000000000000000000000000001";
        let also_stale = "al-00000000000000000000000000000002";
        // The ID of the third album starts with the characters of the first.
        // No row has it, and its file stays: a file of an album is removed
        // for a named album only.
        let kept = "al-00000000000000000000000000000011";
        let names = [
            format!("{stale}-320-1000.jpg"),
            format!("{stale}-80-1000.jpg"),
            format!("{also_stale}-320-2000.jpg"),
            format!("{kept}-320-3000.jpg"),
            "notes.txt".to_owned(),
        ];
        write_files(dir.path(), &names);

        let named = [stale.to_owned(), also_stale.to_owned()];
        remove_cached_covers(&library.db, dir.path(), &named);

        let mut expected = names[3..].to_vec();
        expected.sort();
        assert_eq!(names_in(dir.path()), expected);

        // No album is named: no file is removed.
        remove_cached_covers(&library.db, dir.path(), &[]);
        assert_eq!(names_in(dir.path()), expected);
    }

    #[test]
    fn remove_cached_covers_removes_the_files_of_the_tracks_without_a_row() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let with_row = library.id_at("a.wav").expect("the track of the file");
        let other = library.id_at("b.wav").expect("the track of the file");
        let album = library.album().id;
        let deleted = "tr-00000000000000000000000000000001";
        // An ID that differs from the ID of a row in its last character.
        let similar = {
            let last = if with_row.ends_with('0') { '1' } else { '0' };
            format!("{}{last}", &with_row[..with_row.len() - 1])
        };
        let dir = tempfile::tempdir().unwrap();

        let stale = [
            // A step, a size that is no step, as an earlier version of the
            // server wrote it, and a time before 1970.
            format!("{deleted}-80-1000.jpg"),
            format!("{deleted}-300-1000.jpg"),
            format!("{deleted}-2000--5.jpg"),
            format!("{similar}-320-1000.jpg"),
        ];
        let kept = [
            format!("{with_row}-80-1000.jpg"),
            format!("{with_row}-300-1000.jpg"),
            format!("{other}-640-2000.jpg"),
            // The files of albums: one with a row, and one whose ID no row
            // has.
            format!("{album}-320-1000.jpg"),
            "al-00000000000000000000000000000001-320-1000.jpg".to_owned(),
            // Names that start with no track ID: an ID of another kind, a
            // value that is no ID, a temporary file of the cover service
            // and a name without a hyphen after the ID.
            "ar-00000000000000000000000000000001-320-1000.jpg".to_owned(),
            "pl-00000000000000000000000000000001-320-1000.jpg".to_owned(),
            "tr-0000000000000000000000000000001-320-1000.jpg".to_owned(),
            "tr-0000000000000000000000000000000g-320-1000.jpg".to_owned(),
            "TR-00000000000000000000000000000001-320-1000.jpg".to_owned(),
            "tmp.00000000000000000000000000000001".to_owned(),
            deleted.to_owned(),
            "notes.txt".to_owned(),
        ];
        write_files(dir.path(), &stale);
        write_files(dir.path(), &kept);

        remove_cached_covers(&library.db, dir.path(), &[]);

        let mut expected = kept.to_vec();
        expected.sort();
        assert_eq!(names_in(dir.path()), expected);

        // The row of a track is deleted: its files are removed, and the
        // files of the other track and of the album stay.
        library
            .db
            .with_writer(|conn| tracks::delete_tracks(conn, &[&with_row]))
            .unwrap();
        remove_cached_covers(&library.db, dir.path(), &[]);
        expected.retain(|name| !name.starts_with(&with_row));
        assert_eq!(names_in(dir.path()), expected);
        assert_eq!(expected.len(), kept.len() - 2);
    }

    #[test]
    fn remove_cached_covers_looks_up_more_tracks_than_one_group_holds() {
        let library = Library::new(&[("a.wav", 100, 1)]);
        library.scan();
        let with_row = library.id_at("a.wav").expect("the track of the file");
        let dir = tempfile::tempdir().unwrap();
        let count = 2 * TRACK_LOOKUPS + 3;
        let stale: Vec<String> = (0..count)
            .map(|track| format!("tr-{track:032x}-80-1000.jpg"))
            .collect();
        let kept = [
            format!("{with_row}-80-1000.jpg"),
            "al-00000000000000000000000000000001-80-1000.jpg".to_owned(),
        ];
        write_files(dir.path(), &stale);
        write_files(dir.path(), &kept);

        remove_cached_covers(&library.db, dir.path(), &[]);

        let mut expected = kept.to_vec();
        expected.sort();
        assert_eq!(names_in(dir.path()), expected);
    }

    /// The function removes entries of the cache directory and nothing
    /// else: a link in the cache is removed as a link, and a file with the
    /// name of a stale cover in another directory stays.
    #[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
    #[test]
    fn remove_cached_covers_removes_no_file_outside_the_cache_directory() {
        use std::os::unix::fs::symlink;

        let library = Library::new(&[("a.wav", 100, 1)]);
        library.scan();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let track = "tr-00000000000000000000000000000001-80-1000.jpg";
        let album = "al-00000000000000000000000000000001-80-1000.jpg";

        // The music folder and the directory above the cache hold files
        // with the names of stale covers.
        for name in [track, album] {
            fs::write(library.path(name), b"music").unwrap();
            fs::write(dir.path().join(name), b"above").unwrap();
        }
        // The cache holds links with these names, to a file and to the
        // music folder, and a directory with the name of a stale cover.
        symlink(library.path("a.wav"), cache.join(track)).unwrap();
        symlink(library.dir.path(), cache.join(album)).unwrap();
        let directory = "tr-00000000000000000000000000000002-80-1000.jpg";
        fs::create_dir(cache.join(directory)).unwrap();
        fs::write(cache.join(directory).join("inner.jpg"), b"inner").unwrap();
        let audio = fs::read(library.path("a.wav")).unwrap();

        let named = ["al-00000000000000000000000000000001".to_owned()];
        remove_cached_covers(&library.db, &cache, &named);

        // The links are gone. The directory cannot be removed as a file.
        assert_eq!(names_in(&cache), [directory]);
        assert_eq!(names_in(&cache.join(directory)), ["inner.jpg"]);
        // The targets of the links and the files of the other directories
        // are as they were.
        assert_eq!(fs::read(library.path("a.wav")).unwrap(), audio);
        assert_eq!(names_in(library.dir.path()), ["a.wav", album, track]);
        assert_eq!(names_in(dir.path()), [album, "cache", track]);
        for name in [track, album] {
            assert_eq!(fs::read(library.path(name)).unwrap(), b"music");
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"above");
        }
    }

    #[test]
    fn remove_cached_covers_accepts_a_cache_directory_that_does_not_exist() {
        let library = Library::new(&[]);
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("cache").join("covers");
        remove_cached_covers(
            &library.db,
            &missing,
            &["al-00000000000000000000000000000001".to_owned()],
        );
        assert!(!missing.exists());
    }

    /// Returns a WAV file with `samples` samples of the value `fill`: mono,
    /// 8 kHz, 16-bit PCM. Its size is 44 bytes plus two bytes per sample.
    fn wav(samples: u32, fill: u8) -> Vec<u8> {
        let data_len = samples * 2;
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
        bytes.resize(bytes.len() + data_len as usize, fill);
        bytes
    }

    /// One music folder of WAV files in a temp directory, a database in
    /// memory, and a reader whose reads a test makes fail. A file system
    /// refuses a read for reasons a test cannot produce on every OS and as
    /// every user, so the tests of unreadable files go through this reader.
    struct Library {
        dir: TempDir,
        db: Db,
        cancel: CancellationToken,
        /// The paths whose fingerprint cannot be read.
        unreadable: Rc<RefCell<HashSet<PathBuf>>>,
        /// The paths whose tags cannot be read.
        untaggable: Rc<RefCell<HashSet<PathBuf>>>,
        /// The artist and the album that the reader reports for a path, in
        /// place of the tags of the file. The WAV files of a test have no
        /// tags.
        tagged: Rc<RefCell<HashMap<PathBuf, (&'static str, &'static str)>>>,
        /// The number of fingerprint reads so far.
        fingerprint_reads: Rc<Cell<usize>>,
        /// The fingerprint read, counted from 1, that cancels the scan. No
        /// read cancels it while the value is 0.
        cancel_at_read: Rc<Cell<usize>>,
        /// The number of tag reads so far.
        tag_reads: Rc<Cell<usize>>,
        /// The tag read, counted from 1, that cancels the scan. No read
        /// cancels it while the value is 0.
        cancel_at_tag_read: Rc<Cell<usize>>,
        /// The number of batches whose hook ran, in every scan so far.
        batches: Arc<AtomicU64>,
    }

    impl Library {
        /// Writes the files, each given as its relative path, its number of
        /// samples and the value of its samples.
        fn new(files: &[(&str, u32, u8)]) -> Self {
            let library = Self {
                dir: tempfile::tempdir().expect("create the library directory"),
                db: Db::open_memory().expect("open the database"),
                cancel: CancellationToken::new(),
                unreadable: Rc::default(),
                untaggable: Rc::default(),
                tagged: Rc::default(),
                fingerprint_reads: Rc::default(),
                cancel_at_read: Rc::default(),
                tag_reads: Rc::default(),
                cancel_at_tag_read: Rc::default(),
                batches: Arc::default(),
            };
            for (rel_path, samples, fill) in files {
                library.write(rel_path, *samples, *fill);
            }
            library
        }

        fn path(&self, rel_path: &str) -> PathBuf {
            self.dir.path().join(rel_path)
        }

        fn write(&self, rel_path: &str, samples: u32, fill: u8) {
            fs::write(self.path(rel_path), wav(samples, fill)).expect("write the file");
        }

        /// Moves a file and creates the directory it moves to. The file
        /// keeps its modification time.
        fn rename(&self, from: &str, to: &str) {
            let to = self.path(to);
            fs::create_dir_all(to.parent().expect("the path has a parent")).unwrap();
            fs::rename(self.path(from), to).expect("move the file");
        }

        fn set_mtime(&self, rel_path: &str, unix_seconds: u64) {
            let file = fs::File::options()
                .write(true)
                .open(self.path(rel_path))
                .expect("open the file");
            file.set_modified(UNIX_EPOCH + Duration::from_secs(unix_seconds))
                .expect("set the modification time");
        }

        /// Makes the reader report `artist` as the artist and the album
        /// artist and `album` as the album of the file at `rel_path`. The
        /// tags belong to the path, so a file that moves needs them at its
        /// new path when the scan reads its tags there.
        fn tag(&self, rel_path: &str, artist: &'static str, album: &'static str) {
            self.tagged
                .borrow_mut()
                .insert(self.path(rel_path), (artist, album));
        }

        /// Returns a reader of the files on disk that fails for the paths
        /// in `unreadable` and `untaggable` and reports the tags in
        /// `tagged`. It counts its reads, and it cancels the scan during
        /// the read that `cancel_at_read` or `cancel_at_tag_read` names.
        /// That read returns what it read.
        fn reader(&self) -> FileReader {
            let unreadable = Rc::clone(&self.unreadable);
            let untaggable = Rc::clone(&self.untaggable);
            let tagged = Rc::clone(&self.tagged);
            let reads = Rc::clone(&self.fingerprint_reads);
            let cancel_at_read = Rc::clone(&self.cancel_at_read);
            let cancel = self.cancel.clone();
            let tag_reads = Rc::clone(&self.tag_reads);
            let cancel_at_tag_read = Rc::clone(&self.cancel_at_tag_read);
            let cancel_tags = self.cancel.clone();
            FileReader {
                fingerprint: Box::new(move |path| {
                    reads.set(reads.get() + 1);
                    if reads.get() == cancel_at_read.get() {
                        cancel.cancel();
                    }
                    if unreadable.borrow().contains(path) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "the test denies the read",
                        ));
                    }
                    ids::fingerprint(path)
                }),
                tags: Box::new(move |path| {
                    tag_reads.set(tag_reads.get() + 1);
                    if tag_reads.get() == cancel_at_tag_read.get() {
                        cancel_tags.cancel();
                    }
                    if untaggable.borrow().contains(path) {
                        return Err(TagError::UnsupportedExtension(path.to_owned()));
                    }
                    let mut meta = tags::read(path)?;
                    if let Some((artist, album)) = tagged.borrow().get(path) {
                        meta.artist = (*artist).to_owned();
                        meta.album_artist = (*artist).to_owned();
                        meta.album = (*album).to_owned();
                    }
                    Ok(meta)
                }),
            }
        }

        fn scan(&self) -> ScanSummary {
            self.scan_in_batches_of(DEFAULT_BATCH_SIZE)
        }

        fn scan_in_batches_of(&self, batch_size: usize) -> ScanSummary {
            let batches = Arc::clone(&self.batches);
            let options = ScanOptions {
                batch_size,
                on_batch: Some(Box::new(move || {
                    batches.fetch_add(1, Ordering::SeqCst);
                })),
                ..ScanOptions::default()
            };
            let folders = [self.dir.path().to_path_buf()];
            scan_with(
                &self.db,
                &folders,
                &self.cancel,
                false,
                options,
                &self.reader(),
            )
            .expect("the scan returns a summary")
        }

        /// Returns the index in the order of the relative paths.
        fn index(&self) -> Vec<IndexEntry> {
            let mut index = self.db.with_reader(tracks::load_index).unwrap();
            index.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
            index
        }

        fn id_at(&self, rel_path: &str) -> Option<String> {
            self.index()
                .into_iter()
                .find(|entry| entry.rel_path == rel_path)
                .map(|entry| entry.id)
        }

        fn latest_scan(&self) -> scans::Scan {
            self.db
                .with_reader(scans::latest)
                .unwrap()
                .expect("a scan has run")
        }

        fn track_at(&self, rel_path: &str) -> Track {
            let id = self
                .id_at(rel_path)
                .unwrap_or_else(|| panic!("no track is at {rel_path}"));
            self.db
                .with_reader(|conn| tracks::get_track(conn, &id))
                .unwrap()
                .expect("the row exists")
        }

        /// Returns the one album of the library. Files without tags are in
        /// the album of the tracks without an album tag.
        fn album(&self) -> Album {
            let mut albums: Vec<Album> = self.albums().into_values().collect();
            assert_eq!(albums.len(), 1, "{albums:?}");
            albums.remove(0)
        }

        /// Returns every album by name.
        fn albums(&self) -> BTreeMap<String, Album> {
            self.db
                .with_reader(|conn| {
                    let mut select = conn.prepare(
                        "SELECT name, id, song_count, duration_ms, cover_source, cover_mtime
                         FROM albums",
                    )?;
                    let albums = select
                        .query_map([], |row| {
                            let album = Album {
                                id: row.get(1)?,
                                song_count: row.get(2)?,
                                duration_ms: row.get(3)?,
                                cover_source: row.get(4)?,
                                cover_mtime: row.get(5)?,
                            };
                            Ok((row.get(0)?, album))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    Ok(albums)
                })
                .unwrap()
        }

        /// Returns the ID and the album count of every artist by name.
        fn artists(&self) -> BTreeMap<String, (String, i64)> {
            self.db
                .with_reader(|conn| {
                    let mut select = conn.prepare("SELECT name, id, album_count FROM artists")?;
                    let artists = select
                        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
                        .collect::<rusqlite::Result<_>>()?;
                    Ok(artists)
                })
                .unwrap()
        }
    }

    /// The columns of an album row that the aggregation sets, and its ID.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Album {
        id: String,
        song_count: i64,
        duration_ms: i64,
        cover_source: Option<String>,
        cover_mtime: Option<i64>,
    }

    /// Returns the counts of a summary: added, changed, moved, removed.
    fn counts(summary: &ScanSummary) -> (u64, u64, u64, u64) {
        (
            summary.added,
            summary.changed,
            summary.moved,
            summary.removed,
        )
    }

    #[test]
    fn a_moved_file_whose_fingerprint_cannot_be_read_keeps_its_row_until_it_is_read() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        assert_eq!(counts(&library.scan()), (2, 0, 0, 0));
        let before = library.index();
        let id = library.id_at("a.wav").unwrap();

        library.rename("a.wav", "moved/a.wav");
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("moved/a.wav"));
        let summary = library.scan();

        // The row is not deleted: the file that cannot be read has its size.
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);
        assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
        let error = &summary.errors[0];
        assert!(error.contains("\"moved/a.wav\""), "{error}");
        assert!(
            error.contains("cannot be read: the test denies the read"),
            "{error}"
        );
        assert!(
            error.contains("no track of 244 bytes was deleted"),
            "{error}"
        );
        assert_eq!(
            summary.errors[1],
            "1 track whose file is gone was kept, because a path or a file could not be read"
        );
        let scan = library.latest_scan();
        assert_eq!(scan.error, Some(summary.errors.join("; ")));

        // The file can be read again and the row follows it.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(library.id_at("moved/a.wav"), Some(id));
        assert_eq!(library.id_at("a.wav"), None);
    }

    #[test]
    fn a_file_that_cannot_be_read_keeps_the_missing_rows_of_its_size_only() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let a = library.id_at("a.wav").unwrap();

        // Both files are gone. A new file has the size of the first one and
        // cannot be read.
        fs::remove_file(library.path("a.wav")).unwrap();
        fs::remove_file(library.path("b.wav")).unwrap();
        library.write("c.wav", 100, 3);
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("c.wav"));
        let summary = library.scan();

        assert_eq!(counts(&summary), (0, 0, 0, 1));
        assert_eq!(library.id_at("a.wav"), Some(a.clone()));
        assert_eq!(library.id_at("b.wav"), None);
        assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("\"c.wav\""));
        assert!(summary.errors[1].starts_with("1 track whose file is gone was kept"));

        // The new file is read. It is not the first file, whose row goes.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (1, 0, 0, 1));
        assert_eq!(library.id_at("a.wav"), None);
        assert_ne!(library.id_at("c.wav"), Some(a));
    }

    #[test]
    fn a_changed_file_whose_fingerprint_cannot_be_read_keeps_its_row_as_it_is() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.scan();
        let before = library.track_at("a.wav");
        let index = library.index();

        library.set_mtime("a.wav", 2_000);
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("a.wav"));
        let summary = library.scan();

        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(summary.tags_read, 0);
        assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("\"a.wav\""));
        assert_eq!(library.track_at("a.wav"), before);
        assert_eq!(library.index(), index);

        // The file can be read again. Its row takes the new file state.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 1, 0, 0));
        let after = library.track_at("a.wav");
        assert_eq!(after.id, before.id);
        assert_eq!(after.mtime, 2_000);
    }

    #[test]
    fn a_moved_file_whose_tags_cannot_be_read_stays_at_its_old_path_until_they_are_read() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let before = library.track_at("a.wav");
        assert_eq!(before.suffix, "wav");

        // Only the extension changes, so the modification time and the size
        // of the file are those of the row. The tags are read again because
        // the suffix and the content type come from the extension.
        library.rename("a.wav", "a.aiff");
        library
            .untaggable
            .borrow_mut()
            .insert(library.path("a.aiff"));
        let summary = library.scan();

        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(summary.tags_read, 1);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(library.track_at("a.wav"), before);
        assert_eq!(library.id_at("a.aiff"), None);

        // The tags can be read again. The row follows the file and takes
        // the suffix of its new name.
        library.untaggable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(summary.tags_read, 1);
        let after = library.track_at("a.aiff");
        assert_eq!(after.id, before.id);
        assert_eq!(after.suffix, "aiff");
        assert_eq!(after.content_type, "audio/aiff");
        assert_eq!(library.id_at("a.wav"), None);
    }

    #[test]
    fn a_track_in_an_exchange_whose_tags_cannot_be_read_has_no_file_until_they_are_read() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        library.scan();
        let before = library.index();
        let (a, b) = (before[0].id.clone(), before[1].id.clone());

        // The two files exchange their paths and get new modification
        // times, so the tags of both are read again.
        library.rename("a.wav", "spare.tmp");
        library.rename("b.wav", "a.wav");
        library.rename("spare.tmp", "b.wav");
        library.set_mtime("a.wav", 3_000);
        library.set_mtime("b.wav", 4_000);
        library
            .untaggable
            .borrow_mut()
            .insert(library.path("b.wav"));
        let summary = library.scan();

        // The second track is at its new path. The first track cannot move
        // yet and does not keep its old path, where the file of the second
        // track is.
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(library.id_at("a.wav"), Some(b.clone()));
        assert_eq!(library.id_at("b.wav"), None);
        assert_eq!(
            library.id_at(&tracks::placeholder_path(&a)),
            Some(a.clone())
        );
        assert_eq!(
            summary.errors,
            [format!(
                "track {a} at \"a.wav\" has no file until a scan reads the tags of its file at \
                 \"b.wav\": another file is at its path"
            )]
        );
        assert_eq!(library.latest_scan().error, Some(summary.errors[0].clone()));

        library.untaggable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(library.id_at("a.wav"), Some(b));
        assert_eq!(library.id_at("b.wav"), Some(a));
        assert_eq!(library.index().len(), 2);
    }

    #[test]
    fn a_file_at_the_old_path_of_a_moved_track_gets_its_row_while_the_moved_file_is_untagged() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.scan();
        let a = library.id_at("a.wav").unwrap();

        // The track moves and gets a new modification time, so its tags are
        // read again. Another file takes its old path.
        library.rename("a.wav", "moved/a.wav");
        library.set_mtime("moved/a.wav", 2_000);
        library.write("a.wav", 300, 3);
        library.set_mtime("a.wav", 3_000);
        library
            .untaggable
            .borrow_mut()
            .insert(library.path("moved/a.wav"));
        let summary = library.scan();

        // The new file has its row at the path. The moved track is kept
        // without a file.
        assert_eq!(counts(&summary), (1, 0, 0, 0));
        let placed = library.id_at("a.wav").expect("the new file has a row");
        assert_ne!(placed, a);
        assert_eq!(
            library.id_at(&tracks::placeholder_path(&a)),
            Some(a.clone())
        );
        assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
        assert!(summary.errors[0].contains(&a));
        assert!(summary.errors[0].contains("\"moved/a.wav\""));

        library.untaggable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(library.id_at("moved/a.wav"), Some(a));
        assert_eq!(library.id_at("a.wav"), Some(placed));
    }

    #[test]
    fn a_scan_whose_only_change_is_a_track_that_loses_its_file_changes_the_library() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.scan();
        let a = library.id_at("a.wav").unwrap();
        assert!(library.latest_scan().changed_library);
        assert_eq!(counts(&library.scan()), (0, 0, 0, 0));
        let unchanged = library.latest_scan();
        assert!(!unchanged.changed_library);

        // The track moves and gets a new modification time, so its tags are
        // read again, and they cannot be read. Another file takes its old
        // path, and the tags of that file cannot be read either.
        library.rename("a.wav", "moved/a.wav");
        library.set_mtime("moved/a.wav", 2_000);
        library.write("a.wav", 300, 3);
        library.set_mtime("a.wav", 3_000);
        for rel_path in ["moved/a.wav", "a.wav"] {
            library
                .untaggable
                .borrow_mut()
                .insert(library.path(rel_path));
        }
        let summary = library.scan();

        // No row is inserted, updated in place, relinked or deleted. The
        // one write gives the track its placeholder path, and no count
        // holds it.
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index().len(), 2);
        assert_eq!(
            library.id_at(&tracks::placeholder_path(&a)),
            Some(a.clone())
        );
        assert_eq!(library.id_at("a.wav"), None);
        assert_eq!(summary.errors.len(), 1, "{:?}", summary.errors);
        let detached = library.latest_scan();
        assert_eq!(
            detached.counts,
            ScanCounts {
                seen: 3,
                ..ScanCounts::default()
            }
        );
        assert!(detached.changed_library);
        assert!(detached.finished_at > unchanged.finished_at);

        // The next scan finds the same files, reads the same tags in vain
        // and writes nothing.
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(summary.errors, [""; 0]);
        assert!(!library.latest_scan().changed_library);

        // The tags can be read. The track follows its file and the other
        // file gets a row.
        library.untaggable.borrow_mut().clear();
        assert_eq!(counts(&library.scan()), (1, 0, 1, 0));
        assert!(library.latest_scan().changed_library);
        assert_eq!(library.id_at("moved/a.wav"), Some(a));
    }

    #[test]
    fn a_scan_that_is_cancelled_within_a_batch_changes_the_library_when_it_stored_a_batch_before() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        assert!(library.latest_scan().changed_library);
        assert_eq!(counts(&library.scan()), (0, 0, 0, 0));
        assert!(!library.latest_scan().changed_library);

        // Four new files, in batches of two. The first tag read cancels the
        // scan, so the check before the second file of the first batch
        // stops it. No batch is stored.
        for (rel_path, samples, fill) in [
            ("c.wav", 300, 3),
            ("d.wav", 400, 4),
            ("e.wav", 500, 5),
            ("f.wav", 600, 6),
        ] {
            library.write(rel_path, samples, fill);
        }
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(1);
        let summary = library.scan_in_batches_of(2);
        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index().len(), 2);
        let scan = library.latest_scan();
        assert_eq!(scan.error.as_deref(), Some(CANCELLED));
        assert!(!scan.changed_library);

        // The third tag read cancels the scan: the first batch is stored
        // and the second one is not.
        library.cancel = CancellationToken::new();
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(3);
        let summary = library.scan_in_batches_of(2);
        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(counts(&summary), (2, 0, 0, 0));
        assert_eq!(library.index().len(), 4);
        let scan = library.latest_scan();
        assert_eq!(scan.error.as_deref(), Some(CANCELLED));
        assert!(scan.changed_library);

        // The next scan stores the second batch, and the scan after it
        // stores nothing.
        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        assert_eq!(counts(&library.scan_in_batches_of(2)), (2, 0, 0, 0));
        assert!(library.latest_scan().changed_library);
        assert_eq!(counts(&library.scan_in_batches_of(2)), (0, 0, 0, 0));
        assert!(!library.latest_scan().changed_library);
    }

    #[test]
    fn a_track_whose_path_another_track_takes_has_no_file_while_its_file_may_be_unread() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        library.scan();
        let a = library.id_at("a.wav").unwrap();
        let b = library.id_at("b.wav").unwrap();

        // The first file moves to a place where it cannot be read. The
        // second file moves to the old path of the first.
        library.rename("a.wav", "moved/a.wav");
        library.rename("b.wav", "a.wav");
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("moved/a.wav"));
        let summary = library.scan();

        // The second track is at its new path. The first track is not
        // deleted and no longer has the path, where the file of the second
        // track is.
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(library.id_at("a.wav"), Some(b.clone()));
        let placeholder = tracks::placeholder_path(&a);
        assert_eq!(library.id_at(&placeholder), Some(a.clone()));
        assert!(!library.track_at(&placeholder).has_file());
        assert!(library.track_at("a.wav").has_file());
        assert_eq!(summary.errors.len(), 2, "{:?}", summary.errors);
        assert!(summary.errors[0].contains("\"moved/a.wav\""));
        assert_eq!(
            summary.errors[1],
            format!(
                "track {a} at \"a.wav\" has no file until a scan finds it: another track took \
                 its path, and a path or a file that could not be read may hold its file"
            )
        );
        assert_eq!(library.latest_scan().error, Some(summary.errors.join("; ")));

        // The file can be read. The first track follows it and keeps its ID.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(library.id_at("moved/a.wav"), Some(a.clone()));
        assert_eq!(library.id_at(&tracks::placeholder_path(&a)), None);
        assert_eq!(library.id_at("a.wav"), Some(b));
    }

    #[test]
    fn a_track_without_a_file_counts_for_its_album_and_is_not_the_source_of_its_cover() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        let summary = library.scan();
        assert_eq!((summary.albums, summary.artists), (1, 1));
        let a = library.id_at("a.wav").unwrap();
        let duration =
            library.track_at("a.wav").duration_ms + library.track_at("b.wav").duration_ms;
        let duration = i64::try_from(duration).unwrap();
        assert!(duration > 0);

        // The first track has an embedded picture. A WAV file of the test
        // holds none, so the row is given the mark. It keeps the mark until
        // a scan reads the tags of the file again.
        let mark = || {
            library
                .db
                .with_writer(|conn| {
                    conn.execute("UPDATE tracks SET has_picture = 1 WHERE id = ?1", [&a])?;
                    Ok(())
                })
                .unwrap();
        };
        mark();
        library.scan();
        let before = library.album();
        assert_eq!(
            before,
            Album {
                id: before.id.clone(),
                song_count: 2,
                duration_ms: duration,
                cover_source: Some(format!("embedded:{a}")),
                cover_mtime: Some(1_000),
            }
        );

        // The first file moves to a place where it cannot be read. The
        // second file moves to the old path of the first, so the first
        // track is kept without a file.
        library.rename("a.wav", "moved/a.wav");
        library.rename("b.wav", "a.wav");
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("moved/a.wav"));
        let summary = library.scan();
        let placeholder = tracks::placeholder_path(&a);
        assert!(!library.track_at(&placeholder).has_file());

        // The album keeps its row and counts both tracks. The track without
        // a file is not the source of the cover, and the other track has no
        // picture.
        assert_eq!((summary.albums, summary.artists), (1, 1));
        assert_eq!(
            library.album(),
            Album {
                cover_source: None,
                cover_mtime: None,
                ..before.clone()
            }
        );

        // The file can be read, and the track has a file again. The scan
        // read its tags again, so the row is given the mark again. The
        // track is then the source of the cover.
        library.unreadable.borrow_mut().clear();
        library.scan();
        assert_eq!(library.id_at("moved/a.wav"), Some(a.clone()));
        mark();
        library.scan();
        assert_eq!(library.album(), before);
    }

    #[test]
    fn an_album_whose_only_track_has_no_file_keeps_its_row_and_its_id() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        // Each file is the one track of an album of its own artist.
        library.tag("a.wav", "First Artist", "First Album");
        library.tag("moved/a.wav", "First Artist", "First Album");
        library.tag("b.wav", "Second Artist", "Second Album");
        let summary = library.scan();
        assert_eq!((summary.albums, summary.artists), (2, 2));
        let a = library.id_at("a.wav").unwrap();
        let b = library.id_at("b.wav").unwrap();
        let albums = library.albums();
        let artists = library.artists();
        let first = albums["First Album"].clone();
        assert_eq!(first.song_count, 1);
        assert_eq!(library.track_at("a.wav").album_id, first.id);
        assert_eq!(artists["First Artist"].1, 1);

        // The first file moves to a place where it cannot be read. The
        // second file, which is a track of the other album, moves to the old
        // path of the first. The first track is kept without a file.
        library.rename("a.wav", "moved/a.wav");
        library.rename("b.wav", "a.wav");
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("moved/a.wav"));
        let summary = library.scan();

        // The scan read no tags, so the second track is in its album.
        assert_eq!(summary.tags_read, 0);
        assert_eq!(library.id_at("a.wav"), Some(b));
        assert_eq!(
            library.track_at("a.wav").album_id,
            albums["Second Album"].id
        );
        // The only track of the first album has a placeholder path.
        let placeholder = tracks::placeholder_path(&a);
        let kept = library.track_at(&placeholder);
        assert_eq!(kept.id, a);
        assert!(!tracks::has_file(&kept.rel_path));
        assert_eq!(kept.album_id, first.id);
        assert_eq!(kept.artist_id, artists["First Artist"].0);
        // The album and its artist keep their rows, their IDs and their
        // counts.
        assert_eq!((summary.albums, summary.artists), (2, 2));
        assert_eq!(library.albums(), albums);
        assert_eq!(library.artists(), artists);

        // The file can be read. The track has a file again, in the album
        // row that it kept.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(library.id_at("moved/a.wav"), Some(a));
        assert_eq!(library.track_at("moved/a.wav").album_id, first.id);
        assert_eq!(library.albums(), albums);
        assert_eq!(library.artists(), artists);
    }

    #[test]
    fn a_track_without_a_file_is_deleted_when_every_file_is_read_and_none_is_its_file() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        library.scan();
        let a = library.id_at("a.wav").unwrap();

        // The second file moves over the first. A new file of the size of
        // the first cannot be read.
        library.rename("b.wav", "a.wav");
        library.write("c.wav", 100, 3);
        library
            .unreadable
            .borrow_mut()
            .insert(library.path("c.wav"));
        let summary = library.scan();
        assert_eq!(counts(&summary), (0, 0, 1, 0));
        assert_eq!(
            library.id_at(&tracks::placeholder_path(&a)),
            Some(a.clone())
        );

        // The new file is read. It is another file, so the first track is
        // gone.
        library.unreadable.borrow_mut().clear();
        let summary = library.scan();
        assert_eq!(counts(&summary), (1, 0, 0, 1));
        assert_eq!(summary.errors, [""; 0]);
        assert!(library.index().iter().all(|entry| entry.id != a));
    }

    #[test]
    fn cancelling_while_the_fingerprints_are_read_stops_the_scan() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let before = library.index();

        // A file is gone, so the scan reads the fingerprint of every new
        // file before it writes. The second read cancels the scan.
        fs::remove_file(library.path("a.wav")).unwrap();
        library.write("c.wav", 300, 3);
        library.write("d.wav", 400, 4);
        library.write("e.wav", 500, 5);
        library.fingerprint_reads.set(0);
        library.cancel_at_read.set(2);
        let summary = library.scan_in_batches_of(1);

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        // The check before the third read stopped the scan.
        assert_eq!(library.fingerprint_reads.get(), 2);
        assert_eq!(library.index(), before);
        let scan = library.latest_scan();
        assert_eq!(scan.error.as_deref(), Some(CANCELLED));
    }

    #[test]
    fn cancelling_during_the_fingerprint_pass_stops_before_the_next_read_within_a_batch() {
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let before = library.index();

        // A file is gone, so the scan reads the fingerprint of every new
        // file before it writes. The four new files are fewer than one
        // batch of five, so no position after the first is a multiple of
        // the batch size. The second read cancels the scan.
        fs::remove_file(library.path("a.wav")).unwrap();
        library.write("c.wav", 300, 3);
        library.write("d.wav", 400, 4);
        library.write("e.wav", 500, 5);
        library.write("f.wav", 600, 6);
        library.fingerprint_reads.set(0);
        library.tag_reads.set(0);
        library.cancel_at_read.set(2);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        // The check before the third read stopped the scan.
        assert_eq!(library.fingerprint_reads.get(), 2);
        assert_eq!(library.tag_reads.get(), 0);
        assert_eq!(summary.tags_read, 0);
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);
        assert_eq!(library.latest_scan().error.as_deref(), Some(CANCELLED));
    }

    /// The relative paths of the rows of `index`.
    fn paths(index: &[IndexEntry]) -> Vec<&str> {
        index.iter().map(|entry| entry.rel_path.as_str()).collect()
    }

    #[test]
    fn cancelling_within_a_batch_stores_no_row_of_it_and_the_next_scan_stores_them() {
        // Twelve new files of twelve sizes. The database has no row, so the
        // scan skips the fingerprint pass and reads the fingerprint and
        // then the tags of each file when it comes to the batch of the file.
        const FILES: [(&str, u32, u8); 12] = [
            ("01.wav", 100, 1),
            ("02.wav", 200, 2),
            ("03.wav", 300, 3),
            ("04.wav", 400, 4),
            ("05.wav", 500, 5),
            ("06.wav", 600, 6),
            ("07.wav", 700, 7),
            ("08.wav", 800, 8),
            ("09.wav", 900, 9),
            ("10.wav", 1000, 10),
            ("11.wav", 1100, 11),
            ("12.wav", 1200, 12),
        ];
        let names: Vec<&str> = FILES.iter().map(|file| file.0).collect();
        let mut library = Library::new(&FILES);

        // The batches hold five files. The seventh tag read is the second
        // one of the second batch, and it cancels the scan.
        library.cancel_at_tag_read.set(7);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        // The five reads of the first batch and two reads of the second.
        assert_eq!(summary.tags_read, 7);
        // The check before the third file of the batch stopped the scan:
        // neither the fingerprint nor the tags of that file were read.
        assert_eq!(library.tag_reads.get(), 7);
        assert_eq!(library.fingerprint_reads.get(), 7);
        // The first batch is stored. No row of the second batch is, and
        // the hook did not run for it.
        assert_eq!(counts(&summary), (5, 0, 0, 0));
        let stored = library.index();
        assert_eq!(paths(&stored), names[..5]);
        assert_eq!(library.batches.load(Ordering::SeqCst), 1);
        let scan = library.latest_scan();
        assert_eq!(scan.error.as_deref(), Some(CANCELLED));
        assert_eq!((scan.counts.added, scan.counts.removed), (5, 0));

        // The next scan stores the two files that the cancelled scan had
        // read and the five that it had not, and it keeps the five rows.
        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan_in_batches_of(5);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (7, 0, 0, 0));
        assert_eq!(summary.tags_read, 7);
        let complete = library.index();
        assert_eq!(paths(&complete), names);
        assert_eq!(complete[..5], stored[..]);
        // The batches of five and of two files.
        assert_eq!(library.batches.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cancelling_during_the_fingerprint_read_of_a_batch_stops_before_the_tags_of_the_file() {
        // The database has no row, so the fingerprint of each file is read
        // in its batch, before its tags. The second fingerprint read
        // cancels the scan.
        let library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2), ("c.wav", 300, 3)]);
        library.cancel_at_read.set(2);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.fingerprint_reads.get(), 2);
        // The tags of the first file were read. The check after the second
        // fingerprint read stopped the scan before the tags of that file.
        assert_eq!(library.tag_reads.get(), 1);
        assert_eq!(summary.tags_read, 1);
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), []);
        assert_eq!(library.batches.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancelling_within_a_batch_of_changed_files_leaves_every_row_as_it_is() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2), ("c.wav", 300, 3)]);
        for name in ["a.wav", "b.wav", "c.wav"] {
            library.set_mtime(name, 1_000);
        }
        library.scan();
        let before = library.index();

        // Each file has a new modification time and its old content, so
        // each row is updated in place from the tags of its file. The
        // second tag read cancels the scan.
        for name in ["a.wav", "b.wav", "c.wav"] {
            library.set_mtime(name, 2_000);
        }
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(2);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.tag_reads.get(), 2);
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);

        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan_in_batches_of(5);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (0, 3, 0, 0));
    }

    #[test]
    fn cancelling_within_a_batch_of_moved_files_moves_no_row() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2), ("c.wav", 300, 3)]);
        library.scan();
        let before = library.index();

        // Each file moves and gets a new modification time, so each row
        // follows its file and the tags of the file are read again. The
        // second tag read cancels the scan.
        for name in ["a.wav", "b.wav", "c.wav"] {
            let moved = format!("moved/{name}");
            library.rename(name, &moved);
            library.set_mtime(&moved, 2_000);
        }
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(2);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.tag_reads.get(), 2);
        // No row moved, and none was deleted, although no file is at the
        // path of a row.
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);

        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan_in_batches_of(5);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (0, 0, 3, 0));
        let ids = |index: &[IndexEntry]| -> Vec<String> {
            index.iter().map(|entry| entry.id.clone()).collect()
        };
        assert_eq!(ids(&library.index()), ids(&before));
    }

    #[test]
    fn a_scan_that_is_cancelled_within_a_batch_deletes_no_row() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let before = library.index();
        let b = library.id_at("b.wav").unwrap();

        // The file of a row is gone, and three files of other sizes are
        // new. A scan that completes inserts the three and deletes the row.
        // This one is cancelled by the second tag read of its one batch.
        fs::remove_file(library.path("a.wav")).unwrap();
        library.write("c.wav", 300, 3);
        library.write("d.wav", 400, 4);
        library.write("e.wav", 500, 5);
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(2);
        let summary = library.scan_in_batches_of(5);

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.tag_reads.get(), 2);
        assert_eq!(summary.tags_read, 2);
        // The scan did not read every file, so it does not know that the
        // file of the row is gone. It stored nothing and deleted nothing.
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);

        // The next scan reads every file and does both.
        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan_in_batches_of(5);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (3, 0, 0, 1));
        assert_eq!(
            paths(&library.index()),
            ["b.wav", "c.wav", "d.wav", "e.wav"]
        );
        assert_eq!(library.id_at("b.wav"), Some(b));
    }

    #[test]
    fn a_scan_that_is_cancelled_by_its_last_file_read_stores_the_batch_and_deletes_no_row() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.scan();
        let a = library.id_at("a.wav").unwrap();

        // The file of a row is gone, and three files of other sizes are
        // new. The third tag read is the last file read of the scan, and
        // it cancels the scan.
        fs::remove_file(library.path("a.wav")).unwrap();
        library.write("c.wav", 300, 3);
        library.write("d.wav", 400, 4);
        library.write("e.wav", 500, 5);
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(3);
        let summary = library.scan_in_batches_of(5);

        // The batch was read in full, so it is stored. The scan is a
        // cancelled scan all the same, and a cancelled scan deletes no row.
        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.tag_reads.get(), 3);
        assert_eq!(counts(&summary), (3, 0, 0, 0));
        assert_eq!(library.id_at("a.wav"), Some(a));
        assert_eq!(library.index().len(), 5);

        // The next scan deletes the row.
        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan_in_batches_of(5);
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (0, 0, 0, 1));
        assert_eq!(library.id_at("a.wav"), None);
    }

    #[test]
    fn cancelling_within_the_exchange_batch_stores_none_of_its_changes() {
        let mut library = Library::new(&[("a.wav", 100, 1), ("b.wav", 200, 2)]);
        library.set_mtime("a.wav", 1_000);
        library.set_mtime("b.wav", 2_000);
        library.scan();
        let before = library.index();
        let a = library.id_at("a.wav").unwrap();
        let b = library.id_at("b.wav").unwrap();

        // Each file moves, and a new file takes its path. Both rows are
        // displaced, so the exchange batch holds four changes: the new row
        // of each new file, and the move of each row to its file.
        library.rename("a.wav", "moved/a.wav");
        library.rename("b.wav", "moved/b.wav");
        library.write("a.wav", 300, 3);
        library.write("b.wav", 400, 4);
        // The tags of the two new files are read. The first read cancels
        // the scan.
        library.tag_reads.set(0);
        library.cancel_at_tag_read.set(1);
        let summary = library.scan();

        assert_eq!(summary.errors, [CANCELLED]);
        assert_eq!(library.tag_reads.get(), 1);
        // No change of the batch is stored: each row is at its old path
        // with its old fingerprint, and no row is new.
        assert_eq!(counts(&summary), (0, 0, 0, 0));
        assert_eq!(library.index(), before);

        // The next scan stores the whole batch. Each row follows its file.
        library.cancel = CancellationToken::new();
        library.cancel_at_tag_read.set(0);
        let summary = library.scan();
        assert_eq!(summary.errors, [""; 0]);
        assert_eq!(counts(&summary), (2, 0, 2, 0));
        assert_eq!(library.id_at("moved/a.wav"), Some(a));
        assert_eq!(library.id_at("moved/b.wav"), Some(b));
        assert_eq!(library.index().len(), 4);
    }
}
