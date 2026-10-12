//! The scan methods: `startScan` and `getScanStatus` (`docs/design.md`
//! section 4).
//!
//! Both answer with the `scanStatus` element. Its `scanning` and `count`
//! describe the scans of this server process ([`ScanController::status`]). A
//! scan that `wn-server scan` runs in another process is not reported as
//! running.
//!
//! [`ScanController::status`]: crate::library::scan_controller::ScanController::status

use axum::extract::State;
use serde::Serialize;

use super::envelope::ApiError;
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::scans;
use crate::library::scan_controller::StartOutcome;

/// The message of the error that `startScan` returns while another process
/// scans the library.
const RUNNING_ELSEWHERE: &str = "another process is scanning the library";

/// The `scanStatus` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanStatus {
    /// Whether a scan of the server process runs.
    scanning: bool,
    /// The supported files that the running scan has finished with. When no
    /// scan runs, the number of the scan of the server process that ended
    /// last, and 0 before the first one.
    count: u64,
    /// The end time of the scan that ended last, in ISO-8601 and UTC, as in
    /// `2026-10-11T08:15:30Z`. `getScanStatus` sets it.
    ///
    /// The field is not part of the OpenSubsonic `scanStatus` response.
    /// Navidrome sends it, and it is here for the clients that read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_scan: Option<String>,
}

/// `startScan`: starts a scan of the music folders and returns without
/// waiting for it. `fullScan=true` reads the tags of every file again.
///
/// The answer has `scanning: true` when the call started a scan, and also
/// when a scan of the server already runs. In that case the call starts no
/// second scan and queues none, so a `fullScan` that arrives during a scan
/// has no effect. While another process scans the library, the call starts
/// no scan and the answer is error 0.
pub async fn start_scan(State(state): State<AppState>, _user: Authed, params: Params) -> ApiResult {
    let full = params.opt_bool("fullScan")?.unwrap_or(false);
    match state.scan.start(full) {
        Ok(StartOutcome::Started | StartOutcome::AlreadyRunning) => {
            let status = ScanStatus {
                // A scan ran when the call was handled. It may have ended
                // since; `getScanStatus` reports that.
                scanning: true,
                count: state.scan.status().count,
                last_scan: None,
            };
            Ok(Payload::new("scanStatus", status))
        }
        Ok(StartOutcome::RunningElsewhere) => Err(ApiError::Other(RUNNING_ELSEWHERE.to_owned())),
        Err(error) => {
            tracing::error!(%error, "cannot start a scan");
            Err(ApiError::Other("the server cannot start a scan".to_owned()))
        }
    }
}

/// `getScanStatus`: whether a scan of the server runs, its count, and
/// `lastScan`, the end time of the scan that ended last.
///
/// `lastScan` comes from the `scans` table, so it also names a scan that
/// `wn-server scan` ran and a scan from before the server started. A scan
/// that ended with an error counts, and so does a cancelled scan. The field
/// is absent until a scan has ended.
pub async fn get_scan_status(State(state): State<AppState>, _user: Authed) -> ApiResult {
    let last_scan = state.db.read(scans::last_finished).await.map_err(|error| {
        tracing::error!(%error, "cannot read the scans");
        ApiError::Other("the server cannot read its scans".to_owned())
    })?;
    let status = state.scan.status();
    Ok(Payload::new(
        "scanStatus",
        ScanStatus {
            scanning: status.scanning,
            count: status.count,
            last_scan,
        },
    ))
}
