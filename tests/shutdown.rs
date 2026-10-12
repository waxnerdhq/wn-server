//! The stop of a running server over a real connection (`docs/design.md`
//! section 3.13).
//!
//! The tests of this file are the only ones of the suite that open a socket.
//! Each one runs `run_server` on `127.0.0.1` with port 0, so the system
//! chooses a free port, and speaks HTTP/1.1 over a `TcpStream`. No test
//! starts the mDNS daemon: the step that withdraws the advertisement is a
//! closure of the test.
//!
//! A test holds a request in progress with a route that it adds to the
//! server. Two tests wait for a time to pass: the slow route of the first
//! test answers after 200 ms, and the grace of the test of the grace is
//! 100 ms. Every other wait ends with an event, and one second bounds it.

mod common;

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::get;
use futures_util::StreamExt;
use futures_util::stream;
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

use common::{OWNER, OWNER_PASSWORD, captured_log, fixtures_dir};

/// The longest time a test waits for an event. The limit ends a test that
/// does not get there. It does not delay a passing run.
const WAIT: Duration = Duration::from_secs(1);

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
    _dir: TempDir,
}

impl Running {
    /// Waits for `run_server` to return and returns its result.
    async fn ended(&mut self) -> Result<(), ServerError> {
        tokio::time::timeout(WAIT, &mut self.task)
            .await
            .expect("the server ends")
            .expect("the server does not panic")
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
/// system chooses, and no scan at startup.
fn config() -> Config {
    let mut config = Config::default();
    config.server.bind = "127.0.0.1".parse().expect("an address");
    config.server.port = 0;
    config.library.scan_on_start = false;
    config
}

/// Creates a data directory whose database holds the owner account, and
/// runs `run_server` on it with `config`, `cancel` and `options`. Returns
/// when the listener is bound.
async fn start(config: Config, cancel: CancellationToken, mut options: ServerOptions) -> Running {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
    let db = Db::open(data_dir.db_path()).expect("create the database");
    db.write(|conn| users::insert_owner(conn, OWNER, OWNER_PASSWORD))
        .await
        .expect("insert the owner")
        .expect("a new database has no owner");
    // The server opens the database itself.
    drop(db);

    let (listening, bound) = oneshot::channel();
    options.listening = Some(listening);
    let task = tokio::spawn({
        let (cancel, data_dir) = (cancel.clone(), data_dir.clone());
        async move { run_server(config, &data_dir, cancel, options).await }
    });
    let addr = tokio::time::timeout(WAIT, bound)
        .await
        .expect("the listener is bound")
        .expect("the server sends its address");
    Running {
        addr,
        cancel,
        task,
        data_dir,
        _dir: dir,
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
    let mut stream = tokio::time::timeout(WAIT, TcpStream::connect(addr))
        .await
        .expect("the connection opens in time")
        .expect("the server accepts the connection");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send the request");
    stream
}

/// Reads from `stream` until `received` holds `until`.
async fn read_until(stream: &mut TcpStream, received: &mut Vec<u8>, until: &str) {
    let found = async {
        while !String::from_utf8_lossy(received).contains(until) {
            let read = stream.read_buf(received).await.expect("read the response");
            assert_ne!(read, 0, "the connection ended before `{until}`");
        }
    };
    tokio::time::timeout(WAIT, found)
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
    tokio::time::timeout(WAIT, ended)
        .await
        .expect("the server ends the connection");
    String::from_utf8_lossy(&received).into_owned()
}

/// Whether `addr` accepts no connection. A system that does not answer a
/// connection attempt within the wait refuses it as well.
async fn refuses(addr: SocketAddr) -> bool {
    !matches!(
        tokio::time::timeout(WAIT, TcpStream::connect(addr)).await,
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
    tokio::time::timeout(WAIT, events.recv())
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

#[tokio::test]
async fn request_in_progress_is_answered_when_the_server_stops() {
    let _one = one_server().await;
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
    let stop_began = Instant::now();
    server.cancel.cancel();

    // The response arrives, and the server closes the connection after it.
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the slow route answered"), "{response}");
    server.ended().await.expect("the server stops in order");
    assert!(
        stop_began.elapsed() < Duration::from_secs(1),
        "{:?}",
        stop_began.elapsed()
    );
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
    let mut idle = tokio::time::timeout(WAIT, TcpStream::connect(server.addr))
        .await
        .expect("the connection opens in time")
        .expect("the server accepts the connection");
    let mut client = send_get(server.addr, "/held").await;
    event(&mut requests, "the request reaches the route").await;
    server.cancel.cancel();

    // The listener closes, and the port refuses a connection. The grace of
    // the request is the default of 10 s, so the server still waits for it.
    let closed = async {
        while !refuses(server.addr).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(WAIT, closed)
        .await
        .expect("the listener closes");
    assert!(!server.task.is_finished());
    // The connection without a request is closed without a response.
    assert_eq!(read_to_end(&mut idle).await, "");
    assert!(!server.task.is_finished());

    answer.notify_one();
    let response = read_to_end(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("the held route answered"), "{response}");
    server.ended().await.expect("the server stops in order");
}

#[tokio::test]
async fn requests_that_outlast_the_grace_are_dropped() {
    let _one = one_server().await;
    let log = captured_log();
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
    // The server drops both requests when the grace is over, and it does
    // not wait longer for them.
    event(&mut drops, "the server drops the first request").await;
    event(&mut drops, "the server drops the second request").await;
    server.ended().await.expect("the server stops in order");
    let stopped_after = stop_began.elapsed();
    assert!(
        stopped_after >= Duration::from_millis(100),
        "{stopped_after:?}"
    );
    assert!(stopped_after < Duration::from_secs(1), "{stopped_after:?}");

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
    let warnings = log.lines_with("requests were still in progress 100ms after the stop began");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(" WARN "), "{}", warnings[0]);
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

/// What the step that withdraws the advertisement finds when it runs.
struct AtWithdraw {
    /// Whether the port of the server refuses a connection.
    refused: bool,
    /// The rows of the `scans` table.
    scans: Vec<Scan>,
    /// The size of the write-ahead log.
    wal_len: u64,
}

/// Reads the `scans` rows of the database at `path` through a connection of
/// its own, which is closed before the function returns.
fn read_scans(path: &Path) -> Vec<Scan> {
    let connection = rusqlite::Connection::open(path).expect("open the database");
    scans::list(&connection).expect("read the scans")
}

#[tokio::test]
async fn stop_ends_the_scan_then_withdraws_then_closes_the_database() {
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
    // The step runs on a blocking thread. It learns the address and the
    // data directory of the server when the test knows them.
    let (place, place_known) = std::sync::mpsc::channel::<(SocketAddr, DataDir)>();
    let (found, mut step_found) = unbounded_channel();
    let withdraw: StopStep = Box::new(move || {
        let (addr, data_dir) = place_known
            .recv_timeout(WAIT)
            .expect("the test sends the address");
        let refused = std::net::TcpStream::connect_timeout(&addr, WAIT).is_err();
        let at_withdraw = AtWithdraw {
            refused,
            scans: read_scans(&data_dir.db_path()),
            wal_len: wal_len(&data_dir),
        };
        found.send(at_withdraw).expect("the test listens");
    });
    let options = ServerOptions {
        withdraw: Some(withdraw),
        scan,
        ..ServerOptions::default()
    };
    let mut server = start(config, cancel, options).await;
    place
        .send((server.addr, server.data_dir.clone()))
        .expect("the step listens");

    // The server stops without a signal of the test, with status 0.
    server.ended().await.expect("the server stops in order");
    assert!(server.cancel.is_cancelled());

    let at_withdraw = step_found.try_recv().expect("the step ran before the end");
    // Step 1 was over: the listener was closed.
    assert!(at_withdraw.refused);
    // Step 2 was over: the scan had ended as a cancelled scan with its
    // first batch stored.
    assert_eq!(at_withdraw.scans.len(), 1);
    assert!(at_withdraw.scans[0].finished_at.is_some());
    assert_eq!(at_withdraw.scans[0].error.as_deref(), Some(CANCELLED));
    assert_eq!(at_withdraw.scans[0].counts.added, 2);
    // Step 4 had not run: the write-ahead log held the rows.
    assert!(at_withdraw.wal_len > 0);

    // Step 4 moved the log into the database file. A new handle reads the
    // rows of the scan from that file.
    assert_eq!(server.wal_len(), 0);
    let db = Db::open(server.data_dir.db_path()).expect("open the database");
    assert_eq!(db.read(tracks::count_tracks).await.expect("count"), 2);
    let rows = db.read(scans::list).await.expect("read the scans");
    assert_eq!(rows, at_withdraw.scans);
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
