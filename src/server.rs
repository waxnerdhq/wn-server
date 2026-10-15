//! The HTTP server: the state, the listener, the router and the stop
//! (`docs/design.md` section 3.13).

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use crate::api::{self, AppState};
use crate::config::{self, Config, DataDir, Library};
use crate::db::queries::{folders, users};
use crate::db::{Db, DbError};
use crate::library::scan_controller::{ScanController, ScanHooks, ScanSettings};
use crate::media::transcode::{FfmpegTranscoder, Transcoder};

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
    /// The stop cannot move the write-ahead log into the database file. The
    /// log stays next to the file and holds every change that was stored.
    #[error("cannot checkpoint the database: {0}")]
    Checkpoint(#[source] DbError),
}

/// A step of the stop that the caller of [`run_server`] supplies. It may
/// block its thread.
pub type StopStep = Box<dyn FnOnce() + Send>;

/// Whether [`run_server`] runs ffmpeg when the server starts.
///
/// `run_server` starts a program in one place: the probe of
/// `transcoding.ffmpeg`. The probe runs for [`FfmpegProbe::Run`] only, which
/// the `run` command passes. Every other caller gets the default and a
/// server that starts no program, whatever its config names and whatever
/// the machine has installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FfmpegProbe {
    /// `run_server` runs no program. The transcoder of the server has no
    /// target ([`FfmpegTranscoder::without_program`]): a request that needs
    /// a transcode gets the error of a server without ffmpeg, and the
    /// server serves original files.
    #[default]
    Skip,
    /// `run_server` probes the program of `transcoding.ffmpeg`
    /// ([`FfmpegTranscoder::probe`]): it runs the program twice and logs
    /// what it found.
    Run,
}

/// What the caller of [`run_server`] adds to the server. The default adds
/// nothing.
#[derive(Default)]
pub struct ServerOptions {
    /// Withdraws what advertises the server on the network. The `run`
    /// command passes the stop of the mDNS advertiser. `run_server` calls it
    /// on a blocking thread as soon as the server is asked to stop, and it
    /// closes the database only when the call has returned.
    pub withdraw: Option<StopStep>,
    /// Receives the address of the listener when the listener is bound. A
    /// test that binds port 0 reads the port here.
    pub listening: Option<oneshot::Sender<SocketAddr>>,
    /// Routes next to the API route. The server has none. A test adds a
    /// route that holds a request in progress.
    pub routes: Router<AppState>,
    /// The test hooks of the scans.
    pub scan: ScanHooks,
    /// Makes the stop take its short limits. `run_server` reads the token
    /// when the stop begins: steps 2 and 3 have
    /// `server.shutdown_grace_short_ms` each when it is cancelled, and
    /// `server.shutdown_grace_ms` otherwise. The `run` command cancels it
    /// before it asks the server to stop, when the operating system ends
    /// the process a fixed time after the stop signal. The default is a
    /// token that nothing cancels.
    pub hurry: CancellationToken,
    /// Whether the server probes ffmpeg when it starts. The `run` command
    /// passes [`FfmpegProbe::Run`]. The default is [`FfmpegProbe::Skip`], so
    /// a test that runs `run_server` starts no ffmpeg.
    pub probe: FfmpegProbe,
}

/// The time limit of step 2 and of step 3 of the stop.
///
/// This type decides which limit a stop has, and no part of it depends on
/// the operating system. The caller of [`run_server`] cancels
/// [`ServerOptions::hurry`], and the type reads the token.
#[derive(Debug, Clone)]
struct Grace {
    /// `server.shutdown_grace_ms`.
    long: Duration,
    /// `server.shutdown_grace_short_ms`.
    short: Duration,
    /// [`ServerOptions::hurry`].
    hurry: CancellationToken,
}

impl Grace {
    /// The limits of `server`, and the token that selects the short one.
    fn new(server: &config::Server, hurry: CancellationToken) -> Self {
        Self {
            long: Duration::from_millis(server.shutdown_grace_ms),
            short: Duration::from_millis(server.shutdown_grace_short_ms),
            hurry,
        }
    }

    /// The limit of a step that begins now: the short one when `hurry` is
    /// cancelled, and the long one otherwise.
    fn limit(&self) -> Duration {
        if self.hurry.is_cancelled() {
            self.short
        } else {
            self.long
        }
    }
}

/// The address the listener binds: `server.bind` and `server.port`.
pub fn bind_addr(config: &Config) -> SocketAddr {
    SocketAddr::new(config.server.bind, config.server.port)
}

/// Opens the database of `data_dir`, binds the address of `config`, logs one
/// line with the version, the data directory, the address, the number of
/// music folders and the authentication state, and serves the API until
/// `cancel` is cancelled.
///
/// The database and its owner account must exist. The function does not
/// create them, and it fails before it binds the address when one is
/// missing. When `auth.disabled_unsafe` is `true`, the function logs a
/// warning first.
///
/// A music folder of `config` that does not exist or cannot be read does not
/// keep the server from starting. The function logs one warning for each
/// such folder when it starts, with the path of the folder. No request logs
/// that line, so the log has it once for a run of the server. The server
/// serves the tracks of the other folders. The tracks of the folder stay in
/// the database, and a scan reads the folder when it is back
/// (`docs/design.md` section 3.10).
///
/// Before it serves the first request, the function stores the music folders
/// of `config` in the database and starts the scans that `config` asks for
/// (`start_scans`). It does not wait for a scan.
///
/// When `cancel` is cancelled, the function stops the server in four steps
/// and returns after the last one (`docs/design.md` section 3.13). Steps 1
/// and 2 start at once and run at the same time:
///
/// 1. It calls `options.withdraw` (`withdraw_at_stop`). The server accepts
///    no connection from this moment, so it is no longer advertised.
/// 2. It closes the listener. The requests in progress have the grace to
///    finish. Then the connections that are still open are closed
///    (`serve`).
/// 3. It waits for the running scan, which `cancel` has cancelled, to end as
///    a cancelled scan. It waits the length of the grace at most.
/// 4. When step 1 has ended, it checkpoints the write-ahead log and closes
///    the database.
///
/// The grace is `server.shutdown_grace_ms`. It is
/// `server.shutdown_grace_short_ms` when `options.hurry` is cancelled at the
/// moment the step begins (`Grace`).
///
/// Steps 3 and 4 are `stop`. A caller that drops the future during the
/// stop skips the steps that have not run. The database is consistent after
/// every step and between them.
pub async fn run_server(
    config: Config,
    data_dir: &DataDir,
    cancel: CancellationToken,
    options: ServerOptions,
) -> Result<(), ServerError> {
    if config.auth.disabled_unsafe {
        tracing::warn!("{AUTH_DISABLED_WARNING}");
    }
    config.library.warn_unavailable_folders();
    let db = open_database(data_dir).await?;
    let covers = crate::library::covers::Covers::new(
        data_dir.covers_cache_dir(),
        db.clone(),
        config.library.folders.clone(),
    );

    let addr = bind_addr(&config);
    let bind_error = |source| ServerError::Bind { addr, source };
    let listener = TcpListener::bind(addr).await.map_err(bind_error)?;
    // With port 0 the system chooses the port, so the log names the address
    // of the listener and not the address of the config.
    let local_addr = listener.local_addr().map_err(bind_error)?;

    // The address is bound first: a second server on the same data directory
    // fails above, stores no folder and starts no scan.
    //
    // `stopping` starts the parts of the stop that do not wait for the
    // requests: it cancels the scans, and it starts the step that withdraws
    // the advertisement. It is a child of `cancel`, so both happen when the
    // server is asked to stop. A server that stops for another reason
    // cancels it below.
    let stopping = cancel.child_token();
    let scan = start_scans(
        &config,
        data_dir,
        &db,
        stopping.clone(),
        MINUTE,
        options.scan,
    )
    .await?;
    tracing::info!(
        version = crate::VERSION,
        data_dir = %data_dir.path().display(),
        folders = config.library.folders.len(),
        auth = if config.auth.disabled_unsafe { "none" } else { "password" },
        "listening on {local_addr}"
    );
    let grace = Grace::new(&config.server, options.hurry);
    // This handle stays out of the state, so that the stop can close the
    // database when the router and its state are gone.
    let closing = db.clone();
    // The probe runs `ffmpeg -version` and `ffmpeg -encoders` once and logs
    // which formats the server can transcode to. A server without ffmpeg
    // serves original files, and so does a server whose caller asked for no
    // probe.
    let transcoder: Arc<dyn Transcoder> = Arc::new(match options.probe {
        FfmpegProbe::Run => FfmpegTranscoder::probe(&config.transcoding).await,
        FfmpegProbe::Skip => FfmpegTranscoder::without_program(&config.transcoding),
    });
    let streams = Arc::new(Semaphore::new(config.server.max_streams));

    // The fields follow the layout rule in the module comment of `api`.
    #[rustfmt::skip]
    let state = AppState {
        config: Arc::new(config),

        db,

        covers,

        transcoder,

        streams,

        scan: scan.clone(),

        cancel: cancel.clone(),

        started_at: Instant::now(),
    };

    let router = api::build_router_with(state, options.routes);
    if let Some(listening) = options.listening {
        // A receiver that is gone needs no address.
        let _ = listening.send(local_addr);
    }
    let withdrawn = tokio::spawn(withdraw_at_stop(stopping.clone(), options.withdraw));
    let served = serve(listener, router, &cancel, &grace).await;
    stopping.cancel();
    let stopped = stop(scan, withdrawn, closing, grace).await;
    tracing::info!("the server has stopped");
    served.map_err(ServerError::Serve)?;
    stopped
}

/// Step 1 of the stop. Waits until `stopping` is cancelled and then calls
/// `withdraw` on a blocking thread. The future ends when the call has
/// returned.
///
/// The step starts when the stop starts and does not wait for the requests
/// or the scan. The listener is closed at the same moment, so from then on
/// the advertisement would name a server that refuses every connection. A
/// process that is ended from outside during a later step has sent its
/// goodbye packet.
async fn withdraw_at_stop(stopping: CancellationToken, withdraw: Option<StopStep>) {
    stopping.cancelled().await;
    if let Some(withdraw) = withdraw
        && let Err(error) = tokio::task::spawn_blocking(withdraw).await
    {
        tracing::error!("the step that withdraws the advertisement failed: {error}");
    }
}

/// Step 2 of the stop. Serves `router` on `listener` until `cancel` is
/// cancelled. Then the listener is closed, so no connection is accepted. A
/// connection without a request in progress is closed at once. A request in
/// progress is answered, and its connection is closed after the response.
///
/// The function returns when every connection is closed. Connections that
/// are still open one limit of `grace` after the cancellation are closed
/// then, with one warning: the request of such a connection is dropped, and
/// its client reads the end of the connection in place of the rest of the
/// response. The limit is the one that `grace` has at the moment of the
/// cancellation.
async fn serve(
    listener: TcpListener,
    router: Router,
    cancel: &CancellationToken,
    grace: &Grace,
) -> io::Result<()> {
    let close = CancellationToken::new();
    let listener = ClosableListener {
        listener,
        close: close.clone(),
    };
    let mut served = axum::serve(listener, router)
        .with_graceful_shutdown(cancel.clone().cancelled_owned())
        .into_future();
    let grace_over = async {
        cancel.cancelled().await;
        let limit = grace.limit();
        tokio::time::sleep(limit).await;
        limit
    };
    let limit = tokio::select! {
        result = &mut served => return result,
        limit = grace_over => limit,
    };
    tracing::warn!(
        "requests were still in progress {limit:?} after the stop began; \
         their connections are closed"
    );
    close.cancel();
    served.await
}

/// Steps 3 and 4 of the stop. `withdrawn` is the task of step 1.
///
/// 3. Waits for the periodic scans and for the running scan to end, for one
///    limit of `grace` at most: the one that `grace` has when the function
///    is called. The token of the scans is cancelled, so the scanner stops
///    before its next file read and closes its `scans` row as `cancelled`.
///    A scan that has not ended after the limit is left behind with one
///    warning: it reads a file that does not answer, or one of its write
///    transactions takes longer. Its thread ends with the process, its
///    `scans` row keeps no end time, and the next scan finishes the work, as
///    after a process that was killed during a scan.
/// 4. Waits for step 1, so that the process does not exit before the goodbye
///    packet is sent. Then moves the write-ahead log into the database file
///    and closes the database. `db` is the last handle of the database when
///    no request is in progress and no scan was left behind, so its
///    connections close here. A checkpoint that another process blocks is
///    logged and is no error: that process holds the log.
async fn stop(
    scan: ScanController,
    withdrawn: JoinHandle<()>,
    db: Db,
    grace: Grace,
) -> Result<(), ServerError> {
    let scan_limit = grace.limit();
    if scan.status().scanning {
        tracing::info!("waiting for the running scan to stop");
    }
    if tokio::time::timeout(scan_limit, scan.stop()).await.is_err() {
        tracing::warn!(
            "the scan has not stopped after {scan_limit:?}; the server stops without it, \
             and the next scan finishes its work"
        );
    }
    // The controller holds a handle of the database.
    drop(scan);

    if let Err(error) = withdrawn.await {
        tracing::error!("the task that withdraws the advertisement failed: {error}");
    }

    checkpointed(db.close().await)
}

/// The result of the stop for the result of its checkpoint.
fn checkpointed(closed: Result<(), DbError>) -> Result<(), ServerError> {
    match closed {
        Ok(()) => Ok(()),
        Err(DbError::CheckpointBusy) => {
            tracing::info!(
                "another process uses the database, so the write-ahead log was not moved \
                 into the database file; the log holds every change"
            );
            Ok(())
        }
        Err(error) => Err(ServerError::Checkpoint(error)),
    }
}

/// The listener of the server. A connection that it accepted ends when
/// `close` is cancelled.
///
/// `axum::serve` runs each connection in a task of its own and gives the
/// caller no handle of that task. The stop of the server ends a connection
/// through its stream instead: every read and write of a
/// [`ClosableStream`] fails after the cancellation, and the task of the
/// connection ends with that error.
struct ClosableListener {
    listener: TcpListener,
    close: CancellationToken,
}

impl Listener for ClosableListener {
    type Io = ClosableStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // The implementation of axum for `TcpListener` logs an error of the
        // system and tries again.
        let (stream, addr) = Listener::accept(&mut self.listener).await;
        let closed = Box::pin(self.close.clone().cancelled_owned());
        (ClosableStream { stream, closed }, addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// A connection of a [`ClosableListener`].
struct ClosableStream {
    stream: TcpStream,
    /// Completes when the token of the listener is cancelled.
    closed: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl ClosableStream {
    /// Fails when the token of the listener is cancelled. Otherwise the task
    /// that polls the connection is woken when the token is cancelled, so
    /// that it polls again and gets the error.
    fn poll_open(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        match self.closed.as_mut().poll(cx) {
            Poll::Ready(()) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the server stopped",
            )),
            Poll::Pending => Ok(()),
        }
    }
}

impl AsyncRead for ClosableStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.poll_open(cx)?;
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for ClosableStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_open(cx)?;
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_open(cx)?;
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_open(cx)?;
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_open(cx)?;
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
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
/// A folder of the config that does not exist or cannot be read is stored
/// like every other folder: it gets a row when it has none, and it keeps the
/// row and the tracks it has. The function reads no folder. The scan treats
/// such a folder as an unread root and deletes no track.
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
    use crate::library::scanner::{self, CANCELLED, ScanOptions};
    use tokio::sync::mpsc::unbounded_channel;

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
        run_server(
            Config::default(),
            data_dir,
            cancel,
            ServerOptions::default(),
        )
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

    /// The index entries of the tracks, in the order of their folders and
    /// their paths.
    async fn index_by_folder(db: &Db) -> Vec<tracks::IndexEntry> {
        let mut entries = db.read(tracks::load_index).await.expect("read the index");
        entries.sort_by(|a, b| (a.folder_id, &a.rel_path).cmp(&(b.folder_id, &b.rel_path)));
        entries
    }

    #[tokio::test]
    async fn start_scans_stores_the_row_of_a_folder_that_does_not_exist() {
        let present = music_folder(2);
        let volumes = TempDir::new().expect("a temporary directory");
        let missing = volumes.path().join("unplugged");
        let (_dir, data_dir, db) = library();
        let mut config = config(&[present.path(), &missing]);

        // The folder that does not exist gets its row, so the folder list of
        // the server names it before a scan and without one.
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        let rows = folder_rows(&db).await;
        let stored: Vec<_> = rows
            .iter()
            .map(|folder| (folder.id, folder.path.as_path(), folder.name.as_str()))
            .collect();
        let name = present.path().file_name().expect("a name");
        let name = name.to_str().expect("a UTF-8 name");
        assert_eq!(
            stored,
            [
                (1, present.path(), name),
                (2, missing.as_path(), "unplugged")
            ]
        );
        assert_eq!(scan_rows(&db).await, []);
        scan.stop().await;

        // The scan at startup reads the folder that exists and records the
        // other one as an error.
        config.library.scan_on_start = true;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        let scans = scan_rows(&db).await;
        assert_eq!(scans.len(), 1);
        assert_eq!((scans[0].counts.seen, scans[0].counts.added), (2, 2));
        let error = scans[0].error.as_deref().expect("the scan has an error");
        assert!(
            error.contains(&format!("\"unplugged\" ({})", missing.display())),
            "{error}"
        );
        assert!(error.contains("cannot be read"), "{error}");
        assert_eq!(track_count(&db).await, 2);
        assert_eq!(folder_rows(&db).await, rows);
    }

    #[tokio::test]
    async fn start_scans_keeps_the_row_and_the_tracks_of_a_folder_that_became_unavailable() {
        let (present, away) = (music_folder(2), music_folder(3));
        let (_dir, data_dir, db) = library();
        let mut config = config(&[present.path(), away.path()]);
        config.library.scan_on_start = true;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        let rows = folder_rows(&db).await;
        let tracks = index_by_folder(&db).await;
        assert_eq!((rows.len(), tracks.len()), (2, 5));
        assert!(config.library.unavailable_folders().is_empty());

        // The volume of the second folder is unplugged: its path does not
        // exist when the server starts again.
        let volumes = TempDir::new().expect("a temporary directory");
        let unplugged = volumes.path().join("unplugged");
        fs::rename(away.path(), &unplugged).expect("move the folder away");
        let unavailable = config.library.unavailable_folders();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].path, away.path());

        // A server without a scan at startup stores the folders only. The
        // row of the folder stays, and so does every track.
        config.library.scan_on_start = false;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        scan.stop().await;
        assert_eq!(folder_rows(&db).await, rows);
        assert_eq!(index_by_folder(&db).await, tracks);
        assert_eq!(scan_rows(&db).await.len(), 1);

        // The scan at startup reads the other folder, deletes nothing and
        // records an error that names the folder.
        config.library.scan_on_start = true;
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        let scans = scan_rows(&db).await;
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[1].counts.seen, 2);
        assert_eq!(scans[1].counts.removed, 0);
        let error = scans[1].error.as_deref().expect("the scan has an error");
        assert!(
            error.contains(&away.path().display().to_string()),
            "{error}"
        );
        assert!(error.contains("cannot be read"), "{error}");
        assert!(error.contains("no track was deleted"), "{error}");
        assert_eq!(folder_rows(&db).await, rows);
        assert_eq!(index_by_folder(&db).await, tracks);

        // The volume is back. The scan at the next startup finds every file
        // at the path of its row, so every track keeps its ID.
        fs::rename(&unplugged, away.path()).expect("move the folder back");
        let scan = start(&config, &data_dir, &db, ScanHooks::default()).await;
        ended(&scan).await;
        scan.stop().await;
        let scans = scan_rows(&db).await;
        assert_eq!(scans.len(), 3);
        assert_eq!(scans[2].error, None);
        assert_eq!(scans[2].counts.seen, 5);
        assert_eq!(
            (
                scans[2].counts.added,
                scans[2].counts.moved,
                scans[2].counts.removed
            ),
            (0, 0, 0)
        );
        assert_eq!(folder_rows(&db).await, rows);
        assert_eq!(index_by_folder(&db).await, tracks);
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

    /// The scan removes the resized covers of the tracks that have no row.
    /// The removal is a part of the scan, which runs on a thread of its own:
    /// the server has started, and it reads its database, while every file
    /// is still in the cache.
    #[tokio::test]
    async fn scan_at_startup_removes_the_covers_of_tracks_without_a_row_after_the_start() {
        let folder = music_folder(1);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;

        // The cache holds the covers of 1000 tracks that no row has, as a
        // server leaves them whose scans removed no cover of a track, and
        // the cover of an album.
        let cache = data_dir.covers_cache_dir();
        fs::create_dir_all(&cache).expect("create the cache");
        let stale: Vec<PathBuf> = (0..1000)
            .map(|track| cache.join(format!("tr-{track:032x}-300-1000.jpg")))
            .collect();
        for path in &stale {
            fs::write(path, "a resized cover").expect("write the cached cover");
        }
        let of_album = cache.join("al-00000000000000000000000000000001-300-1000.jpg");
        fs::write(&of_album, "a resized cover").expect("write the cached cover");

        // The function returns while the scan runs, and the scan has removed
        // no file when it has stored its first batch.
        let (hooks, mut gate) = Gate::hooks();
        let scan = start(&config, &data_dir, &db, hooks).await;
        gate.reached().await;
        assert!(scan.status().scanning);
        assert!(stale.iter().all(|path| path.exists()));
        assert_eq!(folder_rows(&db).await.len(), 1);
        assert_eq!(track_count(&db).await, 1);

        gate.open();
        ended(&scan).await;
        scan.stop().await;
        let left: Vec<&PathBuf> = stale.iter().filter(|path| path.exists()).collect();
        assert!(left.is_empty(), "{left:?}");
        // The cover of the album stays, and so does the music folder.
        assert!(of_album.exists());
        assert_eq!(fs::read_dir(folder.path()).expect("list").count(), 1);
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

    /// The size of the write-ahead log of the database of `data_dir`, and 0
    /// when the file does not exist.
    fn wal_len(data_dir: &DataDir) -> u64 {
        let mut name = data_dir.db_path().into_os_string();
        name.push("-wal");
        fs::metadata(name).map_or(0, |file| file.len())
    }

    /// The longest time a test of the stop waits for an event, and the time
    /// that such a test gives a stop for a scan that ends. Each of these
    /// waits ends with its event, after milliseconds, so the limit does not
    /// delay a passing run. It is 10 s, far above what a loaded machine
    /// needs for a checkpoint of a small file, so that it only ends a test
    /// whose stop hangs.
    const LIMIT: Duration = Duration::from_secs(10);

    /// The grace of a stop whose two limits are `limit`, so the stop has
    /// that limit with and without [`ServerOptions::hurry`].
    fn fixed(limit: Duration) -> Grace {
        Grace {
            long: limit,
            short: limit,
            hurry: CancellationToken::new(),
        }
    }

    /// The task of step 1 of a stop that has nothing to withdraw.
    fn nothing_to_withdraw(stopping: &CancellationToken) -> JoinHandle<()> {
        tokio::spawn(withdraw_at_stop(stopping.clone(), None))
    }

    /// Waits until no scan of `scan` runs. The scan of a test ends
    /// milliseconds after the test lets it go on.
    async fn idle(scan: &ScanController) {
        let idle = async {
            while scan.status().scanning {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(LIMIT, idle)
            .await
            .expect("the scan ends");
    }

    /// The index entries of the tracks, in the order of their paths.
    async fn index(db: &Db) -> Vec<tracks::IndexEntry> {
        let mut entries = db.read(tracks::load_index).await.expect("read the index");
        entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        entries
    }

    /// Checks the database file with SQLite: `PRAGMA integrity_check`
    /// answers `ok`, and `PRAGMA foreign_key_check` finds no row whose
    /// folder, album or artist is missing.
    async fn assert_sound(db: &Db) {
        let (integrity, orphans) = db
            .read(|conn| {
                let integrity: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                let mut check = conn.prepare("PRAGMA foreign_key_check")?;
                let orphans = check.query_map([], |_| Ok(()))?.count();
                Ok((integrity, orphans))
            })
            .await
            .expect("check the database");
        assert_eq!(integrity, "ok");
        assert_eq!(orphans, 0);
    }

    #[tokio::test]
    async fn cancelling_the_shared_token_stops_the_running_scan_as_cancelled() {
        let folder = music_folder(3);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        // The token of the server. The token of the scans is its child, as
        // in `run_server`. The hook cancels the token of the server when the
        // first batch of two files is committed.
        let shared = CancellationToken::new();
        let stopping = shared.child_token();
        let hooks = ScanHooks {
            batch_size: Some(2),
            on_batch: Some(Arc::new({
                let shared = shared.clone();
                move || shared.cancel()
            })),
        };
        let minute = Duration::from_millis(20);
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");

        // The server stops when its token is cancelled, which the hook
        // does. The stop waits for the scan, which ends before its second
        // batch.
        tokio::time::timeout(LIMIT, shared.cancelled())
            .await
            .expect("the hook cancels the token");
        let withdrawn = nothing_to_withdraw(&stopping);
        tokio::time::timeout(LIMIT, stop(scan, withdrawn, db.clone(), fixed(LIMIT)))
            .await
            .expect("the stop ends")
            .expect("the stop succeeds");

        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].finished_at.is_some());
        assert_eq!(rows[0].error.as_deref(), Some(CANCELLED));
        assert_eq!(rows[0].counts.added, 2);
        assert_eq!(track_count(&db).await, 2);
        // The scan lock is free for the next start.
        let lock_path = data_dir.scan_lock_path();
        assert!(ScanLock::try_acquire(&lock_path).expect("open").is_some());
    }

    #[tokio::test]
    async fn stop_withdraws_at_once_and_checkpoints_after_the_scan_and_the_withdrawal() {
        let folder = music_folder(3);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        let (hooks, mut gate) = Gate::hooks();
        let stopping = CancellationToken::new();
        let minute = Duration::from_millis(20);
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");
        gate.reached().await;

        // The step that withdraws the advertisement tells the test that it
        // runs, and it returns when the test drops `release`.
        let (entered, mut step_entered) = unbounded_channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let withdraw: StopStep = Box::new(move || {
            entered.send(()).expect("the test listens");
            let _ = released.recv();
        });
        let withdrawn = tokio::spawn(withdraw_at_stop(stopping.clone(), Some(withdraw)));

        // Nothing is withdrawn while the server runs.
        settle().await;
        assert!(step_entered.try_recv().is_err());

        // Step 1 starts when the stop starts. It does not wait for the scan,
        // which the hook of its first batch still holds.
        stopping.cancel();
        tokio::time::timeout(LIMIT, step_entered.recv())
            .await
            .expect("the step starts with the stop")
            .expect("the step is alive");
        assert!(scan.status().scanning);

        // Step 3 waits for the scan.
        let stopped = tokio::spawn(stop(scan.clone(), withdrawn, db.clone(), fixed(LIMIT)));
        settle().await;
        assert!(!stopped.is_finished());

        // The scan ends. Step 1 has not ended, so the stop has not
        // checkpointed the log and does not end: the process would exit
        // during the goodbye packet.
        gate.open();
        idle(&scan).await;
        settle().await;
        assert!(!stopped.is_finished());
        assert!(wal_len(&data_dir) > 0);

        // Step 1 ends, and step 4 moves the log into the database file.
        drop(release);
        tokio::time::timeout(LIMIT, stopped)
            .await
            .expect("the stop ends")
            .expect("the stop does not panic")
            .expect("the stop succeeds");
        assert_eq!(wal_len(&data_dir), 0);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].finished_at.is_some());
        assert_eq!(rows[0].error.as_deref(), Some(CANCELLED));
        assert_eq!(track_count(&db).await, 1);
    }

    #[tokio::test]
    async fn stop_leaves_a_scan_behind_that_does_not_end_within_the_limit() {
        let folder = music_folder(3);
        let (_dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        let (hooks, mut gate) = Gate::hooks();
        let stopping = CancellationToken::new();
        let minute = Duration::from_millis(20);
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");
        gate.reached().await;
        // The scan has stored its first batch of one file. The hook of the
        // batch holds it from here on, as a file read that does not return
        // holds a scan.
        let committed = index(&db).await;
        assert_eq!(committed.len(), 1);

        stopping.cancel();
        let limit = Duration::from_millis(50);
        let began = Instant::now();
        let withdrawn = nothing_to_withdraw(&stopping);
        let stopping_now = stop(scan.clone(), withdrawn, db.clone(), fixed(limit));
        tokio::time::timeout(LIMIT, stopping_now)
            .await
            .expect("the stop ends without the scan")
            .expect("the stop succeeds");
        // The stop gave the scan its limit. A wait does not end early, so a
        // slow machine cannot fail this.
        assert!(began.elapsed() >= limit, "{:?}", began.elapsed());

        // The scan is left as a process that is killed leaves it: it has
        // not ended, and its row has no end time and no error.
        assert!(scan.status().scanning);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].finished_at, None);
        assert_eq!(rows[0].error, None);
        // Step 4 ran: the log is in the database file. The file is sound,
        // and its tracks are those of the batch that the scan committed.
        assert_eq!(wal_len(&data_dir), 0);
        assert_sound(&db).await;
        assert_eq!(index(&db).await, committed);

        // The next scan finishes the work, as the scan at the next start
        // does: it adds the two other files and keeps the row of the first.
        let next = tokio::task::spawn_blocking({
            let (db, folders) = (db.clone(), config.library.folders.clone());
            let never = CancellationToken::new();
            move || scanner::scan(&db, &folders, &never, false, ScanOptions::default())
        })
        .await
        .expect("the scan does not panic")
        .expect("the scan succeeds");
        assert_eq!((next.seen, next.added, next.changed), (3, 2, 0));
        assert_eq!((next.moved, next.removed), (0, 0));
        assert_eq!(next.errors, [""; 0]);
        let complete = index(&db).await;
        assert_eq!(complete.len(), 3);
        assert!(complete.contains(&committed[0]), "{complete:?}");
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 2);
        assert!(rows[1].finished_at.is_some());
        assert_eq!(rows[1].error, None);
        // The album rows count the tracks that refer to them.
        let (counted, tracks): (i64, i64) = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT (SELECT sum(song_count) FROM albums), (SELECT count(*) FROM tracks)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .expect("count the songs");
        assert_eq!((counted, tracks), (3, 3));
        assert_sound(&db).await;

        // A process ends the thread of the scan by exiting. The test lets
        // it end, so that it does not outlive the database file.
        gate.open();
        idle(&scan).await;
    }

    /// One hour: a limit that no test waits for.
    const HOUR: Duration = Duration::from_secs(3600);

    /// A scan at startup that the hook of its first batch holds, as a file
    /// read that does not return holds a scan.
    struct HeldScan {
        scan: ScanController,
        /// Lets the scan go on.
        gate: Gate,
        /// The token of the scans, which the stop cancels.
        stopping: CancellationToken,
        db: Db,
        data_dir: DataDir,
        /// The music folder and the data directory.
        _dirs: [TempDir; 2],
    }

    /// Starts the scan at startup of a server with a music folder of three
    /// files, and returns when the scan has stored its first batch of one
    /// file and waits in the hook.
    async fn held_scan() -> HeldScan {
        let folder = music_folder(3);
        let (dir, data_dir, db) = library();
        let mut config = config(&[folder.path()]);
        config.library.scan_on_start = true;
        let (hooks, mut gate) = Gate::hooks();
        let stopping = CancellationToken::new();
        let minute = Duration::from_millis(20);
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");
        gate.reached().await;
        HeldScan {
            scan,
            gate,
            stopping,
            db,
            data_dir,
            _dirs: [folder, dir],
        }
    }

    #[tokio::test]
    async fn stop_with_hurry_leaves_a_held_scan_behind_after_the_short_limit() {
        let HeldScan {
            scan,
            gate,
            stopping,
            db,
            data_dir,
            _dirs,
        } = held_scan().await;
        let committed = index(&db).await;
        assert_eq!(committed.len(), 1);

        // The long limit is one hour. The signal has a short limit, so the
        // caller has cancelled `hurry` before it asks the server to stop.
        let short = Duration::from_millis(50);
        let grace = Grace {
            long: HOUR,
            short,
            hurry: CancellationToken::new(),
        };
        grace.hurry.cancel();
        stopping.cancel();
        let began = Instant::now();
        let withdrawn = nothing_to_withdraw(&stopping);
        tokio::time::timeout(LIMIT, stop(scan.clone(), withdrawn, db.clone(), grace))
            .await
            .expect("the stop ends without the scan, long before the long limit")
            .expect("the stop succeeds");
        // The stop gave the scan the short limit. A wait does not end
        // early, so a slow machine cannot fail this.
        assert!(began.elapsed() >= short, "{:?}", began.elapsed());

        // The scan is left behind as after the long limit: it has not
        // ended, its row has no end time, the log is in the database file,
        // and the file holds the batch that the scan committed.
        assert!(scan.status().scanning);
        let rows = scan_rows(&db).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].finished_at, rows[0].error.as_deref()),
            (None, None)
        );
        assert_eq!(wal_len(&data_dir), 0);
        assert_sound(&db).await;
        assert_eq!(index(&db).await, committed);

        gate.open();
        idle(&scan).await;
    }

    #[tokio::test]
    async fn stop_without_hurry_waits_for_a_held_scan_past_the_short_limit() {
        let HeldScan {
            scan,
            gate,
            stopping,
            db,
            data_dir,
            _dirs,
        } = held_scan().await;

        // The stop after Ctrl-C and after SIGTERM: nothing cancels `hurry`.
        // The short limit is 1 ms and is over long before the wait below
        // ends, so a stop that took it would have ended.
        let grace = Grace {
            long: HOUR,
            short: Duration::from_millis(1),
            hurry: CancellationToken::new(),
        };
        stopping.cancel();
        let withdrawn = nothing_to_withdraw(&stopping);
        let stopped = tokio::spawn(stop(scan.clone(), withdrawn, db.clone(), grace));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!stopped.is_finished());
        assert!(scan.status().scanning);
        assert!(wal_len(&data_dir) > 0);

        // The scan ends, and the stop goes on to its last step.
        gate.open();
        tokio::time::timeout(LIMIT, stopped)
            .await
            .expect("the stop ends with the scan")
            .expect("the stop does not panic")
            .expect("the stop succeeds");
        assert!(!scan.status().scanning);
        assert_eq!(wal_len(&data_dir), 0);
        let rows = scan_rows(&db).await;
        assert_eq!(rows[0].error.as_deref(), Some(CANCELLED));
    }

    #[test]
    fn grace_is_the_long_limit_until_hurry_is_cancelled() {
        let hurry = CancellationToken::new();
        let grace = Grace::new(&Config::default().server, hurry.clone());
        // The stop after every signal of Linux and macOS, and after Ctrl-C
        // and Ctrl-Break on Windows.
        assert_eq!(grace.limit(), Duration::from_secs(10));
        // The stop after the close and the shutdown of a Windows console.
        hurry.cancel();
        assert_eq!(grace.limit(), Duration::from_millis(1500));

        let mut server = Config::default().server;
        server.shutdown_grace_ms = 70;
        server.shutdown_grace_short_ms = 30;
        let hurry = CancellationToken::new();
        let grace = Grace::new(&server, hurry.clone());
        assert_eq!(grace.limit(), Duration::from_millis(70));
        hurry.cancel();
        assert_eq!(grace.limit(), Duration::from_millis(30));
    }

    #[test]
    fn short_limits_reach_the_checkpoint_two_seconds_before_windows_ends_the_process() {
        // Windows ends a console process 5 s after the close event and
        // after the shutdown event (`docs/design.md` section 3.13).
        let windows_limit = Duration::from_secs(5);
        // Step 1 runs during steps 2 and 3 and takes 2.2 s at most
        // (`Advertiser::stop`).
        let withdrawal = Duration::from_millis(2200);

        let hurry = CancellationToken::new();
        hurry.cancel();
        let grace = Grace::new(&Config::default().server, hurry);
        // Steps 2 and 3 run one after the other with one limit each. Step 4
        // begins when steps 1 and 3 have ended.
        let checkpoint_begins = (2 * grace.limit()).max(withdrawal);
        assert_eq!(checkpoint_begins, Duration::from_secs(3));
        assert!(windows_limit - checkpoint_begins >= Duration::from_secs(2));
    }

    #[tokio::test]
    async fn stop_of_an_idle_server_checkpoints_the_database() {
        let folder = music_folder(1);
        let (_dir, data_dir, db) = library();
        let stopping = CancellationToken::new();
        let minute = Duration::from_millis(20);
        let config = config(&[folder.path()]);
        let hooks = ScanHooks::default();
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");
        assert!(wal_len(&data_dir) > 0);

        stopping.cancel();
        let withdrawn = nothing_to_withdraw(&stopping);
        tokio::time::timeout(LIMIT, stop(scan, withdrawn, db.clone(), fixed(LIMIT)))
            .await
            .expect("the stop ends")
            .expect("the stop succeeds");

        assert_eq!(wal_len(&data_dir), 0);
        assert_eq!(scan_rows(&db).await, []);
        assert_eq!(folder_rows(&db).await.len(), 1);
    }

    #[tokio::test]
    async fn stop_succeeds_when_another_connection_blocks_the_checkpoint() {
        let folder = music_folder(1);
        let (_dir, data_dir, db) = library();
        let stopping = CancellationToken::new();
        let minute = Duration::from_millis(20);
        let config = config(&[folder.path()]);
        let hooks = ScanHooks::default();
        let scan = start_scans(&config, &data_dir, &db, stopping.clone(), minute, hooks)
            .await
            .expect("the scans start");
        // The checkpoint waits 20 ms for the other connection, not the 5 s
        // of the server.
        db.write(|conn| Ok(conn.busy_timeout(Duration::from_millis(20))?))
            .await
            .expect("set the timeout");

        // A read transaction of a connection outside the handle pins the
        // log, as a `wn-server scan` that runs does.
        let outside = rusqlite::Connection::open(data_dir.db_path()).expect("open the database");
        outside.execute_batch("BEGIN").expect("begin");
        let folders: i64 = outside
            .query_row("SELECT count(*) FROM folders", [], |row| row.get(0))
            .expect("count the folders");
        assert_eq!(folders, 1);

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let withdraw: StopStep = Box::new({
            let ran = Arc::clone(&ran);
            move || ran.store(true, std::sync::atomic::Ordering::SeqCst)
        });
        stopping.cancel();
        let withdrawn = tokio::spawn(withdraw_at_stop(stopping.clone(), Some(withdraw)));
        tokio::time::timeout(LIMIT, stop(scan, withdrawn, db.clone(), fixed(LIMIT)))
            .await
            .expect("the stop ends")
            .expect("a blocked checkpoint is no error");
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        // The log holds the rows, and the database reads them.
        assert!(wal_len(&data_dir) > 0);
        assert_eq!(folder_rows(&db).await.len(), 1);
    }

    #[test]
    fn checkpoint_error_other_than_a_blocked_one_is_the_error_of_the_stop() {
        assert!(checkpointed(Ok(())).is_ok());
        assert!(checkpointed(Err(DbError::CheckpointBusy)).is_ok());

        let error = checkpointed(Err(DbError::Cancelled)).expect_err("the checkpoint failed");
        assert!(matches!(error, ServerError::Checkpoint(_)), "{error:?}");
        assert_eq!(
            error.to_string(),
            "cannot checkpoint the database: the runtime shut down before the database task ran"
        );
    }

    #[test]
    fn server_options_add_nothing_by_default() {
        let options = ServerOptions::default();
        assert!(options.withdraw.is_none());
        assert!(options.listening.is_none());
        assert!(!options.routes.has_routes());
        assert!(options.scan.batch_size.is_none());
        assert!(options.scan.on_batch.is_none());
        // Nothing holds a clone of the token, so nothing cancels it.
        assert!(!options.hurry.is_cancelled());
        // No program runs.
        assert_eq!(options.probe, FfmpegProbe::Skip);
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
    fn default_config_binds_every_ipv4_address_on_port_4533() {
        assert_eq!(bind_addr(&Config::default()).to_string(), "0.0.0.0:4533");
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
            addr: "127.0.0.1:4533".parse().expect("an address"),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot listen on 127.0.0.1:4533: "),
            "{error}"
        );
    }
}
