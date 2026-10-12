//! The HTTP server: the state, the listener and the router.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::api::{self, AppState};
use crate::config::{Config, DataDir, Library};
use crate::db::queries::{folders, users};
use crate::db::{Db, DbError};
use crate::library::scan_controller::{ScanController, ScanHooks, ScanSettings};

/// What [`run_server`] logs at startup when `auth.disabled_unsafe` is `true`.
const AUTH_DISABLED_WARNING: &str = "authentication is disabled (auth.disabled_unsafe = true). \
     This is unsafe: every request acts as the owner without a password";

/// The length of one unit of `library.rescan_interval_minutes`.
const MINUTE: Duration = Duration::from_secs(60);

/// A server that cannot start or that stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The data directory has no database. The server runs with a config
    /// file, so the message says that `init` keeps it.
    #[error(
        "the database {} does not exist; `wn-server init` creates it and the owner account \
         and keeps config.toml",
        .0.display()
    )]
    NoDatabase(PathBuf),
    /// The database cannot be opened or read.
    #[error("cannot open the database {}: {source}", path.display())]
    Database { path: PathBuf, source: DbError },
    /// The database has no owner account, so no request can have a user.
    #[error(
        "the database {} has no owner account; `wn-server init` creates the account \
         and keeps the database and config.toml",
        .0.display()
    )]
    NoOwner(PathBuf),
    /// The listener cannot bind its address.
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: io::Error },
    /// The music folders of the config cannot be stored in the database.
    #[error("cannot store the music folders in the database: {0}")]
    SyncFolders(#[source] DbError),
    /// The server stopped with an I/O error.
    #[error("the server stopped: {0}")]
    Serve(#[source] io::Error),
}

/// The address the listener binds: `server.bind` and `server.port`.
pub fn bind_addr(config: &Config) -> SocketAddr {
    SocketAddr::new(config.server.bind, config.server.port)
}

/// Opens the database of `data_dir`, binds the address of `config`, logs it
/// and serves the API until `cancel` is cancelled. Requests that are in
/// progress at that moment are answered before the function returns.
///
/// The database and its owner account must exist. The function does not
/// create them, and it fails before it binds the address when one is
/// missing. When `auth.disabled_unsafe` is `true`, the function logs a
/// warning first.
///
/// Before it serves the first request, the function stores the music folders
/// of `config` in the database and starts the scans that `config` asks for
/// (`start_scans`). It does not wait for a scan. When the server has
/// stopped, the function cancels the scan that runs and waits until that
/// scan has ended.
pub async fn run_server(
    config: Config,
    data_dir: &DataDir,
    cancel: CancellationToken,
) -> Result<(), ServerError> {
    if config.auth.disabled_unsafe {
        tracing::warn!("{AUTH_DISABLED_WARNING}");
    }
    let db = open_database(data_dir).await?;

    let addr = bind_addr(&config);
    let bind_error = |source| ServerError::Bind { addr, source };
    let listener = TcpListener::bind(addr).await.map_err(bind_error)?;
    // With port 0 the system chooses the port, so the log names the address
    // of the listener and not the address of the config.
    let local_addr = listener.local_addr().map_err(bind_error)?;

    // The address is bound first: a second server on the same data directory
    // fails above, stores no folder and starts no scan. The token of the
    // scans is a child of `cancel`, so the scans end when the server is asked
    // to stop, and `stop` below ends them when the server stops for another
    // reason.
    let scan = start_scans(
        &config,
        data_dir,
        &db,
        cancel.child_token(),
        MINUTE,
        ScanHooks::default(),
    )
    .await?;
    tracing::info!("listening on {local_addr}");

    // The fields follow the layout rule in the module comment of `api`.
    #[rustfmt::skip]
    let state = AppState {
        config: Arc::new(config),

        db,

        // covers

        // transcoder

        scan: scan.clone(),

        // cancel

        started_at: Instant::now(),
    };

    let served = axum::serve(listener, api::build_router(state))
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .map_err(ServerError::Serve);
    scan.stop().await;
    served
}

/// Stores the music folders of `config` in the `folders` table, builds the
/// controller of the scans of the server, starts a scan when
/// `library.scan_on_start` is set, and starts the periodic scans when
/// `library.rescan_interval_minutes` is not 0. `minute` is the length of one
/// unit of that key, and `hooks` are the test hooks of the scans.
///
/// The folder rows are stored when the function returns, so the first
/// request finds them, with or without a scan. A folder that left the config
/// keeps its row and its tracks here. The scanner deletes them under its
/// rules for a removed folder (`docs/design.md` section 3.10).
///
/// A scan runs on a blocking thread, and the function does not wait for it.
/// Every scan of the controller holds the scan lock of `data_dir` and
/// removes the stale files of its cover cache. `cancel` cancels the scans.
async fn start_scans(
    config: &Config,
    data_dir: &DataDir,
    db: &Db,
    cancel: CancellationToken,
    minute: Duration,
    hooks: ScanHooks,
) -> Result<ScanController, ServerError> {
    let paths = config.library.folders.clone();
    db.write(move |conn| {
        let tx = conn.transaction()?;
        folders::sync_folders(&tx, &paths)?;
        tx.commit()?;
        Ok(())
    })
    .await
    .map_err(ServerError::SyncFolders)?;

    let scan = ScanController::new(ScanSettings {
        db: db.clone(),
        folders: config.library.folders.clone(),
        covers_cache_dir: Some(data_dir.covers_cache_dir()),
        lock_path: Some(data_dir.scan_lock_path()),
        cancel,
        hooks,
    });
    if config.library.scan_on_start {
        scan.start_unattended("the scan at startup");
    }
    if let Some(interval) = rescan_interval(&config.library, minute) {
        scan.start_periodic(interval);
    }
    Ok(scan)
}

/// The time between two periodic scans: `library.rescan_interval_minutes`
/// units of `minute`. `None` when the key is 0, which turns the periodic
/// scans off and is the default.
fn rescan_interval(library: &Library, minute: Duration) -> Option<Duration> {
    (library.rescan_interval_minutes > 0).then(|| minute * library.rescan_interval_minutes)
}

/// Opens the database of `data_dir` and checks that it has an owner account.
///
/// `Db::open` creates a missing file. Only `init` creates the database, so a
/// missing file is an error here.
async fn open_database(data_dir: &DataDir) -> Result<Db, ServerError> {
    let path = data_dir.db_path();
    if !path.exists() {
        return Err(ServerError::NoDatabase(path));
    }
    let opened = async {
        let db = Db::open(&path)?;
        let owner = db.read(users::get_owner).await?;
        Ok::<_, DbError>((db, owner))
    };
    match opened.await {
        Ok((db, Some(_))) => Ok(db),
        Ok((_, None)) => Err(ServerError::NoOwner(path)),
        Err(source) => Err(ServerError::Database { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::db::queries::folders::Folder;
    use crate::db::queries::scans::{self, Scan};
    use crate::db::queries::tracks;
    use crate::library::scan_controller::testing::{Gate, WAIT, ended, music_folder};
    use crate::library::scan_controller::{ScanLock, ScanStatus};

    /// A data directory in a new temporary directory. The guard keeps the
    /// directory for the life of the test.
    fn data_dir() -> (TempDir, DataDir) {
        let dir = TempDir::new().expect("a temporary directory");
        let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
        (dir, data_dir)
    }

    /// Runs the server with the default config and a token that is already
    /// cancelled. Returns the error that ends the start.
    async fn start_error(data_dir: &DataDir) -> ServerError {
        let cancel = CancellationToken::new();
        cancel.cancel();
        run_server(Config::default(), data_dir, cancel)
            .await
            .expect_err("the server does not start")
    }

    #[tokio::test]
    async fn run_server_fails_without_a_database_and_does_not_create_one() {
        let (_dir, data_dir) = data_dir();
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::NoDatabase(path) if *path == data_dir.db_path()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the database {} does not exist; `wn-server init` creates it and the owner \
                 account and keeps config.toml",
                data_dir.db_path().display()
            )
        );
        assert!(!data_dir.db_path().exists());
    }

    #[tokio::test]
    async fn run_server_fails_without_an_owner_account() {
        let (_dir, data_dir) = data_dir();
        drop(Db::open(data_dir.db_path()).expect("create the database"));
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::NoOwner(path) if *path == data_dir.db_path()),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the database {} has no owner account; `wn-server init` creates the account \
                 and keeps the database and config.toml",
                data_dir.db_path().display()
            )
        );
    }

    #[tokio::test]
    async fn run_server_names_a_database_that_it_cannot_open() {
        let (_dir, data_dir) = data_dir();
        std::fs::write(
            data_dir.db_path(),
            "this file is not a database, it is text",
        )
        .expect("write the file");
        let error = start_error(&data_dir).await;
        assert!(
            matches!(&error, ServerError::Database { path, .. } if *path == data_dir.db_path()),
            "{error:?}"
        );
        let prefix = format!(
            "cannot open the database {}: ",
            data_dir.db_path().display()
        );
        assert!(error.to_string().starts_with(&prefix), "{error}");
    }

    /// A data directory with an empty database, as the server has opened it.
    fn library() -> (TempDir, DataDir, Db) {
        let (dir, data_dir) = data_dir();
        let db = Db::open(data_dir.db_path()).expect("create the database");
        (dir, data_dir, db)
    }

    /// The config of a server with the music folders `folders`, without a
    /// scan at startup and without periodic scans.
    fn config(folders: &[&Path]) -> Config {
        let mut config = Config::default();
        config.library.folders = folders.iter().map(|path| path.to_path_buf()).collect();
        config.library.scan_on_start = false;
        config
    }

    /// Runs [`start_scans`] as `run_server` does, with a minute of 20 ms.
    async fn start(
        config: &Config,
        data_dir: &DataDir,
        db: &Db,
        hooks: ScanHooks,
    ) -> ScanController {
        let minute = Duration::from_millis(20);
        start_scans(
            config,
            data_dir,
            db,
            CancellationToken::new(),
            minute,
            hooks,
        )
        .await
        .expect("the scans start")
    }

    async fn folder_rows(db: &Db) -> Vec<Folder> {
        db.read(folders::list).await.expect("read the folders")
    }

    async fn scan_rows(db: &Db) -> Vec<Scan> {
        db.read(scans::list).await.expect("read the scans")
    }

    async fn track_count(db: &Db) -> u64 {
        db.read(tracks::count_tracks)
            .await
            .expect("count the tracks")
    }

    /// Lets the tasks that are ready run up to their next wait.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn start_scans_stores_the_folders_and_starts_no_scan_without_scan_on_start() {
        let (rock, jazz) = (music_folder(2), music_folder(1));
        let (_dir, data_dir, db) = library();
        let config = config(&[rock.path(), jazz.path()]);
        assert_eq!(config.library.rescan_interval_minutes, 0);

        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;

        // The rows are stored when the function returns, in the order of
        // the config.
        let paths: Vec<_> = folder_rows(&db)
            .await
            .into_iter()
            .map(|folder| (folder.id, folder.path))
            .collect();
        assert_eq!(
            paths,
            [
                (1, rock.path().to_path_buf()),
                (2, jazz.path().to_path_buf())
            ]
        );

        // No scan starts, at once or later: the periodic scans are off.
        settle().await;
        assert_eq!(
            scan.status(),
            ScanStatus {
                scanning: false,
                count: 0
            }
        );
        assert_eq!(scan_rows(&db).await, []);
        assert_eq!(track_count(&db).await, 0);
        assert!(!data_dir.scan_lock_path().exists());

        // A second start with the same config keeps the rows and their IDs.
        scan.stop().await;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        let ids: Vec<i64> = folder_rows(&db).await.iter().map(|row| row.id).collect();
        assert_eq!(ids, [1, 2]);
        scan.stop().await;
    }

    #[tokio::test]
    async fn start_scans_starts_the_scan_at_startup_and_does_not_wait_for_it() {
        let folder = music_folder(3);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        let (hooks, mut gate) = Gate::hooks();

        // The function returns while the scan runs.
        let scan = start(&config, &data_dir, &db, hooks).await;
        assert!(scan.status().scanning);
        assert_eq!(folder_rows(&db).await.len(), 1);
        gate.reached().await;
        assert_eq!(
            scan.status(),
            ScanStatus {
                scanning: true,
                count: 1
            }
        );
        // The scan holds the scan lock of the data directory, so
        // `wn-server scan` starts none.
        let lock_path = data_dir.scan_lock_path();
        assert!(ScanLock::try_acquire(&lock_path).expect("open").is_none());

        gate.open();
        ended(&scan).await;
        assert_eq!(
            scan.status(),
            ScanStatus {
                scanning: false,
                count: 3
            }
        );
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counts.added, 3);
        assert_eq!(rows[0].error, None);
        assert_eq!(track_count(&db).await, 3);
        assert!(ScanLock::try_acquire(&lock_path).expect("open").is_some());
        scan.stop().await;
    }

    #[tokio::test]
    async fn start_scans_keeps_the_tracks_of_a_folder_that_left_the_config() {
        let (old, new) = (music_folder(3), music_folder(0));
        let (_dir, data_dir, db) = library();
        let mut scanned = config(&[old.path()]);
        scanned.library.scan_on_start = true;
        let scan = start(&scanned, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        assert_eq!(track_count(&db).await, 3);

        // The config names another folder now. Storing the folders deletes
        // no row: the row of the old folder and its tracks stay.
        let mut config = config(&[new.path()]);
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        let paths: Vec<_> = folder_rows(&db)
            .await
            .into_iter()
            .map(|folder| folder.path)
            .collect();
        assert_eq!(paths, [old.path().to_path_buf(), new.path().to_path_buf()]);
        assert_eq!(track_count(&db).await, 3);
        assert_eq!(scan_rows(&db).await.len(), 1);
        scan.stop().await;

        // The scan at startup decides by the rules of the scanner. The new
        // folder holds no supported file, so the files of the tracks may be
        // on a volume that is not mounted, and the tracks stay.
        config.library.scan_on_start = true;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 2);
        let error = rows[1].error.as_deref().expect("the scan has an error");
        assert!(
            error.contains("3 tracks of the removed music folder"),
            "{error}"
        );
        assert_eq!(track_count(&db).await, 3);
        assert_eq!(folder_rows(&db).await.len(), 2);
    }

    #[tokio::test]
    async fn start_scans_with_a_rescan_interval_starts_a_scan_at_each_interval() {
        let folder = music_folder(1);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.rescan_interval_minutes = 1;

        // One minute is 20 ms here. No scan runs when the function returns:
        // the first one starts one interval later.
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        assert!(!scan.status().scanning);

        let two_scans = async {
            while scan_rows(&db).await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, two_scans)
            .await
            .expect("a second scan row appears");

        scan.stop().await;
        let rows = scan_rows(&db).await;
        assert!(rows.iter().all(|row| row.finished_at.is_some()), "{rows:?}");
        assert_eq!(rows[0].counts.added, 1);
        assert_eq!(track_count(&db).await, 1);
    }

    #[tokio::test]
    async fn scan_at_startup_removes_the_stale_files_of_the_cover_cache_of_the_data_directory() {
        let folder = music_folder(1);
        let cover = folder.path().join("cover.jpg");
        fs::write(&cover, "an image").expect("write the cover");
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;

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
        let cache = data_dir.covers_cache_dir();
        fs::create_dir_all(&cache).expect("create the cache");
        let stale = cache.join(format!("{album_id}-64-{cover_mtime}.jpg"));
        fs::write(&stale, "a resized cover").expect("write the cached cover");

        // The cover file is replaced by a newer one while the server is
        // down. The scan at the next startup removes the resized cover.
        fs::write(&cover, "another image").expect("replace the cover");
        let newer = u64::try_from(cover_mtime + 10).expect("a time");
        let file = fs::File::options()
            .write(true)
            .open(&cover)
            .expect("open the cover");
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(newer))
            .expect("set the time");
        drop(file);

        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        assert!(!stale.exists(), "{}", stale.display());
    }

    // A path that is not valid Unicode is built from raw bytes, which only
    // Unix paths allow.
    #[cfg(unix)]
    #[tokio::test]
    async fn start_scans_fails_when_a_folder_cannot_be_stored() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let (_dir, data_dir, db) = library();
        let folder = Path::new(OsStr::from_bytes(b"/music/\xff"));
        let mut config = config(&[folder]);
        config.library.scan_on_start = true;

        let minute = Duration::from_millis(20);
        let hooks = ScanHooks::default();
        let started = start_scans(
            &config,
            &data_dir,
            &db,
            CancellationToken::new(),
            minute,
            hooks,
        );
        let Err(error) = started.await else {
            panic!("the folder is stored");
        };
        assert!(matches!(error, ServerError::SyncFolders(_)), "{error:?}");
        assert!(
            error
                .to_string()
                .starts_with("cannot store the music folders in the database: "),
            "{error}"
        );
        assert_eq!(folder_rows(&db).await, []);
        assert_eq!(scan_rows(&db).await, []);
    }

    #[test]
    fn periodic_scans_are_off_by_default_and_follow_the_minutes() {
        let mut library = Config::default().library;
        assert_eq!(library.rescan_interval_minutes, 0);
        assert_eq!(rescan_interval(&library, MINUTE), None);

        library.rescan_interval_minutes = 1;
        assert_eq!(
            rescan_interval(&library, MINUTE),
            Some(Duration::from_secs(60))
        );
        library.rescan_interval_minutes = 90;
        assert_eq!(
            rescan_interval(&library, MINUTE),
            Some(Duration::from_secs(5400))
        );
        assert_eq!(
            rescan_interval(&library, Duration::from_millis(20)),
            Some(Duration::from_millis(1800))
        );
    }

    #[test]
    fn auth_disabled_warning_names_the_key_and_says_unsafe() {
        assert!(AUTH_DISABLED_WARNING.contains("auth.disabled_unsafe = true"));
        assert!(AUTH_DISABLED_WARNING.contains("unsafe"));
        assert!(!AUTH_DISABLED_WARNING.contains("  "));
    }

    #[test]
    fn default_config_binds_every_ipv4_address_on_port_4200() {
        assert_eq!(bind_addr(&Config::default()).to_string(), "0.0.0.0:4200");
    }

    #[test]
    fn bind_addr_follows_the_config() {
        let mut config = Config::default();
        config.server.bind = "::1".parse().expect("an address");
        config.server.port = 8080;
        assert_eq!(bind_addr(&config).to_string(), "[::1]:8080");
    }

    #[test]
    fn bind_error_names_the_address() {
        let error = ServerError::Bind {
            addr: "127.0.0.1:4200".parse().expect("an address"),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot listen on 127.0.0.1:4200: "),
            "{error}"
        );
    }
}
