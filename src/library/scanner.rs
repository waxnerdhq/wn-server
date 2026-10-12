//! The scanner: brings the `tracks` table in line with the files in the
//! music folders (`docs/design.md` section 3.10).
//!
//! A track keeps its ID while the scanner can tell where its file is. A
//! file that is unchanged at its path, a file that changed at its path, and a
//! file that moved, alone or with its folder, all keep the row. A row is
//! deleted only when the scanner knows that its file is gone: it read the
//! directory of the file and the file is not there, or the folder was
//! removed from the configuration.
//!
//! [`scan`] blocks its thread on file and database work. Files are read
//! outside the write transactions.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::db::queries::folders::{self, Folder};
use crate::db::queries::scans::{self, ScanCounts};
use crate::db::queries::tracks::{self, IndexEntry, TrackContent};
use crate::db::queries::{albums, artists};
use crate::db::{Db, Result};
use crate::ids::{self, Kind};
use crate::library::tags::{self, TrackMeta};

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
    /// that is unchanged, a file whose changes are committed, and a file
    /// that cannot be read. A scan that completes adds [`ScanSummary::seen`]
    /// in total.
    pub progress: Arc<AtomicU64>,
}

impl Default for ScanOptions {
    /// Returns a batch size of [`DEFAULT_BATCH_SIZE`], no hook and a new
    /// counter at 0.
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            on_batch: None,
            progress: Arc::default(),
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
    /// One message for each folder problem: a folder or a path in it that
    /// cannot be read, a folder that had tracks and has no supported file,
    /// and a removed folder whose tracks were kept. A cancelled scan has
    /// [`CANCELLED`] as its last message.
    pub errors: Vec<String>,
}

/// Scans `folders` and stores the result in `db`.
///
/// The folder rows are synchronised with `folders` first. A track whose file
/// is unchanged is not read. A track whose file changed at its path keeps
/// its row and has its tags read again. A track whose file is gone is
/// matched by fingerprint to a file the database does not know and follows
/// it. A track without a match is deleted, unless the path of its file could
/// not be read or its folder holds no supported file any more. The tracks of
/// a folder that is no longer in `folders` are deleted when every folder in
/// `folders` was read in full and holds a supported file. `full` reads the
/// fingerprint and the tags of every file.
///
/// The function blocks. An async caller runs it with
/// `tokio::task::spawn_blocking`. `cancel` is checked during the walk,
/// while the fingerprints are read and between batches. A cancelled scan
/// keeps the batches it committed, deletes nothing and returns a summary
/// whose last error is [`CANCELLED`].
///
/// # Errors
///
/// Returns a database error, and the error of a folder path that is not
/// valid Unicode. A file or a folder that cannot be read is not an error of
/// the scan.
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
    let start = db.with_writer(|conn| begin(conn, folders))?;
    let scan_id = start.scan_id;
    let mut scanner = Scanner {
        db,
        cancel,
        full,
        options,
        summary: ScanSummary::default(),
    };
    let outcome = scanner.run(start);
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
    let finished = db.with_writer(|conn| scans::finish(conn, scan_id, &counts, error.as_deref()));
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
}

/// Inserts the `scans` row, synchronises the folder rows with `paths` and
/// loads the index, in one transaction.
fn begin(conn: &mut Connection, paths: &[PathBuf]) -> Result<Start> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let scan_id = scans::insert(&tx)?;
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
    })
}

/// How a scan ended when no database error stopped it.
enum Outcome {
    Completed,
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

/// A supported file that the walk saw.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundFile {
    folder_id: i64,
    /// The path below the folder, components joined with `/`.
    rel_path: String,
    /// The path that opens the file.
    path: PathBuf,
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
}

impl FolderWalk {
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
}

/// The changes of one scan.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// The changes that vacate or take the path of a displaced row. They
    /// are stored in one transaction.
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
    Relink {
        id: &'a str,
        /// Whether the row moves to a temporary path before any row takes
        /// its new path.
        via_temporary: bool,
        file: FoundFile,
        /// The modification time to store.
        mtime: i64,
        /// The fingerprint of the row, which is the one of the file.
        fingerprint: &'a str,
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

/// The state of one scan.
struct Scanner<'a> {
    db: &'a Db,
    cancel: &'a CancellationToken,
    full: bool,
    options: ScanOptions,
    summary: ScanSummary,
}

impl Scanner<'_> {
    fn run(&mut self, start: Start) -> Result<Outcome> {
        let Start {
            configured,
            unconfigured,
            index,
            ..
        } = start;
        let mut states = vec![RowState::Unseen; index.len()];
        let with_tracks: HashSet<i64> = index.iter().map(|entry| entry.folder_id).collect();

        // The walk reads directories and file states only.
        let mut found = Found::default();
        let mut walks: HashMap<i64, FolderWalk> = HashMap::with_capacity(configured.len());
        {
            let lookup: HashMap<(i64, &str), usize> = index
                .iter()
                .enumerate()
                .map(|(row, entry)| (locator(entry), row))
                .collect();
            for folder in &configured {
                let walk = self.walk(folder, &lookup, &index, &mut states, &mut found);
                if self.cancel.is_cancelled() {
                    return Ok(Outcome::Cancelled);
                }
                if walk.unread.is_empty() && walk.supported == 0 && with_tracks.contains(&folder.id)
                {
                    self.problem(format!(
                        "music folder {} has no supported file; none of its tracks was deleted",
                        describe(folder)
                    ));
                }
                walks.insert(folder.id, walk);
            }
        }

        let any_gone = (0..index.len()).any(|row| is_gone(&index[row], states[row], &walks));
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
            let Some((updates, candidates)) = self.fingerprints(found, &index, &mut states) else {
                return Ok(Outcome::Cancelled);
            };
            let mut missing: Vec<usize> = (0..index.len())
                .filter(|&row| {
                    states[row] == RowState::Displaced || is_gone(&index[row], states[row], &walks)
                })
                .collect();
            missing.sort_by(|&a, &b| locator(&index[a]).cmp(&locator(&index[b])));
            plan(&index, &states, &missing, candidates, updates)
        };

        let Plan {
            exchange,
            rest,
            unmatched,
        } = plan;
        let batch_size = self.options.batch_size.max(1);
        let mut rest = rest.into_iter();
        let mut batch = exchange;
        loop {
            if batch.is_empty() {
                batch = rest.by_ref().take(batch_size).collect();
            }
            if batch.is_empty() {
                break;
            }
            if self.cancel.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            self.apply(&index, &states, std::mem::take(&mut batch))?;
        }
        if self.cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }

        self.remove(&index, &walks, &configured, &unconfigured, &unmatched)?;
        Ok(Outcome::Completed)
    }

    /// Walks `folder`. Marks the rows whose path holds a file and sorts the
    /// supported files into unchanged, changed and new.
    fn walk(
        &mut self,
        folder: &Folder,
        lookup: &HashMap<(i64, &str), usize>,
        index: &[IndexEntry],
        states: &mut [RowState],
        found: &mut Found,
    ) -> FolderWalk {
        let mut walk = FolderWalk::default();
        let mut entries = WalkDir::new(&folder.path)
            .follow_links(true)
            .sort_by_file_name()
            .into_iter();
        while let Some(entry) = entries.next() {
            if self.cancel.is_cancelled() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.walk_error(folder, &mut walk, &error);
                    continue;
                }
            };
            let is_dir = entry.file_type().is_dir();
            if entry.depth() == 0 {
                if !is_dir {
                    walk.unread.push(String::new());
                    self.problem(format!(
                        "music folder {} cannot be read: it is not a directory; none of its \
                         tracks was deleted",
                        describe(folder)
                    ));
                }
                continue;
            }
            if is_hidden(entry.file_name()) {
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
            if !entry.file_type().is_file() || !tags::is_supported(entry.path()) {
                continue;
            }
            let (mtime, size) = match file_state(&entry) {
                Ok(state) => state,
                Err(cause) => {
                    self.unread(folder, &mut walk, rel_path, &cause);
                    continue;
                }
            };
            walk.supported += 1;
            self.summary.seen += 1;
            let file = FoundFile {
                folder_id: folder.id,
                rel_path,
                path: entry.into_path(),
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
        walk
    }

    /// Records what the walk of `folder` could not read.
    fn walk_error(&mut self, folder: &Folder, walk: &mut FolderWalk, error: &walkdir::Error) {
        let cause = error
            .io_error()
            .map_or_else(|| error.to_string(), ToString::to_string);
        if error.depth() == 0 {
            walk.unread.push(String::new());
            self.problem(format!(
                "music folder {} cannot be read: {cause}; none of its tracks was deleted",
                describe(folder)
            ));
            return;
        }
        let relative = error
            .path()
            .and_then(|path| path.strip_prefix(&folder.path).ok());
        let Some(relative) = relative else {
            // The error does not say which directory it is about.
            walk.unread.push(String::new());
            self.problem(format!(
                "music folder {} cannot be read in full: {cause}; none of its tracks was deleted",
                describe(folder)
            ));
            return;
        };
        if relative.file_name().is_some_and(is_hidden) {
            return;
        }
        match rel_path(relative) {
            Some(rel_path) => self.unread(folder, walk, rel_path, &cause),
            // No track is stored below a name that is not valid Unicode.
            None => tracing::warn!(
                path = %relative.display(),
                cause,
                "the entry cannot be read; it is skipped"
            ),
        }
    }

    /// Records that `rel_path` in `folder` could not be read.
    fn unread(&mut self, folder: &Folder, walk: &mut FolderWalk, rel_path: String, cause: &str) {
        self.problem(format!(
            "\"{rel_path}\" in music folder {} cannot be read: {cause}; the tracks at and below \
             it were kept",
            describe(folder)
        ));
        walk.unread.push(rel_path);
    }

    /// Reads the fingerprint of every changed and every new file. Returns
    /// the updates of the rows whose file kept its fingerprint, and the
    /// candidates. Marks the rows whose file has another fingerprint as
    /// displaced. Returns `None` when the scan is cancelled.
    fn fingerprints(
        &mut self,
        found: Found,
        index: &[IndexEntry],
        states: &mut [RowState],
    ) -> Option<(Vec<Op>, Vec<Candidate>)> {
        let batch_size = self.options.batch_size.max(1);
        let mut updates = Vec::new();
        let mut candidates = Vec::new();
        let changed = found
            .changed
            .into_iter()
            .map(|(row, file)| (Some(row), file));
        let new = found.new.into_iter().map(|file| (None, file));
        for (position, (row, file)) in changed.chain(new).enumerate() {
            if position % batch_size == 0 && self.cancel.is_cancelled() {
                return None;
            }
            let Some(fingerprint) = read_fingerprint(&file) else {
                // The row of an unreadable file stays as it is.
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
    fn apply(&mut self, index: &[IndexEntry], states: &[RowState], batch: Vec<Op>) -> Result<()> {
        let mut writes = Vec::with_capacity(batch.len());
        let mut files = 0;
        for op in batch {
            match op {
                Op::Delete { row } => writes.push(Write::Delete { id: &index[row].id }),
                Op::Update {
                    row,
                    file,
                    fingerprint,
                } => {
                    files += 1;
                    // The row of a file whose tags cannot be read stays as
                    // it is, so the next scan reads the file again.
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
                    files += 1;
                    let known = &index[row];
                    let meta = if reread { self.read_tags(&file) } else { None };
                    // When the tags cannot be read, the row keeps its
                    // modification time, so the next scan reads them.
                    let mtime = if reread && meta.is_none() {
                        known.mtime
                    } else {
                        file.mtime
                    };
                    writes.push(Write::Relink {
                        id: &known.id,
                        via_temporary: states[row] == RowState::Displaced,
                        file,
                        mtime,
                        fingerprint: &known.fingerprint,
                        meta,
                    });
                }
                Op::Insert { file, fingerprint } => {
                    files += 1;
                    let Some(fingerprint) = fingerprint.or_else(|| read_fingerprint(&file)) else {
                        continue;
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

        let counts = self.db.with_writer(|conn| store(conn, &writes))?;
        self.summary.added += counts.added;
        self.summary.changed += counts.changed;
        self.summary.moved += counts.moved;
        self.summary.removed += counts.removed;
        self.advance(files);
        if let Some(on_batch) = &self.options.on_batch {
            on_batch();
        }
        Ok(())
    }

    /// Deletes the rows whose file is gone, and the folder rows that are no
    /// longer configured.
    ///
    /// A configured folder that had tracks and holds no supported file keeps
    /// its rows. A folder that is no longer configured keeps its rows unless
    /// at least one folder is configured and every configured folder was
    /// read in full and holds a supported file. Otherwise its files may be
    /// in a folder that the scan could not read.
    fn remove(
        &mut self,
        index: &[IndexEntry],
        walks: &HashMap<i64, FolderWalk>,
        configured: &[Folder],
        unconfigured: &[Folder],
        unmatched: &[usize],
    ) -> Result<()> {
        let all_hold_files = !configured.is_empty() && walks.values().all(FolderWalk::holds_files);
        let mut deleted_ids: Vec<&str> = Vec::new();
        let mut kept: HashSet<i64> = HashSet::new();
        for &row in unmatched {
            let entry = &index[row];
            let delete = match walks.get(&entry.folder_id) {
                Some(walk) => walk.supported > 0,
                None => all_hold_files,
            };
            if delete {
                deleted_ids.push(&entry.id);
            } else {
                kept.insert(entry.folder_id);
            }
        }
        for folder in unconfigured
            .iter()
            .filter(|folder| kept.contains(&folder.id))
        {
            self.problem(format!(
                "the tracks of the removed music folder {} were kept, because a configured \
                 music folder could not be read in full or has no supported file",
                describe(folder)
            ));
        }

        let removed = self.db.with_writer(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let removed = tracks::delete_tracks(&tx, &deleted_ids)?;
            for folder in unconfigured {
                if tracks::count_tracks_in_folder(&tx, folder.id)? == 0 {
                    folders::delete(&tx, folder.id)?;
                }
            }
            aggregate(&tx)?;
            tx.commit()?;
            Ok(removed)
        })?;
        self.summary.removed += removed;
        Ok(())
    }

    /// Reads the tags of `file`. Logs a warning and returns `None` when the
    /// file cannot be read.
    fn read_tags(&mut self, file: &FoundFile) -> Option<TrackMeta> {
        self.summary.tags_read += 1;
        match tags::read(&file.path) {
            Ok(meta) => Some(meta),
            Err(error) => {
                tracing::warn!(%error, "the file is skipped");
                None
            }
        }
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
/// of a file that was retagged in place.
fn plan(
    index: &[IndexEntry],
    states: &[RowState],
    missing: &[usize],
    candidates: Vec<Candidate>,
    updates: Vec<Op>,
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
                // The row at the path of the file has no file any more.
                if let Some(replaced) = stays {
                    plan.exchange.push(Op::Delete { row: replaced });
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
            (None, None) => plan.rest.push(Op::Insert {
                file,
                fingerprint: Some(fingerprint),
            }),
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

/// Stores the changes of one batch in one transaction and returns what it
/// changed.
///
/// The deletions run first. Then every displaced row that moves is given a
/// temporary path that no file can have. Then the rows are given their new
/// paths. In this order two rows can exchange paths, although SQLite checks
/// the unique index on the locator after every statement.
fn store(conn: &mut Connection, writes: &[Write<'_>]) -> Result<ScanCounts> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        if let Write::Relink {
            id,
            via_temporary: true,
            file,
            mtime,
            ..
        } = write
        {
            // A relative path never starts with `/`.
            let temporary = format!("/{id}");
            tracks::relink_track(&tx, id, file.folder_id, &temporary, *mtime, file.size)?;
        }
    }

    for write in writes {
        match write {
            Write::Delete { .. } => {}
            Write::Relink {
                id,
                file,
                mtime,
                fingerprint,
                meta,
                ..
            } => {
                tracks::relink_track(&tx, id, file.folder_id, &file.rel_path, *mtime, file.size)?;
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
    tx.commit()?;
    Ok(counts)
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

/// Step 6 of the scan, the aggregates of albums and artists. It changes
/// nothing: album and artist rows keep the values their upserts gave them.
fn aggregate(_conn: &Connection) -> Result<()> {
    Ok(())
}

/// Reads the fingerprint of `file`. Logs a warning and returns `None` when
/// the file cannot be read.
fn read_fingerprint(file: &FoundFile) -> Option<String> {
    match ids::fingerprint(&file.path) {
        Ok(fingerprint) => Some(fingerprint),
        Err(error) => {
            tracing::warn!(
                path = %file.path.display(),
                %error,
                "cannot read the file; it is skipped"
            );
            None
        }
    }
}

/// Returns the modification time in Unix seconds and the size of the file
/// of `entry`, or the reason why they cannot be read.
fn file_state(entry: &walkdir::DirEntry) -> std::result::Result<(i64, u64), String> {
    let metadata = entry.metadata().map_err(|error| error.to_string())?;
    let modified = metadata.modified().map_err(|error| error.to_string())?;
    Ok((unix_seconds(modified), metadata.len()))
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

/// Returns the directory components of a relative path.
fn directories(rel_path: &str) -> impl Iterator<Item = &str> {
    let directory = rel_path
        .rsplit_once('/')
        .map_or("", |(directory, _)| directory);
    directory.split('/').filter(|name| !name.is_empty())
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
    use std::time::Duration;

    use super::*;

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
            path: PathBuf::from(rel_path),
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

    /// Plans with every row in `missing`, in the order of the rows.
    fn plan_of(index: &[IndexEntry], states: &[RowState], candidates: Vec<Candidate>) -> Plan {
        let mut missing: Vec<usize> = (0..index.len())
            .filter(|&row| states[row] != RowState::Present)
            .collect();
        missing.sort_by(|&a, &b| locator(&index[a]).cmp(&locator(&index[b])));
        plan(index, states, &missing, candidates, Vec::new())
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
        };
        assert!(failed.is_unread("Artist/Album/01.flac"));

        let read = FolderWalk {
            unread: Vec::new(),
            supported: 1,
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
                exchange: vec![relink(0, "q.flac")],
                rest: vec![insert("p.flac", "10:cc")],
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
        );
        assert_eq!(plan.rest, [update]);
    }
}
