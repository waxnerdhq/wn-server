//! The stop of a running server over a real connection (`docs/design.md`
//! section 3.13), and the start of a server whose music folder is
//! unavailable (section 3.10).
//!
//! The tests of this file and four tests of `tests/transcode.rs` are the only
//! ones of the suite that connect to a socket. Each one runs `run_server` on
//! `127.0.0.1` with port 0, so the system chooses a free port, and speaks
//! HTTP/1.1 over a `TcpStream`. No test starts the mDNS daemon: the step that
//! withdraws the advertisement is a closure of the test.
//!
//! A test holds a request in progress with a route that it adds to the
//! server, and a scan with the batch hook of the scanner. The tests assert
//! the order of events: what has happened when another thing happens. The
//! times of the file are the limit that ends a test whose server hangs
//! ([`LIMIT`]), waits that must not end early (a grace of 100 ms, 120 ms or
//! 130 ms), and short waits in which a server that must wait does not end
//! ([`SHORT`]).
//!
//! Two tests require that something is done within a time, because the
//! short limits of a stop exist to meet a time: the 5 s after which Windows
//! ends a console process ([`WINDOWS_LIMIT`]). Their servers have a grace of
//! one hour and a short grace of 120 ms or 130 ms. One requires that a
//! request is dropped within 1 s. The other requires that a stop with two
//! waits of 130 ms ends within 2.26 s, which is the 5 s less the 2.74 s by
//! which the default short grace makes the two waits longer. Each bound is
//! many times what the work takes.
//!
//! The log of the test process is one buffer for every test of the file, and
//! a test counts the warnings of its server by the length of the grace in
//! them. The servers that log such a warning therefore have graces of
//! different lengths.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use futures_util::StreamExt;
use futures_util::stream;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Mutex, MutexGuard, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wn_server::api::AppState;
use wn_server::config::{Config, DataDir};
use wn_server::db::Db;
use wn_server::db::queries::scans::{self, Scan};
use wn_server::db::queries::{tracks, users};
use wn_server::library::scan_controller::ScanHooks;
use wn_server::library::scanner::CANCELLED;
use wn_server::server::{ServerError, ServerOptions, StopStep, run_server};

use common::transcoder::no_ffmpeg;
use common::{
    API_VERSION, CLIENT, LibraryBuilder, OWNER, OWNER_PASSWORD, captured_log, fixtures_dir,
    subsonic_response, token, urlencode,
};

/// The longest time a test waits for an event of its server. Each of these
/// waits ends with its event, milliseconds after the cause of the event, so
/// the limit does not delay a passing run. It is 10 s, far above the time
/// that a loaded machine needs for an answer on loopback or for a checkpoint
/// of a small file, so it only ends a test whose server hangs.
const LIMIT: Duration = Duration::from_secs(10);

/// The time after which a connection attempt without an answer counts as
/// refused. Linux and macOS refuse a connection to a closed loopback port at
/// once. Windows answers after about two seconds.
const REFUSAL: Duration = Duration::from_secs(1);

/// A time in which a server that must wait does not end. A server that ends
/// in it fails the test, so a slow machine cannot fail the test.
const SHORT: Duration = Duration::from_millis(50);

/// The grace of the servers of this file, in milliseconds: one hour. No test
/// waits that long, so a server that ends within [`LIMIT`] did not wait for
/// its grace.
const HOUR_MS: u64 = 3_600_000;

/// The time after which Windows ends a console process that received the
/// close event (`docs/design.md` section 3.13).
const WINDOWS_LIMIT: Duration = Duration::from_secs(5);

/// One test of this file runs at a time. A test checks that the port of its
/// server refuses a connection after the listener is closed, and the server
/// of another test must not bind that port in the meantime.
static ONE_SERVER: Mutex<()> = Mutex::const_new(());

async fn one_server() -> MutexGuard<'static, ()> {
    ONE_SERVER.lock().await
}

/// A server that `run_server` runs in a task.
struct Running {
    addr: SocketAddr,
    /// The token that asks the server to stop.
    cancel: CancellationToken,
    task: JoinHandle<Result<(), ServerError>>,
    data_dir: DataDir,
    /// The data directory. Dropping the value deletes it.
    dir: TempDir,
}

impl Running {
    /// Asks the server to stop and waits for its end in order. Returns the
    /// data directory, on which the test runs its next server.
    async fn stopped(mut self) -> TempDir {
        self.cancel.cancel();
        self.ended().await.expect("the server stops in order");
        self.dir
    }

    /// Waits for `run_server` to return and returns its result.
    async fn ended(&mut self) -> Result<(), ServerError> {
        tokio::time::timeout(LIMIT, &mut self.task)
            .await
            .expect("the server ends")
            .expect("the server does not panic")
    }

    /// Checks that the server does not end within [`SHORT`].
    async fn does_not_end(&mut self) {
        let ended = tokio::time::timeout(SHORT, &mut self.task).await;
        assert!(ended.is_err(), "the server ended: {ended:?}");
    }

    /// The size of the write-ahead log of the database, and 0 when the file
    /// does not exist.
    fn wal_len(&self) -> u64 {
        wal_len(&self.data_dir)
    }
}

/// The size of the write-ahead log of the database of `data_dir`, and 0 when
/// the file does not exist.
fn wal_len(data_dir: &DataDir) -> u64 {
    let mut name = data_dir.db_path().into_os_string();
    name.push("-wal");
    fs::metadata(name).map_or(0, |file| file.len())
}

/// The config of the servers of this file: `127.0.0.1` with a port that the
/// system chooses, a grace of one hour, no scan at startup and no ffmpeg.
fn config() -> Config {
    let mut config = Config::default();
    config.server.bind = "127.0.0.1".parse().expect("an address");
    config.server.port = 0;
    config.server.shutdown_grace_ms = HOUR_MS;
    config.library.scan_on_start = false;
    // The server probes this path for ffmpeg and finds no program, so no
    // test of this file starts the ffmpeg of the machine.
    config.transcoding.ffmpeg = no_ffmpeg();
    config
}

/// Creates a data directory whose database holds the owner account, and
/// runs `run_server` on it with `config`, `cancel` and `options`. Returns
/// when the listener is bound.
async fn start(config: Config, cancel: CancellationToken, options: ServerOptions) -> Running {
    start_in(new_data_dir().await, config, cancel, options).await
}

/// Creates a data directory whose database holds the owner account.
async fn new_data_dir() -> TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
    let db = Db::open(data_dir.db_path()).expect("create the database");
    db.write(|conn| users::insert_owner(conn, OWNER, OWNER_PASSWORD))
        .await
        .expect("insert the owner")
        .expect("a new database has no owner");
    // The server opens the database itself.
    drop(db);
    dir
}

/// Runs `run_server` on the data directory `dir` with `config`, `cancel` and
/// `options`. The database of the directory holds the owner account. Returns
/// when the listener is bound.
async fn start_in(
    dir: TempDir,
    config: Config,
    cancel: CancellationToken,
    mut options: ServerOptions,
) -> Running {
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");

    let (listening, bound) = oneshot::channel();
    options.listening = Some(listening);
    let task = tokio::spawn({
        let (cancel, data_dir) = (cancel.clone(), data_dir.clone());
        async move { run_server(config, &data_dir, cancel, options).await }
    });
    let addr = tokio::time::timeout(LIMIT, bound)
        .await
        .expect("the listener is bound")
        .expect("the server sends its address");
    Running {
        addr,
        cancel,
        task,
        data_dir,
        dir,
    }
}

/// Starts a server with the config of [`config`] and the routes `routes`
/// next to the API.
async fn start_with_routes(routes: Router<AppState>) -> Running {
    let options = ServerOptions {
        routes,
        ..ServerOptions::default()
    };
    start(config(), CancellationToken::new(), options).await
}

/// Opens a connection to `addr` and sends a GET request for `path`.
async fn send_get(addr: SocketAddr, path: &str) -> TcpStream {
    send(
        addr,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
    )
    .await
}

/// Opens a connection to `addr` and sends `request`.
async fn send(addr: SocketAddr, request: &str) -> TcpStream {
    let mut stream = tokio::time::timeout(LIMIT, TcpStream::connect(addr))
        .await
        .expect("the connection opens in time")
        .expect("the server accepts the connection");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send the request");
    stream
}

/// Calls the API method `method` of the server at `addr` as the owner, with
/// `params` and `f=json`, on a connection of its own that the server closes
/// after the response. Checks that the response is HTTP 200 and returns the
/// value of `subsonic-response`.
async fn api(addr: SocketAddr, method: &str, params: &[(&str, &str)]) -> Value {
    let salt = "c0ffee";
    let token = token(OWNER_PASSWORD, salt);
    let mut query = params.to_vec();
    query.extend([
        ("u", OWNER),
        ("t", token.as_str()),
        ("s", salt),
        ("v", API_VERSION),
        ("c", CLIENT),
        ("f", "json"),
    ]);
    let path = format!("/rest/{method}?{}", urlencode(&query));
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let mut stream = send(addr, &request).await;
    let response = read_to_end(&mut stream).await;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("{method}: the response has no body: {response}"));
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{method}: {head}");
    subsonic_response(body)
}

/// Reads from `stream` until `received` holds `until`.
async fn read_until(stream: &mut TcpStream, received: &mut Vec<u8>, until: &str) {
    let found = async {
        while !String::from_utf8_lossy(received).contains(until) {
            let read = stream.read_buf(received).await.expect("read the response");
            assert_ne!(read, 0, "the connection ended before `{until}`");
        }
    };
    tokio::time::timeout(LIMIT, found)
        .await
        .unwrap_or_else(|_| panic!("the response holds `{until}`"));
}

/// Reads from `stream` until the server ends the connection, and returns
/// what arrived as text. A connection that the server closes while the
/// client holds unsent data ends with a reset on some systems, which counts
/// as the end.
async fn read_to_end(stream: &mut TcpStream) -> String {
    let mut received = Vec::new();
    let ended = async {
        loop {
            match stream.read_buf(&mut received).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    };
    tokio::time::timeout(LIMIT, ended)
        .await
        .expect("the server ends the connection");
    String::from_utf8_lossy(&received).into_owned()
}

/// Whether `addr` accepts no connection. A system that does not answer a
/// connection attempt within [`REFUSAL`] refuses it as well.
async fn refuses(addr: SocketAddr) -> bool {
    !matches!(
        tokio::time::timeout(REFUSAL, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Sends a unit to its channel when it is dropped.
struct DropSignal(UnboundedSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Waits for one unit of `events`.
async fn event(events: &mut UnboundedReceiver<()>, what: &str) {
    tokio::time::timeout(LIMIT, events.recv())
        .await
        .unwrap_or_else(|_| panic!("{what}"))
        .unwrap_or_else(|| panic!("{what}: the sender is gone"));
}

/// A body whose first chunk is `first` and that never ends. `dropped`
/// receives a unit when the server drops the body.
fn endless_body(first: &'static str, dropped: UnboundedSender<()>) -> Body {
    let signal = DropSignal(dropped);
    let chunks = stream::once(async move { Ok::<_, io::Error>(first) })
        .chain(stream::pending())
        // The closure owns the signal, and the stream owns the closure.
        .map(move |chunk| {
            let _owned = &signal;
            chunk
        });
    Body::from_stream(chunks)
}

/// The lines of the log that warn that the grace of a server was over.
fn grace_warnings(grace: &str) -> Vec<String> {
    captured_log().lines_with(&format!(
        "requests were still in progress {grace} after the stop began"
    ))
}

#[tokio::test]
async fn request_in_progress_is_answered_and_the_server_ends_without_the_grace() {
    let _one = one_server().await;
    captured_log();
    // A route that answers after 200 ms.
    let (entered, mut requests) = unbounded_channel();
    let routes = Router::new().route(
        "/slow",
        get(move || {
            let entered = entered.clone();
            async move {
                let _ = entered.send(());
                tokio::time::sleep(Duration::from_millis(200)).await;
                "the slow route answered"
            }
        }),
    );
    let mut server = start_with_routes(routes).await;

    let mut client = send_get(server.addr, "/slow").await;
    event(&mut requests, "the request reaches the route").await;
    server.cancel.cancel();

    // The response arrives whole, and the server closes the connection
    // after it.
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the slow route answered"), "{response}");
    // The server ends when the request is answered. Its grace is one hour,
    // so it did not wait for the grace, and it closed no connection that
    // had a request in progress.
    server.ended().await.expect("the server stops in order");
    assert_eq!(grace_warnings("3600s"), Vec::<String>::new());
    assert!(refuses(server.addr).await);
}

#[tokio::test]
async fn listener_closes_at_once_while_a_request_in_progress_finishes() {
    let _one = one_server().await;
    // A route that answers when the test says so.
    let (entered, mut requests) = unbounded_channel();
    let answer = Arc::new(Notify::new());
    let routes = Router::new().route(
        "/held",
        get({
            let answer = Arc::clone(&answer);
            move || {
                let (entered, answer) = (entered.clone(), Arc::clone(&answer));
                async move {
                    let _ = entered.send(());
                    answer.notified().await;
                    "the held route answered"
                }
            }
        }),
    );
    let mut server = start_with_routes(routes).await;

    // An idle connection, and a connection with a request in progress.
    let mut idle = tokio::time::timeout(LIMIT, TcpStream::connect(server.addr))
        .await
        .expect("the connection opens in time")
        .expect("the server accepts the connection");
    let mut client = send_get(server.addr, "/held").await;
    event(&mut requests, "the request reaches the route").await;
    server.cancel.cancel();

    // The listener closes, and the port refuses a connection. The test
    // holds the request, so the server has not ended.
    let closed = async {
        while !refuses(server.addr).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(LIMIT, closed)
        .await
        .expect("the listener closes");
    assert!(!server.task.is_finished());
    // The connection without a request is closed without a response.
    assert_eq!(read_to_end(&mut idle).await, "");
    server.does_not_end().await;

    answer.notify_one();
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the held route answered"), "{response}");
    server.ended().await.expect("the server stops in order");
}

#[tokio::test]
async fn requests_that_outlast_the_grace_are_dropped() {
    let _one = one_server().await;
    captured_log();
    // A route whose handler never returns, and a route whose body never
    // ends.
    let (entered, mut requests) = unbounded_channel();
    let (dropped, mut drops) = unbounded_channel();
    let routes = Router::new()
        .route(
            "/stuck",
            get({
                let (entered, dropped) = (entered.clone(), dropped.clone());
                move || {
                    let (entered, signal) = (entered.clone(), DropSignal(dropped.clone()));
                    async move {
                        let _signal = signal;
                        let _ = entered.send(());
                        std::future::pending::<&'static str>().await
                    }
                }
            }),
        )
        .route(
            "/endless",
            get(move || {
                let (entered, dropped) = (entered.clone(), dropped.clone());
                async move {
                    let _ = entered.send(());
                    endless_body("the first chunk", dropped)
                }
            }),
        );
    let mut config = config();
    config.server.shutdown_grace_ms = 100;
    let options = ServerOptions {
        routes,
        ..ServerOptions::default()
    };
    let mut server = start(config, CancellationToken::new(), options).await;

    let mut stuck = send_get(server.addr, "/stuck").await;
    let mut endless = send_get(server.addr, "/endless").await;
    let mut received = Vec::new();
    read_until(&mut endless, &mut received, "the first chunk").await;
    event(&mut requests, "the first request reaches its route").await;
    event(&mut requests, "the second request reaches its route").await;

    let stop_began = Instant::now();
    server.cancel.cancel();
    // Neither request ends by itself, so the server drops them. It then
    // ends: it waits for nothing else.
    event(&mut drops, "the server drops the first request").await;
    event(&mut drops, "the server drops the second request").await;
    server.ended().await.expect("the server stops in order");
    // The requests had the grace. A wait does not end early, so a slow
    // machine cannot fail this.
    let stopped_after = stop_began.elapsed();
    assert!(
        stopped_after >= Duration::from_millis(100),
        "{stopped_after:?}"
    );

    // The client of the handler reads the end of the connection and no
    // response. The client of the body reads a response that is cut: the
    // first chunk, and not the empty chunk that ends a body.
    assert_eq!(read_to_end(&mut stuck).await, "");
    let mut response = String::from_utf8_lossy(&received).into_owned();
    response.push_str(&read_to_end(&mut endless).await);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.trim_end().ends_with("the first chunk"),
        "{response}"
    );
    let warnings = grace_warnings("100ms");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(" WARN "), "{}", warnings[0]);
}

#[tokio::test]
async fn advertisement_is_withdrawn_at_once_and_the_server_ends_after_the_step() {
    let _one = one_server().await;
    // A route that answers when the test says so.
    let (entered, mut requests) = unbounded_channel();
    let answer = Arc::new(Notify::new());
    let routes = Router::new().route(
        "/held",
        get({
            let answer = Arc::clone(&answer);
            move || {
                let (entered, answer) = (entered.clone(), Arc::clone(&answer));
                async move {
                    let _ = entered.send(());
                    answer.notified().await;
                    "the held route answered"
                }
            }
        }),
    );
    // The step that withdraws the advertisement tells the test that it
    // runs, and it returns when the test drops `release`.
    let (step_entered, mut steps) = unbounded_channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let withdraw: StopStep = Box::new(move || {
        step_entered.send(()).expect("the test listens");
        let _ = released.recv();
    });
    let options = ServerOptions {
        withdraw: Some(withdraw),
        routes,
        ..ServerOptions::default()
    };
    let mut server = start(config(), CancellationToken::new(), options).await;

    let mut client = send_get(server.addr, "/held").await;
    event(&mut requests, "the request reaches the route").await;
    // Nothing is withdrawn while the server runs.
    assert!(steps.try_recv().is_err());

    // The step starts with the stop. The request is still in progress: the
    // test has not let the route answer.
    server.cancel.cancel();
    event(&mut steps, "the step starts with the stop").await;
    assert!(!server.task.is_finished());

    // The request is answered, and no scan runs. The step has not
    // returned, so the server does not checkpoint the log and does not
    // end: the process would exit while the goodbye packet is sent.
    answer.notify_one();
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the held route answered"), "{response}");
    server.does_not_end().await;
    assert!(server.wal_len() > 0);

    // The step returns, and the server closes the database and ends.
    drop(release);
    server.ended().await.expect("the server stops in order");
    assert_eq!(server.wal_len(), 0);
    // The step ran once.
    assert!(steps.try_recv().is_err());
}

/// Copies the three audio fixtures `a.flac`, `a.mp3` and `a.ogg` into a new
/// temporary directory.
fn music_folder() -> TempDir {
    let folder = tempfile::tempdir().expect("a temporary directory");
    for name in ["a.flac", "a.mp3", "a.ogg"] {
        fs::copy(fixtures_dir().join(name), folder.path().join(name)).expect("copy a fixture");
    }
    folder
}

/// Reads the `scans` rows of the database at `path` through a connection of
/// its own, which is closed before the function returns.
fn read_scans(path: &Path) -> Vec<Scan> {
    let connection = rusqlite::Connection::open(path).expect("open the database");
    scans::list(&connection).expect("read the scans")
}

/// Waits until the database at `path` holds `count` scans and the last of
/// them has ended, and returns the row of that scan. A scan of a library of
/// a few files ends milliseconds after the server started it.
async fn scan_ended(path: &Path, count: usize) -> Scan {
    let ended = async {
        loop {
            let mut rows = read_scans(path);
            if rows.len() == count && rows[count - 1].finished_at.is_some() {
                return rows.swap_remove(count - 1);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(LIMIT, ended)
        .await
        .unwrap_or_else(|_| panic!("scan {count} ends"))
}

#[tokio::test]
async fn token_cancelled_during_a_scan_ends_the_scan_as_cancelled_and_closes_the_database() {
    let _one = one_server().await;
    let folder = music_folder();
    let mut config = config();
    config.library.folders = vec![folder.path().to_path_buf()];
    config.library.scan_on_start = true;

    // The scan at startup commits a batch of two files. Its hook then
    // cancels the token of the server, as a stop signal does, while the
    // third file waits for the second batch.
    let cancel = CancellationToken::new();
    let scan = ScanHooks {
        batch_size: Some(2),
        on_batch: Some(Arc::new({
            let cancel = cancel.clone();
            move || cancel.cancel()
        })),
    };
    let (step_ran, mut steps) = unbounded_channel();
    let withdraw: StopStep = Box::new(move || step_ran.send(()).expect("the test listens"));
    let options = ServerOptions {
        withdraw: Some(withdraw),
        scan,
        ..ServerOptions::default()
    };
    let mut server = start(config, cancel, options).await;

    // The server stops without a signal of the test, and its result is the
    // one of a stop in order.
    server.ended().await.expect("the server stops in order");
    assert!(server.cancel.is_cancelled());
    // The step that withdraws ran once.
    assert!(steps.try_recv().is_ok());
    assert!(steps.try_recv().is_err());

    // The scan ended as a cancelled scan with its first batch stored. The
    // log is in the database file, and a new handle reads the rows there.
    assert_eq!(server.wal_len(), 0);
    let db = Db::open(server.data_dir.db_path()).expect("open the database");
    assert_eq!(db.read(tracks::count_tracks).await.expect("count"), 2);
    let rows = db.read(scans::list).await.expect("read the scans");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].finished_at.is_some());
    assert_eq!(rows[0].error.as_deref(), Some(CANCELLED));
    assert_eq!(rows[0].counts.added, 2);
}

#[tokio::test]
async fn scan_that_does_not_stop_within_its_limit_is_left_behind() {
    let _one = one_server().await;
    let log = captured_log();
    let folder = music_folder();
    let mut config = config();
    config.library.folders = vec![folder.path().to_path_buf()];
    config.library.scan_on_start = true;
    // The limit of the scan is the length of the grace.
    config.server.shutdown_grace_ms = 100;

    // The scan at startup commits a batch of one file. Its hook then holds
    // it until the test drops `release`, as a file read that does not
    // return holds a scan.
    let (reached, mut batches) = unbounded_channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let scan = ScanHooks {
        batch_size: Some(1),
        on_batch: Some(Arc::new(move || {
            let _ = reached.send(());
            let _ = released.lock().expect("the lock of the hook").recv();
        })),
    };
    let options = ServerOptions {
        scan,
        ..ServerOptions::default()
    };
    let mut server = start(config, CancellationToken::new(), options).await;
    event(&mut batches, "the scan commits its first batch").await;

    // The server ends although the scan does not: it does not wait longer
    // than the limit.
    let stop_began = Instant::now();
    server.cancel.cancel();
    server.ended().await.expect("the server stops in order");
    // The scan had its limit. A wait does not end early, so a slow machine
    // cannot fail this.
    let stopped_after = stop_began.elapsed();
    assert!(
        stopped_after >= Duration::from_millis(100),
        "{stopped_after:?}"
    );
    let warnings = log.lines_with("the scan has not stopped after 100ms");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(" WARN "), "{}", warnings[0]);

    // The database is left as a process that is killed during a scan
    // leaves it. The log is in the file, the file is sound, the batch that
    // the scan committed is stored, and the row of the scan has no end
    // time.
    let db_path = server.data_dir.db_path();
    assert_eq!(server.wal_len(), 0);
    let connection = rusqlite::Connection::open(&db_path).expect("open the database");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("check the database");
    assert_eq!(integrity, "ok");
    assert_eq!(tracks::count_tracks(&connection).expect("count"), 1);
    drop(connection);
    let rows = read_scans(&db_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].finished_at, None);
    assert_eq!(rows[0].error, None);

    // A process ends the thread of the scan by exiting. The test lets the
    // scan end, which closes its row, so that the thread does not outlive
    // the data directory.
    drop(release);
    let closed = async {
        while read_scans(&db_path)[0].finished_at.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(LIMIT, closed)
        .await
        .expect("the scan ends");
}

/// A route whose handler never returns. `entered` receives a unit when a
/// request reaches the handler, and `dropped` when the server drops it.
fn stuck_route(entered: UnboundedSender<()>, dropped: UnboundedSender<()>) -> Router<AppState> {
    Router::new().route(
        "/stuck",
        get(move || {
            let (entered, signal) = (entered.clone(), DropSignal(dropped.clone()));
            async move {
                let _signal = signal;
                let _ = entered.send(());
                std::future::pending::<&'static str>().await
            }
        }),
    )
}

#[tokio::test]
async fn stop_with_hurry_drops_a_request_after_the_short_grace() {
    let _one = one_server().await;
    captured_log();
    let (entered, mut requests) = unbounded_channel();
    let (dropped, mut drops) = unbounded_channel();
    // The grace is one hour, and the short grace is 120 ms.
    let mut config = config();
    config.server.shutdown_grace_short_ms = 120;
    let hurry = CancellationToken::new();
    let options = ServerOptions {
        routes: stuck_route(entered, dropped),
        hurry: hurry.clone(),
        ..ServerOptions::default()
    };
    let mut server = start(config, CancellationToken::new(), options).await;

    let mut stuck = send_get(server.addr, "/stuck").await;
    event(&mut requests, "the request reaches its route").await;

    // The stop signal has a short limit, so `hurry` is cancelled before the
    // token of the stop, as `run` does it.
    hurry.cancel();
    let stop_began = Instant::now();
    server.cancel.cancel();
    event(&mut drops, "the server drops the request").await;
    let dropped_after = stop_began.elapsed();
    server.ended().await.expect("the server stops in order");

    // The request had the short grace, which does not end early, and it did
    // not have the hour.
    assert!(
        dropped_after >= Duration::from_millis(120),
        "{dropped_after:?}"
    );
    assert!(dropped_after < Duration::from_secs(1), "{dropped_after:?}");
    assert_eq!(read_to_end(&mut stuck).await, "");
    let warnings = grace_warnings("120ms");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(" WARN "), "{}", warnings[0]);
}

#[tokio::test]
async fn stop_without_hurry_waits_for_a_request_past_the_short_grace() {
    let _one = one_server().await;
    captured_log();
    // A route that answers when the test says so. `dropped` receives a unit
    // when its handler ends, with an answer or without one.
    let (entered, mut requests) = unbounded_channel();
    let (dropped, mut drops) = unbounded_channel();
    let answer = Arc::new(Notify::new());
    let routes = Router::new().route(
        "/held",
        get({
            let answer = Arc::clone(&answer);
            move || {
                let (entered, answer) = (entered.clone(), Arc::clone(&answer));
                let signal = DropSignal(dropped.clone());
                async move {
                    let _signal = signal;
                    let _ = entered.send(());
                    answer.notified().await;
                    "the held route answered"
                }
            }
        }),
    );
    // The grace is one hour. The short grace is 10 ms, which is over long
    // before the wait below ends.
    let mut config = config();
    config.server.shutdown_grace_short_ms = 10;
    // The stop after Ctrl-C and after SIGTERM: `run` has a token for the
    // short limits and does not cancel it.
    let hurry = CancellationToken::new();
    let options = ServerOptions {
        routes,
        hurry: hurry.clone(),
        ..ServerOptions::default()
    };
    let mut server = start(config, CancellationToken::new(), options).await;

    let mut client = send_get(server.addr, "/held").await;
    event(&mut requests, "the request reaches the route").await;
    server.cancel.cancel();

    // A server that took the short grace would have dropped the request and
    // ended by now. This one waits for the request.
    server.does_not_end().await;
    assert!(drops.try_recv().is_err(), "the request was dropped");

    answer.notify_one();
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the held route answered"), "{response}");
    server.ended().await.expect("the server stops in order");
    assert!(!hurry.is_cancelled());
    assert_eq!(grace_warnings("10ms"), Vec::<String>::new());
}

#[tokio::test]
async fn stop_with_hurry_during_a_scan_and_open_requests_ends_within_the_time_of_windows() {
    let _one = one_server().await;
    let log = captured_log();
    let folder = music_folder();
    let mut config = config();
    config.library.folders = vec![folder.path().to_path_buf()];
    config.library.scan_on_start = true;
    // The grace is one hour. The short grace is 130 ms in place of the
    // 1.5 s of a server, so that the test does not wait 3 s.
    let default_short = Duration::from_millis(config.server.shutdown_grace_short_ms);
    let short = Duration::from_millis(130);
    config.server.shutdown_grace_short_ms = 130;

    // The scan at startup commits a batch of one file. Its hook then holds
    // it until the test drops `release`, as a file read that does not
    // return holds a scan.
    let (reached, mut batches) = unbounded_channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    let scan = ScanHooks {
        batch_size: Some(1),
        on_batch: Some(Arc::new(move || {
            let _ = reached.send(());
            let _ = released.lock().expect("the lock of the hook").recv();
        })),
    };
    // A request whose handler never returns, and a stream whose body never
    // ends.
    let (entered, mut requests) = unbounded_channel();
    let (dropped, mut drops) = unbounded_channel();
    let routes = stuck_route(entered.clone(), dropped.clone()).route(
        "/endless",
        get(move || {
            let (entered, dropped) = (entered.clone(), dropped.clone());
            async move {
                let _ = entered.send(());
                endless_body("the first chunk", dropped)
            }
        }),
    );
    let hurry = CancellationToken::new();
    let options = ServerOptions {
        routes,
        scan,
        hurry: hurry.clone(),
        ..ServerOptions::default()
    };
    let mut server = start(config, CancellationToken::new(), options).await;
    event(&mut batches, "the scan commits its first batch").await;
    let mut stuck = send_get(server.addr, "/stuck").await;
    let mut endless = send_get(server.addr, "/endless").await;
    let mut received = Vec::new();
    read_until(&mut endless, &mut received, "the first chunk").await;
    event(&mut requests, "the first request reaches its route").await;
    event(&mut requests, "the second request reaches its route").await;

    // The console of the server is closed: `hurry` is cancelled before the
    // token of the stop. Neither the requests nor the scan end by
    // themselves.
    hurry.cancel();
    let stop_began = Instant::now();
    server.cancel.cancel();
    event(&mut drops, "the server drops the first request").await;
    event(&mut drops, "the server drops the second request").await;
    server.ended().await.expect("the server stops in order");
    let stopped_after = stop_began.elapsed();

    // Step 2 and step 3 waited one short grace each. A wait does not end
    // early, so a slow machine cannot fail this.
    assert!(stopped_after >= 2 * short, "{stopped_after:?}");
    for warning in [
        "requests were still in progress 130ms after the stop began",
        "the scan has not stopped after 130ms",
    ] {
        let warnings = log.lines_with(warning);
        assert_eq!(warnings.len(), 1, "{warning}: {warnings:?}");
        assert!(warnings[0].contains(" WARN "), "{}", warnings[0]);
    }
    // A server with the default short grace waits 1.5 s in each of the two
    // steps, where this one waited 130 ms. Its stop takes that much longer,
    // and it ends before Windows ends the process.
    let with_default_limits = stopped_after + 2 * (default_short - short);
    assert!(
        with_default_limits < WINDOWS_LIMIT,
        "{stopped_after:?}, {with_default_limits:?}"
    );

    // The stop reached its last step: the log is in the database file. The
    // file is sound and holds the batch that the scan committed, and the
    // row of the scan has no end time.
    let db_path = server.data_dir.db_path();
    assert_eq!(server.wal_len(), 0);
    let connection = rusqlite::Connection::open(&db_path).expect("open the database");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("check the database");
    assert_eq!(integrity, "ok");
    assert_eq!(tracks::count_tracks(&connection).expect("count"), 1);
    drop(connection);
    let rows = read_scans(&db_path);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].finished_at, rows[0].error.as_deref()),
        (None, None)
    );
    // The clients read the end of their connections.
    assert_eq!(read_to_end(&mut stuck).await, "");
    let mut response = String::from_utf8_lossy(&received).into_owned();
    response.push_str(&read_to_end(&mut endless).await);
    assert!(
        response.trim_end().ends_with("the first chunk"),
        "{response}"
    );

    // A process ends the thread of the scan by exiting. The test lets the
    // scan end, which closes its row, so that the thread does not outlive
    // the data directory.
    drop(release);
    let closed = async {
        while read_scans(&db_path)[0].finished_at.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(LIMIT, closed)
        .await
        .expect("the scan ends");
}

#[tokio::test]
async fn startup_log_names_version_data_directory_address_folders_and_auth() {
    let _one = one_server().await;
    let log = captured_log();
    let (rock, jazz) = (music_folder(), music_folder());

    for (disabled, auth) in [(false, r#"auth="password""#), (true, r#"auth="none""#)] {
        let mut config = config();
        config.library.folders = vec![rock.path().to_path_buf(), jazz.path().to_path_buf()];
        config.auth.disabled_unsafe = disabled;
        let mut server = start(config, CancellationToken::new(), ServerOptions::default()).await;

        // The data directory is in the line of this server only.
        let data_dir = format!("data_dir={}", server.data_dir.path().display());
        let lines = log.lines_with(&data_dir);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        for field in [
            " INFO ".to_owned(),
            format!("listening on {}", server.addr),
            format!(r#"version="{}""#, wn_server::VERSION),
            "folders=2".to_owned(),
            auth.to_owned(),
        ] {
            assert!(line.contains(&field), "{field}: {line}");
        }

        server.cancel.cancel();
        server.ended().await.expect("the server stops in order");
    }
}

/// How a music folder becomes unavailable in [`unavailable_folder_cycle`].
enum Outage {
    /// The path of the folder does not exist, which is the state of a volume
    /// that is unplugged.
    Missing,
    /// The folder exists and cannot be listed. File modes exist on Unix
    /// only.
    #[cfg(unix)]
    Unlistable,
}

/// A music folder that a test made unavailable. Dropping the value makes the
/// folder available again, so the directory of the library can be deleted
/// when a test fails.
enum Unplugged {
    /// The folder at the first path was moved to the second path.
    Moved(PathBuf, PathBuf),
    /// The folder lost its permissions.
    #[cfg(unix)]
    Unlistable(PathBuf),
}

impl Unplugged {
    /// Makes `folder` unavailable in the way of `outage`. A folder that is
    /// moved goes into the directory `parking`. Returns `None` when the
    /// process can list a folder without permissions, which is the case for
    /// a process that runs as root: such a test has nothing to check.
    fn new(outage: Outage, folder: &Path, parking: &Path) -> Option<Self> {
        match outage {
            Outage::Missing => {
                let parked = parking.join("unplugged");
                fs::rename(folder, &parked).expect("move the folder away");
                Some(Self::Moved(folder.to_path_buf(), parked))
            }
            #[cfg(unix)]
            Outage::Unlistable => {
                set_mode(folder, 0o000);
                let unplugged = Self::Unlistable(folder.to_path_buf());
                if fs::read_dir(folder).is_ok() {
                    eprintln!("not checked: the process lists a directory that has no permission");
                    return None;
                }
                Some(unplugged)
            }
        }
    }
}

impl Drop for Unplugged {
    fn drop(&mut self) {
        match self {
            // A folder that cannot be moved back stays in the parking
            // directory, which is deleted with it.
            Self::Moved(folder, parked) => drop(fs::rename(parked, folder)),
            #[cfg(unix)]
            Self::Unlistable(folder) => set_mode(folder, 0o755),
        }
    }
}

/// Sets the permission bits of a directory.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|error| panic!("set the mode of {}: {error}", path.display()));
}

/// Loads the config file at `path` as `run` does, and gives the server the
/// address, the grace and the ffmpeg path of the servers of this file. The
/// file cannot name port 0.
fn load_config(path: &Path) -> Config {
    let mut loaded = Config::load(path).expect("the config loads");
    let test = config();
    loaded.server.bind = test.server.bind;
    loaded.server.port = test.server.port;
    loaded.server.shutdown_grace_ms = test.server.shutdown_grace_ms;
    loaded.transcoding.ffmpeg = test.transcoding.ffmpeg;
    loaded
}

/// The ID of every track that `search3` lists for the server at `addr`, by
/// the title of the track.
async fn song_ids(addr: SocketAddr) -> BTreeMap<String, String> {
    let response = api(addr, "search3", &[("query", ""), ("songCount", "100")]).await;
    assert_eq!(response["status"], "ok", "{response}");
    let songs = response["searchResult3"]["song"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let text = |song: &Value, key: &str| {
        let value = song[key].as_str();
        value.unwrap_or_else(|| panic!("{key}: {song}")).to_owned()
    };
    let ids: BTreeMap<String, String> = songs
        .iter()
        .map(|song| (text(song, "title"), text(song, "id")))
        .collect();
    assert_eq!(ids.len(), songs.len(), "two tracks have one title");
    ids
}

/// The IDs of the items of `list`, which is a list of songs or of playlist
/// entries, in the order of the list.
fn ids_of(list: &Value) -> Vec<&str> {
    let items = list.as_array().map(Vec::as_slice).unwrap_or_default();
    items
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect()
}

/// Runs a server three times on one data directory whose config names two
/// music folders: with both folders, with the second folder unavailable in
/// the way of `outage`, and with the folder back. Each run is `run_server`
/// with the config that `Config::load` reads from the file, as `run` starts
/// it, and each run scans at startup.
async fn unavailable_folder_cycle(outage: Outage) {
    let _one = one_server().await;
    let log = captured_log();
    // One folder is on the disk of the machine. The other one is on a volume
    // that can be unplugged.
    let home = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
        .track("Aurora Field", "Glass Harbour", "Breakwater", 2, "flac")
        .track("Aurora Field", "Glass Harbour", "Night Ferry", 3, "flac")
        .build();
    let volume = LibraryBuilder::new()
        .track("Marlow Quartet", "Slow Rivers", "Upstream", 1, "m4a")
        .track("Marlow Quartet", "Slow Rivers", "Delta", 2, "m4a")
        .track("Tessa Lund", "Field Notes", "Marsh", 1, "ogg")
        .build();
    let (home_path, volume_path) = (home.root.path(), volume.root.path());
    let parking = tempfile::tempdir().expect("a temporary directory");

    let dir = new_data_dir().await;
    let config_path = dir.path().join("config.toml");
    let folders = format!(
        "[library]\nfolders = ['{}', '{}']\n",
        home_path.display(),
        volume_path.display()
    );
    fs::write(&config_path, folders).expect("write the config");
    let run = |dir| {
        let config = load_config(&config_path);
        start_in(
            dir,
            config,
            CancellationToken::new(),
            ServerOptions::default(),
        )
    };

    // Both folders are there. The scan at startup stores the six tracks.
    let server = run(dir).await;
    let db_path = server.data_dir.db_path();
    let first = scan_ended(&db_path, 1).await;
    assert_eq!((first.counts.added, first.error), (6, None));
    let ids = song_ids(server.addr).await;
    let titles: Vec<&str> = ids.keys().map(String::as_str).collect();
    assert_eq!(
        titles,
        [
            "Breakwater",
            "Delta",
            "Low Tide",
            "Marsh",
            "Night Ferry",
            "Upstream"
        ]
    );
    // The owner stars a track of the volume and puts it into a playlist,
    // followed by a track of the other folder.
    let (delta, low_tide) = (ids["Delta"].as_str(), ids["Low Tide"].as_str());
    let starred = api(server.addr, "star", &[("id", delta)]).await;
    assert_eq!(starred["status"], "ok", "{starred}");
    let request = [("name", "Mix"), ("songId", delta), ("songId", low_tide)];
    let created = api(server.addr, "createPlaylist", &request).await;
    assert_eq!(created["status"], "ok", "{created}");
    let playlist = created["playlist"]["id"].as_str().expect("a playlist ID");
    let dir = server.stopped().await;

    // The volume goes away while the server is down, and a file of the
    // other folder is deleted.
    let Some(unplugged) = Unplugged::new(outage, volume_path, parking.path()) else {
        return;
    };
    fs::remove_file(&home.files[2]).expect("remove the file of Night Ferry");

    // The config loads and the server starts. It logs the folder once.
    let unavailable = format!("music folder {} is unavailable: ", volume_path.display());
    assert_eq!(log.lines_with(&unavailable), Vec::<String>::new());
    let server = run(dir).await;
    let lines = log.lines_with(&unavailable);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(" WARN "), "{}", lines[0]);

    // The scan at startup read the folder that is there. It deleted no
    // track: not one of the volume, and not the one whose file is deleted,
    // because that file may be on the volume now. Its error names the
    // folder and counts the track that it kept.
    let second = scan_ended(&db_path, 2).await;
    assert_eq!((second.counts.seen, second.counts.removed), (2, 0));
    let error = second.error.as_deref().expect("the scan has an error");
    let unread = format!("({}) cannot be read: ", volume_path.display());
    assert!(error.contains(&unread), "{error}");
    assert!(error.contains("; no track was deleted; "), "{error}");
    assert!(
        error.ends_with(
            "1 track whose file is gone was kept, because a path or a file could not be read"
        ),
        "{error}"
    );

    // The server lists both folders and serves the tracks of the one that
    // is there.
    let addr = server.addr;
    let listed = api(addr, "getMusicFolders", &[]).await;
    let listed = listed["musicFolders"]["musicFolder"].as_array();
    assert_eq!(listed.map(Vec::len), Some(2), "{listed:?}");
    for title in ["Low Tide", "Breakwater"] {
        let song = api(addr, "getSong", &[("id", ids[title].as_str())]).await;
        assert_eq!(song["status"], "ok", "{song}");
        assert_eq!(song["song"]["title"], title);
    }
    // Every track has its row and its ID, and what the owner attached to
    // the track of the volume is there.
    assert_eq!(song_ids(addr).await, ids);
    let starred = api(addr, "getStarred2", &[]).await;
    assert_eq!(ids_of(&starred["starred2"]["song"]), [delta]);
    let mix = api(addr, "getPlaylist", &[("id", playlist)]).await;
    assert_eq!(ids_of(&mix["playlist"]["entry"]), [delta, low_tide]);
    // A scan that recorded errors is no error of `getScanStatus`.
    let status = api(addr, "getScanStatus", &[]).await;
    assert_eq!(status["status"], "ok", "{status}");
    assert!(status["scanStatus"]["lastScan"].is_string(), "{status}");

    // No request added a line about the folder.
    assert_eq!(log.lines_with(&unavailable).len(), 1);
    let dir = server.stopped().await;

    // The volume is back. The scan at the next startup finds its files at
    // the paths of their rows, so every track of it keeps its ID. The scan
    // read every folder, so it deletes the track whose file is gone.
    drop(unplugged);
    let server = run(dir).await;
    let third = scan_ended(&db_path, 3).await;
    assert_eq!(third.error, None);
    let counts = &third.counts;
    assert_eq!(
        (counts.seen, counts.added, counts.moved, counts.removed),
        (5, 0, 0, 1)
    );
    let addr = server.addr;
    let mut kept = ids.clone();
    kept.remove("Night Ferry");
    assert_eq!(song_ids(addr).await, kept);
    let starred = api(addr, "getStarred2", &[]).await;
    assert_eq!(ids_of(&starred["starred2"]["song"]), [delta]);
    let mix = api(addr, "getPlaylist", &[("id", playlist)]).await;
    assert_eq!(ids_of(&mix["playlist"]["entry"]), [delta, low_tide]);
    // The folder is available, so this run logged nothing about it.
    assert_eq!(log.lines_with(&unavailable).len(), 1);
    server.stopped().await;
}

#[tokio::test]
async fn server_starts_without_a_music_folder_that_does_not_exist_and_keeps_its_tracks() {
    unavailable_folder_cycle(Outage::Missing).await;
}

// File modes exist on Unix only.
#[cfg(unix)]
#[tokio::test]
async fn server_starts_without_a_music_folder_that_cannot_be_listed_and_keeps_its_tracks() {
    unavailable_folder_cycle(Outage::Unlistable).await;
}

#[tokio::test]
async fn stream_that_the_client_aborts_is_logged_with_one_line() {
    let _one = one_server().await;
    let log = captured_log();
    // A route that streams a body without an end. Its handler gives the
    // span of the request a client name, by which the test finds the lines
    // of the request.
    const CLIENT: &str = "shutdown-aborted-stream";
    let (dropped, mut drops) = unbounded_channel();
    let routes = Router::new().route(
        "/media",
        get(move || {
            let dropped = dropped.clone();
            async move {
                tracing::Span::current().record("c", CLIENT);
                endless_body("the first chunk", dropped)
            }
        }),
    );
    let mut server = start_with_routes(routes).await;

    let mut client = send_get(server.addr, "/media").await;
    let mut received = Vec::new();
    read_until(&mut client, &mut received, "the first chunk").await;
    // The client closes the connection during the body.
    drop(client);
    event(&mut drops, "the server drops the body").await;

    server.cancel.cancel();
    server.ended().await.expect("the server stops in order");

    // The one line of the request, written when the response began. The end
    // of the connection adds no line.
    let lines = log.lines_with(&format!(r#"c="{CLIENT}""#));
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    for field in [" INFO ", "status=200", "finished processing request"] {
        assert!(line.contains(field), "{field}: {line}");
    }
}
