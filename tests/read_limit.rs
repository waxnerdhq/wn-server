//! Router tests of the time limit of a read (`docs/design.md` section 3.4):
//! what a client and the log get for a request whose statement runs past the
//! limit, the requests after it, and a scan while the limit is zero.
//!
//! A test makes the statement of `getScanStatus` slow with a temporary view.
//! The database of a `TestServer` has one connection. SQLite reads a
//! temporary view of that connection in place of the table with its name, so
//! the statement counts 50 000 000 rows. That takes seconds, which no test
//! waits for: a statement that is not interrupted fails its test with the
//! answer of the method.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use wn_server::VERSION;
use wn_server::config::Config;
use wn_server::db::READ_LIMIT;
use wn_server::db::queries::{scans, tracks};

use common::{LibraryBuilder, TestServer, body_string, captured_log, content_type};

/// The limit of the reads of a request that a test expects to fail. The
/// read of the accounts of that request has the same limit and takes
/// microseconds.
const LIMIT: Duration = Duration::from_millis(300);

/// A view that the statement of `getScanStatus` reads in place of the
/// `scans` table.
const SLOW_SCANS: &str = "CREATE TEMP VIEW scans AS
     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50000000)
     SELECT i AS finished_at FROM n";

/// The message of `getScanStatus` for a database that it cannot read.
const MESSAGE: &str = "the server cannot read its scans";

/// A server whose `getScanStatus` runs a statement of seconds.
async fn server_with_slow_scans() -> TestServer {
    let server = TestServer::bare(Config::default()).await;
    server
        .db()
        .write(|conn| Ok(conn.execute_batch(SLOW_SCANS)?))
        .await
        .expect("create the view");
    server
}

/// The lines of the captured log that carry the span of a request of the
/// client `client` and the text `text`. Every test of this file logs to the
/// same buffer, so a test finds its request by a client name that only it
/// sends.
fn log_lines(client: &str, text: &str) -> Vec<String> {
    captured_log()
        .lines_with(&format!(r#"c="{client}""#))
        .into_iter()
        .filter(|line| line.contains(text))
        .collect()
}

#[tokio::test]
async fn request_whose_statement_passes_the_limit_is_error_0_and_the_log_has_the_cause() {
    captured_log();
    let server = server_with_slow_scans().await;
    server.db().set_read_limit(LIMIT);

    // The whole body is compared, so it holds no SQL text, no name of a
    // table and no path.
    let response = server
        .get_raw("getScanStatus", &[("f", "json"), ("c", "read-limit-json")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/json");
    assert_eq!(
        body_string(response).await,
        format!(
            concat!(
                r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","#,
                r#""type":"wn-server","serverVersion":"{}","openSubsonic":true,"#,
                r#""error":{{"code":0,"message":"{}"}}}}}}"#
            ),
            VERSION, MESSAGE
        )
    );

    let response = server
        .get_raw("getScanStatus.view", &[("c", "read-limit-xml")])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), "application/xml");
    assert_eq!(
        body_string(response).await,
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="failed""#,
                r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true">"#,
                r#"<error code="0" message="{}"/></subsonic-response>"#
            ),
            VERSION, MESSAGE
        )
    );

    for client in ["read-limit-json", "read-limit-xml"] {
        let lines = log_lines(client, "cannot read the scans");
        assert_eq!(lines.len(), 1, "{client}: {}", captured_log().text());
        let line = &lines[0];
        assert!(line.contains("ERROR"), "{line}");
        assert!(line.contains("getScanStatus"), "{line}");
        assert!(
            line.contains("a statement of a read was interrupted at the time limit of 300ms"),
            "{line}"
        );
    }
}

#[tokio::test]
async fn requests_after_an_interrupted_statement_are_answered() {
    let server = server_with_slow_scans().await;
    server.db().set_read_limit(LIMIT);
    let failed = server.get_json("getScanStatus", &[]).await;
    assert_eq!(failed["status"], "failed", "{failed}");
    assert_eq!(failed["error"], json!({ "code": 0, "message": MESSAGE }));
    server.db().set_read_limit(READ_LIMIT);

    // The database has one connection, so every later request reads with
    // the connection of the interrupted statement.
    let folders = server.get_json("getMusicFolders", &[]).await;
    assert_eq!(folders["status"], "ok", "{folders}");

    server
        .db()
        .write(|conn| Ok(conn.execute_batch("DROP VIEW temp.scans")?))
        .await
        .expect("drop the view");
    let status = server.get_json("getScanStatus", &[]).await;
    assert_eq!(status["status"], "ok", "{status}");
    assert_eq!(
        status["scanStatus"],
        json!({ "scanning": false, "count": 0 })
    );
}

#[tokio::test]
async fn scan_completes_while_the_limit_of_a_read_is_zero() {
    let library = LibraryBuilder::standard().build();
    let mut config = Config::default();
    config.library.folders = vec![library.root.path().to_path_buf()];
    config.library.scan_poll_ms = 10;
    let server = TestServer::bare(config).await;

    // Every read of `Db::read` has a limit of zero. The scan writes and
    // reads through the write connection, which has no limit.
    server.db().set_read_limit(Duration::ZERO);
    server.scan_and_wait().await;

    server.db().set_read_limit(READ_LIMIT);
    let (tracks, scans) = server
        .db()
        .read(|conn| Ok((tracks::count_tracks(conn)?, scans::list(conn)?)))
        .await
        .expect("read the rows of the scan");
    assert_eq!(tracks, 10);
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0].counts.seen, 10);
    assert_eq!(scans[0].counts.added, 10);
    assert_eq!(scans[0].error, None);
    assert!(scans[0].finished_at.is_some());
}
