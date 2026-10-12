//! The media method `stream` (`docs/design.md` sections 3.9 and 4).
//!
//! The method answers with the bytes of a file and not with the envelope.
//! An error is an envelope, as for every other method.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::Response;

use super::AppState;
use super::envelope::ApiError;
use super::params::Params;
use crate::auth::extract::Authed;
use crate::db::Db;
use crate::db::queries::browse;
use crate::db::queries::tracks::Track;
use crate::ids::{self, Kind};
use crate::media::stream::{self, StreamPlan, StreamRequest};

/// The message of the error for a request that the rules select a transcode
/// for. The server has no transcoder.
const TRANSCODING_UNAVAILABLE: &str = "transcoding not available yet";

/// `stream`: the original file of the track `id`, unchanged, with the
/// content type of the track.
///
/// `id` is required; the error has code 10. Error 70 answers an ID that is
/// no track ID or that no track has, a track without a file, and a file that
/// `stream::raw` does not serve: one that is gone, one outside the music
/// folders, and one that was replaced since the scan.
///
/// `format`, `maxBitRate` and `timeOffset` select between the original file
/// and a transcode (`stream::decide`). A request for which the rules select
/// a transcode gets error 0, because the server has no transcoder. A request
/// for which they select the original file gets it whatever these
/// parameters and `estimateContentLength` hold.
///
/// The request is a GET, a HEAD or a POST with a form body. `Range`,
/// `If-Range`, `If-None-Match` and `If-Modified-Since` are the request
/// headers that the response depends on. The method counts no play.
pub async fn stream(
    State(state): State<AppState>,
    _user: Authed,
    params: Params,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = params.req_str("id")?;
    let request = StreamRequest::from_params(&params)?;
    let (track, path) = file_of_track(&state.db, id).await?;
    match stream::decide(&track, &request, &state.config.transcoding)? {
        StreamPlan::Raw => {
            let folders = &state.config.library.folders;
            stream::raw(&track, path, folders, &method, &headers).await
        }
        StreamPlan::Transcode { .. } => Err(ApiError::Other(TRANSCODING_UNAVAILABLE.to_owned())),
    }
}

/// Reads the track `id` and the absolute path of its file from the database.
///
/// `id` is the value of a request parameter. It is compared with the form of
/// a track ID and is then a parameter of an SQL statement. The path is built
/// from the rows alone, so no value of the request is part of it.
///
/// Error 70 answers a value that is no track ID, an ID that no track has and
/// a track without a file (`docs/design.md` section 3.4).
async fn file_of_track(db: &Db, id: &str) -> Result<(Track, PathBuf), ApiError> {
    if !matches!(ids::parse(id), Some((Kind::Track, _))) {
        return Err(ApiError::NotFound);
    }
    let id = id.to_owned();
    let found = db
        .read(move |conn| {
            let Some(song) = browse::track(conn, &id)? else {
                return Ok(None);
            };
            // `file_path` returns no path for a track without a file.
            let path = browse::file_path(conn, &song.track)?;
            Ok(path.map(|path| (song.track, path)))
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "cannot read the track");
            ApiError::Other("the server cannot read its library".to_owned())
        })?;
    found.ok_or(ApiError::NotFound)
}
