//! SQLite access: the `Db` handle, connection pragmas and migrations
//! (`docs/design.md` section 3.4).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use tokio::sync::{Mutex, MutexGuard, OwnedMutexGuard};

/// Query functions, one module per group of tables.
pub mod queries {
    pub mod folders;
}

/// The migrations in order. Migration `n` (counted from 1) moves the schema
/// from version `n - 1` to version `n`. `PRAGMA user_version` holds the
/// version of a database.
const MIGRATIONS: [&str; 1] = [include_str!("../../migrations/0001_init.sql")];

/// The number of read connections of a file database.
const READERS: usize = 4;

/// How long a connection waits for a lock that another connection holds.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// An error from the database layer.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("cannot create the database file: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the database has schema version {found}; this build supports versions 0 to {supported}"
    )]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("the checkpoint did not finish because another connection uses the database")]
    CheckpointBusy,
    #[error("the runtime shut down before the database task ran")]
    Cancelled,
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
}

impl Db {
    /// Opens the database file at `path`, creates it when it does not exist,
    /// and applies the migrations it lacks.
    ///
    /// On Unix a new file is created with mode 0600.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref();
        // File modes exist on Unix only. On Windows the ACL of the user
        // profile protects the data directory.
        #[cfg(unix)]
        create_owner_only(path)?;

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
            }),
        }
    }

    /// Runs `f` with a read connection on a blocking thread.
    ///
    /// `f` runs to completion even when the returned future is dropped. A
    /// panic in `f` resumes in the caller.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let connection = self.lock_reader().await;
        run_blocking(move || f(&connection)).await
    }

    /// Runs `f` with the write connection on a blocking thread. Writes run
    /// one at a time.
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

    /// Runs `f` with a read connection on the calling thread and blocks the
    /// thread while it waits for the connection.
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
    /// the thread while it waits for the connection.
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

/// Applies every migration that the database lacks, each in one transaction.
fn migrate(connection: &mut Connection) -> Result<()> {
    loop {
        let version = user_version(connection)?;
        let Some(sql) = pending_migration(version)? else {
            return Ok(());
        };
        apply_migration(connection, version, sql)?;
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

/// Returns the migration that moves a database from `version` to the next
/// version, or `None` when `version` is the latest.
fn pending_migration(version: i64) -> Result<Option<&'static str>> {
    match usize::try_from(version) {
        Ok(applied) if applied <= MIGRATIONS.len() => Ok(MIGRATIONS.get(applied).copied()),
        _ => Err(DbError::UnsupportedSchema {
            found: version,
            // The number of migrations is small, so the cast keeps its value.
            supported: MIGRATIONS.len() as i64,
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
    use std::sync::mpsc;

    use rusqlite::{ErrorCode, params};
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

    const INDEXES: [&str; 11] = [
        "albums_created",
        "albums_genre",
        "albums_key",
        "albums_year",
        "annotations_starred",
        "playlist_tracks_track",
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

    #[test]
    fn open_creates_every_table_and_index_and_sets_user_version_1() {
        let (_dir, dbs) = both_dbs();
        for db in dbs {
            let tables = db.with_reader(|c| schema_names(c, "table")).unwrap();
            assert_eq!(tables, TABLES);
            let indexes = db.with_reader(|c| schema_names(c, "index")).unwrap();
            assert_eq!(indexes, INDEXES);
            let version = db.with_reader(|c| Ok(user_version(c)?)).unwrap();
            assert_eq!(version, 1);
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
        assert_eq!(version, 1);
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
        assert_eq!(user_version(&connection).unwrap(), 1);
    }

    #[test]
    fn a_migration_that_another_connection_applied_does_not_run_again() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("library.db");
        let mut first = Connection::open(&path).unwrap();
        let mut second = Connection::open(&path).unwrap();
        migrate(&mut first).unwrap();
        let schema_changes = pragma_i64(&second, "schema_version");

        // The second connection read version 0 before the first one migrated.
        apply_migration(&mut second, 0, MIGRATIONS[0]).unwrap();
        assert_eq!(pragma_i64(&second, "schema_version"), schema_changes);
        assert_eq!(user_version(&second).unwrap(), 1);
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
    fn open_refuses_a_schema_version_it_does_not_know() {
        for found in [2, -1] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("library.db");
            Connection::open(&path)
                .unwrap()
                .pragma_update(None, "user_version", found)
                .unwrap();
            let error = Db::open(&path).unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::UnsupportedSchema { found: f, supported: 1 } if f == found
                ),
                "{error}"
            );
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
        let counts = tokio::time::timeout(Duration::from_secs(10), async {
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
