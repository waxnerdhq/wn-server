//! The media method `stream` (`docs/design.md` sections 3.9 and 4).
//!
//! The method answers with the bytes of a file or of a transcode and not
//! with the envelope. An error is an envelope, as for every other method.

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
use crate::media::transcode::TranscodeJob;

/// `stream`: the track `id` as its original file, unchanged and with the
/// content type of the track, or as a transcode to Opus or MP3.
///
/// `id` is required; the error has code 10. Error 70 answers an ID that is
/// no track ID or that no track has, a track without a file, and a file that
/// the server does not serve: one that is gone, one outside the music
/// folders, and one that was replaced since the scan.
///
/// `format` and `maxBitRate` select between the original file and a
/// transcode. `stream::decide` is the one place of that rule: the answer is
/// the original file unless the request asks for a format or for a bitrate
/// below the bitrate of the file.
///
/// - The original file is the answer of `stream::raw`. `Range`, `If-Range`,
///   `If-None-Match` and `If-Modified-Since` are the request headers that
///   it depends on. `timeOffset` and `estimateContentLength` are read and
///   have no effect on it.
/// - A transcode is the answer of `stream::transcoded`, with the transcoder
///   of the state. The stream starts at the second `timeOffset` of the
///   track, and `estimateContentLength=true` gives the response an
///   estimated `Content-Length`. No request header changes it. HTTP 503
///   with `Retry-After: 5` answers a request for which no transcode slot is
///   free, and error 0 answers a request on a server without ffmpeg.
///
/// The request is a GET, a HEAD or a POST with a form body. The method
/// counts no play.
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
    let folders = &state.config.library.folders;
    match stream::decide(&track, &request, &state.config.transcoding) {
        StreamPlan::Raw => stream::raw(&track, path, folders, &method, &headers).await,
        StreamPlan::Transcode {
            target,
            bitrate_kbps,
            offset_s,
        } => {
            let job = TranscodeJob {
                path,
                target,
                bitrate_kbps,
                offset_s,
            };
            let estimate = request.estimate_content_length;
            let transcoder = state.transcoder.as_ref();
            stream::transcoded(&track, job, folders, &method, estimate, transcoder).await
        }
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
