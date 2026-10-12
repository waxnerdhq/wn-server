//! Streaming of a track (`docs/design.md` section 3.9): what a `stream`
//! request asks for, the decision between the original file and a transcode,
//! and the response that serves the original file.
//!
//! # Which file is served
//!
//! The path of a file comes from the track row and the row of its music
//! folder. No value of the request is part of it. [`raw`] serves the file
//! only when two things hold:
//!
//! - The file is inside a configured music folder after every symbolic link
//!   of its path is resolved. The scanner follows links, so a track row can
//!   name a path that leads out of the folders. Such a file is not served.
//! - The file has the size and the modification time that the scan stored on
//!   the row. A file that was replaced at the path since the scan has other
//!   bytes than the track of the ID, and it is not served under that ID.
//!
//! The second check is the one by which the scanner decides that a file did
//! not change. It does not tell a file from one that replaced it with the
//! same size in the same second.
//!
//! Both checks read the path and not an open file. `ServeFile` opens the
//! path after them, so a file that is exchanged between the checks and the
//! open is served for that one request.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mime_guess::mime::{self, Mime};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tower_http::services::fs::ServeFileSystemResponseBody;

use crate::api::envelope::ApiError;
use crate::api::params::Params;
use crate::config::Transcoding;
use crate::db::queries::tracks::Track;

/// The bitrates of a transcode, in kbps. A bitrate outside the range is
/// moved to its nearest end.
const BITRATE_KBPS: RangeInclusive<u32> = 32..=320;

/// The message of the error for a `format` that names no target.
const UNKNOWN_FORMAT: &str = "parameter format must be raw, opus or mp3";

/// The message of the error for a file that the server cannot read.
const CANNOT_READ: &str = "the server cannot read the file of the track";

/// The message of the error for a file whose modification time has no HTTP
/// date.
const NO_HTTP_DATE: &str =
    "the modification time of the file of the track is outside the years 1970 to 9999";

/// The modification times, in Unix seconds, that an HTTP date can hold: from
/// 1970 to the end of the year 9999. `ServeFile` writes the modification
/// time of a file as an HTTP date and panics for a time outside this range.
const HTTP_DATE_SECONDS: Range<i64> = 0..253_402_300_800;

/// What a `stream` request asks for besides the track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamRequest {
    /// `format`: `raw`, or the name of a transcode target. `None` when the
    /// parameter is absent or empty.
    pub format: Option<String>,
    /// `maxBitRate`: the highest bitrate the client accepts, in kbps. 0 is
    /// no limit, as an absent parameter is.
    pub max_bitrate_kbps: Option<u32>,
    /// `timeOffset`: the second of the track at which a transcoded stream
    /// starts.
    pub time_offset_s: Option<u32>,
    /// `estimateContentLength`: whether a transcoded stream reports an
    /// estimated length.
    pub estimate_content_length: bool,
}

impl StreamRequest {
    /// Reads the parameters of a `stream` request. A value of the wrong type
    /// is an error with code 0 that names the parameter.
    pub fn from_params(params: &Params) -> Result<Self, ApiError> {
        Ok(Self {
            format: params
                .opt_str("format")
                .filter(|format| !format.is_empty())
                .map(str::to_owned),
            max_bitrate_kbps: params.opt_u32("maxBitRate")?,
            time_offset_s: params.opt_u32("timeOffset")?,
            estimate_content_length: params.opt_bool("estimateContentLength")?.unwrap_or(false),
        })
    }
}

/// The format of a transcoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Opus in an Ogg container.
    Opus,
    /// MP3.
    Mp3,
}

/// How the server answers a `stream` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPlan {
    /// The original file, unchanged.
    Raw,
    /// A transcode of the file to `target` at `bitrate_kbps`, which starts
    /// at the second `offset_s` of the track.
    Transcode {
        target: Target,
        bitrate_kbps: u32,
        offset_s: u32,
    },
}

/// Decides between the original file of `track` and a transcode.
///
/// - `format=raw` is the original file. No `format` and no `maxBitRate`, or
///   a `maxBitRate` of 0, is the original file.
/// - `format=opus` and `format=mp3` are a transcode at `maxBitRate`, or at
///   the bitrate of `config` for the target when the request has none.
/// - A `maxBitRate` without a `format` is a transcode to Opus at that
///   bitrate when the track is lossless or its bitrate exceeds `maxBitRate`.
///   Otherwise it is the original file. A lossy track whose bitrate is not
///   known is served as the original file.
/// - Any other `format` is an error with code 0.
///
/// `maxBitRate` and the bitrates of `config` are moved into 32 to 320 kbps
/// before these rules read them.
///
/// Remote-mode seam: remote mode adds an argument for the kind of the
/// connection to this function, which then forces a lossy transcode for a
/// connection that is not direct (`docs/design.md` section 3.14). It is the
/// only function that decides between the original file and a transcode.
pub fn decide(
    track: &Track,
    request: &StreamRequest,
    config: &Transcoding,
) -> Result<StreamPlan, ApiError> {
    let limit = request
        .max_bitrate_kbps
        .filter(|&kbps| kbps > 0)
        .map(clamp_bitrate);
    let transcode = |target, bitrate_kbps| StreamPlan::Transcode {
        target,
        bitrate_kbps,
        offset_s: request.time_offset_s.unwrap_or(0),
    };
    match (request.format.as_deref(), limit) {
        (Some("raw"), _) | (None, None) => Ok(StreamPlan::Raw),
        (Some("opus"), _) => {
            let bitrate = limit.unwrap_or_else(|| clamp_bitrate(config.opus_bitrate_kbps));
            Ok(transcode(Target::Opus, bitrate))
        }
        (Some("mp3"), _) => {
            let bitrate = limit.unwrap_or_else(|| clamp_bitrate(config.mp3_bitrate_kbps));
            Ok(transcode(Target::Mp3, bitrate))
        }
        (None, Some(limit)) => {
            let above = track.bitrate_kbps.is_some_and(|kbps| kbps > limit);
            if track.lossless || above {
                Ok(transcode(Target::Opus, limit))
            } else {
                Ok(StreamPlan::Raw)
            }
        }
        (Some(_), _) => Err(ApiError::Other(UNKNOWN_FORMAT.to_owned())),
    }
}

/// Moves `kbps` into [`BITRATE_KBPS`].
fn clamp_bitrate(kbps: u32) -> u32 {
    kbps.clamp(*BITRATE_KBPS.start(), *BITRATE_KBPS.end())
}

/// Why the file of a track is not served.
#[derive(Debug)]
enum Refusal {
    /// The path cannot be resolved or the state of the file cannot be read.
    /// The file is gone, a link of the path leads nowhere, or the server may
    /// not read a directory of the path.
    Unreadable(io::Error),
    /// The file is in no configured music folder. A symbolic link of the
    /// path leads out of the folders. The value is the resolved path.
    Outside(PathBuf),
    /// The path holds no regular file, or a file with another size or
    /// another modification time than the track row has.
    Replaced,
}

/// Serves the original file of `track` for a request with `method` and
/// `headers`. `path` is the absolute path that the track row and the row of
/// its music folder give, and `folders` are the configured music folders.
///
/// The file is served by `tower_http::services::ServeFile`, which reads it
/// in chunks of 64 KiB as the client takes the body. No task outlives the
/// body, and the file is closed when the body is dropped, which is what a
/// client that disconnects causes.
///
/// - `ServeFile` answers GET and HEAD. A HEAD request is passed on as HEAD,
///   so no file is opened for it. Every other request is passed on as GET:
///   the router has read the form body of a POST, and the method has no
///   other meaning here.
/// - Only `Range`, `If-None-Match` and `If-Modified-Since` of the client
///   request are passed on. `Range` is not passed on for HEAD, because a
///   range is defined for GET only.
/// - `If-Range` is evaluated here, because `ServeFile` does not read it: a
///   range request whose `If-Range` names another version of the file gets
///   the whole file.
/// - A request for several ranges gets the whole file, because `ServeFile`
///   builds no multipart response.
/// - The content type is the one of the track row.
///
/// Error 70 answers a file that is gone, that is outside the music folders
/// or that was replaced since the scan (see the module comment).
pub async fn raw(
    track: &Track,
    path: PathBuf,
    folders: &[PathBuf],
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    if !HTTP_DATE_SECONDS.contains(&track.mtime) {
        tracing::warn!(track = %track.id, mtime = track.mtime, "{NO_HTTP_DATE}");
        return Err(ApiError::Other(NO_HTTP_DATE.to_owned()));
    }

    let (size, mtime) = (track.size, track.mtime);
    let folders = folders.to_vec();
    let checked = tokio::task::spawn_blocking(move || library_file(&path, &folders, size, mtime));
    let path = match checked.await {
        Ok(Ok(path)) => path,
        Ok(Err(refusal)) => return Err(refused(track, &refusal)),
        Err(error) => {
            tracing::error!(%error, track = %track.id, "the check of the file did not finish");
            return Err(ApiError::Other(CANNOT_READ.to_owned()));
        }
    };

    let mime = track
        .content_type
        .parse::<Mime>()
        .unwrap_or(mime::APPLICATION_OCTET_STREAM);
    let head = method == Method::HEAD;
    let range = headers
        .get(header::RANGE)
        .filter(|_| !head)
        .map(|range| whole_file_for_long_suffix(range, size));

    let response = serve(&path, &mime, file_request(head, headers, range.as_ref())).await;
    let ignore_range = match (&range, headers.get(header::IF_RANGE)) {
        (Some(_), Some(validator)) => {
            let answers_range = matches!(
                response.status(),
                StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE
            );
            answers_range && !if_range_holds(validator, response.headers())
        }
        _ => false,
    };
    let response = if ignore_range {
        // The client holds a part of another version of the file. The first
        // response is dropped, which closes its file, before the file is
        // opened again.
        drop(response);
        serve(&path, &mime, file_request(head, headers, None)).await
    } else {
        response
    };
    finish(response, track)
}

/// Logs why the file of `track` is not served and returns the error of the
/// request, which is error 70 for every reason.
fn refused(track: &Track, refusal: &Refusal) -> ApiError {
    match refusal {
        Refusal::Unreadable(error) => tracing::info!(
            track = %track.id,
            path = %track.rel_path,
            %error,
            "the file of the track cannot be read; it is not served"
        ),
        Refusal::Outside(real) => tracing::info!(
            track = %track.id,
            path = %track.rel_path,
            resolved = %real.display(),
            "the file of the track is outside the music folders; it is not served"
        ),
        Refusal::Replaced => tracing::info!(
            track = %track.id,
            path = %track.rel_path,
            "the file of the track changed since the scan; it is not served until the next scan"
        ),
    }
    ApiError::NotFound
}

/// Returns the resolved path of the file at `path` when the file may be
/// served as the track whose row has `size` and `mtime`.
///
/// `fs::canonicalize` resolves every symbolic link of the path, so the
/// result names the file itself. That file must be inside the resolved path
/// of one of `folders`. A folder may itself be reached through a link. A
/// folder that cannot be resolved holds no file. The folders are resolved
/// for every call and no result is kept, so a folder link that changes is
/// followed from the next request on.
///
/// The file must be a regular file with the size `size` and the modification
/// time `mtime`, in whole Unix seconds as the scanner stores it. This is the
/// test by which the scanner decides that a file did not change.
fn library_file(
    path: &Path,
    folders: &[PathBuf],
    size: u64,
    mtime: i64,
) -> Result<PathBuf, Refusal> {
    let real = fs::canonicalize(path).map_err(Refusal::Unreadable)?;
    let in_library = folders
        .iter()
        .filter_map(|folder| fs::canonicalize(folder).ok())
        .any(|folder| real.starts_with(folder));
    if !in_library {
        return Err(Refusal::Outside(real));
    }

    let state = fs::metadata(&real).map_err(Refusal::Unreadable)?;
    let modified = state.modified().map_err(Refusal::Unreadable)?;
    // A time before 1970 equals no `mtime` that reaches this function.
    let seconds = modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|age| i64::try_from(age.as_secs()).ok());
    if state.is_file() && state.len() == size && seconds == Some(mtime) {
        Ok(real)
    } else {
        Err(Refusal::Replaced)
    }
}

/// Returns `range`, the value of a `Range` header, as `ServeFile` gets it
/// for a file of `size` bytes.
///
/// A suffix range that is longer than the file selects the whole file
/// (RFC 9110 section 14.1.2). `ServeFile` answers such a range with HTTP
/// 416, so it is replaced by `bytes=0-`, which selects the same bytes.
fn whole_file_for_long_suffix(range: &HeaderValue, size: u64) -> HeaderValue {
    let longer_than_the_file = range
        .to_str()
        .ok()
        .and_then(|range| range.strip_prefix("bytes=-"))
        .and_then(|length| length.parse::<u64>().ok())
        .is_some_and(|length| length > size);
    if longer_than_the_file {
        HeaderValue::from_static("bytes=0-")
    } else {
        range.clone()
    }
}

/// Builds the request that `ServeFile` answers: a new GET request, or a HEAD
/// request when `head` is set, with `If-None-Match` and `If-Modified-Since`
/// of `headers` and with `range` as its `Range` header. Nothing else of the
/// client request is part of it.
fn file_request(head: bool, headers: &HeaderMap, range: Option<&HeaderValue>) -> Request<()> {
    let mut request = Request::new(());
    if head {
        *request.method_mut() = Method::HEAD;
    }
    for name in [header::IF_NONE_MATCH, header::IF_MODIFIED_SINCE] {
        if let Some(value) = headers.get(&name) {
            request.headers_mut().insert(name, value.clone());
        }
    }
    if let Some(range) = range {
        request.headers_mut().insert(header::RANGE, range.clone());
    }
    request
}

/// Answers `request` with the file at `path` and the content type `mime`.
async fn serve(
    path: &Path,
    mime: &Mime,
    request: Request<()>,
) -> Response<ServeFileSystemResponseBody> {
    let service = ServeFile::new_with_mime(path, mime).ignore_multi_range_requests(true);
    match service.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// Returns whether the `If-Range` value `validator` names the version of the
/// file that the response headers `current` describe (RFC 9110 section
/// 13.1.5).
///
/// An entity tag must equal the `ETag` of the file, and a weak tag equals
/// none. A date must equal the `Last-Modified` value of the file byte for
/// byte, which holds for a client that sends back the value it received.
fn if_range_holds(validator: &HeaderValue, current: &HeaderMap) -> bool {
    let validator = validator.as_bytes();
    if validator.starts_with(b"W/") {
        false
    } else if validator.starts_with(b"\"") {
        current
            .get(header::ETAG)
            .is_some_and(|tag| tag.as_bytes() == validator)
    } else {
        current
            .get(header::LAST_MODIFIED)
            .is_some_and(|date| date.as_bytes() == validator)
    }
}

/// Turns the response of `ServeFile` into the response of the request.
///
/// HTTP 404 is a file that was removed after the check of [`raw`], and it
/// becomes error 70. HTTP 200 and 206 get the `Content-Disposition` of the
/// file name. HTTP 304 and 416 pass as they are. Any other status is a file
/// that the server cannot read, and it becomes error 0.
fn finish(
    response: Response<ServeFileSystemResponseBody>,
    track: &Track,
) -> Result<Response, ApiError> {
    match response.status() {
        StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
            let mut response = response.into_response();
            let disposition = content_disposition(file_name(&track.rel_path));
            if let Ok(value) = HeaderValue::from_str(&disposition) {
                response
                    .headers_mut()
                    .insert(header::CONTENT_DISPOSITION, value);
            }
            Ok(response)
        }
        StatusCode::NOT_MODIFIED | StatusCode::RANGE_NOT_SATISFIABLE => {
            Ok(response.into_response())
        }
        StatusCode::NOT_FOUND => Err(ApiError::NotFound),
        status => {
            tracing::error!(track = %track.id, %status, "the file of the track cannot be served");
            Err(ApiError::Other(CANNOT_READ.to_owned()))
        }
    }
}

/// Returns the file name of a relative path: its last component.
fn file_name(rel_path: &str) -> &str {
    rel_path.rsplit('/').next().unwrap_or(rel_path)
}

/// Returns the value of the `Content-Disposition` header for a file with the
/// name `name`: `inline; filename="<plain>"; filename*=UTF-8''<encoded>`.
///
/// `<plain>` is the name in which `_` replaces every character that a
/// quoted string of a header cannot hold as it is: a character outside
/// ASCII, a control character, `"` and `\`. `<encoded>` is the exact name,
/// percent-encoded (RFC 8187). A client that reads `filename*` gets the
/// exact name.
pub fn content_disposition(name: &str) -> String {
    let plain: String = name
        .chars()
        .map(|character| match character {
            ' '..='~' if character != '"' && character != '\\' => character,
            _ => '_',
        })
        .collect();
    let encoded = percent_encode(name);
    format!("inline; filename=\"{plain}\"; filename*=UTF-8''{encoded}")
}

/// Percent-encodes the UTF-8 bytes of `text`. A byte of the unreserved set
/// of RFC 3986 (`A-Z`, `a-z`, `0-9`, `-`, `.`, `_` and `~`) stays as it is.
/// Every other byte becomes `%` and two upper-case hex digits.
pub fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => write!(encoded, "%{byte:02X}").expect("a write to a string succeeds"),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use tempfile::TempDir;

    use super::*;

    /// A track row with the values that `decide` and `finish` read.
    fn track(lossless: bool, bitrate_kbps: Option<u32>) -> Track {
        Track {
            id: "tr-00000000000000000000000000000001".to_owned(),
            folder_id: 1,
            rel_path: "Aurora Field/Glass Harbour/01 Low Tide.flac".to_owned(),
            fingerprint: "8:00".to_owned(),
            title: "Low Tide".to_owned(),
            album_id: "al-00000000000000000000000000000001".to_owned(),
            artist_id: "ar-00000000000000000000000000000001".to_owned(),
            artist_name: "Aurora Field".to_owned(),
            album_name: "Glass Harbour".to_owned(),
            track_no: Some(1),
            disc_no: None,
            year: None,
            genre: None,
            duration_ms: 500,
            bitrate_kbps,
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            size: 8,
            suffix: "flac".to_owned(),
            content_type: "audio/flac".to_owned(),
            lossless,
            has_picture: false,
            has_lyrics: false,
            mtime: 1_700_000_000,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn flac() -> Track {
        track(true, Some(900))
    }

    fn mp3(bitrate_kbps: u32) -> Track {
        track(false, Some(bitrate_kbps))
    }

    /// A request with `format` and `maxBitRate`.
    fn asks(format: Option<&str>, max_bitrate_kbps: Option<u32>) -> StreamRequest {
        StreamRequest {
            format: format.map(str::to_owned),
            max_bitrate_kbps,
            ..StreamRequest::default()
        }
    }

    fn transcode(target: Target, bitrate_kbps: u32) -> StreamPlan {
        StreamPlan::Transcode {
            target,
            bitrate_kbps,
            offset_s: 0,
        }
    }

    #[test]
    fn decide_follows_the_rules_of_the_design() {
        use StreamPlan::Raw;
        use Target::{Mp3, Opus};

        // The config bitrates are 192 kbps for Opus and 320 kbps for MP3.
        let config = Transcoding::default();
        let cases: [(&str, Track, StreamRequest, StreamPlan); 22] = [
            ("no parameter", flac(), asks(None, None), Raw),
            ("maxBitRate=0", flac(), asks(None, Some(0)), Raw),
            ("format=raw", flac(), asks(Some("raw"), None), Raw),
            (
                "format=raw wins over maxBitRate",
                flac(),
                asks(Some("raw"), Some(128)),
                Raw,
            ),
            (
                "format=raw of a lossy track above the limit",
                mp3(320),
                asks(Some("raw"), Some(64)),
                Raw,
            ),
            (
                "format=opus at the config bitrate",
                flac(),
                asks(Some("opus"), None),
                transcode(Opus, 192),
            ),
            (
                "format=opus with maxBitRate=0 at the config bitrate",
                flac(),
                asks(Some("opus"), Some(0)),
                transcode(Opus, 192),
            ),
            (
                "format=opus at maxBitRate",
                flac(),
                asks(Some("opus"), Some(96)),
                transcode(Opus, 96),
            ),
            (
                "format=opus of a lossy track below the limit",
                mp3(64),
                asks(Some("opus"), Some(128)),
                transcode(Opus, 128),
            ),
            (
                "format=mp3 at the config bitrate",
                flac(),
                asks(Some("mp3"), None),
                transcode(Mp3, 320),
            ),
            (
                "format=mp3 at maxBitRate",
                mp3(320),
                asks(Some("mp3"), Some(128)),
                transcode(Mp3, 128),
            ),
            (
                "maxBitRate=128 on a FLAC",
                flac(),
                asks(None, Some(128)),
                transcode(Opus, 128),
            ),
            (
                "maxBitRate=64 on a 128 kbps MP3",
                mp3(128),
                asks(None, Some(64)),
                transcode(Opus, 64),
            ),
            (
                "maxBitRate=320 on a 32 kbps MP3",
                mp3(32),
                asks(None, Some(320)),
                Raw,
            ),
            (
                "maxBitRate equal to the bitrate of the track",
                mp3(128),
                asks(None, Some(128)),
                Raw,
            ),
            (
                "maxBitRate on a lossy track without a known bitrate",
                track(false, None),
                asks(None, Some(64)),
                Raw,
            ),
            (
                "maxBitRate on a lossless track without a known bitrate",
                track(true, None),
                asks(None, Some(64)),
                transcode(Opus, 64),
            ),
            (
                "maxBitRate=8 is 32",
                flac(),
                asks(None, Some(8)),
                transcode(Opus, 32),
            ),
            (
                "maxBitRate=8 with a format is 32",
                flac(),
                asks(Some("mp3"), Some(8)),
                transcode(Mp3, 32),
            ),
            (
                "maxBitRate=8 on a 32 kbps MP3 is compared as 32",
                mp3(32),
                asks(None, Some(8)),
                Raw,
            ),
            (
                "maxBitRate=1000 is 320",
                flac(),
                asks(None, Some(1000)),
                transcode(Opus, 320),
            ),
            (
                "maxBitRate=1000 with a format is 320",
                flac(),
                asks(Some("opus"), Some(1000)),
                transcode(Opus, 320),
            ),
        ];
        for (case, track, request, plan) in cases {
            assert_eq!(decide(&track, &request, &config), Ok(plan), "{case}");
        }
    }

    #[test]
    fn decide_moves_the_config_bitrates_into_the_range() {
        let config = Transcoding {
            opus_bitrate_kbps: 16,
            mp3_bitrate_kbps: 512,
            ..Transcoding::default()
        };
        assert_eq!(
            decide(&flac(), &asks(Some("opus"), None), &config),
            Ok(transcode(Target::Opus, 32))
        );
        assert_eq!(
            decide(&flac(), &asks(Some("mp3"), None), &config),
            Ok(transcode(Target::Mp3, 320))
        );
    }

    #[test]
    fn decide_passes_the_time_offset_to_a_transcode() {
        let request = StreamRequest {
            format: Some("opus".to_owned()),
            time_offset_s: Some(42),
            ..StreamRequest::default()
        };
        assert_eq!(
            decide(&flac(), &request, &Transcoding::default()),
            Ok(StreamPlan::Transcode {
                target: Target::Opus,
                bitrate_kbps: 192,
                offset_s: 42,
            })
        );

        // The original file has no offset: a client seeks in it with a range.
        let request = StreamRequest {
            time_offset_s: Some(42),
            estimate_content_length: true,
            ..StreamRequest::default()
        };
        assert_eq!(
            decide(&flac(), &request, &Transcoding::default()),
            Ok(StreamPlan::Raw)
        );
    }

    #[test]
    fn decide_answers_an_unknown_format_with_error_0() {
        for format in ["flac", "aac", "ogg", "RAW", "Opus", "mp3 "] {
            for limit in [None, Some(0), Some(128)] {
                let error = decide(&flac(), &asks(Some(format), limit), &Transcoding::default())
                    .expect_err("an unknown format");
                assert_eq!(error.code(), 0, "{format}");
                assert_eq!(error.message(), UNKNOWN_FORMAT, "{format}");
            }
        }
    }

    #[test]
    fn stream_request_reads_its_parameters() {
        assert_eq!(
            StreamRequest::from_params(&Params::from_query("id=tr-1")),
            Ok(StreamRequest::default())
        );
        assert_eq!(
            StreamRequest::from_params(&Params::from_query(
                "format=opus&maxBitRate=128&timeOffset=30&estimateContentLength=true"
            )),
            Ok(StreamRequest {
                format: Some("opus".to_owned()),
                max_bitrate_kbps: Some(128),
                time_offset_s: Some(30),
                estimate_content_length: true,
            })
        );
        // An empty value is an absent parameter.
        assert_eq!(
            StreamRequest::from_params(&Params::from_query(
                "format=&maxBitRate=&timeOffset=&estimateContentLength="
            )),
            Ok(StreamRequest::default())
        );
        assert_eq!(
            StreamRequest::from_params(&Params::from_query("maxBitRate=0")),
            Ok(StreamRequest {
                max_bitrate_kbps: Some(0),
                ..StreamRequest::default()
            })
        );
    }

    #[test]
    fn stream_request_with_a_value_of_the_wrong_type_is_error_0() {
        for (query, name) in [
            ("maxBitRate=high", "maxBitRate"),
            ("maxBitRate=-1", "maxBitRate"),
            ("timeOffset=1.5", "timeOffset"),
            ("estimateContentLength=yes", "estimateContentLength"),
        ] {
            let error = StreamRequest::from_params(&Params::from_query(query))
                .expect_err("a value of the wrong type");
            assert_eq!(error.code(), 0, "{query}");
            assert!(error.message().contains(name), "{query}: {error}");
        }
    }

    #[test]
    fn percent_encoding_keeps_the_unreserved_set() {
        let unreserved = "ABCXYZabcxyz0189-._~";
        assert_eq!(percent_encode(unreserved), unreserved);
        assert_eq!(percent_encode(""), "");
        assert_eq!(
            percent_encode(" !\"#%&'()*+,/:;=?@[\\]"),
            "%20%21%22%23%25%26%27%28%29%2A%2B%2C%2F%3A%3B%3D%3F%40%5B%5C%5D"
        );
        assert_eq!(percent_encode("é"), "%C3%A9");
        assert_eq!(percent_encode("日本"), "%E6%97%A5%E6%9C%AC");
        assert_eq!(percent_encode("\u{1f3b5}"), "%F0%9F%8E%B5");
        assert_eq!(percent_encode("a\nb\u{7f}"), "a%0Ab%7F");
    }

    #[test]
    fn content_disposition_of_a_plain_name_holds_the_name_twice() {
        assert_eq!(
            content_disposition("01 Low Tide.flac"),
            "inline; filename=\"01 Low Tide.flac\"; filename*=UTF-8''01%20Low%20Tide.flac"
        );
    }

    #[test]
    fn content_disposition_replaces_what_a_quoted_string_cannot_hold() {
        // A double quote, a backslash, characters outside ASCII and control
        // characters. Each character is one `_`, whatever its length in
        // UTF-8.
        assert_eq!(
            content_disposition("07 \"Señal\" \\ 日本\t\n\u{7f}.flac"),
            concat!(
                "inline; filename=\"07 _Se_al_ _ _____.flac\"; ",
                "filename*=UTF-8''07%20%22Se%C3%B1al%22%20%5C%20%E6%97%A5%E6%9C%AC%09%0A%7F.flac"
            )
        );
        // The characters that end a parameter or start an escape in other
        // places stay in the plain form and are encoded in the other.
        assert_eq!(
            content_disposition("a;b%41'c.mp3"),
            "inline; filename=\"a;b%41'c.mp3\"; filename*=UTF-8''a%3Bb%2541%27c.mp3"
        );
    }

    #[test]
    fn content_disposition_is_a_header_value_for_every_name() {
        for name in [
            "",
            "\"",
            "\\",
            "\r\nSet-Cookie: a=b",
            "\u{0}\u{1f}\u{7f}\u{80}\u{ff}",
            "日本語のファイル名.flac",
        ] {
            let value = content_disposition(name);
            let header = HeaderValue::from_str(&value).expect("a header value");
            let text = header.to_str().expect("visible ASCII");
            let (plain, encoded) = text
                .strip_prefix("inline; filename=\"")
                .and_then(|rest| rest.split_once("\"; filename*=UTF-8''"))
                .expect("the two forms of the name");
            assert!(!plain.contains(['"', '\\']), "{text}");
            assert_eq!(plain.chars().count(), name.chars().count(), "{text}");
            assert_eq!(encoded, percent_encode(name), "{text}");
        }
    }

    #[test]
    fn file_name_is_the_last_component() {
        assert_eq!(
            file_name("Aurora Field/Glass Harbour/01 Low Tide.flac"),
            "01 Low Tide.flac"
        );
        assert_eq!(file_name("untitled.mp3"), "untitled.mp3");
    }

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| (name.clone(), value.parse().expect("a header value")))
            .collect()
    }

    #[test]
    fn if_range_holds_for_the_tag_or_the_date_of_the_file() {
        const DATE: &str = "Tue, 14 Nov 2023 22:13:20 GMT";
        let current = headers(&[
            (header::ETAG, "\"6553f100.00000000-8\""),
            (header::LAST_MODIFIED, DATE),
        ]);
        let holds =
            |validator: &str| if_range_holds(&validator.parse().expect("a header value"), &current);
        assert!(holds("\"6553f100.00000000-8\""));
        assert!(holds(DATE));

        // Another tag, a weak tag, another date, another form of the date and
        // a value that is neither a tag nor a date.
        assert!(!holds("\"6553f100.00000000-9\""));
        assert!(!holds("W/\"6553f100.00000000-8\""));
        assert!(!holds("Tue, 14 Nov 2023 22:13:21 GMT"));
        assert!(!holds("Tuesday, 14-Nov-23 22:13:20 GMT"));
        assert!(!holds("6553f100.00000000-8"));
        assert!(!holds(""));

        // A response without validators names no version.
        let none = HeaderMap::new();
        for validator in ["\"6553f100.00000000-8\"", DATE] {
            assert!(!if_range_holds(
                &validator.parse().expect("a header value"),
                &none
            ));
        }
    }

    #[test]
    fn suffix_range_longer_than_the_file_becomes_the_whole_file() {
        let sent = |range: &str| {
            let range = range.parse().expect("a header value");
            whole_file_for_long_suffix(&range, 1000)
        };
        assert_eq!(sent("bytes=-1001"), "bytes=0-");
        assert_eq!(sent("bytes=-18446744073709551615"), "bytes=0-");
        // Every other range is passed on as it is.
        for range in [
            "bytes=-1000",
            "bytes=-1",
            "bytes=-0",
            "bytes=0-",
            "bytes=5000-",
            "bytes=0-99,-5000",
            "bytes=-5000,0-99",
            "bytes=-abc",
            "items=-5000",
        ] {
            assert_eq!(sent(range), range, "{range}");
        }
    }

    #[test]
    fn file_request_carries_three_headers_of_the_client_and_no_other() {
        let client = headers(&[
            (header::RANGE, "bytes=0-9"),
            (header::IF_RANGE, "\"tag\""),
            (header::IF_NONE_MATCH, "\"tag\""),
            (header::IF_MODIFIED_SINCE, "Tue, 14 Nov 2023 22:13:20 GMT"),
            (header::IF_MATCH, "\"other\""),
            (header::IF_UNMODIFIED_SINCE, "Tue, 14 Nov 2023 22:13:20 GMT"),
            (header::ACCEPT_ENCODING, "gzip"),
            (header::COOKIE, "a=b"),
            (header::HOST, "music.example"),
        ]);
        let range = HeaderValue::from_static("bytes=0-9");

        let request = file_request(false, &client, Some(&range));
        assert_eq!(request.method(), Method::GET);
        assert_eq!(request.uri(), "/");
        let mut names: Vec<&str> = request.headers().keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["if-modified-since", "if-none-match", "range"]);
        assert_eq!(request.headers()[header::RANGE], "bytes=0-9");

        let request = file_request(true, &client, None);
        assert_eq!(request.method(), Method::HEAD);
        let mut names: Vec<&str> = request.headers().keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["if-modified-since", "if-none-match"]);

        let request = file_request(false, &HeaderMap::new(), None);
        assert!(request.headers().is_empty());
    }

    /// A directory with the file `music/a.flac` of 8 bytes, whose
    /// modification time is [`MTIME`], and the directory `elsewhere`.
    struct Disk {
        dir: TempDir,
    }

    /// The modification time of the file of a [`Disk`], in Unix seconds.
    const MTIME: i64 = 1_700_000_000;

    impl Disk {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let disk = Self { dir };
            fs::create_dir(disk.path("music")).unwrap();
            fs::create_dir(disk.path("elsewhere")).unwrap();
            disk.write("music/a.flac", b"fLaC0123", MTIME);
            disk
        }

        fn path(&self, relative: &str) -> PathBuf {
            let mut path = self.dir.path().to_path_buf();
            path.extend(relative.split('/'));
            path
        }

        /// Writes `bytes` to `relative` and sets the modification time to
        /// `mtime` Unix seconds and half a second.
        fn write(&self, relative: &str, bytes: &[u8], mtime: i64) {
            let path = self.path(relative);
            fs::write(&path, bytes).unwrap();
            let seconds = Duration::from_secs(mtime.unsigned_abs());
            let half = Duration::from_millis(500);
            let time = if mtime >= 0 {
                UNIX_EPOCH + seconds + half
            } else {
                UNIX_EPOCH - seconds + half
            };
            set_modified(&path, time);
        }

        /// Checks the file at `relative` as the file of a track row with
        /// `size` and `mtime`, with `music` as the one music folder.
        fn check(&self, relative: &str, size: u64, mtime: i64) -> Result<PathBuf, Refusal> {
            library_file(&self.path(relative), &[self.path("music")], size, mtime)
        }
    }

    fn set_modified(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(time))
            .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
    }

    #[test]
    fn file_with_the_state_of_the_row_is_served_from_its_resolved_path() {
        let disk = Disk::new();
        let real = disk.check("music/a.flac", 8, MTIME).expect("the file");
        assert_eq!(real, fs::canonicalize(disk.path("music/a.flac")).unwrap());
    }

    #[test]
    fn file_with_another_size_or_time_is_replaced() {
        let disk = Disk::new();
        for (case, size, mtime) in [
            ("another size", 9, MTIME),
            ("a later time", 8, MTIME + 1),
            ("an earlier time", 8, MTIME - 1),
            ("both", 7, MTIME + 60),
        ] {
            let refusal = disk.check("music/a.flac", size, mtime).unwrap_err();
            assert!(matches!(refusal, Refusal::Replaced), "{case}: {refusal:?}");
        }
    }

    #[test]
    fn directory_at_the_path_is_replaced() {
        let disk = Disk::new();
        fs::create_dir(disk.path("music/album.flac")).unwrap();
        let state = fs::metadata(disk.path("music/album.flac")).unwrap();
        let mtime = state
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let refusal = disk
            .check("music/album.flac", state.len(), mtime.cast_signed())
            .unwrap_err();
        assert!(matches!(refusal, Refusal::Replaced), "{refusal:?}");
    }

    #[test]
    fn file_before_1970_equals_no_row_that_is_checked() {
        let disk = Disk::new();
        disk.write("music/old.flac", b"fLaC0123", -10);
        let state = fs::metadata(disk.path("music/old.flac")).unwrap();
        if state.modified().unwrap() >= UNIX_EPOCH {
            // The filesystem keeps no time before 1970.
            return;
        }
        for mtime in [-10, -9, 0, 10] {
            let refusal = disk.check("music/old.flac", 8, mtime).unwrap_err();
            assert!(matches!(refusal, Refusal::Replaced), "{mtime}: {refusal:?}");
        }
    }

    #[test]
    fn missing_file_is_unreadable() {
        let disk = Disk::new();
        let refusal = disk.check("music/gone.flac", 8, MTIME).unwrap_err();
        match refusal {
            Refusal::Unreadable(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn file_outside_the_folders_is_refused() {
        let disk = Disk::new();
        disk.write("elsewhere/a.flac", b"fLaC0123", MTIME);

        // A path that leaves the folder through `..`, as a row with such a
        // relative path gives it.
        let refusal = disk.check("music/../elsewhere/a.flac", 8, MTIME);
        match refusal {
            Err(Refusal::Outside(real)) => {
                assert_eq!(
                    real,
                    fs::canonicalize(disk.path("elsewhere/a.flac")).unwrap()
                );
            }
            other => panic!("{other:?}"),
        }

        // A folder whose name starts with the name of the music folder.
        fs::create_dir(disk.path("music2")).unwrap();
        disk.write("music2/a.flac", b"fLaC0123", MTIME);
        let refusal = disk.check("music2/a.flac", 8, MTIME);
        assert!(matches!(refusal, Err(Refusal::Outside(_))), "{refusal:?}");

        // No folder, and a folder that does not exist.
        let path = disk.path("music/a.flac");
        for folders in [vec![], vec![disk.path("unmounted")]] {
            let refusal = library_file(&path, &folders, 8, MTIME);
            assert!(matches!(refusal, Err(Refusal::Outside(_))), "{refusal:?}");
        }
    }

    #[test]
    fn file_in_any_of_several_folders_is_served() {
        let disk = Disk::new();
        let folders = [disk.path("elsewhere"), disk.path("music")];
        let real = library_file(&disk.path("music/a.flac"), &folders, 8, MTIME);
        assert!(real.is_ok(), "{real:?}");
    }

    // A symbolic link is created with a Unix call. On Windows the call needs
    // a privilege that a test run does not have.
    #[cfg(unix)]
    mod links {
        use std::os::unix::fs::symlink;

        use super::*;

        #[test]
        fn link_to_a_file_outside_the_folders_is_refused() {
            let disk = Disk::new();
            // The target has the size and the time of the row.
            disk.write("elsewhere/secret.flac", b"SECRET!!", MTIME);
            symlink(
                disk.path("elsewhere/secret.flac"),
                disk.path("music/link.flac"),
            )
            .unwrap();
            let refusal = disk.check("music/link.flac", 8, MTIME);
            match refusal {
                Err(Refusal::Outside(real)) => assert_eq!(
                    real,
                    fs::canonicalize(disk.path("elsewhere/secret.flac")).unwrap()
                ),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn file_below_a_directory_link_out_of_the_folders_is_refused() {
            let disk = Disk::new();
            disk.write("elsewhere/b.flac", b"fLaC0123", MTIME);
            symlink(disk.path("elsewhere"), disk.path("music/archive")).unwrap();
            let refusal = disk.check("music/archive/b.flac", 8, MTIME);
            assert!(matches!(refusal, Err(Refusal::Outside(_))), "{refusal:?}");
        }

        #[test]
        fn link_to_a_file_inside_the_folders_is_served_as_its_target() {
            let disk = Disk::new();
            fs::create_dir(disk.path("music/favourites")).unwrap();
            // A relative link inside one folder.
            symlink("../a.flac", disk.path("music/favourites/a.flac")).unwrap();
            let real = disk.check("music/favourites/a.flac", 8, MTIME).unwrap();
            assert_eq!(real, fs::canonicalize(disk.path("music/a.flac")).unwrap());

            // A link from one folder into another one.
            disk.write("elsewhere/b.flac", b"fLaC0123", MTIME);
            symlink(disk.path("elsewhere/b.flac"), disk.path("music/b.flac")).unwrap();
            let folders = [disk.path("music"), disk.path("elsewhere")];
            let real = library_file(&disk.path("music/b.flac"), &folders, 8, MTIME).unwrap();
            assert_eq!(
                real,
                fs::canonicalize(disk.path("elsewhere/b.flac")).unwrap()
            );
        }

        #[test]
        fn folder_behind_a_link_holds_its_files() {
            let disk = Disk::new();
            symlink(disk.path("music"), disk.path("library")).unwrap();
            let folders = [disk.path("library")];
            let real = library_file(&disk.path("library/a.flac"), &folders, 8, MTIME).unwrap();
            assert_eq!(real, fs::canonicalize(disk.path("music/a.flac")).unwrap());
        }

        #[test]
        fn link_whose_target_is_gone_is_unreadable() {
            let disk = Disk::new();
            symlink(disk.path("elsewhere/gone.flac"), disk.path("music/c.flac")).unwrap();
            let refusal = disk.check("music/c.flac", 8, MTIME);
            assert!(
                matches!(refusal, Err(Refusal::Unreadable(_))),
                "{refusal:?}"
            );
        }
    }

    /// The status and the headers of a response, and its body.
    async fn read(response: Response) -> (StatusCode, HeaderMap, Vec<u8>) {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .expect("the body");
        (parts.status, parts.headers, bytes.to_vec())
    }

    /// The row of the file `music/a.flac` of a [`Disk`].
    fn disk_track() -> Track {
        Track {
            rel_path: "a.flac".to_owned(),
            mtime: MTIME,
            ..track(true, Some(900))
        }
    }

    #[tokio::test]
    async fn raw_serves_the_file_of_the_row() {
        let disk = Disk::new();
        let folders = [disk.path("music")];
        let response = raw(
            &disk_track(),
            disk.path("music/a.flac"),
            &folders,
            &Method::GET,
            &HeaderMap::new(),
        )
        .await
        .expect("the file");
        let (status, headers, body) = read(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "audio/flac");
        assert_eq!(headers[header::CONTENT_LENGTH], "8");
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "inline; filename=\"a.flac\"; filename*=UTF-8''a.flac"
        );
        assert_eq!(body, b"fLaC0123");
    }

    #[tokio::test]
    async fn raw_answers_a_row_whose_time_has_no_http_date_with_error_0() {
        let disk = Disk::new();
        let folders = [disk.path("music")];
        // `ServeFile` panics for the file of such a row, so the row is
        // answered before the file is read: the path names no file.
        for mtime in [-1, i64::MIN, HTTP_DATE_SECONDS.end, i64::MAX] {
            let track = Track {
                mtime,
                ..disk_track()
            };
            let error = raw(
                &track,
                disk.path("music/gone.flac"),
                &folders,
                &Method::GET,
                &HeaderMap::new(),
            )
            .await
            .expect_err("no response");
            assert_eq!(error, ApiError::Other(NO_HTTP_DATE.to_owned()), "{mtime}");
        }
    }

    #[tokio::test]
    async fn file_removed_after_the_check_is_error_70() {
        let disk = Disk::new();
        // `ServeFile` finds no file at the path, as it does when the file is
        // removed between the check and the open.
        let mime = "audio/flac".parse::<Mime>().unwrap();
        for head in [false, true] {
            let request = file_request(head, &HeaderMap::new(), None);
            let response = serve(&disk.path("music/gone.flac"), &mime, request).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                finish(response, &disk_track()).expect_err("no file"),
                ApiError::NotFound
            );
        }
    }

    #[tokio::test]
    async fn other_status_of_the_file_service_is_error_0() {
        let response = Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(ServeFileSystemResponseBody::default())
            .unwrap();
        assert_eq!(
            finish(response, &disk_track()).expect_err("no file"),
            ApiError::Other(CANNOT_READ.to_owned())
        );
    }
}
