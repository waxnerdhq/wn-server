//! The scans of a running server (`docs/design.md` section 3.10).
//!
//! One scan of a library runs at a time. Two rules keep it so:
//!
//! - [`ScanController`] runs at most one scan in its process. The scan at
//!   startup, `startScan` and the periodic scans all start through it.
//! - [`ScanLock`] is a lock on the file `scan.lock` in the data directory.
//!   Every scan holds it from its start to its end: a scan of the controller
//!   and a scan of the `wn-server scan` command. A process that finds the
//!   lock held starts no scan.
//!
//! The scanner decides its changes from the rows it read when it started.
//! A second scan that writes rows in the meantime would make those
//! decisions stale, so a second scan is refused and never queued.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::Db;
use crate::library::scanner::{self, DEFAULT_BATCH_SIZE, ScanOptions};

/// A scan lock that cannot be taken for another reason than a holder: the
/// file cannot be created or opened, or the file system has no locks.
#[derive(Debug, thiserror::Error)]
#[error("cannot lock {}: {source}", path.display())]
pub struct ScanLockError {
    path: PathBuf,
    source: io::Error,
}

/// The lock of the scans of one data directory. The process that holds it
/// is the one that scans the library.
///
/// The lock is an exclusive lock of the operating system on the lock file:
/// `flock` on Unix and `LockFileEx` on Windows. It ends when the value is
/// dropped, and the operating system ends it when the process ends, so a
/// process that was killed leaves no lock behind. The file stays and has no
/// content.
#[derive(Debug)]
pub struct ScanLock {
    /// The open lock file. Closing it releases the lock.
    _file: File,
}

impl ScanLock {
    /// Takes the lock on the file at `path` and creates the file when it is
    /// missing. Returns `None` when the lock is held: by another process, or
    /// by another value of this type in this process.
    ///
    /// The call does not wait for the lock.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>, ScanLockError> {
        let error = |source| ScanLockError {
            path: path.to_path_buf(),
            source,
        };
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(error)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(error(source)),
        }
    }
}

/// The two values of [`ScanOptions`] with which a test stops a scan of a
/// [`ScanController`] at a known point. The server uses the defaults.
#[derive(Clone, Default)]
pub struct ScanHooks {
    /// The number of files in a batch. `None` is the default of the
    /// scanner.
    pub batch_size: Option<usize>,
    /// Runs after each batch of a scan is committed. The scan holds no
    /// database connection while it runs.
    pub on_batch: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// What a [`ScanController`] scans.
pub struct ScanSettings {
    /// The library database.
    pub db: Db,
    /// The configured music folders.
    pub folders: Vec<PathBuf>,
    /// The directory of the resized covers (`DataDir::covers_cache_dir`). A
    /// server sets it, so that every scan removes the resized covers that
    /// are stale. `None` is for a library without a data directory, which a
    /// test has.
    pub covers_cache_dir: Option<PathBuf>,
    /// The lock file of the data directory (`DataDir::scan_lock_path`). A
    /// server sets it, so that its scans and the scans of `wn-server scan`
    /// exclude each other. `None` is for a library without a data
    /// directory.
    pub lock_path: Option<PathBuf>,
    /// Cancels the scans of the controller and ends its periodic scans.
    pub cancel: CancellationToken,
    /// The test hooks.
    pub hooks: ScanHooks,
}

/// What [`ScanController::start`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// The call started a scan.
    Started,
    /// A scan of this controller runs. The call started none.
    AlreadyRunning,
    /// Another process scans the library and holds the scan lock. The call
    /// started none.
    RunningElsewhere,
}

/// What [`ScanController::status`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanStatus {
    /// Whether a scan of the controller runs.
    pub scanning: bool,
    /// The supported files that the running scan has finished with. When no
    /// scan runs, the number of the scan of the controller that ended last,
    /// and 0 before the first one.
    pub count: u64,
}

/// Runs the scans of one server process, at most one at a time. Clones
/// share one state.
#[derive(Clone)]
pub struct ScanController {
    inner: Arc<Inner>,
}

struct Inner {
    settings: ScanSettings,
    /// Whether a scan of the controller runs. [`ScanController::start`] sets
    /// it and the scan clears it when it ends.
    scanning: AtomicBool,
    /// The progress counter of the scan that started last.
    progress: Arc<AtomicU64>,
    /// The task of the scan that started last. `start` holds this lock from
    /// the test of `scanning` until the task is stored, so the task is
    /// always the one of the scan that started last.
    handle: Mutex<Option<JoinHandle<()>>>,
    /// The task of the periodic scans.
    periodic: Mutex<Option<JoinHandle<()>>>,
}

impl ScanController {
    /// A controller that has started no scan.
    pub fn new(settings: ScanSettings) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings,
                scanning: AtomicBool::new(false),
                progress: Arc::default(),
                handle: Mutex::new(None),
                periodic: Mutex::new(None),
            }),
        }
    }

    /// Starts a scan on a blocking thread and returns without waiting for
    /// it. `full` reads the fingerprint and the tags of every file.
    ///
    /// The call starts no scan while a scan of this controller runs, and
    /// none while another process holds the scan lock. It does not queue
    /// the scan either: the caller asks again later. The scan removes the
    /// stale files of the cover cache of the settings.
    ///
    /// The call opens the lock file and asks for the lock without waiting,
    /// so an async caller calls it directly.
    ///
    /// # Errors
    ///
    /// Returns the error of a lock file that cannot be opened or locked. No
    /// scan was started.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn start(&self, full: bool) -> Result<StartOutcome, ScanLockError> {
        let inner = &self.inner;
        let mut handle = lock(&inner.handle);
        if inner
            .scanning
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(StartOutcome::AlreadyRunning);
        }
        // `running` clears `scanning` when it is dropped: at the end of the
        // scan, and at each return below that starts none.
        let mut running = Running {
            inner: Arc::clone(inner),
            lock: None,
        };
        if let Some(path) = &inner.settings.lock_path {
            match ScanLock::try_acquire(path)? {
                Some(lock) => running.lock = Some(lock),
                None => return Ok(StartOutcome::RunningElsewhere),
            }
        }
        inner.progress.store(0, Ordering::SeqCst);
        *handle = Some(tokio::task::spawn_blocking(move || running.scan(full)));
        Ok(StartOutcome::Started)
    }

    /// Starts a scan that is not full for a trigger that no request waits
    /// on, and logs what happened. `what` names the trigger at the start of
    /// the log line, as in `the scan at startup`.
    pub fn start_unattended(&self, what: &str) {
        match self.start(false) {
            Ok(StartOutcome::Started) => tracing::info!("{what} started"),
            Ok(StartOutcome::AlreadyRunning) => {
                tracing::info!("{what} was not started: a scan is running");
            }
            Ok(StartOutcome::RunningElsewhere) => {
                tracing::warn!("{what} was not started: another process scans the library");
            }
            Err(error) => tracing::warn!("{what} was not started: {error}"),
        }
    }

    /// Whether a scan of this controller runs, and its progress. A scan of
    /// another process is not reported.
    pub fn status(&self) -> ScanStatus {
        ScanStatus {
            scanning: self.inner.scanning.load(Ordering::SeqCst),
            count: self.inner.progress.load(Ordering::SeqCst),
        }
    }

    /// Starts the task of the periodic scans: a scan that is not full every
    /// `interval`, the first one `interval` after this call. The task ends
    /// when the token of the settings is cancelled.
    ///
    /// A time at which a scan runs, in this process or in another, starts no
    /// scan and queues none. The next scan starts one interval later.
    ///
    /// The server calls this once. A second call starts a second task.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub fn start_periodic(&self, interval: Duration) {
        let scan = self.clone();
        let task = tokio::spawn(async move {
            let cancel = &scan.inner.settings.cancel;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(interval) => {}
                }
                scan.start_unattended("the periodic scan");
            }
        });
        *lock(&self.inner.periodic) = Some(task);
    }

    /// Cancels the token of the settings and waits for the periodic scans
    /// and for the running scan to end. The scanner stops during its walk,
    /// before its next file read or between two batches, and stores
    /// `cancelled` as the error of the scan.
    pub async fn stop(&self) {
        self.inner.settings.cancel.cancel();
        // The periodic task ends first, so that it starts no scan after the
        // running one is taken below.
        let periodic = lock(&self.inner.periodic).take();
        join(periodic, "the task of the periodic scans").await;
        let scan = lock(&self.inner.handle).take();
        join(scan, "the scan").await;
    }
}

/// Locks `mutex`. A thread that panicked while it held the lock left a value
/// that is still valid, because the lock guards one assignment.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Waits for `task` to end and logs a panic of it.
async fn join(task: Option<JoinHandle<()>>, what: &str) {
    if let Some(task) = task
        && let Err(error) = task.await
    {
        tracing::error!("{what} ended with a failure: {error}");
    }
}

/// The scan that a [`ScanController`] runs. Dropping the value releases the
/// scan lock and then marks the controller as idle. A `start` that finds the
/// controller idle therefore never finds the lock held by a scan of its own
/// controller.
struct Running {
    inner: Arc<Inner>,
    lock: Option<ScanLock>,
}

impl Running {
    /// Runs the scan on the calling thread.
    fn scan(self, full: bool) {
        let settings = &self.inner.settings;
        let on_batch = settings.hooks.on_batch.clone();
        let options = ScanOptions {
            batch_size: settings.hooks.batch_size.unwrap_or(DEFAULT_BATCH_SIZE),
            on_batch: on_batch.map(|hook| Box::new(move || hook()) as Box<dyn Fn() + Send>),
            progress: Arc::clone(&self.inner.progress),
            covers_cache_dir: settings.covers_cache_dir.clone(),
        };
        let scanned = scanner::scan(
            &settings.db,
            &settings.folders,
            &settings.cancel,
            full,
            options,
        );
        // The scanner has logged a scan that finished. A scan that failed
        // has stored its error in the `scans` row when it could.
        if let Err(error) = scanned {
            tracing::error!(%error, "the scan failed");
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.lock = None;
        self.inner.scanning.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Support for the tests of this crate that run a scan through a
    //! [`ScanController`].

    use std::fs;
    use std::sync::mpsc;

    use tempfile::TempDir;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;

    /// The longest time a test waits for an event of a scan. The limit ends
    /// a test whose scan does not get there. It does not delay a passing
    /// run.
    pub(crate) const WAIT: Duration = Duration::from_secs(1);

    /// The committed audio fixtures, by file name.
    const FIXTURES: [&str; 3] = ["a.flac", "a.mp3", "a.ogg"];

    /// A music folder in a new temporary directory that holds `files`
    /// copies of the audio fixtures, at most three. The files carry no
    /// tags.
    pub(crate) fn music_folder(files: usize) -> TempDir {
        let folder = tempfile::tempdir().expect("a temporary directory");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audio");
        for name in &FIXTURES[..files] {
            fs::copy(fixtures.join(name), folder.path().join(name)).expect("copy a fixture");
        }
        folder
    }

    /// Holds a scan after its first batch until the test opens the gate.
    pub(crate) struct Gate {
        reached: UnboundedReceiver<()>,
        open: mpsc::Sender<()>,
    }

    impl Gate {
        /// The hooks of a scan that commits one file per batch and waits
        /// after its first batch, and the gate that lets it go on. A scan
        /// that commits no batch does not wait.
        pub(crate) fn hooks() -> (ScanHooks, Self) {
            let (reached_tx, reached) = unbounded_channel();
            let (open, open_rx) = mpsc::channel::<()>();
            let open_rx = Mutex::new(open_rx);
            let on_batch = move || {
                let _ = reached_tx.send(());
                // The call returns when the test drops the sender. Every
                // later call then returns at once.
                let _ = lock(&open_rx).recv();
            };
            let hooks = ScanHooks {
                batch_size: Some(1),
                on_batch: Some(Arc::new(on_batch)),
            };
            (hooks, Self { reached, open })
        }

        /// Waits until the scan has committed its first batch and waits at
        /// the gate.
        pub(crate) async fn reached(&mut self) {
            tokio::time::timeout(WAIT, self.reached.recv())
                .await
                .expect("the scan commits its first batch")
                .expect("the hook is alive");
        }

        /// Lets the scan go on. No later batch waits.
        pub(crate) fn open(self) {
            drop(self.open);
        }
    }

    /// Waits until the scan that started last has ended. Does not cancel
    /// it.
    pub(crate) async fn ended(scan: &ScanController) {
        let task = lock(&scan.inner.handle).take();
        let Some(task) = task else { return };
        tokio::time::timeout(WAIT, task)
            .await
            .expect("the scan ends")
            .expect("the scan does not panic");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::testing::{Gate, WAIT, ended, music_folder};
    use super::*;
    use crate::db::queries::scans::{self, Scan};
    use crate::db::queries::tracks;
    use crate::library::scanner::CANCELLED;

    /// A controller over a memory database and the music folder `folder`,
    /// with the lock file `scan.lock` in `data` and the cover cache
    /// `data/covers`.
    fn controller(folder: &TempDir, data: &TempDir, hooks: ScanHooks) -> (ScanController, Db) {
        let db = Db::open_memory().expect("a memory database");
        let scan = ScanController::new(ScanSettings {
            db: db.clone(),
            folders: vec![folder.path().to_path_buf()],
            covers_cache_dir: Some(data.path().join("covers")),
            lock_path: Some(lock_path(data)),
            cancel: CancellationToken::new(),
            hooks,
        });
        (scan, db)
    }

    fn lock_path(data: &TempDir) -> PathBuf {
        data.path().join("scan.lock")
    }

    fn temp_dir() -> TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    /// The rows of the `scans` table in ID order.
    async fn scan_rows(db: &Db) -> Vec<Scan> {
        db.read(scans::list).await.expect("read the scans")
    }

    async fn track_count(db: &Db) -> u64 {
        db.read(tracks::count_tracks)
            .await
            .expect("count the tracks")
    }

    fn status(scanning: bool, count: u64) -> ScanStatus {
        ScanStatus { scanning, count }
    }

    #[test]
    fn scan_lock_is_held_by_one_value_at_a_time() {
        let data = temp_dir();
        let path = lock_path(&data);

        let first = ScanLock::try_acquire(&path)
            .expect("lock the file")
            .expect("the lock is free");
        assert!(path.is_file());
        assert!(ScanLock::try_acquire(&path).expect("open").is_none());

        // The lock ends with the value. The file stays, and it is empty.
        drop(first);
        let second = ScanLock::try_acquire(&path).expect("lock the file");
        assert!(second.is_some());
        // Windows does not let another handle read a locked file, so the
        // file is read without the lock.
        drop(second);
        assert_eq!(fs::read(&path).expect("read the lock file"), b"");
    }

    #[test]
    fn scan_lock_keeps_the_content_of_an_existing_file() {
        let data = temp_dir();
        let path = lock_path(&data);
        fs::write(&path, "kept").expect("write the file");

        let lock = ScanLock::try_acquire(&path).expect("lock the file");
        assert!(lock.is_some());
        drop(lock);
        assert_eq!(fs::read(&path).expect("read the lock file"), b"kept");
    }

    #[test]
    fn scan_lock_error_names_the_file() {
        let data = temp_dir();
        let path = data.path().join("missing").join("scan.lock");

        let error = ScanLock::try_acquire(&path).expect_err("the directory does not exist");
        let prefix = format!("cannot lock {}: ", path.display());
        assert!(error.to_string().starts_with(&prefix), "{error}");
    }

    #[tokio::test]
    async fn controller_that_started_no_scan_is_idle_with_count_0() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());

        assert_eq!(scan.status(), status(false, 0));
        assert_eq!(scan_rows(&db).await, []);
        assert!(!lock_path(&data).exists());
    }

    #[tokio::test]
    async fn start_runs_one_scan_and_a_second_start_during_it_starts_none() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (hooks, mut gate) = Gate::hooks();
        let (scan, db) = controller(&folder, &data, hooks);

        // `start` returns while the scan runs.
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        assert!(scan.status().scanning);
        gate.reached().await;
        // The scan has committed its first batch of one file and waits.
        assert_eq!(scan.status(), status(true, 1));
        assert_eq!(track_count(&db).await, 1);

        // Every trigger asks through `start`. None starts a second scan,
        // full or not, and none is queued.
        for full in [false, true, false] {
            let outcome = scan.start(full).expect("start");
            assert_eq!(outcome, StartOutcome::AlreadyRunning);
        }
        scan.start_unattended("the periodic scan");
        assert_eq!(scan.status(), status(true, 1));
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].finished_at, None);

        gate.open();
        ended(&scan).await;
        assert_eq!(scan.status(), status(false, 3));
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1, "no scan was queued: {rows:?}");
        assert!(rows[0].finished_at.is_some());
        assert_eq!(rows[0].counts.added, 3);
        assert_eq!(rows[0].error, None);

        // The controller is idle again, so the next call starts a scan. Its
        // count starts at 0 and ends at the number of files.
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;
        assert_eq!(scan.status(), status(false, 3));
        assert_eq!(scan_rows(&db).await.len(), 2);
    }

    #[tokio::test]
    async fn running_scan_holds_the_scan_lock_until_it_ends() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (hooks, mut gate) = Gate::hooks();
        let (scan, _db) = controller(&folder, &data, hooks);
        let path = lock_path(&data);

        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        gate.reached().await;
        // `wn-server scan` takes the lock in the same way and finds it
        // held.
        assert!(ScanLock::try_acquire(&path).expect("open").is_none());

        gate.open();
        ended(&scan).await;
        assert!(ScanLock::try_acquire(&path).expect("open").is_some());
    }

    #[tokio::test]
    async fn start_while_another_process_holds_the_lock_starts_no_scan() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());

        // The lock of a scan of `wn-server scan`.
        let other = ScanLock::try_acquire(&lock_path(&data))
            .expect("lock the file")
            .expect("the lock is free");
        for full in [false, true] {
            let outcome = scan.start(full).expect("start");
            assert_eq!(outcome, StartOutcome::RunningElsewhere);
        }
        scan.start_unattended("the scan at startup");
        // The controller reports its own scans only, and it wrote nothing.
        assert_eq!(scan.status(), status(false, 0));
        assert_eq!(scan_rows(&db).await, []);
        assert_eq!(track_count(&db).await, 0);

        // The other scan has ended.
        drop(other);
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;
        assert_eq!(scan.status(), status(false, 3));
        assert_eq!(track_count(&db).await, 3);
    }

    #[tokio::test]
    async fn start_fails_when_the_lock_file_cannot_be_opened_and_starts_no_scan() {
        let (folder, data) = (music_folder(1), temp_dir());
        let db = Db::open_memory().expect("a memory database");
        let scan = ScanController::new(ScanSettings {
            db: db.clone(),
            folders: vec![folder.path().to_path_buf()],
            covers_cache_dir: None,
            lock_path: Some(data.path().join("missing").join("scan.lock")),
            cancel: CancellationToken::new(),
            hooks: ScanHooks::default(),
        });

        let error = scan
            .start(false)
            .expect_err("the lock file has no directory");
        assert!(error.to_string().starts_with("cannot lock "), "{error}");
        scan.start_unattended("the scan at startup");
        assert_eq!(scan.status(), status(false, 0));
        assert_eq!(scan_rows(&db).await, []);
    }

    #[tokio::test]
    async fn controller_without_a_lock_path_scans_without_a_lock_file() {
        let folder = music_folder(2);
        let db = Db::open_memory().expect("a memory database");
        let scan = ScanController::new(ScanSettings {
            db: db.clone(),
            folders: vec![folder.path().to_path_buf()],
            covers_cache_dir: None,
            lock_path: None,
            cancel: CancellationToken::new(),
            hooks: ScanHooks::default(),
        });

        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;
        assert_eq!(scan.status(), status(false, 2));
        assert_eq!(track_count(&db).await, 2);
    }

    #[tokio::test]
    async fn full_scan_reads_every_tag_again() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());

        for (full, added, changed) in [(false, 3, 0), (false, 0, 0), (true, 0, 3)] {
            assert_eq!(scan.start(full).expect("start"), StartOutcome::Started);
            ended(&scan).await;
            let row = scan_rows(&db).await.pop().expect("a scan row");
            assert_eq!(row.counts.seen, 3, "full {full}");
            assert_eq!(row.counts.added, added, "full {full}");
            assert_eq!(row.counts.changed, changed, "full {full}");
            assert_eq!(scan.status(), status(false, 3), "full {full}");
        }
    }

    #[tokio::test]
    async fn controller_is_idle_after_a_scan_that_recorded_errors() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;

        // The folder had tracks and holds no supported file now. The scan
        // records that as an error and deletes no track.
        for entry in fs::read_dir(folder.path()).expect("list the folder") {
            fs::remove_file(entry.expect("an entry").path()).expect("remove the file");
        }
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;

        assert_eq!(scan.status(), status(false, 0));
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 2);
        assert!(rows[1].finished_at.is_some());
        let error = rows[1].error.as_deref().expect("the scan has an error");
        assert!(error.contains("has no supported file"), "{error}");
        assert_eq!(track_count(&db).await, 3);

        // The next call starts a scan.
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;
        assert_eq!(scan_rows(&db).await.len(), 3);
    }

    #[tokio::test]
    async fn controller_is_idle_after_a_scan_that_the_database_ended() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());
        db.write(|conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER no_insert BEFORE INSERT ON tracks
                 BEGIN SELECT RAISE(ABORT, 'a track row was inserted'); END;",
            )?;
            Ok(())
        })
        .await
        .expect("create the trigger");

        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;

        assert!(!scan.status().scanning);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].finished_at.is_some());
        let error = rows[0].error.as_deref().expect("the scan has an error");
        assert!(error.contains("a track row was inserted"), "{error}");
        assert_eq!(track_count(&db).await, 0);
        // The lock is free, and the next call starts a scan.
        assert!(
            ScanLock::try_acquire(&lock_path(&data))
                .expect("open")
                .is_some()
        );
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;
        assert_eq!(scan_rows(&db).await.len(), 2);
    }

    #[tokio::test]
    async fn controller_is_idle_and_the_lock_is_free_after_a_scan_that_panicked() {
        let (folder, data) = (music_folder(3), temp_dir());
        let hooks = ScanHooks {
            batch_size: Some(1),
            on_batch: Some(Arc::new(|| panic!("the batch hook of the test panics"))),
        };
        let (scan, db) = controller(&folder, &data, hooks);

        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        let task = lock(&scan.inner.handle).take().expect("the scan task");
        let ended = tokio::time::timeout(WAIT, task)
            .await
            .expect("the scan ends");
        assert!(ended.expect_err("the scan panicked").is_panic());

        assert!(!scan.status().scanning);
        assert!(
            ScanLock::try_acquire(&lock_path(&data))
                .expect("open")
                .is_some()
        );
        // The batch before the panic is committed, and its scan row has no
        // end time.
        assert_eq!(track_count(&db).await, 1);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].finished_at, None);
        assert_eq!(db.read(scans::last_finished).await.expect("read"), None);
    }

    #[tokio::test]
    async fn stop_cancels_the_running_scan_and_waits_for_it() {
        let (folder, data) = (music_folder(3), temp_dir());
        let (hooks, mut gate) = Gate::hooks();
        let (scan, db) = controller(&folder, &data, hooks);

        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        gate.reached().await;
        // The scan waits in the hook of its first batch. `stop` cancels it,
        // and the scan sees that before its second batch.
        let stopping = tokio::spawn({
            let scan = scan.clone();
            async move { scan.stop().await }
        });
        // The task runs up to its wait for the scan.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(scan.inner.settings.cancel.is_cancelled());
        assert!(!stopping.is_finished());
        gate.open();
        tokio::time::timeout(WAIT, stopping)
            .await
            .expect("stop returns")
            .expect("stop does not panic");

        assert!(!scan.status().scanning);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].finished_at.is_some());
        assert_eq!(rows[0].error.as_deref(), Some(CANCELLED));
        assert_eq!(rows[0].counts.added, 1);
        assert_eq!(track_count(&db).await, 1);
    }

    #[tokio::test]
    async fn stop_without_a_scan_returns() {
        let (folder, data) = (music_folder(0), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());
        scan.stop().await;
        assert_eq!(scan.status(), status(false, 0));
        assert_eq!(scan_rows(&db).await, []);
    }

    #[tokio::test]
    async fn periodic_scans_start_after_one_interval_and_not_before() {
        let (folder, data) = (music_folder(1), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());

        scan.start_periodic(Duration::from_secs(3600));
        // The task runs up to its first wait.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(scan.status(), status(false, 0));
        assert_eq!(scan_rows(&db).await, []);

        // `stop` ends the task without a scan.
        tokio::time::timeout(WAIT, scan.stop())
            .await
            .expect("stop returns");
        assert_eq!(scan_rows(&db).await, []);
    }

    #[tokio::test]
    async fn periodic_scans_run_at_the_interval_until_the_controller_stops() {
        let (folder, data) = (music_folder(1), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());

        scan.start_periodic(Duration::from_millis(20));
        let two_scans = async {
            while scan_rows(&db).await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, two_scans)
            .await
            .expect("a second scan row appears");

        // `stop` returns when the task of the periodic scans and the scan
        // that runs have ended, so no scan starts after it.
        scan.stop().await;
        assert!(!scan.status().scanning);
        let rows = scan_rows(&db).await;
        assert!(rows.iter().all(|row| row.finished_at.is_some()), "{rows:?}");
        // The first scan ended before the second one started, and it added
        // the file. The scans after it are not full: they read no tag again.
        assert_eq!(rows[0].counts.added, 1);
        assert_eq!(rows[0].error, None);
        for row in &rows[1..] {
            assert_eq!((row.counts.added, row.counts.changed), (0, 0), "{row:?}");
        }
        assert_eq!(track_count(&db).await, 1);
    }

    #[tokio::test]
    async fn scan_of_the_controller_removes_the_stale_files_of_the_cover_cache() {
        let (folder, data) = (music_folder(1), temp_dir());
        let (scan, db) = controller(&folder, &data, ScanHooks::default());
        let cover = folder.path().join("cover.jpg");
        fs::write(&cover, "an image").expect("write the cover");
        assert_eq!(scan.start(false).expect("start"), StartOutcome::Started);
        ended(&scan).await;

        let (album_id, cover_mtime): (String, i64) = db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT id, cover_mtime FROM albums", [], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?,
                )
            })
            .await
            .expect("the album of the file");
        let cache = data.path().join("covers");
        fs::create_dir_all(&cache).expect("create the cache");

        // The cover file is replaced before each scan, and the cache holds a
        // resized cover of the image before it. The scan that is not full
        // and the full scan both remove it.
        for (round, full) in [(1, false), (2, true)] {
            let stale = cache.join(format!("{album_id}-64-{}.jpg", cover_mtime + round - 1));
            fs::write(&stale, "a resized cover").expect("write the cached cover");
            fs::write(&cover, format!("image {round}")).expect("replace the cover");
            let modified = std::time::UNIX_EPOCH
                + Duration::from_secs(u64::try_from(cover_mtime + round).expect("a time"));
            let file = fs::File::options()
                .write(true)
                .open(&cover)
                .expect("open the cover");
            file.set_modified(modified).expect("set the time");

            assert_eq!(scan.start(full).expect("start"), StartOutcome::Started);
            ended(&scan).await;
            assert!(!stale.exists(), "full {full}: {}", stale.display());
        }
    }
}
