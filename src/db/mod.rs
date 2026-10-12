//! SQLite access: the `Db` handle, connection pragmas and migrations
//! (`docs/design.md` section 3.4).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags, TransactionBehavior};
use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard};

// The list of the query modules is in `queries/mod.rs`, which holds nothing
// else. A change that adds a query module edits that file and not this one.
pub mod queries;

/// One migration: a file in `migrations/`.
#[derive(Debug, Clone, Copy)]
struct Migration {
    /// The name of the file. The error of a migration that fails names it.
    name: &'static str,
    /// The statements of the file.
    sql: &'static str,
}

/// Returns the migration in the file `migrations/<name>`.
macro_rules! migration {
    ($name:literal) => {
        Migration {
            name: $name,
            sql: include_str!(concat!("../../migrations/", $name)),
        }
    };
}

/// The migrations in order. Migration `n` (counted from 1) moves the schema
/// from version `n - 1` to version `n`. `PRAGMA user_version` holds the
/// version of a database.
///
/// A schema change is a new file with the next number and a new line here.
/// A file that is on `main` is not edited: databases exist that applied it,
/// and they do not apply it again (`docs/design.md` section 3.4).
const MIGRATIONS: &[Migration] = &[
    migration!("0001_init.sql"),
    migration!("0002_tracks_album_artist_name.sql"),
    migration!("0003_scans_milliseconds_and_drop_has_lyrics.sql"),
];

/// The number of read connections of a file database.
const READERS: usize = 4;

/// How long a connection waits for a lock that another connection holds.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// The time limit of a read of [`Db::read`] (`docs/design.md` section 3.4).
///
/// The slowest statement that was found within the bounds of the request
/// parameters is the track statement of a search with 32 terms, 31 of which
/// every track matches. It takes 1.4 s on a library of 100 000 tracks. A
/// search of one to three terms takes 50 to 110 ms there. The limit is 3.6
/// times the first value, so a machine that is three times slower answers
/// that request.
pub const READ_LIMIT: Duration = Duration::from_secs(5);

/// How long the timer thread waits before it interrupts the connection of a
/// read that has passed its limit again.
const INTERRUPT_REPEAT: Duration = Duration::from_millis(50);

/// The reads of [`Db::read`] that run now, of every handle of the process.
static READ_TIMERS: ReadTimers = ReadTimers::new();

/// An error from the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot create the database file {}: {source}", .path.display())]
    CreateFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "the database has schema version {found}; this build supports versions 0 to {supported}"
    )]
    UnsupportedSchema { found: i64, supported: i64 },
    /// A migration failed. `name` is the file of the migration, and
    /// `version` is the schema version that the database has after the
    /// failure.
    #[error(
        "migration {name} failed and the database was left at schema version {version}: {source}"
    )]
    Migration {
        name: &'static str,
        version: i64,
        source: rusqlite::Error,
    },
    #[error("the checkpoint did not finish because another connection uses the database")]
    CheckpointBusy,
    #[error("the runtime shut down before the database task ran")]
    Cancelled,
    #[error("a statement of a read was interrupted at the time limit of {limit:?}")]
    ReadLimit { limit: Duration },
    #[error("the path is not valid Unicode: {}", .0.display())]
    NonUnicodePath(PathBuf),
}

/// The result type of the database layer.
pub type Result<T> = std::result::Result<T, DbError>;

/// A handle to the library database. Clones share the same connections.
///
/// A file database has one write connection and four read-only connections,
/// so reads proceed while a write runs. An in-memory database has one
/// connection that serves reads and writes.
///
/// The connections close when the last clone is dropped.
#[derive(Debug, Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    writer: Arc<Mutex<Connection>>,
    /// Empty for an in-memory database.
    readers: Vec<Arc<Mutex<Connection>>>,
    /// The reader at which the next read starts to look for a free connection.
    next_reader: AtomicUsize,
    /// The time limit of a read of [`Db::read`], in nanoseconds.
    read_limit_nanos: AtomicU64,
}

impl Db {
    /// Opens the database file at `path`, creates it when it does not exist,
    /// and applies the migrations it lacks.
    ///
    /// On Unix a new file is created with mode 0600.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::UnsupportedSchema`] for a database whose schema
    /// version is negative or greater than the number of migrations in this
    /// build, which is the case for a database that a newer build has
    /// migrated. The database keeps its schema version and its rows.
    ///
    /// Returns [`DbError::Migration`] for a migration that fails. The error
    /// names the file of the migration and the schema version that the
    /// database was left at. The migration is rolled back, so the database
    /// has the schema and the rows of that version, and the migrations
    /// before the one that failed stay applied.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        // File modes exist on Unix only. On Windows the ACL of the user
        // profile protects the data directory.
        #[cfg(unix)]
        create_owner_only(path).map_err(|source| DbError::CreateFile {
            path: path.to_path_buf(),
            source,
        })?;

        let mut writer = Connection::open(path)?;
        configure(&writer)?;
        let journal_mode: String =
            writer.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if journal_mode != "wal" {
            tracing::warn!(
                path = %path.display(),
                journal_mode,
                "the database is not in WAL mode; reads wait for writes"
            );
        }
        migrate(&mut writer)?;

        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let reader = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            configure(&reader)?;
            readers.push(Arc::new(Mutex::new(reader)));
        }
        Ok(Db::from_connections(writer, readers))
    }

    /// Opens a new in-memory database with the full schema.
    pub fn open_memory() -> Result<Db> {
        let mut connection = Connection::open_in_memory()?;
        configure(&connection)?;
        migrate(&mut connection)?;
        Ok(Db::from_connections(connection, Vec::new()))
    }

    fn from_connections(writer: Connection, readers: Vec<Arc<Mutex<Connection>>>) -> Db {
        Db {
            inner: Arc::new(Inner {
                writer: Arc::new(Mutex::new(writer)),
                readers,
                next_reader: AtomicUsize::new(0),
                read_limit_nanos: AtomicU64::new(nanos(READ_LIMIT)),
            }),
        }
    }

    /// Sets the time limit of the reads of [`Db::read`] that start after the
    /// call, for this handle and its clones. A handle starts with
    /// [`READ_LIMIT`], and the server does not change it. A test sets a
    /// limit of milliseconds.
    pub fn set_read_limit(&self, limit: Duration) {
        self.inner
            .read_limit_nanos
            .store(nanos(limit), Ordering::Relaxed);
    }

    /// Runs `f` with a read connection on a blocking thread.
    ///
    /// The read has a time limit, which is [`READ_LIMIT`] unless
    /// [`Db::set_read_limit`] changed it. The time counts from the moment
    /// `f` starts, so the wait for a free connection is no part of it. When
    /// the time ends, the statement that `f` runs is interrupted, and so is
    /// every statement that `f` starts after it. The read returns
    /// [`DbError::ReadLimit`] when `f` returns the error of such a
    /// statement. The connection then serves the next read.
    ///
    /// `f` runs to completion even when the returned future is dropped, and
    /// the limit holds for it then as well. A panic in `f` resumes in the
    /// caller.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = self.lock_reader().await;
        let limit = Duration::from_nanos(self.inner.read_limit_nanos.load(Ordering::Relaxed));
        run_blocking(move || {
            // The connection is a local that is declared before the timer.
            // Locals are dropped in the reverse order, also when `f` panics,
            // so the timer has ended when the connection is released.
            let connection = connection;
            let mut timer = ReadTimer::start(&connection, limit);
            let result = f(&connection);
            let interrupted = timer.end();
            match result {
                Err(DbError::Sqlite(error)) if interrupted && is_interrupt(&error) => {
                    Err(DbError::ReadLimit { limit })
                }
                result => result,
            }
        })
        .await
    }

    /// Runs `f` with the write connection on a blocking thread. Writes run
    /// one at a time. A write has no time limit.
    ///
    /// `f` runs to completion even when the returned future is dropped. A
    /// panic in `f` resumes in the caller.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let mut connection = Arc::clone(&self.inner.writer).lock_owned().await;
        run_blocking(move || f(&mut connection)).await
    }

    /// Runs `f` as [`Db::read`] does, inside one read transaction, so every
    /// statement of `f` reads one state of the database. The handler of a
    /// method whose answer takes several statements reads with it.
    pub async fn read_snapshot<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.read(move |conn| {
            // The transaction is deferred: its first statement fixes the state
            // that the later ones read. It writes nothing, so dropping it ends
            // it without an effect on the database.
            let snapshot = conn.unchecked_transaction()?;
            f(&snapshot)
        })
        .await
    }

    /// Runs `f` as [`Db::write`] does, with the time of the write in whole
    /// Unix seconds as its second argument.
    ///
    /// The time is read once the write connection is held. While the clock
    /// runs forward, the write that runs later therefore does not get the
    /// earlier time.
    pub async fn write_at<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection, i64) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.write(move |conn| f(conn, unix_now())).await
    }

    /// Runs `f` with a read connection on the calling thread and blocks the
    /// thread while it waits for the connection. The read has no time limit:
    /// the commands of the CLI use this method, and no request does.
    ///
    /// # Panics
    ///
    /// Panics when called from an async task. Async callers use
    /// [`Db::read`].
    pub fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self.blocking_lock_reader();
        f(&connection)
    }

    /// Runs `f` with the write connection on the calling thread and blocks
    /// the thread while it waits for the connection. A write has no time
    /// limit.
    ///
    /// # Panics
    ///
    /// Panics when called from an async task. Async callers use
    /// [`Db::write`].
    pub fn with_writer<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self.inner.writer.blocking_lock();
        f(&mut connection)
    }

    /// Waits for running reads and writes of this database handle and its
    /// clones, then moves the write-ahead log into the database file and
    /// truncates the log with `PRAGMA wal_checkpoint(TRUNCATE)`.
    ///
    /// Returns [`DbError::CheckpointBusy`] when a connection outside this
    /// handle, such as another process, keeps the checkpoint from finishing.
    pub async fn close(self) -> Result<()> {
        // A read in progress pins the log, so every reader is held during the
        // checkpoint. No other code holds two connections, so the order of
        // these locks cannot deadlock.
        let mut readers = Vec::with_capacity(self.inner.readers.len());
        for reader in &self.inner.readers {
            readers.push(Arc::clone(reader).lock_owned().await);
        }
        let writer = Arc::clone(&self.inner.writer).lock_owned().await;
        run_blocking(move || {
            let _readers = readers;
            checkpoint(&writer)
        })
        .await
    }

    /// Returns a free read connection, or waits for one when all are in use.
    async fn lock_reader(&self) -> OwnedMutexGuard<Connection> {
        let readers = &self.inner.readers;
        if readers.is_empty() {
            return Arc::clone(&self.inner.writer).lock_owned().await;
        }
        let start = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % readers.len();
        for offset in 0..readers.len() {
            let reader = &readers[(start + offset) % readers.len()];
            if let Ok(connection) = Arc::clone(reader).try_lock_owned() {
                return connection;
            }
        }
        Arc::clone(&readers[start]).lock_owned().await
    }

    /// The blocking form of [`Db::lock_reader`].
    fn blocking_lock_reader(&self) -> MutexGuard<'_, Connection> {
        let readers = &self.inner.readers;
        if readers.is_empty() {
            return self.inner.writer.blocking_lock();
        }
        let start = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % readers.len();
        for offset in 0..readers.len() {
            if let Ok(connection) = readers[(start + offset) % readers.len()].try_lock() {
                return connection;
            }
        }
        readers[start].blocking_lock()
    }
}

/// The time now in whole Unix seconds.
fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        // The clock is set to a time before 1970.
        Err(error) => i64::try_from(error.duration().as_secs()).map_or(i64::MIN, |before| -before),
    }
}

/// The time limits of the reads of [`Db::read`]: the reads that run now and
/// the thread that interrupts the connection of a read when the read has run
/// for its limit, and again every [`INTERRUPT_REPEAT`] until the read ends.
///
/// The thread starts with the first read of the process and serves every
/// handle. It is no part of the runtime, so it keeps the limit of a read
/// whose future was dropped and of a read that runs while the runtime shuts
/// down.
///
/// SQLite applies an interrupt to the statements that run when it arrives,
/// whichever closure started them. The thread therefore interrupts under the
/// lock of the list of the reads, and a read takes that lock to leave the
/// list. No interrupt is issued for a read after [`ReadTimers::remove`] has
/// returned, so none reaches a statement of the next user of the connection.
///
/// An interrupt can arrive while the read runs no statement: between two
/// statements, or after the last one. It stops nothing. SQLite keeps a flag
/// on the connection and clears it when the next statement is prepared or
/// started, so that statement runs; a test holds the bundled SQLite to
/// this. The thread repeats the interrupt for this case: a read whose limit
/// ends between two of its statements would otherwise run the later ones
/// without a limit.
struct ReadTimers {
    state: std::sync::Mutex<TimersState>,
    /// Wakes the thread before the time it set for itself.
    wake: Condvar,
}

struct TimersState {
    reads: Vec<TimedRead>,
    /// The ID of the read that is added next.
    next_id: u64,
    /// Whether the thread was started.
    started: bool,
    /// The time at which the thread wakes without a signal. `None` while it
    /// waits for a signal only.
    wakes_at: Option<Instant>,
}

/// A read that runs and the time of its next interrupt.
struct TimedRead {
    id: u64,
    due: Instant,
    interrupt: InterruptHandle,
    /// The thread has interrupted the connection of the read.
    interrupted: bool,
}

impl ReadTimers {
    const fn new() -> Self {
        ReadTimers {
            state: std::sync::Mutex::new(TimersState {
                reads: Vec::new(),
                next_id: 0,
                started: false,
                wakes_at: None,
            }),
            wake: Condvar::new(),
        }
    }

    /// Adds a read that starts now on the connection of `interrupt` and
    /// returns its ID. Returns `None` for a read that gets no limit: its
    /// limit ends at no time that the clock can hold, or the thread cannot
    /// be started.
    fn add(&'static self, interrupt: InterruptHandle, limit: Duration) -> Option<u64> {
        let due = Instant::now().checked_add(limit)?;
        let mut state = self.lock();
        if !state.started {
            let thread = std::thread::Builder::new().name("db-read-limit".to_owned());
            let run = move || {
                let _ended = ThreadEnd(self);
                self.run();
            };
            match thread.spawn(run) {
                Ok(_) => state.started = true,
                Err(error) => {
                    drop(state);
                    tracing::error!(%error, "cannot start the thread of the read limit");
                    return None;
                }
            }
        }
        let id = state.next_id;
        state.next_id += 1;
        state.reads.push(TimedRead {
            id,
            due,
            interrupt,
            interrupted: false,
        });
        // The thread looks at the list again at `wakes_at`. A read whose
        // limit ends before that needs the signal.
        if state.wakes_at.is_none_or(|wakes_at| due < wakes_at) {
            self.wake.notify_one();
        }
        Some(id)
    }

    /// Removes the read `id` and returns whether its connection was
    /// interrupted.
    fn remove(&self, id: u64) -> bool {
        let mut state = self.lock();
        match state.reads.iter().position(|read| read.id == id) {
            Some(index) => state.reads.swap_remove(index).interrupted,
            None => false,
        }
    }

    /// The thread: interrupts the reads whose time has come and sleeps until
    /// the time of the next one. It returns only by a panic.
    fn run(&self) {
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            let mut next: Option<Instant> = None;
            for read in &mut state.reads {
                if read.due <= now {
                    read.interrupt.interrupt();
                    read.interrupted = true;
                    read.due = now + INTERRUPT_REPEAT;
                }
                next = Some(next.map_or(read.due, |next| next.min(read.due)));
            }
            state.wakes_at = next;
            // A wait releases the lock, and it returns early for a signal
            // and at times for no reason. The list is read again after it.
            state = match next {
                Some(next) => {
                    let (state, _) = self
                        .wake
                        .wait_timeout(state, next.saturating_duration_since(now))
                        .unwrap_or_else(PoisonError::into_inner);
                    state
                }
                None => self
                    .wake
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
            };
        }
    }

    /// Locks the list. A poisoned lock is taken as it is, so that the end of
    /// a read never panics.
    fn lock(&self) -> std::sync::MutexGuard<'_, TimersState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Records that the thread of a [`ReadTimers`] has ended. The thread drops
/// this value when a panic ends it. The reads in the list then have no
/// limit until the next read starts a new thread, which keeps the limits of
/// all of them.
struct ThreadEnd(&'static ReadTimers);

impl Drop for ThreadEnd {
    fn drop(&mut self) {
        self.0.lock().started = false;
        tracing::error!("the thread of the read limit ended; the next read starts it again");
    }
}

/// The time limit of one read of [`Db::read`]: the read is in the list of
/// [`READ_TIMERS`] while this value lives.
struct ReadTimer<'c> {
    connection: &'c Connection,
    /// The ID of the read in the list. `None` when the read has no limit and
    /// when it has ended.
    id: Option<u64>,
}

impl<'c> ReadTimer<'c> {
    /// Starts the time of a read that is about to run on `connection`. The
    /// caller holds the connection until the timer has ended.
    fn start(connection: &'c Connection, limit: Duration) -> Self {
        let id = READ_TIMERS.add(connection.get_interrupt_handle(), limit);
        ReadTimer { connection, id }
    }

    /// Ends the timer and returns whether the connection was interrupted.
    /// After an interrupt it also ends a transaction that the read left
    /// open.
    fn end(&mut self) -> bool {
        let interrupted = self.id.take().is_some_and(|id| READ_TIMERS.remove(id));
        // A handler reads inside a transaction, which rusqlite rolls back
        // when the handler drops it. An interrupt can stop that `ROLLBACK`,
        // and rusqlite ignores the error. The transaction would stay open on
        // the connection: the next handler could start none, and a
        // checkpoint could not truncate the write-ahead log. No interrupt
        // arrives from here on, so this `ROLLBACK` runs.
        if interrupted
            && !self.connection.is_autocommit()
            && let Err(error) = self.connection.execute_batch("ROLLBACK")
        {
            tracing::error!(%error, "cannot end the transaction of an interrupted read");
        }
        interrupted
    }
}

impl Drop for ReadTimer<'_> {
    /// Ends the timer of a read whose closure panicked.
    fn drop(&mut self) {
        self.end();
    }
}

/// Whether `error` is the error of a statement that an interrupt stopped.
fn is_interrupt(error: &rusqlite::Error) -> bool {
    error.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

/// The whole nanoseconds of `duration`, or the largest number that fits.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Runs `f` on a blocking thread and returns its result.
async fn run_blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => Err(DbError::Cancelled),
        },
    }
}

/// Creates the file at `path` with mode 0600 when it does not exist. An
/// existing file keeps its mode.
#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match created {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Sets the pragmas that every connection uses. The journal mode belongs to
/// the database file and is set once, on the write connection.
fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
}

/// Applies every migration of this build that the database lacks
/// ([`MIGRATIONS`]).
fn migrate(connection: &mut Connection) -> Result<()> {
    migrate_with(connection, MIGRATIONS)
}

/// Applies every migration of `migrations` that the database lacks, each in
/// one transaction. `migrations` is the list of one build: [`MIGRATIONS`],
/// or in a test the list of an earlier build or a list with a migration that
/// fails.
///
/// A migration that fails is rolled back, and the function returns
/// [`DbError::Migration`]: the database keeps the version and the content
/// that it had before that migration. A database whose version is not one
/// that `migrations` leads to or starts from is an error, and nothing is
/// applied to it.
fn migrate_with(connection: &mut Connection, migrations: &[Migration]) -> Result<()> {
    loop {
        let version = user_version(connection)?;
        let Some(migration) = pending_migration(version, migrations)? else {
            return Ok(());
        };
        if let Err(source) = apply_migration(connection, version, migration.sql) {
            // The transaction of the migration is rolled back. The version
            // is read again, so that the error states what the database
            // holds now. When it cannot be read, the error names the version
            // from before the migration, which is the one that the rollback
            // leaves.
            return Err(DbError::Migration {
                name: migration.name,
                version: user_version(connection).unwrap_or(version),
                source,
            });
        }
    }
}

/// Runs `sql` and sets the schema version to `version + 1` in one
/// transaction, when the database is still at `version`.
///
/// Another process can apply the same migration after the caller read the
/// version. The version is read again under the write lock, and nothing runs
/// when it has changed.
fn apply_migration(connection: &mut Connection, version: i64, sql: &str) -> rusqlite::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if user_version(&transaction)? == version {
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", version + 1)?;
        transaction.commit()?;
    }
    Ok(())
}

/// Returns the migration of `migrations` that moves a database from
/// `version` to the next version, or `None` when `version` is the latest
/// version of `migrations`.
fn pending_migration(version: i64, migrations: &[Migration]) -> Result<Option<Migration>> {
    match usize::try_from(version) {
        Ok(applied) if applied <= migrations.len() => Ok(migrations.get(applied).copied()),
        _ => Err(DbError::UnsupportedSchema {
            found: version,
            // The number of migrations is small, so the cast keeps its value.
            supported: migrations.len() as i64,
        }),
    }
}

fn user_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Runs a truncating checkpoint. The pragma returns one row whose first
/// column is 1 when another connection blocked the checkpoint.
fn checkpoint(connection: &Connection) -> Result<()> {
    let busy: i64 =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(DbError::CheckpointBusy);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::Instant;

    use rusqlite::params;
    use rusqlite::types::Value;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::*;

    const TABLES: [&str; 11] = [
        "albums",
        "annotations",
        "artists",
        "folders",
        "play_queue",
        "play_queue_tracks",
        "playlist_tracks",
        "playlists",
        "scans",
        "tracks",
        "users",
    ];

    const INDEXES: [&str; 13] = [
        "albums_created",
        "albums_genre",
        "albums_key",
        "albums_year",
        "annotations_starred",
        "play_queue_tracks_track",
        "playlist_tracks_track",
        "scans_changed",
        "tracks_album",
        "tracks_artist",
        "tracks_fingerprint",
        "tracks_genre",
        "tracks_title_lc",
    ];

    const ARTIST_ID: &str = "ar-00000000000000000000000000000001";
    const ALBUM_ID: &str = "al-00000000000000000000000000000002";
    const TRACK_ID: &str = "tr-00000000000000000000000000000003";

    fn temp_db() -> (TempDir, PathBuf, Db) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let db = Db::open(&path).unwrap();
        (dir, path, db)
    }

    /// Returns an in-memory database and a temp-file database. The directory
    /// guard keeps the file for the life of the test.
    fn both_dbs() -> (TempDir, [Db; 2]) {
        let (dir, _, file_db) = temp_db();
        (dir, [Db::open_memory().unwrap(), file_db])
    }

    /// Returns the path of the file that SQLite keeps next to the database
    /// file under the same name plus `suffix`.
    fn side_file(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    /// A limit that a statement of [`MANY_ROWS`] rows passes on every machine.
    const SHORT_LIMIT: Duration = Duration::from_millis(20);

    /// The rows of a statement that a test expects to be interrupted. The
    /// statement runs for seconds, which no test waits for: a read that is
    /// not interrupted fails its test with the count.
    const MANY_ROWS: i64 = 50_000_000;

    /// Counts the numbers from 1 to `rows` with one statement and returns
    /// their number. The statement reads no table and holds one row in
    /// memory.
    fn count_to(connection: &Connection, rows: i64) -> Result<i64> {
        Ok(connection.query_row(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
             SELECT count(*) FROM n",
            [rows],
            |row| row.get(0),
        )?)
    }

    /// Runs statements on `connection` one after the other for `time` at
    /// least. An interrupt that arrives in that time stops the statement
    /// that runs then.
    fn count_for(connection: &Connection, time: Duration) -> Result<()> {
        let started = Instant::now();
        while started.elapsed() < time {
            assert_eq!(count_to(connection, 20_000)?, 20_000);
        }
        Ok(())
    }

    /// Locks every read connection of `db` but the first, so that every read
    /// of the handle runs on one connection while the guards live. A memory
    /// database has one connection and gives no guard.
    fn hold_all_readers_but_one(db: &Db) -> Vec<MutexGuard<'_, Connection>> {
        db.inner
            .readers
            .iter()
            .skip(1)
            .map(|reader| reader.try_lock().expect("a free reader"))
            .collect()
    }

    fn assert_read_limit(error: &DbError, expected: Duration) {
        assert!(
            matches!(error, DbError::ReadLimit { limit } if *limit == expected),
            "{error}"
        );
    }

    fn assert_send_and_sync<T: Send + Sync>() {}

    #[test]
    fn db_handle_is_send_and_sync() {
        assert_send_and_sync::<Db>();
    }

    /// Returns the names of the schema objects of one type that the
    /// migrations created, in name order.
    fn schema_names(connection: &Connection, object_type: &str) -> Result<Vec<String>> {
        let mut statement = connection.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = ?1 AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names = statement
            .query_map([object_type], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    fn pragma_i64(connection: &Connection, name: &str) -> i64 {
        connection
            .pragma_query_value(None, name, |row| row.get(0))
            .unwrap()
    }

    fn insert_folder(connection: &Connection, path: &str) -> Result<()> {
        connection.execute("INSERT INTO folders (path, name) VALUES (?1, ?1)", [path])?;
        Ok(())
    }

    fn count_folders(connection: &Connection) -> Result<i64> {
        Ok(connection.query_row("SELECT count(*) FROM folders", [], |row| row.get(0))?)
    }

    fn insert_track(connection: &Connection) -> rusqlite::Result<usize> {
        connection.execute(
            "INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc,
               album_id, artist_id, artist_name, album_name, duration_ms, size, suffix,
               content_type, lossless, mtime, created_at, updated_at)
             VALUES (?1, 1, 'a/b/01.flac', '1:00', 'T', 't', ?2, ?3, 'A', 'B', 1000, 1,
               'flac', 'audio/flac', 1, 0, 0, 0)",
            params![TRACK_ID, ALBUM_ID, ARTIST_ID],
        )
    }

    /// Returns the schema version of a database that has every migration.
    fn latest() -> i64 {
        i64::try_from(MIGRATIONS.len()).unwrap()
    }

    #[test]
    fn open_creates_every_table_and_index_and_sets_the_latest_user_version() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let tables = db.with_reader(|c| schema_names(c, "table")).unwrap();
            assert_eq!(tables, TABLES);
            let indexes = db.with_reader(|c| schema_names(c, "index")).unwrap();
            assert_eq!(indexes, INDEXES);
            let version = db.with_reader(|c| Ok(user_version(c)?)).unwrap();
            assert_eq!(version, latest());
        }
    }

    #[tokio::test]
    async fn reopening_does_not_run_migrations_again() {
        let (_dir, path, db) = temp_db();
        db.write(|c| insert_folder(c, "/music")).await.unwrap();
        let schema_changes = db
            .read(|c| Ok(pragma_i64(c, "schema_version")))
            .await
            .unwrap();
        db.close().await.unwrap();

        let db = Db::open(&path).unwrap();
        let (version, schema_changes_after, folders) = db
            .read(|c| {
                Ok((
                    user_version(c)?,
                    pragma_i64(c, "schema_version"),
                    count_folders(c)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(version, latest());
        // SQLite increments `schema_version` on every schema change.
        assert_eq!(schema_changes_after, schema_changes);
        assert_eq!(folders, 1);
    }

    #[test]
    fn migrate_applies_nothing_to_a_migrated_database() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let schema_changes = pragma_i64(&connection, "schema_version");
        migrate(&mut connection).unwrap();
        assert_eq!(pragma_i64(&connection, "schema_version"), schema_changes);
        assert_eq!(user_version(&connection).unwrap(), latest());
    }

    #[test]
    fn a_migration_that_another_connection_applied_does_not_run_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let mut first = Connection::open(&path).unwrap();
        let mut second = Connection::open(&path).unwrap();
        migrate(&mut first).unwrap();
        let schema_changes = pragma_i64(&second, "schema_version");

        // The second connection read each version before the first one
        // applied the migration of that version.
        for (version, migration) in (0..).zip(MIGRATIONS) {
            apply_migration(&mut second, version, migration.sql).unwrap();
        }
        assert_eq!(pragma_i64(&second, "schema_version"), schema_changes);
        assert_eq!(user_version(&second).unwrap(), latest());
    }

    #[test]
    fn a_failed_migration_leaves_no_change() {
        let mut connection = Connection::open_in_memory().unwrap();
        let sql = "CREATE TABLE first (x); CREATE TABLE first (x);";
        let error = apply_migration(&mut connection, 0, sql).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(schema_names(&connection, "table").unwrap(), [""; 0]);
        assert_eq!(user_version(&connection).unwrap(), 0);
    }

    #[test]
    fn open_refuses_a_schema_version_it_does_not_know_and_leaves_the_database_as_it_is() {
        // The version after the latest one is the version of a database
        // that a build with one more migration has migrated.
        for found in [latest() + 1, -1] {
            let (_dir, path, db) = temp_db();
            db.with_writer(|c| insert_folder(c, "/music")).unwrap();
            drop(db);
            let outside = Connection::open(&path).unwrap();
            outside.pragma_update(None, "user_version", found).unwrap();
            let schema_changes = pragma_i64(&outside, "schema_version");

            let error = Db::open(&path).unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::UnsupportedSchema { found: f, supported }
                        if f == found && supported == latest()
                ),
                "{error}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "the database has schema version {found}; this build supports versions 0 \
                     to {}",
                    latest()
                )
            );
            assert_eq!(user_version(&outside).unwrap(), found);
            assert_eq!(pragma_i64(&outside, "schema_version"), schema_changes);
            assert_eq!(count_folders(&outside).unwrap(), 1);
        }
    }

    /// Rows for every table of schema version 1, in the columns of that
    /// version: two users, two folders, two artists, two albums, three
    /// tracks (one with every value, one with few values and a placeholder
    /// path, one in the second folder), stars, ratings and play counts on a
    /// track, an album and an artist, two playlists, a play queue and two
    /// scans.
    ///
    /// Schema version 1 does not change, so this text does not change. Every
    /// later migration is tested against these rows.
    const V1_ROWS: &str = "
        INSERT INTO users (id, name, role, password, created_at, updated_at) VALUES
          (1, 'owner', 'owner', 'sesame', 100, 200),
          (2, 'guest', 'guest', 'secret', 300, 300);
        INSERT INTO folders (id, path, name) VALUES
          (1, '/music', 'music'),
          (2, '/more', 'more');
        INSERT INTO artists (id, key, name, sort_name, album_count) VALUES
          ('ar-000000000000000000000000000000a1', 'aurora field', 'AURORA FIELD',
           'AURORA FIELD', 1),
          ('ar-000000000000000000000000000000a2', 'various artists', 'Various Artists',
           'Various Artists', 1);
        INSERT INTO albums (id, artist_id, key, name, artist_name, year, genre, song_count,
          duration_ms, compilation, cover_source, cover_mtime, created_at) VALUES
          ('al-000000000000000000000000000000b1', 'ar-000000000000000000000000000000a1',
           'glass harbour', 'Glass Harbour', 'AURORA FIELD', 2019, 'Electronic', 2, 1000, 0,
           '/music/Aurora Field/Glass Harbour/cover.jpg', 400, 500),
          ('al-000000000000000000000000000000b2', 'ar-000000000000000000000000000000a2',
           'summer sampler', 'Summer Sampler', 'Various Artists', NULL, NULL, 1, 1234, 1,
           NULL, NULL, 600);
        INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc, album_id,
          artist_id, artist_name, album_name, track_no, disc_no, year, genre, duration_ms,
          bitrate_kbps, sample_rate, bit_depth, channels, size, suffix, content_type,
          lossless, has_picture, has_lyrics, mtime, created_at, updated_at) VALUES
          ('tr-000000000000000000000000000000c1', 1,
           'Aurora Field/Glass Harbour/01 Low Tide.flac', '100:aa', 'Low Tide', 'low tide',
           'al-000000000000000000000000000000b1', 'ar-000000000000000000000000000000a1',
           'AURORA FIELD', 'Glass Harbour', 1, 2, 2019, 'Electronic', 500, 58, 22050, 16, 1,
           100, 'flac', 'audio/flac', 1, 1, 1, 700, 800, 900),
          ('tr-000000000000000000000000000000c2', 1, '/tr-000000000000000000000000000000c2',
           '200:bb', 'Breakwater', 'breakwater', 'al-000000000000000000000000000000b1',
           'ar-000000000000000000000000000000a1', 'aurora field', 'glass harbour', NULL,
           NULL, NULL, NULL, 500, NULL, NULL, NULL, NULL, 200, 'mp3', 'audio/mpeg', 0, 0, 0,
           701, 801, 901),
          ('tr-000000000000000000000000000000c3', 2, 'Last Train.opus', '300:cc',
           'Last Train', 'last train', 'al-000000000000000000000000000000b2',
           'ar-000000000000000000000000000000a2', 'Tessa Lund', 'Summer Sampler', 2, NULL,
           NULL, 'Pop', 1234, 96, 24000, NULL, 1, 300, 'opus', 'audio/ogg', 0, 0, 0, 702,
           802, 902);
        INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating, play_count,
          last_played_at) VALUES
          (1, 'tr-000000000000000000000000000000c1', 'track', 1000, 5, 7, 2000),
          (1, 'al-000000000000000000000000000000b1', 'album', 1001, 4, 0, NULL),
          (1, 'ar-000000000000000000000000000000a1', 'artist', 1002, NULL, 0, NULL),
          (2, 'tr-000000000000000000000000000000c1', 'track', NULL, 2, 1, 2001);
        INSERT INTO playlists (id, owner_id, name, comment, public, created_at, updated_at)
        VALUES
          ('pl-000000000000000000000000000000d1', 1, 'Mix', 'for the road', 1, 10, 20),
          ('pl-000000000000000000000000000000d2', 1, 'Empty', NULL, 0, 11, 11);
        INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES
          ('pl-000000000000000000000000000000d1', 0, 'tr-000000000000000000000000000000c3'),
          ('pl-000000000000000000000000000000d1', 1, 'tr-000000000000000000000000000000c1'),
          ('pl-000000000000000000000000000000d1', 2, 'tr-000000000000000000000000000000c3');
        INSERT INTO play_queue (user_id, current_id, position_ms, changed_at, changed_by)
        VALUES (1, 'tr-000000000000000000000000000000c1', 1500, 30, 'a client');
        INSERT INTO play_queue_tracks (user_id, position, track_id) VALUES
          (1, 0, 'tr-000000000000000000000000000000c1'),
          (1, 1, 'tr-000000000000000000000000000000c2');
        INSERT INTO scans (id, started_at, finished_at, tracks_seen, tracks_added,
          tracks_changed, tracks_moved, tracks_removed, error) VALUES
          (1, 40, 50, 3, 3, 0, 0, 0, NULL),
          (2, 60, NULL, 3, 0, 1, 0, 0, 'cancelled');
    ";

    /// Rows that a build at schema version 2 stored in a database that holds
    /// [`V1_ROWS`], in the columns of that version: the album artist of a
    /// track whose file a scan read, a track with an album artist, what a
    /// user attached to it, and three scans. The scans 3 and 4 ended in one
    /// second, 2026-10-11T01:02:03Z, and the scan 5 has an error.
    ///
    /// Schema version 2 does not change, so this text does not change.
    const V2_ROWS: &str = "
        UPDATE tracks SET album_artist_name = 'Aurora Field'
        WHERE id = 'tr-000000000000000000000000000000c1';
        INSERT INTO tracks (id, folder_id, rel_path, fingerprint, title, title_lc, album_id,
          artist_id, artist_name, album_name, track_no, disc_no, year, genre, duration_ms,
          bitrate_kbps, sample_rate, bit_depth, channels, size, suffix, content_type,
          lossless, has_picture, mtime, created_at, updated_at, album_artist_name) VALUES
          ('tr-000000000000000000000000000000c4', 2, 'Night Bus.opus', '400:dd', 'Night Bus',
           'night bus', 'al-000000000000000000000000000000b2',
           'ar-000000000000000000000000000000a2', 'Nadia Ortiz', 'Summer Sampler', 3, NULL,
           2021, 'Pop', 2000, 96, 24000, NULL, 2, 400, 'opus', 'audio/ogg', 0, 1, 703, 803,
           903, 'Various Artists');
        INSERT INTO annotations (user_id, item_id, item_type, starred_at, rating, play_count,
          last_played_at) VALUES
          (2, 'tr-000000000000000000000000000000c4', 'track', 1003, 3, 9, 2002);
        INSERT INTO playlist_tracks (playlist_id, position, track_id) VALUES
          ('pl-000000000000000000000000000000d1', 3, 'tr-000000000000000000000000000000c4');
        INSERT INTO play_queue_tracks (user_id, position, track_id) VALUES
          (1, 2, 'tr-000000000000000000000000000000c4');
        INSERT INTO scans (id, started_at, finished_at, tracks_seen, tracks_added,
          tracks_changed, tracks_moved, tracks_removed, error) VALUES
          (3, 1791680400, 1791680523, 4, 1, 0, 0, 0, NULL),
          (4, 1791680523, 1791680523, 4, 0, 0, 0, 0, NULL),
          (5, 1791684000, 1791684123, 4, 0, 1, 0, 0, 'a path could not be read');
    ";

    /// The columns that the migrations after the first one add: the schema
    /// version that has the column first, the table, the column, and the
    /// value that a row from before that version holds in it. SQLite adds a
    /// column after the columns that a table has.
    const ADDED_SINCE_V1: &[(i64, &str, &str, Value)] = &[
        (2, "tracks", "album_artist_name", Value::Null),
        (3, "scans", "changed_library", Value::Integer(1)),
    ];

    /// The columns that the migrations after the first one remove: the
    /// first schema version without the column, the table and the column.
    const REMOVED_SINCE_V1: &[(i64, &str, &str)] = &[(3, "tracks", "has_lyrics")];

    /// The integer columns whose values a migration after the first one
    /// multiplies: the schema version that has the new values first, the
    /// table, the column and the factor. NULL stays NULL.
    const MULTIPLIED_SINCE_V1: &[(i64, &str, &str, i64)] = &[
        (3, "scans", "started_at", 1000),
        (3, "scans", "finished_at", 1000),
    ];

    /// The name that [`dump`] gives the rowid of a row.
    const ROWID: &str = "_rowid";

    /// The rows of every table: for each row in rowid order, the rowid
    /// under the name [`ROWID`], and the name and the value of every column.
    type Dump = BTreeMap<&'static str, Vec<Vec<(String, Value)>>>;

    fn dump(connection: &Connection) -> Result<Dump> {
        let mut tables = Dump::new();
        for table in TABLES {
            let mut select = connection.prepare(&format!(
                "SELECT rowid AS {ROWID}, * FROM {table} ORDER BY rowid"
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

    /// Brings an empty database to schema version 1 with the first
    /// migration alone, stores [`V1_ROWS`] and returns the rows.
    fn fill_at_version_1(connection: &mut Connection) -> Dump {
        apply_migration(connection, 0, MIGRATIONS[0].sql).unwrap();
        assert_eq!(user_version(connection).unwrap(), 1);
        connection.execute_batch(V1_ROWS).unwrap();
        let rows = dump(connection).unwrap();
        for table in TABLES {
            assert!(!rows[table].is_empty(), "{table} has no row");
        }
        rows
    }

    /// Brings an empty database to schema version 2 with the first two
    /// migrations alone, stores [`V1_ROWS`] and [`V2_ROWS`] and returns the
    /// rows. The database is what a build with two migrations left.
    fn fill_at_version_2(connection: &mut Connection) -> Dump {
        migrate_with(connection, &MIGRATIONS[..2]).unwrap();
        assert_eq!(user_version(connection).unwrap(), 2);
        connection.execute_batch(V1_ROWS).unwrap();
        connection.execute_batch(V2_ROWS).unwrap();
        let rows = dump(connection).unwrap();
        for table in TABLES {
            assert!(!rows[table].is_empty(), "{table} has no row");
        }
        rows
    }

    /// Returns an in-memory connection and a connection to a new database
    /// file in WAL mode, both with the pragmas of [`configure`] and without
    /// a schema.
    fn empty_connections() -> (TempDir, PathBuf, [Connection; 2]) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let file = Connection::open(&path).unwrap();
        let journal_mode: String = file
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        let memory = Connection::open_in_memory().unwrap();
        for connection in [&memory, &file] {
            configure(connection).unwrap();
        }
        (dir, path, [memory, file])
    }

    /// Returns what the migrations make of the rows `before` of a database
    /// of schema version `from` when they bring it to version `to`: every
    /// row with its rowid and its values, except that the columns of
    /// [`REMOVED_SINCE_V1`] are gone, the values of [`MULTIPLIED_SINCE_V1`]
    /// are multiplied, and the columns of [`ADDED_SINCE_V1`] follow the
    /// other columns with the value of a row from before.
    fn migrated(before: &Dump, from: i64, to: i64) -> Dump {
        let applies = |version: &i64| (from + 1..=to).contains(version);
        let mut after = before.clone();
        for (table, rows) in &mut after {
            for row in rows {
                for (_, _, column, factor) in MULTIPLIED_SINCE_V1
                    .iter()
                    .filter(|(version, of, ..)| applies(version) && of == table)
                {
                    let (_, value) = row
                        .iter_mut()
                        .find(|(name, _)| name == column)
                        .unwrap_or_else(|| panic!("{table} has no column {column}"));
                    match value {
                        Value::Integer(integer) => *integer *= factor,
                        Value::Null => {}
                        other => panic!("{table}.{column} holds {other:?}"),
                    }
                }
                for (_, _, column) in REMOVED_SINCE_V1
                    .iter()
                    .filter(|(version, of, _)| applies(version) && of == table)
                {
                    let columns = row.len();
                    row.retain(|(name, _)| name != column);
                    assert_eq!(row.len(), columns - 1, "{table} has no column {column}");
                }
                for (_, _, column, value) in ADDED_SINCE_V1
                    .iter()
                    .filter(|(version, of, ..)| applies(version) && of == table)
                {
                    row.push(((*column).to_owned(), value.clone()));
                }
            }
        }
        after
    }

    /// Checks that `after`, the rows of a database of the latest schema
    /// version, holds every row that the database had at schema version
    /// `from` as [`migrated`] says, and no other column and no other row.
    fn assert_rows_kept(before: &Dump, from: i64, after: &Dump) {
        let expected = migrated(before, from, latest());
        for table in TABLES {
            assert_eq!(after[table], expected[table], "{table}");
        }
    }

    /// Checks that no row names a parent row that does not exist and that
    /// SQLite finds no damage.
    fn assert_consistent(connection: &Connection) {
        let orphans: Vec<String> = connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(orphans, [""; 0]);
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    fn a_database_of_schema_version_1_migrates_and_keeps_every_row() {
        let (_dir, path, [mut memory, mut file]) = empty_connections();

        let before = fill_at_version_1(&mut memory);
        migrate(&mut memory).unwrap();
        assert_eq!(user_version(&memory).unwrap(), latest());
        assert_rows_kept(&before, 1, &dump(&memory).unwrap());
        assert_consistent(&memory);

        // The file is what a build with the first migration alone left on
        // disk. `Db::open` of this build migrates it in place.
        let before = fill_at_version_1(&mut file);
        drop(file);
        let db = Db::open(&path).unwrap();
        db.with_reader(|c| {
            assert_eq!(user_version(c)?, latest());
            assert_rows_kept(&before, 1, &dump(c)?);
            assert_consistent(c);
            Ok(())
        })
        .unwrap();

        // A second open finds nothing to apply and the same rows.
        drop(db);
        let db = Db::open(&path).unwrap();
        db.with_reader(|c| {
            assert_rows_kept(&before, 1, &dump(c)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_migration_that_fails_part_way_leaves_the_database_at_its_version_with_its_rows() {
        // The first two statements run and the third one fails.
        let failing = "ALTER TABLE tracks ADD COLUMN album_artist_name TEXT;
                       UPDATE tracks SET album_artist_name = artist_name;
                       INSERT INTO missing VALUES (1);";
        let (_dir, path, connections) = empty_connections();
        for (mut connection, on_disk) in connections.into_iter().zip([false, true]) {
            let before = fill_at_version_1(&mut connection);

            let error = apply_migration(&mut connection, 1, failing).unwrap_err();
            assert!(error.to_string().contains("no such table"), "{error}");
            // The dump names every column of every row, so the table is
            // without the column that the first statement added.
            assert_eq!(user_version(&connection).unwrap(), 1);
            assert_eq!(dump(&connection).unwrap(), before);
            assert_consistent(&connection);

            // A connection that opens the file afterwards finds the same.
            if on_disk {
                let other = Connection::open(&path).unwrap();
                assert_eq!(user_version(&other).unwrap(), 1);
                assert_eq!(dump(&other).unwrap(), before);
            }

            // The database is at a version that the migrations apply to.
            migrate(&mut connection).unwrap();
            assert_eq!(user_version(&connection).unwrap(), latest());
            assert_rows_kept(&before, 1, &dump(&connection).unwrap());
        }
    }

    /// The name of the file of the third migration.
    const THIRD: &str = "0003_scans_milliseconds_and_drop_has_lyrics.sql";

    /// Returns the names of the columns of `table` in their order.
    fn columns(connection: &Connection, table: &str) -> Vec<String> {
        connection
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .unwrap()
            .query_map([table], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Returns the type, the name and the SQL text of every object of the
    /// schema, in name order.
    fn schema(connection: &Connection) -> Vec<(String, String, Option<String>)> {
        connection
            .prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Returns `row` without the column `column`.
    fn without(row: &[(String, Value)], column: &str) -> Vec<(String, Value)> {
        let kept: Vec<_> = row
            .iter()
            .filter(|(name, _)| name != column)
            .cloned()
            .collect();
        assert_eq!(kept.len(), row.len() - 1, "the row has no column {column}");
        kept
    }

    /// Returns the value of the column `column` of `row`.
    fn value<'a>(row: &'a [(String, Value)], column: &str) -> &'a Value {
        let found = row.iter().find(|(name, _)| name == column);
        &found
            .unwrap_or_else(|| panic!("the row has no column {column}"))
            .1
    }

    #[test]
    fn a_database_of_schema_version_2_migrates_and_keeps_every_row() {
        let (_dir, path, [mut memory, mut file]) = empty_connections();

        let before = fill_at_version_2(&mut memory);
        migrate(&mut memory).unwrap();
        assert_eq!(user_version(&memory).unwrap(), latest());
        assert_rows_kept(&before, 2, &dump(&memory).unwrap());
        assert_consistent(&memory);

        // The file is what a build with two migrations left on disk.
        // `Db::open` of this build migrates it in place.
        let before = fill_at_version_2(&mut file);
        drop(file);
        let db = Db::open(&path).unwrap();
        db.with_reader(|c| {
            assert_eq!(user_version(c)?, latest());
            assert_rows_kept(&before, 2, &dump(c)?);
            assert_consistent(c);
            Ok(())
        })
        .unwrap();

        // A second open finds nothing to apply and the same rows.
        drop(db);
        let db = Db::open(&path).unwrap();
        db.with_reader(|c| {
            assert_rows_kept(&before, 2, &dump(c)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_third_migration_changes_the_scan_times_and_the_two_columns_and_nothing_else() {
        let (_dir, _path, connections) = empty_connections();
        for mut connection in connections {
            let before = fill_at_version_2(&mut connection);
            // A row of an earlier build can hold 1 in the column that the
            // migration removes.
            let lyrics: Vec<&Value> = before["tracks"]
                .iter()
                .map(|row| value(row, "has_lyrics"))
                .collect();
            assert_eq!(
                lyrics,
                [
                    &Value::Integer(1),
                    &Value::Integer(0),
                    &Value::Integer(0),
                    &Value::Integer(0)
                ]
            );

            // The third migration alone, as a build with three migrations
            // applies it.
            migrate_with(&mut connection, &MIGRATIONS[..3]).unwrap();
            assert_eq!(user_version(&connection).unwrap(), 3);
            let after = dump(&connection).unwrap();
            assert_consistent(&connection);

            // Every table but two has the rows it had: the users, the
            // folders, the artists, the albums, the stars, the ratings and
            // the play counts, the playlists and the play queue.
            for table in TABLES {
                if table != "scans" && table != "tracks" {
                    assert_eq!(after[table], before[table], "{table}");
                }
            }

            // A track row has its rowid, its ID and every other value, and
            // no `has_lyrics`.
            let expected: Vec<_> = before["tracks"]
                .iter()
                .map(|row| without(row, "has_lyrics"))
                .collect();
            assert_eq!(after["tracks"], expected);
            assert_eq!(
                columns(&connection, "tracks"),
                [
                    "id",
                    "folder_id",
                    "rel_path",
                    "fingerprint",
                    "title",
                    "title_lc",
                    "album_id",
                    "artist_id",
                    "artist_name",
                    "album_name",
                    "track_no",
                    "disc_no",
                    "year",
                    "genre",
                    "duration_ms",
                    "bitrate_kbps",
                    "sample_rate",
                    "bit_depth",
                    "channels",
                    "size",
                    "suffix",
                    "content_type",
                    "lossless",
                    "has_picture",
                    "mtime",
                    "created_at",
                    "updated_at",
                    "album_artist_name",
                ]
            );
            // No table, index, trigger or view names the column.
            for (object_type, name, sql) in schema(&connection) {
                let sql = sql.unwrap_or_default();
                assert!(!sql.contains("has_lyrics"), "{object_type} {name}: {sql}");
            }

            // A scan row has its times in milliseconds and the flag. The
            // scans 3 and 4 ended in one second and have one end time.
            let scans: Vec<(i64, i64, Option<i64>, i64)> = connection
                .prepare(
                    "SELECT id, started_at, finished_at, changed_library FROM scans ORDER BY id",
                )
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                scans,
                [
                    (1, 40_000, Some(50_000), 1),
                    (2, 60_000, None, 1),
                    (3, 1_791_680_400_000, Some(1_791_680_523_000), 1),
                    (4, 1_791_680_523_000, Some(1_791_680_523_000), 1),
                    (5, 1_791_684_000_000, Some(1_791_684_123_000), 1),
                ]
            );
            // The other columns of a scan row have the values they had.
            let times = ["started_at", "finished_at"];
            for (row, old) in after["scans"].iter().zip(&before["scans"]) {
                let mut row = without(row, "changed_library");
                let mut old = old.clone();
                for column in times {
                    row = without(&row, column);
                    old = without(&old, column);
                }
                assert_eq!(row, old);
            }
            assert_eq!(
                columns(&connection, "scans"),
                [
                    "id",
                    "started_at",
                    "finished_at",
                    "tracks_seen",
                    "tracks_added",
                    "tracks_changed",
                    "tracks_moved",
                    "tracks_removed",
                    "error",
                    "changed_library",
                ]
            );
        }
    }

    #[test]
    fn the_third_migration_that_fails_at_its_last_statement_leaves_version_2_as_it_was() {
        let (_dir, path, [_, mut file]) = empty_connections();
        fill_at_version_2(&mut file);
        // An index names the column that the last statement of the third
        // migration removes, so that statement fails. The three statements
        // before it have run by then.
        file.execute_batch("CREATE INDEX tracks_lyrics ON tracks(has_lyrics)")
            .unwrap();
        let before = dump(&file).unwrap();
        let schema_before = schema(&file);
        drop(file);

        for _ in 0..2 {
            let error = Db::open(&path).unwrap_err();
            assert!(
                matches!(
                    &error,
                    DbError::Migration { name, version: 2, .. } if *name == THIRD
                ),
                "{error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "migration {THIRD} failed and the database was left at schema version 2: \
                     error in index tracks_lyrics after drop column: no such column: has_lyrics"
                )
            );

            // The times are seconds, the table has no new column, the
            // index of the migration does not exist, and every row is as it
            // was.
            let connection = Connection::open(&path).unwrap();
            assert_eq!(user_version(&connection).unwrap(), 2);
            assert_eq!(dump(&connection).unwrap(), before);
            assert_eq!(schema(&connection), schema_before);
            assert_consistent(&connection);
        }

        // Without the index the same file migrates.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP INDEX tracks_lyrics")
            .unwrap();
        drop(connection);
        let db = Db::open(&path).unwrap();
        db.with_reader(|c| {
            assert_eq!(user_version(c)?, latest());
            assert_rows_kept(&before, 2, &dump(c)?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_error_of_a_failed_migration_names_the_migration_and_the_version_that_is_kept() {
        // The list of a build whose third migration fails at its third
        // statement.
        let failing = Migration {
            name: "0003_fails.sql",
            sql: "ALTER TABLE scans ADD COLUMN changed_library INTEGER NOT NULL DEFAULT 1;
                  UPDATE scans SET started_at = started_at * 1000;
                  INSERT INTO missing VALUES (1);",
        };
        let migrations = [MIGRATIONS[0], MIGRATIONS[1], failing];
        let (_dir, path, connections) = empty_connections();
        for (mut connection, on_disk) in connections.into_iter().zip([false, true]) {
            let before = fill_at_version_1(&mut connection);

            let error = migrate_with(&mut connection, &migrations).unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::Migration {
                        name: "0003_fails.sql",
                        version: 2,
                        ..
                    }
                ),
                "{error:?}"
            );
            assert_eq!(
                error.to_string(),
                "migration 0003_fails.sql failed and the database was left at schema version 2: \
                 no such table: missing"
            );

            // The second migration stays applied, and nothing of the third
            // one does: the rows are those of schema version 2.
            let kept = migrated(&before, 1, 2);
            assert_eq!(user_version(&connection).unwrap(), 2);
            assert_eq!(dump(&connection).unwrap(), kept);
            assert_consistent(&connection);
            if on_disk {
                let other = Connection::open(&path).unwrap();
                assert_eq!(user_version(&other).unwrap(), 2);
                assert_eq!(dump(&other).unwrap(), kept);
            }

            // The next run fails in the same way and leaves the same.
            let again = migrate_with(&mut connection, &migrations).unwrap_err();
            assert_eq!(again.to_string(), error.to_string());
            assert_eq!(dump(&connection).unwrap(), kept);

            // The migrations of this build apply to what is left.
            migrate(&mut connection).unwrap();
            assert_eq!(user_version(&connection).unwrap(), latest());
            assert_rows_kept(&before, 1, &dump(&connection).unwrap());
        }
    }

    #[test]
    fn a_build_without_the_last_migration_refuses_the_database_and_changes_nothing() {
        // The list of the build before this one.
        let older = &MIGRATIONS[..MIGRATIONS.len() - 1];
        let (_dir, path, connections) = empty_connections();
        for (mut connection, on_disk) in connections.into_iter().zip([false, true]) {
            fill_at_version_2(&mut connection);
            migrate(&mut connection).unwrap();
            let before = dump(&connection).unwrap();
            let schema_before = schema(&connection);
            let schema_changes = pragma_i64(&connection, "schema_version");

            let error = migrate_with(&mut connection, older).unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::UnsupportedSchema { found, supported }
                        if found == latest() && supported == latest() - 1
                ),
                "{error}"
            );
            assert_eq!(
                error.to_string(),
                format!(
                    "the database has schema version {}; this build supports versions 0 to {}",
                    latest(),
                    latest() - 1
                )
            );
            assert_eq!(user_version(&connection).unwrap(), latest());
            assert_eq!(pragma_i64(&connection, "schema_version"), schema_changes);
            assert_eq!(dump(&connection).unwrap(), before);
            assert_eq!(schema(&connection), schema_before);
            if on_disk {
                let other = Connection::open(&path).unwrap();
                assert_eq!(user_version(&other).unwrap(), latest());
                assert_eq!(dump(&other).unwrap(), before);
            }
        }
    }

    #[test]
    fn a_track_with_a_missing_album_fails_to_insert() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.with_writer(|c| {
                insert_folder(c, "/music")?;
                c.execute(
                    "INSERT INTO artists (id, key, name, sort_name) VALUES (?1, 'a', 'A', 'A')",
                    [ARTIST_ID],
                )?;
                Ok(())
            })
            .unwrap();

            let error = db.with_writer(|c| Ok(insert_track(c)?)).unwrap_err();
            let DbError::Sqlite(error) = error else {
                panic!("{error}");
            };
            assert_eq!(
                error.sqlite_error_code(),
                Some(ErrorCode::ConstraintViolation)
            );
            assert!(error.to_string().contains("FOREIGN KEY"), "{error}");

            // With the album present the same insert succeeds, so the album
            // was the missing parent.
            let inserted = db
                .with_writer(|c| {
                    c.execute(
                        "INSERT INTO albums (id, artist_id, key, name, artist_name, created_at)
                         VALUES (?1, ?2, 'b', 'B', 'A', 0)",
                        [ALBUM_ID, ARTIST_ID],
                    )?;
                    Ok(insert_track(c)?)
                })
                .unwrap();
            assert_eq!(inserted, 1);
        }
    }

    #[test]
    fn a_user_insert_that_omits_the_role_fails() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let error = db
                .with_writer(|c| {
                    Ok(c.execute(
                        "INSERT INTO users (name, password, created_at, updated_at)
                         VALUES ('adnan', 'secret', 0, 0)",
                        [],
                    )?)
                })
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("NOT NULL constraint failed: users.role"),
                "{error}"
            );

            let inserted = db
                .with_writer(|c| {
                    Ok(c.execute(
                        "INSERT INTO users (name, role, password, created_at, updated_at)
                         VALUES ('adnan', 'owner', 'secret', 0, 0)",
                        [],
                    )?)
                })
                .unwrap();
            assert_eq!(inserted, 1);
        }
    }

    #[test]
    fn open_in_a_missing_directory_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("library.db");
        let error = Db::open(&path).unwrap_err();
        // On Unix the file is created before SQLite opens it. On other
        // systems SQLite reports the failure.
        #[cfg(unix)]
        assert!(
            matches!(&error, DbError::CreateFile { path: failed, .. } if *failed == path),
            "{error}"
        );
        #[cfg(not(unix))]
        assert!(matches!(error, DbError::Sqlite(_)), "{error}");
    }

    #[test]
    fn file_database_connections_have_the_pragmas() {
        let (_dir, _path, db) = temp_db();
        let writer = db.inner.writer.blocking_lock();
        let journal_mode: String = writer
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        // 1 is NORMAL.
        assert_eq!(pragma_i64(&writer, "synchronous"), 1);
        assert_eq!(pragma_i64(&writer, "foreign_keys"), 1);
        assert_eq!(pragma_i64(&writer, "busy_timeout"), 5000);

        assert_eq!(db.inner.readers.len(), 4);
        for reader in &db.inner.readers {
            let reader = reader.blocking_lock();
            assert_eq!(pragma_i64(&reader, "synchronous"), 1);
            assert_eq!(pragma_i64(&reader, "foreign_keys"), 1);
            assert_eq!(pragma_i64(&reader, "busy_timeout"), 5000);
        }
    }

    #[test]
    fn memory_database_has_one_connection_with_the_pragmas() {
        let db = Db::open_memory().unwrap();
        assert!(db.inner.readers.is_empty());
        let connection = db.inner.writer.blocking_lock();
        assert_eq!(pragma_i64(&connection, "synchronous"), 1);
        assert_eq!(pragma_i64(&connection, "foreign_keys"), 1);
        assert_eq!(pragma_i64(&connection, "busy_timeout"), 5000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn twenty_reads_complete_while_a_write_runs() {
        let (_dir, _path, db) = temp_db();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        // The write holds an open transaction until the test releases it.
        let writer = db.clone();
        let write = tokio::spawn(async move {
            writer
                .write(move |c| {
                    let transaction = c.transaction()?;
                    insert_folder(&transaction, "/music")?;
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    transaction.commit()?;
                    Ok(())
                })
                .await
        });
        started_rx.await.unwrap();

        let reads: Vec<_> = (0..20)
            .map(|_| {
                let reader = db.clone();
                tokio::spawn(async move { reader.read(count_folders).await })
            })
            .collect();
        // The limit ends the test when the reads wait for the write. It does
        // not delay a passing run.
        let counts = tokio::time::timeout(Duration::from_secs(2), async {
            let mut counts = Vec::new();
            for read in reads {
                counts.push(read.await.unwrap().unwrap());
            }
            counts
        })
        .await
        .expect("the reads finish while the write runs");

        // Every read finished before the write committed.
        assert_eq!(counts, [0; 20]);
        assert!(!write.is_finished());

        release_tx.send(()).unwrap();
        write.await.unwrap().unwrap();
        assert_eq!(db.read(count_folders).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn memory_database_reads_see_writes() {
        let db = Db::open_memory().unwrap();
        db.write(|c| insert_folder(c, "/music")).await.unwrap();
        assert_eq!(db.read(count_folders).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn read_connections_of_a_file_database_reject_writes() {
        let (_dir, _path, db) = temp_db();
        let error = db.read(|c| insert_folder(c, "/music")).await.unwrap_err();
        let DbError::Sqlite(error) = error else {
            panic!("{error}");
        };
        assert_eq!(error.sqlite_error_code(), Some(ErrorCode::ReadOnly));
    }

    #[tokio::test]
    async fn a_closure_error_reaches_the_caller() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let error = db
                .write(|c| Ok(c.execute("INSERT INTO missing DEFAULT VALUES", [])?))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("no such table"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_closure_panic_reaches_the_caller_and_the_database_stays_usable() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let panicking = db.clone();
            let outcome = tokio::spawn(async move {
                panicking
                    .write(|_| -> Result<()> { panic!("closure panic") })
                    .await
            })
            .await;
            assert!(outcome.unwrap_err().is_panic());

            db.write(|c| insert_folder(c, "/music")).await.unwrap();
            assert_eq!(db.read(count_folders).await.unwrap(), 1);
        }
    }

    #[test]
    fn blocking_access_works_on_plain_threads() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.with_writer(|c| insert_folder(c, "/music")).unwrap();
            let other = db.clone();
            let count = std::thread::spawn(move || other.with_reader(count_folders))
                .join()
                .unwrap()
                .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[tokio::test]
    async fn blocking_access_works_inside_spawn_blocking() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let count = tokio::task::spawn_blocking(move || {
                db.with_writer(|c| insert_folder(c, "/music"))?;
                db.with_reader(count_folders)
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(count, 1);
        }
    }

    #[tokio::test]
    async fn close_truncates_the_write_ahead_log() {
        let (_dir, path, db) = temp_db();
        db.write(|c| insert_folder(c, "/music")).await.unwrap();
        let wal = side_file(&path, "-wal");
        assert!(wal.metadata().unwrap().len() > 0);

        // The clone keeps the connections open, so the empty log is the work
        // of the checkpoint and not of the last connection closing.
        let open = db.clone();
        db.close().await.unwrap();
        assert_eq!(wal.metadata().unwrap().len(), 0);
        assert_eq!(open.read(count_folders).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn close_of_a_memory_database_succeeds() {
        Db::open_memory().unwrap().close().await.unwrap();
    }

    #[tokio::test]
    async fn close_reports_a_checkpoint_that_another_connection_blocks() {
        let (_dir, path, db) = temp_db();
        db.write(|c| {
            c.busy_timeout(Duration::from_millis(20))?;
            insert_folder(c, "/music")
        })
        .await
        .unwrap();

        // A read transaction of a connection outside the handle pins the log.
        let outside = Connection::open(&path).unwrap();
        outside.execute_batch("BEGIN").unwrap();
        assert_eq!(count_folders(&outside).unwrap(), 1);

        let error = db.close().await.unwrap_err();
        assert!(matches!(error, DbError::CheckpointBusy), "{error}");
    }

    #[tokio::test]
    async fn a_read_that_runs_past_its_limit_is_interrupted() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.set_read_limit(SHORT_LIMIT);
            let error = db.read(|c| count_to(c, MANY_ROWS)).await.unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);
            assert_eq!(
                error.to_string(),
                "a statement of a read was interrupted at the time limit of 20ms"
            );
        }
    }

    #[test]
    fn a_handle_starts_with_the_limit_of_five_seconds() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            assert_eq!(READ_LIMIT, Duration::from_secs(5));
            assert_eq!(
                db.inner.read_limit_nanos.load(Ordering::Relaxed),
                5_000_000_000
            );
        }
    }

    #[tokio::test]
    async fn the_connection_of_an_interrupted_read_serves_the_next_read() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.write(|c| insert_folder(c, "/music")).await.unwrap();
            let _held = hold_all_readers_but_one(&db);

            db.set_read_limit(SHORT_LIMIT);
            let error = db.read(|c| count_to(c, MANY_ROWS)).await.unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);

            db.set_read_limit(READ_LIMIT);
            assert_eq!(db.read(count_folders).await.unwrap(), 1);
            assert_eq!(db.read(|c| count_to(c, 1000)).await.unwrap(), 1000);
        }
    }

    #[tokio::test]
    async fn the_timer_of_an_ended_read_interrupts_no_later_statement() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let _held = hold_all_readers_but_one(&db);
            db.set_read_limit(SHORT_LIMIT);
            let error = db.read(|c| count_to(c, MANY_ROWS)).await.unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);

            // A timer that outlived its read would interrupt the connection
            // again within `INTERRUPT_REPEAT`.
            db.set_read_limit(READ_LIMIT);
            db.read(|c| count_for(c, 3 * INTERRUPT_REPEAT))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_statement_that_starts_after_the_limit_of_its_read_is_interrupted() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.set_read_limit(SHORT_LIMIT);

            // The limit ends while the read runs no statement, so the first
            // interrupt stops nothing.
            let error = db
                .read(|c| {
                    assert_eq!(count_to(c, 1000)?, 1000);
                    std::thread::sleep(3 * SHORT_LIMIT);
                    count_to(c, MANY_ROWS)
                })
                .await
                .unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);

            // The read goes on after the statement that was interrupted.
            let error = db
                .read(|c| {
                    assert!(count_to(c, MANY_ROWS).is_err());
                    count_to(c, MANY_ROWS)
                })
                .await
                .unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);
        }
    }

    #[tokio::test]
    async fn a_panic_in_a_read_ends_its_timer() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let _held = hold_all_readers_but_one(&db);
            db.set_read_limit(SHORT_LIMIT);
            let panicking = db.clone();
            let outcome = tokio::spawn(async move {
                panicking
                    .read(|_| -> Result<()> { panic!("closure panic") })
                    .await
            })
            .await;
            assert!(outcome.unwrap_err().is_panic());

            // The limit of the read that panicked ends while these
            // statements run on its connection.
            db.set_read_limit(READ_LIMIT);
            db.read(|c| count_for(c, SHORT_LIMIT + 3 * INTERRUPT_REPEAT))
                .await
                .unwrap();
        }
    }

    /// The state of a connection after an interrupt that arrived late: the
    /// flag of the interrupt is set and no statement runs. The timer of a
    /// read leaves a connection so when its interrupt arrives between two
    /// statements of the read or after the last one.
    #[test]
    fn an_interrupt_that_finds_no_statement_running_stops_no_later_statement() {
        const CACHED: &str = "SELECT count(*) + 0 FROM folders";

        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.with_writer(|c| {
                insert_folder(c, "/music")?;
                drop(c.prepare_cached(CACHED)?);
                let interrupt = c.get_interrupt_handle();

                // A statement that is prepared after the interrupt.
                interrupt.interrupt();
                assert!(c.is_interrupted());
                assert_eq!(count_folders(c)?, 1);
                assert!(!c.is_interrupted());

                // A statement of the statement cache, which is not prepared
                // again.
                interrupt.interrupt();
                assert!(c.is_interrupted());
                let count: i64 = c.prepare_cached(CACHED)?.query_row([], |row| row.get(0))?;
                assert_eq!(count, 1);
                assert!(!c.is_interrupted());
                Ok(())
            })
            .unwrap();
        }
    }

    #[tokio::test]
    async fn an_interrupted_read_leaves_no_transaction_open() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let _held = hold_all_readers_but_one(&db);
            db.set_read_limit(SHORT_LIMIT);
            let error = db
                .read(|c| {
                    // No guard rolls this transaction back. The read ends as
                    // a read does whose `ROLLBACK` an interrupt stopped.
                    c.execute_batch("BEGIN")?;
                    count_to(c, MANY_ROWS)
                })
                .await
                .unwrap_err();
            assert_read_limit(&error, SHORT_LIMIT);

            db.set_read_limit(READ_LIMIT);
            let (autocommit, folders) = db
                .read(|c| {
                    let autocommit = c.is_autocommit();
                    // A handler reads inside a transaction of its own.
                    let snapshot = c.unchecked_transaction()?;
                    Ok((autocommit, count_folders(&snapshot)?))
                })
                .await
                .unwrap();
            assert!(autocommit);
            assert_eq!(folders, 0);
        }
    }

    #[tokio::test]
    async fn a_read_whose_caller_is_gone_is_interrupted_at_its_limit() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.set_read_limit(SHORT_LIMIT);
            let (started_tx, started_rx) = oneshot::channel();
            let (ended_tx, ended_rx) = oneshot::channel();
            let reader = db.clone();
            let read = tokio::spawn(async move {
                reader
                    .read(move |c| {
                        started_tx.send(()).unwrap();
                        let counted = count_to(c, MANY_ROWS);
                        let interrupted = matches!(
                            &counted,
                            Err(DbError::Sqlite(error)) if is_interrupt(error)
                        );
                        ended_tx
                            .send((interrupted, format!("{counted:?}")))
                            .unwrap();
                        counted
                    })
                    .await
            });
            started_rx.await.unwrap();
            // The task is cancelled at its await, which drops the future of
            // the read while the statement runs. On a machine that stalls
            // the test for the limit, the read ends first with its error.
            read.abort();
            match read.await {
                Err(cancelled) => assert!(cancelled.is_cancelled()),
                Ok(result) => assert_read_limit(&result.unwrap_err(), SHORT_LIMIT),
            }

            let (interrupted, counted) = ended_rx.await.unwrap();
            assert!(interrupted, "{counted}");
        }
    }

    #[tokio::test]
    async fn a_write_that_runs_longer_than_the_read_limit_completes() {
        const LIMIT: Duration = Duration::from_millis(5);

        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.set_read_limit(LIMIT);
            db.write(|c| count_for(c, 10 * LIMIT)).await.unwrap();

            // The scanner writes with `with_writer` on a blocking thread.
            let scanner = db.clone();
            tokio::task::spawn_blocking(move || scanner.with_writer(|c| count_for(c, 10 * LIMIT)))
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_read_that_waits_for_its_connection_interrupts_no_write() {
        const LIMIT: Duration = Duration::from_millis(400);
        // The write holds the connection for longer than the limit.
        const WRITE: Duration = Duration::from_millis(500);
        // How long the read runs statements once it holds the connection.
        // The time does not depend on the speed of the machine. An
        // interrupt that arrives before the first statement stops nothing,
        // so the time holds the repetition of that interrupt.
        const READ: Duration = Duration::from_millis(100);
        assert!(READ >= 2 * INTERRUPT_REPEAT);

        // A memory database has one connection, so the read waits for the
        // write.
        let db = Db::open_memory().unwrap();
        db.set_read_limit(LIMIT);
        let (started_tx, started_rx) = oneshot::channel();
        let writer = db.clone();
        let write = tokio::spawn(async move {
            writer
                .write(move |c| {
                    insert_folder(c, "/music")?;
                    started_tx.send(()).unwrap();
                    count_for(c, WRITE)
                })
                .await
        });
        started_rx.await.unwrap();

        // The read waits for longer than its limit. Its time starts when it
        // holds the connection: a read whose wait counted would be
        // interrupted while it runs its statements.
        let folders = db
            .read(|c| {
                count_for(c, READ)?;
                count_folders(c)
            })
            .await
            .unwrap();
        assert_eq!(folders, 1);
        write.await.unwrap().unwrap();
    }

    /// The thread ends only by a panic, which a test cannot cause. This test
    /// drops the value that the thread drops when it ends. The list has its
    /// own thread, so the test ends no thread of another test.
    #[test]
    fn a_thread_that_has_ended_is_started_again_by_the_next_read() {
        let timers: &'static ReadTimers = Box::leak(Box::new(ReadTimers::new()));
        let connection = Connection::open_in_memory().unwrap();

        assert!(!timers.lock().started);
        let id = timers
            .add(connection.get_interrupt_handle(), READ_LIMIT)
            .unwrap();
        assert!(timers.lock().started);
        assert!(!timers.remove(id));

        drop(ThreadEnd(timers));
        assert!(!timers.lock().started);

        let id = timers
            .add(connection.get_interrupt_handle(), SHORT_LIMIT)
            .unwrap();
        assert!(timers.lock().started);
        let error = count_to(&connection, MANY_ROWS).unwrap_err();
        assert!(
            matches!(&error, DbError::Sqlite(error) if is_interrupt(error)),
            "{error}"
        );
        assert!(timers.remove(id));
    }

    /// `Db::open` runs `migrate` on the write connection before the handle
    /// exists, and `migrate` applies each migration with `apply_migration`.
    /// This test applies one more migration with `apply_migration` on the
    /// write connection of a handle, after the migrations of the build. A
    /// limit that reached that connection would stop it.
    #[test]
    fn a_migration_that_runs_longer_than_the_read_limit_completes() {
        const LIMIT: Duration = Duration::from_millis(5);
        const SLOW_MIGRATION: &str = "CREATE TABLE counted AS
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 400000)
             SELECT count(*) AS numbers FROM n;";

        let (_dir, dbs) = both_dbs();
        for db in dbs {
            db.set_read_limit(LIMIT);
            let started = Instant::now();
            db.with_writer(|c| Ok(apply_migration(c, latest(), SLOW_MIGRATION)?))
                .unwrap();
            assert!(started.elapsed() > LIMIT);

            let (version, numbers) = db
                .with_reader(|c| {
                    let numbers: i64 =
                        c.query_row("SELECT numbers FROM counted", [], |row| row.get(0))?;
                    Ok((user_version(c)?, numbers))
                })
                .unwrap();
            assert_eq!(version, latest() + 1);
            assert_eq!(numbers, 400_000);
        }
    }

    // File modes exist on Unix only.
    #[cfg(unix)]
    #[test]
    fn new_database_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path, _db) = temp_db();
        let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        // SQLite gives the log and the shared-memory file the mode of the
        // database file.
        for suffix in ["-wal", "-shm"] {
            assert_eq!(mode(&side_file(&path, suffix)), 0o600, "{suffix}");
        }
    }

    // File modes exist on Unix only.
    #[cfg(unix)]
    #[test]
    fn open_keeps_the_mode_of_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path, db) = temp_db();
        drop(db);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let _db = Db::open(&path).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o640);
    }
}
