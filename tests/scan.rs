//! Router tests of the scan methods `startScan` and `getScanStatus`
//! (`docs/design.md` section 4), on servers whose music folders are
//! libraries that `LibraryBuilder` wrote.
//!
//! A test that needs a scan to run while it sends requests holds the scan
//! with a [`Gate`]: the scan commits its first batch of one file and then
//! waits in the batch hook of the scanner until the test opens the gate. No
//! test waits for a time to pass.

mod common;

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use wn_server::config::Config;
use wn_server::db::queries::scans::{self, Scan};
use wn_server::db::queries::tracks;
use wn_server::library::scan_controller::{ScanHooks, ScanLock};

use common::{LibraryBuilder, TestServer, body_string, subsonic_response};

/// The protocol version that the requests without the auth defaults send.
const V: (&str, &str) = ("v", "1.16.1");

/// The client name that the requests without the auth defaults send.
const C: (&str, &str) = ("c", "scan-tests");

/// The message of the error of `startScan` while another process scans.
const RUNNING_ELSEWHERE: &str = "another process is scanning the library";

/// The config of a server with the music folders `folders`. A test that
/// waits for a scan polls every 10 ms.
fn config(folders: &[&Path]) -> Config {
    let mut config = Config::default();
    config.library.folders = folders.iter().map(|path| path.to_path_buf()).collect();
    config.library.scan_poll_ms = 10;
    config
}

/// Holds a scan after its first batch until the test opens the gate.
struct Gate {
    reached: UnboundedReceiver<()>,
    open: mpsc::Sender<()>,
}

impl Gate {
    /// The hooks of a scan that commits one file per batch and waits after
    /// its first batch, and the gate that lets it go on. A scan that commits
    /// no batch does not wait.
    fn hooks() -> (ScanHooks, Self) {
        let (reached_tx, reached) = unbounded_channel();
        let (open, open_rx) = mpsc::channel::<()>();
        let open_rx = Mutex::new(open_rx);
        let on_batch = move || {
            let _ = reached_tx.send(());
            // The call returns when the test drops the sender. Every later
            // call then returns at once.
            let _ = open_rx.lock().expect("the gate lock").recv();
        };
        let hooks = ScanHooks {
            batch_size: Some(1),
            on_batch: Some(Arc::new(on_batch)),
        };
        (hooks, Self { reached, open })
    }

    /// Waits until the scan has committed its first batch and waits at the
    /// gate. The limit ends a test whose scan does not get there.
    async fn reached(&mut self) {
        tokio::time::timeout(Duration::from_secs(1), self.reached.recv())
            .await
            .expect("the scan commits its first batch")
            .expect("the hook is alive");
    }

    /// Lets the scan go on. No later batch waits.
    fn open(self) {
        drop(self.open);
    }
}

/// A server over the standard library whose scans wait at the gate.
async fn gated_server(root: &Path) -> (TestServer, Gate) {
    let (hooks, gate) = Gate::hooks();
    let server = TestServer::with_scan(config(&[root]), |settings| settings.hooks = hooks).await;
    (server, gate)
}

/// The rows of the `scans` table of `server`, in the order of the starts.
async fn scan_rows(server: &TestServer) -> Vec<Scan> {
    server.db().read(scans::list).await.expect("read the scans")
}

async fn track_count(server: &TestServer) -> u64 {
    server
        .db()
        .read(tracks::count_tracks)
        .await
        .expect("count the tracks")
}

/// The `scanStatus` object without `lastScan`.
fn scan_status(scanning: bool, count: u64) -> Value {
    json!({ "scanning": scanning, "count": count })
}

/// The `error` object with code 0 and `message`.
fn error_0(message: &str) -> Value {
    json!({ "code": 0, "message": message })
}

/// The Unix time of `text`, which is `YYYY-MM-DDTHH:MM:SSZ`. The test fails
/// when `text` has another form.
fn unix_seconds(text: &str) -> i64 {
    let bytes = text.as_bytes();
    assert_eq!(bytes.len(), 20, "{text}");
    for (position, separator) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        assert_eq!(bytes[position], separator, "{text}");
    }
    assert_eq!(bytes[19], b'Z', "{text}");
    let number = |range: std::ops::Range<usize>| -> i64 {
        let digits = &text[range];
        assert!(
            digits.bytes().all(|byte| byte.is_ascii_digit()),
            "{text}: {digits}"
        );
        digits.parse().expect("a number")
    };
    let (year, month, day) = (number(0..4), number(5..7), number(8..10));
    let (hour, minute, second) = (number(11..13), number(14..16), number(17..19));

    // The days from 1970-01-01 to the date. A year that starts in March has
    // its leap day at the end.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    days * 86_400 + hour * 3_600 + minute * 60 + second
}

#[test]
fn unix_seconds_reads_an_iso_time() {
    assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), 0);
    assert_eq!(unix_seconds("2000-02-29T00:00:00Z"), 951_782_400);
    assert_eq!(unix_seconds("2026-10-11T01:02:03Z"), 1_791_676_800 + 3_723);
    assert_eq!(unix_seconds("2024-12-31T23:59:59Z"), 1_735_689_599);
}

#[tokio::test]
async fn get_scan_status_before_the_first_scan_is_idle_with_count_0_and_no_last_scan() {
    let library = LibraryBuilder::standard().build();
    let server = TestServer::bare(config(&[library.root.path()])).await;

    for method in ["getScanStatus", "getScanStatus.view"] {
        let response = server.get_json(method, &[]).await;
        assert_eq!(response["status"], "ok", "{method}");
        assert_eq!(response["scanStatus"], scan_status(false, 0), "{method}");
    }
    let body = server.get_xml("getScanStatus", &[]).await;
    assert!(
        body.contains(r#"<scanStatus count="0" scanning="false"/>"#),
        "{body}"
    );
    // The method starts no scan and reads no folder.
    assert_eq!(scan_rows(&server).await, []);
    assert_eq!(track_count(&server).await, 0);
}

#[tokio::test]
async fn start_scan_starts_one_scan_and_a_call_during_the_scan_starts_no_other() {
    let library = LibraryBuilder::standard().build();
    let (server, mut gate) = gated_server(library.root.path()).await;

    // The call returns while the scan runs.
    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["scanStatus"]["scanning"], true);
    assert!(response["scanStatus"].get("lastScan").is_none());
    gate.reached().await;

    // The scan has committed one file and waits. Every further call
    // returns the status of that scan and starts no other, full or not.
    let running = scan_status(true, 1);
    for params in [&[][..], &[("fullScan", "true")], &[("fullScan", "false")]] {
        let response = server.get_json("startScan", params).await;
        assert_eq!(response["status"], "ok", "{params:?}");
        assert_eq!(response["scanStatus"], running, "{params:?}");
    }
    let response = server.post_form("startScan.view", &[("f", "json")]).await;
    let response = subsonic_response(&body_string(response).await);
    assert_eq!(response["scanStatus"], running);
    let body = server.get_xml("startScan", &[]).await;
    assert!(
        body.contains(r#"<scanStatus count="1" scanning="true"/>"#),
        "{body}"
    );

    // The server answers other requests during the scan. No scan has ended,
    // so the status has no `lastScan`.
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["scanStatus"], running);
    assert_eq!(server.get_json("ping", &[]).await["status"], "ok");
    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].finished_at, None);
    assert_eq!(track_count(&server).await, 1);

    gate.open();
    server.wait_for_scan().await;
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["scanStatus"]["scanning"], false);
    assert_eq!(response["scanStatus"]["count"], 10);
    // The calls during the scan queued none.
    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].counts.added, 10);
    assert_eq!(rows[0].error, None);
    assert_eq!(track_count(&server).await, 10);

    // The server is idle, so the next call starts a second scan.
    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["scanStatus"]["scanning"], true);
    server.wait_for_scan().await;
    assert_eq!(scan_rows(&server).await.len(), 2);
}

#[tokio::test]
async fn get_scan_status_after_a_scan_has_the_count_and_the_end_time_of_the_scan() {
    let library = LibraryBuilder::standard().build();
    let server = TestServer::bare(config(&[library.root.path()])).await;
    server.scan_and_wait().await;

    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["status"], "ok");
    let status = &response["scanStatus"];
    assert_eq!(status["scanning"], false);
    assert_eq!(status["count"], 10);
    let last_scan = status["lastScan"].as_str().expect("lastScan is a string");
    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].counts.seen, 10);
    assert_eq!(Some(unix_seconds(last_scan)), rows[0].finished_at);
    assert_eq!(
        status.as_object().map(serde_json::Map::len),
        Some(3),
        "{status}"
    );

    let body = server.get_xml("getScanStatus", &[]).await;
    let element = format!(r#"<scanStatus count="10" lastScan="{last_scan}" scanning="false"/>"#);
    assert!(body.contains(&element), "{body}");

    // A scan that finds nothing to do counts the files again.
    server.scan_and_wait().await;
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["scanStatus"]["scanning"], false);
    assert_eq!(response["scanStatus"]["count"], 10);
    assert_eq!(scan_rows(&server).await.len(), 2);
}

#[tokio::test]
async fn start_scan_answers_scanning_in_xml_and_json() {
    // The server has no music folder, so the count of its scan stays 0.
    let server = TestServer::bare(config(&[])).await;

    let body = server.get_xml("startScan", &[]).await;
    assert!(body.contains(r#"status="ok""#), "{body}");
    assert!(
        body.contains(r#"<scanStatus count="0" scanning="true"/>"#),
        "{body}"
    );
    server.wait_for_scan().await;

    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["scanStatus"], scan_status(true, 0));
    server.wait_for_scan().await;
    assert_eq!(scan_rows(&server).await.len(), 2);
}

#[tokio::test]
async fn full_scan_parameter_selects_a_scan_that_reads_every_tag_again() {
    let library = LibraryBuilder::standard().build();
    let server = TestServer::bare(config(&[library.root.path()])).await;
    server.scan_and_wait().await;

    // `changed` counts the rows whose tags the scan read again.
    for (params, changed) in [
        (&[][..], 0),
        (&[("fullScan", "false")], 0),
        (&[("fullScan", "")], 0),
        (&[("fullScan", "true")], 10),
        (&[("fullScan", "TRUE")], 10),
    ] {
        let response = server.get_json("startScan", params).await;
        assert_eq!(response["scanStatus"]["scanning"], true, "{params:?}");
        server.wait_for_scan().await;
        let row = scan_rows(&server).await.pop().expect("a scan row");
        assert_eq!(row.counts.seen, 10, "{params:?}");
        assert_eq!(row.counts.changed, changed, "{params:?}");
        assert_eq!(row.error, None, "{params:?}");
    }
    let before = scan_rows(&server).await.len();

    // A value that is no boolean starts no scan.
    let response = server.get_json("startScan", &[("fullScan", "maybe")]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(
        response["error"],
        error_0("parameter fullScan must be true or false")
    );
    assert!(response.get("scanStatus").is_none());
    assert!(!server.scan().status().scanning);
    assert_eq!(scan_rows(&server).await.len(), before);
}

#[tokio::test]
async fn get_scan_status_after_a_scan_that_recorded_errors_is_idle_with_its_end_time() {
    let library = LibraryBuilder::standard().build();
    let small = LibraryBuilder::new()
        .track("Tessa Lund", "Field Notes", "Marsh", 1, "ogg")
        .track("Tessa Lund", "Field Notes", "Heron", 2, "ogg")
        .build();
    let server = TestServer::bare(config(&[library.root.path(), small.root.path()])).await;
    server.scan_and_wait().await;
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["scanStatus"]["count"], 12);

    // The second folder had tracks and holds no supported file now. The
    // scan records that as an error and deletes none of its tracks.
    for file in &small.files {
        fs::remove_file(file).expect("remove the file");
    }
    server.scan_and_wait().await;

    let rows = scan_rows(&server).await;
    assert_eq!(rows.len(), 2);
    let error = rows[1].error.as_deref().expect("the scan has an error");
    assert!(error.contains("has no supported file"), "{error}");
    assert!(
        error.contains(&small.root.path().display().to_string()),
        "{error}"
    );
    assert_eq!(track_count(&server).await, 12);

    // The status is the one of a scan that has ended: the server is idle,
    // the count is the number of files the scan saw, and `lastScan` is the
    // end time of this scan. The answer is not an error.
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["status"], "ok");
    let status = &response["scanStatus"];
    assert_eq!(status["scanning"], false);
    assert_eq!(status["count"], 10);
    let last_scan = status["lastScan"].as_str().expect("lastScan is a string");
    assert_eq!(Some(unix_seconds(last_scan)), rows[1].finished_at);
    assert!(response.get("error").is_none());

    // The next call starts a scan.
    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["scanStatus"]["scanning"], true);
    server.wait_for_scan().await;
    assert_eq!(scan_rows(&server).await.len(), 3);
}

#[tokio::test]
async fn start_scan_while_another_process_scans_is_error_0_and_starts_no_scan() {
    let library = LibraryBuilder::standard().build();
    let data = tempfile::tempdir().expect("temp dir");
    let lock_path = data.path().join("scan.lock");
    let server = TestServer::with_scan(config(&[library.root.path()]), |settings| {
        settings.lock_path = Some(lock_path.clone());
    })
    .await;

    // The lock that a scan of `wn-server scan` holds.
    let other = ScanLock::try_acquire(&lock_path)
        .expect("lock the file")
        .expect("the lock is free");
    for params in [&[][..], &[("fullScan", "true")]] {
        let response = server.get_json("startScan", params).await;
        assert_eq!(response["status"], "failed", "{params:?}");
        assert_eq!(response["error"], error_0(RUNNING_ELSEWHERE), "{params:?}");
        assert!(response.get("scanStatus").is_none(), "{params:?}");
    }
    let body = server.get_xml("startScan", &[]).await;
    assert!(
        body.contains(&format!(
            r#"<error code="0" message="{RUNNING_ELSEWHERE}"/>"#
        )),
        "{body}"
    );
    // The server reports its own scans only, and it wrote nothing.
    let response = server.get_json("getScanStatus", &[]).await;
    assert_eq!(response["scanStatus"], scan_status(false, 0));
    assert_eq!(scan_rows(&server).await, []);
    assert_eq!(track_count(&server).await, 0);

    // The other scan has ended.
    drop(other);
    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["scanStatus"]["scanning"], true);
    server.wait_for_scan().await;
    assert_eq!(track_count(&server).await, 10);
}

#[tokio::test]
async fn start_scan_is_error_0_when_the_scan_lock_cannot_be_taken() {
    let library = LibraryBuilder::standard().build();
    let data = tempfile::tempdir().expect("temp dir");
    let lock_path = data.path().join("missing").join("scan.lock");
    let server = TestServer::with_scan(config(&[library.root.path()]), |settings| {
        settings.lock_path = Some(lock_path.clone());
    })
    .await;

    let response = server.get_json("startScan", &[]).await;
    assert_eq!(response["status"], "failed");
    assert_eq!(response["error"], error_0("the server cannot start a scan"));
    // The message holds no path of the server.
    assert!(!response.to_string().contains("scan.lock"), "{response}");
    assert!(!server.scan().status().scanning);
    assert_eq!(scan_rows(&server).await, []);
}

#[tokio::test]
async fn scan_methods_need_a_user_and_start_no_scan_without_one() {
    let library = LibraryBuilder::standard().build();
    let server = TestServer::bare(config(&[library.root.path()])).await;

    for method in ["startScan", "startScan.view", "getScanStatus"] {
        let response = server.get_json_noauth(method, &[V, C]).await;
        assert_eq!(response["error"]["code"], 10, "{method}");
        let wrong = [V, C, ("u", "owner"), ("p", "wrong")];
        let response = server.get_json_noauth(method, &wrong).await;
        assert_eq!(response["error"]["code"], 40, "{method}");
        assert!(response.get("scanStatus").is_none(), "{method}");
    }
    assert!(!server.scan().status().scanning);
    assert_eq!(scan_rows(&server).await, []);
    assert_eq!(track_count(&server).await, 0);
}
