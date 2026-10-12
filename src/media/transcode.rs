//! Transcoding with ffmpeg (`docs/design.md` section 3.9).
//!
//! A [`Transcoder`] turns the file of a track into a stream of another
//! format. [`FfmpegTranscoder`] starts the external program ffmpeg for each
//! job and returns the standard output of the process as the body of the
//! response. The program is not linked and not bundled.
//!
//! # How the process is started
//!
//! The process is started from an argument vector
//! ([`FfmpegTranscoder::argv`]) and never through a shell. The program of
//! `transcoding.ffmpeg` is one element of the vector and the path of the
//! file is another one, whatever characters they hold. The other arguments
//! are fixed texts and two numbers. No value of a request is part of the
//! vector as text.
//!
//! ffmpeg reads the argument after `-i` as a URL: a text that starts with
//! the name of a protocol and a colon, such as `concat:` or `http:`, is not
//! opened as a file. An absolute path starts with a separator or with a
//! drive, which no protocol name does, so ffmpeg opens it as a file.
//! [`FfmpegTranscoder`] therefore starts no process for a path that is not
//! absolute.
//!
//! # What ffmpeg may open
//!
//! ffmpeg decides the format of its input from the content of the file and
//! not from its name. A file with the name of an audio file can therefore be
//! a playlist or a script, which names other files and URLs for ffmpeg to
//! open. Two input options keep a file of the library from making ffmpeg
//! read another file or open a network connection:
//!
//! - `-protocol_whitelist file` ([`PROTOCOLS`]): ffmpeg opens the input, and
//!   whatever the input names, through the `file` protocol only. That
//!   excludes every network protocol and the protocols that wrap another
//!   resource, such as `concat:`, `subfile:` and `data:`.
//! - `-format_whitelist` with [`DEMUXERS`]: ffmpeg reads the input only with
//!   the demuxer of an audio container that the scanner supports. It
//!   refuses a file whose content is anything else, which includes every
//!   playlist and script format.
//!
//! The whitelists do not cover one case. An MP4 file can name a second file
//! as the place of its audio data (a data reference), and the `mov` demuxer,
//! which the list needs for `m4a` files, can open that file through the
//! `file` protocol. ffmpeg does so only with the option `enable_drefs` of
//! that demuxer. The option is off by default, and the server does not pass
//! it, so ffmpeg skips the reference. The server relies on this default of
//! ffmpeg. With an ffmpeg that follows the reference, a crafted MP4 file of
//! the library makes ffmpeg read a local file that a relative path from the
//! file reaches, and the stream goes to the client, which is the
//! authenticated owner.
//!
//! # What the server can transcode to
//!
//! [`FfmpegTranscoder::probe`] runs the program twice when the server
//! starts: `<ffmpeg> -version`, and `<ffmpeg> -hide_banner -encoders`, which
//! lists the encoders of the build. A target is available when the list
//! holds its encoder (`encoder`). A job for a target that is not available
//! is [`TranscodeError::Unavailable`] and starts no process: every job when
//! the program does not run, and the jobs of one target when the build lacks
//! one encoder. The probe logs one warning for what is missing. No request
//! runs the probe again.
//!
//! # How many processes run
//!
//! A semaphore has one permit for each process that may run
//! (`transcoding.max_concurrent`). A job takes a permit before its process
//! is started, and a job that gets none within the permit wait is
//! [`TranscodeError::Busy`]. The permit returns when the process has ended
//! and the system has released it, so the number of ffmpeg processes of the
//! server never exceeds the limit.
//!
//! # How the process ends
//!
//! The body of a [`TranscodeStream`] owns the process.
//!
//! - When the body is dropped before the output has ended, the process is
//!   killed. `stream::transcoded` drops the body when the process writes no
//!   output within the limit, when the client disconnects, when the client
//!   takes no chunk for the write timeout, and when the server stops.
//! - When the output has ended, the process ends by itself. A process that
//!   has not ended after `EXIT_WAIT` is killed.
//!
//! A task then waits for the process, so that the system keeps no entry for
//! it, and returns the permit. A process that ended by itself with a
//! failure status is logged once, at `warn`, with the status and the ID of
//! the track (`ProcessEnd`). A process that the server killed is not a
//! failure.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use futures_util::Stream;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;

use super::stream::Target;
use crate::config::Transcoding;

/// The longest time the probe at startup waits for one run of the program:
/// `ffmpeg -version`, and then `ffmpeg -encoders`. A program that has not
/// answered by then counts as not available.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most bytes the probe reads from the output of one run of the
/// program. The encoder list of ffmpeg 8.0 with 204 encoders has 12 KiB.
///
/// The probe reads no byte behind the limit and closes the output there. A
/// program that writes more ends with a failure or not within the limit of
/// the run, and counts as not available. An encoder whose line is behind
/// the limit of a list that the program still wrote in full counts as
/// missing.
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

/// The targets of a transcode, in the order in which the probe names them.
const TARGETS: [Target; 2] = [Target::Opus, Target::Mp3];

/// The longest part of the version line of ffmpeg that the log holds, in
/// characters.
const VERSION_CHARS: usize = 100;

/// The longest piece of the error output of ffmpeg that one log line holds,
/// in bytes. A longer line is logged in pieces.
const STDERR_LINE_LIMIT: u64 = 1024;

/// The longest time a process may take to end after its output has ended.
/// ffmpeg ends at once after it closed its output. A process that has not
/// ended by then is killed, so that it does not hold its permit.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// The value of `-protocol_whitelist`: the protocols through which ffmpeg
/// may open the input and what the input names. The file of a track is a
/// local file, so the one protocol is `file`.
pub const PROTOCOLS: &str = "file";

/// The value of `-format_whitelist`: the ffmpeg demuxers of the audio
/// containers that the scanner supports (`docs/design.md` section 1.3), as
/// `ffmpeg -demuxers` names them.
///
/// | Suffix | Demuxer |
/// |---|---|
/// | `aac` | `aac`, raw ADTS AAC |
/// | `aif`, `aiff` | `aiff` |
/// | `flac` | `flac` |
/// | `m4a` | `mov`, which ffmpeg lists as `mov,mp4,m4a,3gp,3g2,mj2` |
/// | `mp3` | `mp3` |
/// | `ogg`, `opus` | `ogg` |
/// | `wav` | `wav` |
///
/// ffmpeg matches an entry of the list against each name of a demuxer, so
/// `mov` allows the demuxer of MP4 files. That demuxer is the one of the
/// list that can open a second file: see the module comment.
pub const DEMUXERS: &str = "aac,aiff,flac,mov,mp3,ogg,wav";

/// One transcode: the track, the file, the format, the bitrate and the
/// start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeJob {
    /// The ID of the track. The transcoder names it in its log.
    pub track: String,
    /// The absolute path of the file.
    pub path: PathBuf,
    /// The format of the stream.
    pub target: Target,
    /// The bitrate of the stream, in kbps.
    pub bitrate_kbps: u32,
    /// The second of the track at which the stream starts.
    pub offset_s: u32,
}

/// A transcode that runs.
#[derive(Debug)]
pub struct TranscodeStream {
    /// The bytes of the stream. Dropping the body ends the transcode.
    pub body: Body,
    /// The content type of the stream.
    pub content_type: &'static str,
}

/// Why a transcode did not start.
#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    /// As many transcodes run as may run at the same time, and none ended
    /// within the permit wait.
    #[error("every transcode slot is taken")]
    Busy,
    /// The server has no program that transcodes to the target of the job:
    /// the program does not run, or it lacks the encoder of the target.
    #[error("ffmpeg not available")]
    Unavailable,
    /// The process cannot be started.
    #[error("cannot start ffmpeg: {0}")]
    Spawn(#[source] io::Error),
}

/// The future of one call of [`Transcoder::spawn`].
pub type TranscodeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TranscodeStream, TranscodeError>> + Send + 'a>>;

/// Starts transcodes. The state of the server holds one as
/// `Arc<dyn Transcoder>`, so the method returns a boxed future.
pub trait Transcoder: Send + Sync {
    /// Starts the transcode `job` and returns its stream.
    fn spawn(&self, job: TranscodeJob) -> TranscodeFuture<'_>;
}

/// The transcoder that runs ffmpeg (see the module comment).
pub struct FfmpegTranscoder {
    /// `transcoding.ffmpeg`: the name or the path of the program.
    bin: OsString,
    /// One permit for each process that may run.
    semaphore: Arc<Semaphore>,
    /// The time a job waits for a permit.
    permit_wait: Duration,
    /// The targets that the probe at startup found the program can encode.
    /// It is empty when the program does not run.
    targets: Vec<Target>,
}

impl FfmpegTranscoder {
    /// Builds the transcoder of `config` and probes the program of
    /// `config.ffmpeg` once: `<ffmpeg> -version`, and then
    /// `<ffmpeg> -hide_banner -encoders` (`probed`).
    ///
    /// The result is logged: the version line of the program at `info`, and
    /// at most one warning, which says why the program is not available or
    /// which encoder it lacks (`Probe::warning`). The transcoder starts no
    /// process for a target that the probe did not find, and a job for such
    /// a target is [`TranscodeError::Unavailable`].
    pub async fn probe(config: &Transcoding) -> Self {
        let program = |arguments: &[&str]| {
            let mut command = Command::new(&config.ffmpeg);
            command.args(arguments);
            command
        };
        let found = probed(program, PROBE_LIMIT).await;
        if let Probe::Program { version, .. } = &found {
            tracing::info!(ffmpeg = %config.ffmpeg, %version, "ffmpeg is available");
        }
        if let Some(warning) = found.warning() {
            tracing::warn!(ffmpeg = %config.ffmpeg, "{warning}");
        }
        Self::new(config, found.targets())
    }

    /// The transcoder of `config` for a server that did not probe its
    /// program. It has no target, as the transcoder of a program that does
    /// not run has none: every job is [`TranscodeError::Unavailable`], and
    /// the transcoder starts no process.
    pub fn without_program(config: &Transcoding) -> Self {
        Self::new(config, Vec::new())
    }

    /// The transcoder of `config`. `targets` is the result of the probe: the
    /// targets that the program can encode.
    fn new(config: &Transcoding, targets: Vec<Target>) -> Self {
        Self {
            bin: OsString::from(&config.ffmpeg),
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            permit_wait: Duration::from_millis(config.permit_wait_ms),
            targets,
        }
    }

    /// The argument vector of the process of `job`: the program and its
    /// arguments (`docs/design.md` section 3.9).
    ///
    /// ```text
    /// ffmpeg -nostdin -v error -protocol_whitelist file -format_whitelist <demuxers>
    ///     [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a <codec> -b:a <N>k -f <format> -
    /// ```
    ///
    /// The program and the path are one element each. The options between
    /// `-v error` and `-i` are input options: ffmpeg applies an option to
    /// the next file of the command line, so they precede `-i`. The two
    /// whitelists are [`PROTOCOLS`] and [`DEMUXERS`]. With `-ss` before `-i`
    /// ffmpeg seeks in the input, and the option is absent for a stream
    /// that starts at the start of the track. The last argument writes the
    /// stream to the standard output.
    pub fn argv(&self, job: &TranscodeJob) -> Vec<OsString> {
        let mut argv = vec![self.bin.clone()];
        argv.extend(arguments(job));
        argv
    }

    /// The command of `job`: the program and the arguments of
    /// [`FfmpegTranscoder::argv`]. A command starts its program directly,
    /// without a shell.
    fn command(&self, job: &TranscodeJob) -> Command {
        let mut command = Command::new(&self.bin);
        command.args(arguments(job));
        command
    }

    /// Waits for a permit, starts the process of `command` and returns its
    /// standard output as a stream with the content type `content_type`.
    ///
    /// `command` is called when the permit is held, so a job that gets no
    /// permit builds no command and starts no process. The process reads
    /// nothing from its standard input. Its error output is logged at
    /// `debug` when that level is on, and is discarded otherwise.
    ///
    /// `report` is called once, with how the process ended, when the process
    /// is gone. It is not called for a job that started no process.
    async fn start(
        &self,
        command: impl FnOnce() -> Command,
        content_type: &'static str,
        report: Report,
    ) -> Result<TranscodeStream, TranscodeError> {
        let acquire = Arc::clone(&self.semaphore).acquire_owned();
        let permit = match tokio::time::timeout(self.permit_wait, acquire).await {
            Ok(Ok(permit)) => permit,
            // The wait is over. The semaphore is never closed.
            Ok(Err(_)) | Err(_) => return Err(TranscodeError::Busy),
        };

        let mut command = command();
        let log_errors = tracing::enabled!(tracing::Level::DEBUG);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(if log_errors {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            // The body kills the process when it is dropped. This setting
            // kills a process whose handle is dropped in another way: the
            // task that waits for the process is dropped with the runtime.
            .kill_on_drop(true);
        // A failed start drops the permit.
        let mut child = command.spawn().map_err(TranscodeError::Spawn)?;
        let Some(stdout) = child.stdout.take() else {
            // The output is piped, so the process has one. The process is
            // killed when `child` is dropped.
            let error = io::Error::other("the process has no standard output");
            return Err(TranscodeError::Spawn(error));
        };
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(log_error_output(stderr));
        }
        let output = ChildOutput {
            stdout: ReaderStream::new(stdout),
            process: Some(Process {
                child,
                permit,
                report,
            }),
        };
        Ok(TranscodeStream {
            body: Body::from_stream(output),
            content_type,
        })
    }

    /// Starts ffmpeg for `job` and calls `report` when its process is gone
    /// (see [`Transcoder::spawn`] for the errors).
    async fn run(
        &self,
        job: &TranscodeJob,
        report: Report,
    ) -> Result<TranscodeStream, TranscodeError> {
        if !self.targets.contains(&job.target) {
            return Err(TranscodeError::Unavailable);
        }
        if !job.path.is_absolute() {
            let error = io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path of the file is not absolute",
            );
            return Err(TranscodeError::Spawn(error));
        }
        let content_type = job.target.content_type();
        self.start(|| self.command(job), content_type, report).await
    }
}

impl Transcoder for FfmpegTranscoder {
    /// Starts ffmpeg for `job`.
    ///
    /// [`TranscodeError::Unavailable`] answers a job for a target that the
    /// probe did not find: every job of a transcoder whose program does not
    /// run, and the jobs of one target when the program lacks one encoder.
    /// [`TranscodeError::Spawn`] answers a path that is
    /// not absolute, before a permit is taken (see the module comment), and
    /// a process that the system does not start.
    ///
    /// A process that ends by itself with a failure status is logged at
    /// `warn` with the status and the ID of the track (`log_end`).
    fn spawn(&self, job: TranscodeJob) -> TranscodeFuture<'_> {
        Box::pin(async move {
            let track = job.track.clone();
            let report: Report = Box::new(move |end| log_end(&track, end));
            self.run(&job, report).await
        })
    }
}

/// The ffmpeg encoder of `target`: the value of `-c:a`, and the name of the
/// encoder in the list of `ffmpeg -encoders`.
fn encoder(target: Target) -> &'static str {
    match target {
        Target::Opus => "libopus",
        Target::Mp3 => "libmp3lame",
    }
}

/// The name of the format of `target` in a log line.
fn format_name(target: Target) -> &'static str {
    match target {
        Target::Opus => "Opus",
        Target::Mp3 => "MP3",
    }
}

/// The arguments of ffmpeg for `job`, without the program.
fn arguments(job: &TranscodeJob) -> Vec<OsString> {
    let codec = encoder(job.target);
    let format = match job.target {
        Target::Opus => "ogg",
        Target::Mp3 => "mp3",
    };
    let mut arguments: Vec<OsString> = [
        "-nostdin",
        "-v",
        "error",
        "-protocol_whitelist",
        PROTOCOLS,
        "-format_whitelist",
        DEMUXERS,
    ]
    .map(OsString::from)
    .to_vec();
    if job.offset_s > 0 {
        arguments.push("-ss".into());
        arguments.push(job.offset_s.to_string().into());
    }
    arguments.push("-i".into());
    arguments.push(job.path.clone().into_os_string());
    let bitrate = format!("{}k", job.bitrate_kbps);
    arguments.extend(
        [
            "-map", "0:a:0", "-vn", "-c:a", codec, "-b:a", &bitrate, "-f", format, "-",
        ]
        .map(OsString::from),
    );
    arguments
}

/// What the probe at startup found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// The program is not available: it does not run, or its list of
    /// encoders cannot be read. The text says why.
    NoProgram(String),
    /// The program runs.
    Program {
        /// The version line of the program.
        version: String,
        /// The targets whose encoder the program lacks.
        missing: Vec<Target>,
    },
}

impl Probe {
    /// The targets that the server can transcode to.
    fn targets(&self) -> Vec<Target> {
        match self {
            Self::NoProgram(_) => Vec::new(),
            Self::Program { missing, .. } => TARGETS
                .into_iter()
                .filter(|target| !missing.contains(target))
                .collect(),
        }
    }

    /// The one warning that the probe logs, and `None` when the program has
    /// the encoder of every target. The text says what is missing, which
    /// requests get an error, and what the owner does about it.
    fn warning(&self) -> Option<String> {
        let no_transcode = "The server serves original files, and a stream request that needs \
                            a transcode gets an error";
        match self {
            Self::NoProgram(reason) => Some(format!(
                "ffmpeg is not available: {reason}. {no_transcode}. \
                 Install ffmpeg or set transcoding.ffmpeg, and restart the server"
            )),
            Self::Program { missing, .. } if missing.is_empty() => None,
            Self::Program { missing, .. } => {
                let encoders = missing.iter().copied().map(encoder);
                let encoders = encoders.collect::<Vec<_>>().join(" and ");
                let effect = match self.targets().first() {
                    None => no_transcode.to_owned(),
                    Some(works) => format!(
                        "A stream request that needs a transcode to {} gets an error, \
                         and a transcode to {} works",
                        format_name(missing[0]),
                        format_name(*works)
                    ),
                };
                Some(format!(
                    "ffmpeg has no encoder {encoders}. {effect}. \
                     Install an ffmpeg with {encoders} or set transcoding.ffmpeg, \
                     and restart the server"
                ))
            }
        }
    }
}

/// Probes a program. `program` returns the command that runs the program
/// with the given arguments, and `limit` is the longest time one run may
/// take.
///
/// The first run is `-version`. A program that cannot be started, that ends
/// with a failure status or that has not ended after `limit` is not
/// available, and it is not run again. The second run is
/// `-hide_banner -encoders`, whose output lists the encoders of the build.
/// A program whose list cannot be read is not available either.
async fn probed(program: impl Fn(&[&str]) -> Command, limit: Duration) -> Probe {
    let version = match version(program(&["-version"]), limit).await {
        Ok(version) => version,
        Err(reason) => return Probe::NoProgram(reason),
    };
    match output_of(program(&["-hide_banner", "-encoders"]), limit).await {
        Ok(list) => Probe::Program {
            version,
            missing: TARGETS
                .into_iter()
                .filter(|target| !lists_encoder(&list, encoder(*target)))
                .collect(),
        },
        Err(reason) => Probe::NoProgram(format!("its list of encoders cannot be read: {reason}")),
    }
}

/// Whether `list`, the output of `ffmpeg -encoders`, names the audio encoder
/// `name`.
///
/// A line of an encoder holds six flags, the name of the encoder and a
/// description, each after white space: ` A....D libopus      libopus Opus
/// (codec opus)`. The first flag is `A` for an audio encoder. The name must
/// be equal: the line of the encoder `opus`, which ffmpeg has itself, does
/// not name `libopus`.
fn lists_encoder(list: &str, name: &str) -> bool {
    list.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let flags = fields.next().unwrap_or_default();
        flags.len() == 6 && flags.starts_with('A') && fields.next() == Some(name)
    })
}

/// Runs `command`, which is `<ffmpeg> -version`, and returns the first line
/// of its output that is not empty, cut to [`VERSION_CHARS`] characters. The
/// error is the one of [`output_of`].
async fn version(command: Command, limit: Duration) -> Result<String, String> {
    let text = output_of(command, limit).await?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    Ok(line.chars().take(VERSION_CHARS).collect())
}

/// Runs `command`, a run of the probe, and returns the first
/// [`PROBE_OUTPUT_LIMIT`] bytes of its output as text. The error says why
/// the program is not available: it cannot be started, it ends with a
/// failure status, or it has not ended after `limit`.
///
/// The process reads nothing from its standard input, and its error output
/// is discarded. A process that has not ended after `limit` is killed.
async fn output_of(mut command: Command, limit: Duration) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let run = async {
        let mut child = command.spawn()?;
        let mut output = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            stdout
                .take(PROBE_OUTPUT_LIMIT)
                .read_to_end(&mut output)
                .await?;
        }
        let status = child.wait().await?;
        Ok::<_, io::Error>((status, output))
    };
    match tokio::time::timeout(limit, run).await {
        Ok(Ok((status, output))) if status.success() => {
            Ok(String::from_utf8_lossy(&output).into_owned())
        }
        Ok(Ok((status, _))) => Err(format!("the program ended with {status}")),
        Ok(Err(error)) => Err(format!("the program cannot be run ({error})")),
        // Dropping `run` dropped the process, which kills it.
        Err(_) => Err(format!("the program did not end within {limit:?}")),
    }
}

/// Logs the error output of a transcode process at `debug`, one line of the
/// output for each log line, until the process closes the output. A line of
/// more than [`STDERR_LINE_LIMIT`] bytes is logged in pieces, so the task
/// holds no more than that of the output.
async fn log_error_output(stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        let mut piece = (&mut reader).take(STDERR_LINE_LIMIT);
        match piece.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                // The text comes from a file of the library. The debug form
                // escapes its control characters.
                tracing::debug!(line = ?text.trim_end(), "ffmpeg reported");
            }
        }
    }
}

/// How a transcode process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessEnd {
    /// The process ended by itself, with this status.
    Exited(ExitStatus),
    /// The server killed the process: the body was dropped before the
    /// output had ended, or the process did not end after its output.
    Killed,
}

/// Receives how the process of one stream ended. It is called once.
type Report = Box<dyn FnOnce(ProcessEnd) + Send>;

/// The status with which a transcode process failed: the status of a
/// process that ended by itself without success. A process that the server
/// killed did not fail, whatever status the system reports for it.
fn failure(end: ProcessEnd) -> Option<ExitStatus> {
    match end {
        ProcessEnd::Exited(status) if !status.success() => Some(status),
        ProcessEnd::Exited(_) | ProcessEnd::Killed => None,
    }
}

/// The [`Report`] of the transcoder: logs the end of the process of the
/// track `track`. A failure ([`failure`]) is one line at `warn` with the
/// status and the ID of the track. A process that failed before its first
/// output gave its request an error, and one that failed later gave it an
/// incomplete stream after HTTP 200 (`stream::transcoded`).
fn log_end(track: &str, end: ProcessEnd) {
    match failure(end) {
        Some(status) => tracing::warn!(
            %track,
            %status,
            "ffmpeg ended with a failure. The request for the track got an error when \
             ffmpeg had written nothing, and an incomplete stream otherwise. \
             The messages of ffmpeg are logged at the debug level"
        ),
        None => tracing::debug!(%track, ?end, "the transcode process has ended"),
    }
}

/// A transcode process, the permit under which it runs and the receiver of
/// its end.
struct Process {
    child: Child,
    permit: OwnedSemaphorePermit,
    report: Report,
}

/// The standard output of a transcode process as a stream of chunks. The
/// value owns the process: see the module comment for how the process ends.
struct ChildOutput {
    stdout: ReaderStream<ChildStdout>,
    /// The process, until it is released.
    process: Option<Process>,
}

impl ChildOutput {
    /// Lets go of the process. The first call does this, and a later call
    /// does nothing. `output_ended` says whether the process has closed its
    /// output.
    ///
    /// - A process that has ended is waited for here, and its status is
    ///   reported.
    /// - A process that runs although the body is dropped is killed.
    /// - A process whose output has ended is about to end by itself.
    ///
    /// In the last two cases a task waits for the process, reports its end
    /// and then returns the permit. The wait is what lets the system
    /// release the process, so the permit returns when the process is gone
    /// and not while it ends. Without a runtime there is no task: the
    /// process is killed, and the permit returns at once.
    fn release(&mut self, output_ended: bool) {
        let Some(process) = self.process.take() else {
            return;
        };
        let Process {
            mut child,
            permit,
            report,
        } = process;
        // The check comes before the kill: on Windows the status of a
        // process that the server killed equals a failure status of ffmpeg.
        if let Ok(Some(status)) = child.try_wait() {
            report(ProcessEnd::Exited(status));
            return;
        }
        let runtime = Handle::try_current().ok();
        if !output_ended || runtime.is_none() {
            kill(&mut child);
        }
        let Some(runtime) = runtime else {
            report(ProcessEnd::Killed);
            return;
        };
        runtime.spawn(async move {
            let end = if output_ended {
                ended_by_itself(&mut child, EXIT_WAIT).await
            } else {
                killed(&mut child).await
            };
            report(end);
            drop(permit);
        });
    }
}

/// Sends the kill to `child`. The process ends at once; the caller waits
/// for it.
fn kill(child: &mut Child) {
    if let Err(error) = child.start_kill() {
        // The process has ended and was waited for.
        tracing::debug!(%error, "the transcode process was not killed");
    }
}

/// Waits for `child`, which was sent the kill.
async fn killed(child: &mut Child) -> ProcessEnd {
    if let Err(error) = child.wait().await {
        tracing::warn!(%error, "cannot wait for the transcode process");
    }
    ProcessEnd::Killed
}

/// Waits for `child`, whose output has ended, to end by itself, for `wait`
/// at most. A process that has not ended by then is killed.
async fn ended_by_itself(child: &mut Child, wait: Duration) -> ProcessEnd {
    match tokio::time::timeout(wait, child.wait()).await {
        Ok(Ok(status)) => ProcessEnd::Exited(status),
        Ok(Err(error)) => {
            tracing::warn!(%error, "cannot wait for the transcode process");
            ProcessEnd::Killed
        }
        Err(_) => {
            tracing::warn!(
                "the transcode process did not end {wait:?} after its output; it is killed"
            );
            kill(child);
            killed(child).await
        }
    }
}

impl Stream for ChildOutput {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = Pin::new(&mut self.stdout).poll_next(cx);
        if matches!(next, Poll::Ready(None)) {
            // The process closed its output. The permit does not wait for
            // the response to be dropped.
            self.release(true);
        }
        next
    }
}

impl Drop for ChildOutput {
    fn drop(&mut self) {
        self.release(false);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use std::time::Instant;

    use http_body_util::BodyExt;
    use tokio::sync::oneshot;

    use super::*;

    /// The environment variable that turns [`child_process_of_the_tests`]
    /// into the process that stands in for ffmpeg. Its value is the number
    /// of seconds the process sleeps.
    const SLEEP_VARIABLE: &str = "WN_SERVER_TEST_SLEEP";

    /// The environment variable that names the file on which the child
    /// process holds an exclusive lock while it runs. The system releases
    /// the lock when the process ends, so a test sees through the file
    /// whether the process exists.
    const LOCK_VARIABLE: &str = "WN_SERVER_TEST_LOCK";

    /// The environment variable that names the exit status of the child
    /// process. Without it the process ends with success.
    const EXIT_VARIABLE: &str = "WN_SERVER_TEST_EXIT";

    /// The environment variable that holds a text which the child process
    /// writes to its standard output before everything else: the output of
    /// a program that the probe runs.
    const PRINT_VARIABLE: &str = "WN_SERVER_TEST_PRINT";

    /// The ID of the track of the jobs of the tests.
    const TRACK: &str = "tr-00000000000000000000000000000001";

    /// The name of [`child_process_of_the_tests`] as the test harness knows
    /// it.
    const CHILD_TEST: &str = "media::transcode::tests::child_process_of_the_tests";

    /// What the child process writes to its standard output before it
    /// sleeps. Its process ID and a line end follow.
    const CHILD_STARTED: &str = "the child process of the tests has the process ID ";

    /// The longest time a test waits for a child process to start, to be
    /// killed or to end. A passing run waits some milliseconds.
    const LIMIT: Duration = Duration::from_secs(1);

    /// The permit wait of the transcoders of the tests, in milliseconds.
    const PERMIT_WAIT_MS: u64 = 50;

    /// The permit wait of the transcoders of the tests.
    const PERMIT_WAIT: Duration = Duration::from_millis(PERMIT_WAIT_MS);

    /// The process that stands in for ffmpeg. The tests start the test
    /// binary again with this test as its only test and with
    /// [`SLEEP_VARIABLE`] set. The test then locks the file of
    /// [`LOCK_VARIABLE`] when that variable is set, writes the text of
    /// [`PRINT_VARIABLE`] when that variable is set, writes
    /// [`CHILD_STARTED`] and sleeps for that number of seconds. A process
    /// that no test kills ends by itself after the sleep, with the status
    /// of [`EXIT_VARIABLE`] when that variable is set.
    ///
    /// In a run of the suite the variable is not set, and the test does
    /// nothing.
    #[test]
    fn child_process_of_the_tests() {
        let seconds = std::env::var(SLEEP_VARIABLE)
            .ok()
            .and_then(|seconds| seconds.parse::<u64>().ok());
        if let Some(seconds) = seconds {
            let _lock = std::env::var_os(LOCK_VARIABLE).map(|path| {
                let file = fs::File::create(path).expect("create the lock file");
                file.lock().expect("lock the file");
                file
            });
            if let Ok(text) = std::env::var(PRINT_VARIABLE) {
                println!("{text}");
            }
            println!("{CHILD_STARTED}{}", std::process::id());
            std::thread::sleep(Duration::from_secs(seconds));
            let status = std::env::var(EXIT_VARIABLE)
                .ok()
                .and_then(|status| status.parse::<i32>().ok());
            if let Some(status) = status {
                std::process::exit(status);
            }
        }
    }

    /// The command of a child process that sleeps `seconds` seconds: the
    /// test binary with [`child_process_of_the_tests`] as its only test. The
    /// output of the test is not captured, so it is the output of the
    /// process.
    fn child(seconds: u64) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("the test binary"));
        command
            .args([CHILD_TEST, "--exact", "--nocapture"])
            .env(SLEEP_VARIABLE, seconds.to_string());
        command
    }

    /// A transcoder whose probe found both encoders, with `max_concurrent`
    /// permits and a permit wait of [`PERMIT_WAIT`]. Its program is `bin`.
    fn transcoder(bin: &str, max_concurrent: usize) -> FfmpegTranscoder {
        transcoder_for(bin, max_concurrent, &TARGETS)
    }

    /// As [`transcoder`], for a program that can encode `targets` only.
    fn transcoder_for(bin: &str, max_concurrent: usize, targets: &[Target]) -> FfmpegTranscoder {
        let config = Transcoding {
            ffmpeg: bin.to_owned(),
            max_concurrent,
            permit_wait_ms: PERMIT_WAIT_MS,
            ..Transcoding::default()
        };
        FfmpegTranscoder::new(&config, targets.to_vec())
    }

    /// The command of a child process that ends at once with the exit
    /// status `status`.
    fn failing_child(status: i32) -> Command {
        let mut command = child(0);
        command.env(EXIT_VARIABLE, status.to_string());
        command
    }

    /// A [`Report`] that does nothing.
    fn no_report() -> Report {
        Box::new(|_| {})
    }

    /// A [`Report`] that sends the end of the process to the receiver.
    fn recorded() -> (Report, oneshot::Receiver<ProcessEnd>) {
        let (sender, receiver) = oneshot::channel();
        let report: Report = Box::new(move |end| {
            // A test that does not read the end has dropped the receiver.
            let _ = sender.send(end);
        });
        (report, receiver)
    }

    /// Waits for the end that a [`recorded`] report receives, for [`LIMIT`]
    /// at most. The report is called once, so the receiver holds one end.
    async fn end_of(receiver: oneshot::Receiver<ProcessEnd>) -> ProcessEnd {
        tokio::time::timeout(LIMIT, receiver)
            .await
            .expect("the end of the process is reported within the limit")
            .expect("the report is called")
    }

    fn job(path: &str, target: Target, bitrate_kbps: u32, offset_s: u32) -> TranscodeJob {
        TranscodeJob {
            track: TRACK.to_owned(),
            path: PathBuf::from(path),
            target,
            bitrate_kbps,
            offset_s,
        }
    }

    fn strings(argv: &[OsString]) -> Vec<&str> {
        argv.iter()
            .map(|argument| argument.to_str().expect("a Unicode argument"))
            .collect()
    }

    /// The process ID in the line of [`CHILD_STARTED`] of `output`, when
    /// `output` holds the whole line.
    fn child_id(output: &[u8]) -> Option<u32> {
        let text = String::from_utf8_lossy(output);
        let (_, after) = text.split_once(CHILD_STARTED)?;
        let (id, _) = after.split_once('\n')?;
        Some(id.trim().parse().expect("a process ID"))
    }

    /// Reads `body` until it holds the line of [`CHILD_STARTED`]: the child
    /// process runs and is about to sleep. Returns the ID of the process.
    async fn started(body: &mut Body) -> u32 {
        let mut output = Vec::new();
        let line = async {
            loop {
                if let Some(id) = child_id(&output) {
                    return id;
                }
                let frame = body.frame().await.expect("the output goes on");
                let data = frame.expect("a frame").into_data().expect("a data frame");
                output.extend_from_slice(&data);
            }
        };
        tokio::time::timeout(LIMIT, line)
            .await
            .expect("the child process starts within the limit")
    }

    /// The ID of the parent process of the process `id`, as the system
    /// lists it: for a process that runs, and for one that has ended and
    /// that nothing has waited for. `None` when the system has no entry for
    /// the process. Linux lists its processes in `/proc`.
    #[cfg(target_os = "linux")]
    fn parent_of(id: u32) -> Option<String> {
        match fs::read_to_string(format!("/proc/{id}/stat")) {
            // The fields after the name of the program, which ends at the
            // last `)`, are the state and the ID of the parent process.
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, fields)| fields.split_whitespace().nth(1))
                .map(str::to_owned),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => panic!("read the state of the process {id}: {error}"),
        }
    }

    /// As on Linux. macOS has no `/proc`, and its `ps` lists the process.
    #[cfg(target_os = "macos")]
    fn parent_of(id: u32) -> Option<String> {
        let listed = std::process::Command::new("ps")
            .args(["-o", "ppid=", "-p", &id.to_string()])
            .output()
            .expect("run ps");
        let parent = String::from_utf8_lossy(&listed.stdout).trim().to_owned();
        (!parent.is_empty()).then_some(parent)
    }

    /// Whether the system has an entry for the process `id` as a child of
    /// this process: the process runs, or it has ended and nothing has
    /// waited for it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn is_child_of_this_process(id: u32) -> Option<bool> {
        let this = std::process::id().to_string();
        Some(parent_of(id).is_some_and(|parent| parent == this))
    }

    /// The test does not read the processes of another system. On Windows a
    /// process that has ended leaves no entry that a wait removes.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn is_child_of_this_process(_id: u32) -> Option<bool> {
        None
    }

    /// Whether a process holds the lock on the file at `path`.
    fn locked(path: &Path) -> bool {
        let file = fs::File::open(path).expect("open the lock file");
        match file.try_lock() {
            // The lock of this call ends when `file` is dropped.
            Ok(()) => false,
            Err(fs::TryLockError::WouldBlock) => true,
            Err(fs::TryLockError::Error(error)) => panic!("{}: {error}", path.display()),
        }
    }

    /// Waits until no process holds the lock on the file at `path`, for
    /// [`LIMIT`] at most.
    async fn unlocked(path: &Path) -> bool {
        let released = async {
            while locked(path) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(LIMIT, released).await.is_ok()
    }

    /// Waits until every permit of `transcoder` is back, for [`LIMIT`] at
    /// most. A permit returns when its process was waited for.
    async fn all_permits_back(transcoder: &FfmpegTranscoder, permits: u32) -> bool {
        let back = transcoder.semaphore.acquire_many(permits);
        tokio::time::timeout(LIMIT, back).await.is_ok()
    }

    #[test]
    fn argv_is_the_command_line_of_the_design() {
        let transcoder = transcoder("ffmpeg", 2);
        let path = "/music/Aurora Field/01 Low Tide.flac";
        // The options before `-i` are input options. The two whitelists are
        // written out here, so that a change of a constant shows.
        let input = [
            "-protocol_whitelist",
            "file",
            "-format_whitelist",
            "aac,aiff,flac,mov,mp3,ogg,wav",
        ];
        let expected = |seek: &[&'static str], output: [&'static str; 10]| -> Vec<&str> {
            let mut argv = vec!["ffmpeg", "-nostdin", "-v", "error"];
            argv.extend(input);
            argv.extend(seek);
            argv.extend(["-i", path]);
            argv.extend(output);
            argv
        };
        let opus = |bitrate| {
            [
                "-map", "0:a:0", "-vn", "-c:a", "libopus", "-b:a", bitrate, "-f", "ogg", "-",
            ]
        };
        let mp3 = |bitrate| {
            [
                "-map",
                "0:a:0",
                "-vn",
                "-c:a",
                "libmp3lame",
                "-b:a",
                bitrate,
                "-f",
                "mp3",
                "-",
            ]
        };
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Opus, 96, 0))),
            expected(&[], opus("96k"))
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Opus, 192, 30))),
            expected(&["-ss", "30"], opus("192k"))
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Mp3, 320, 0))),
            expected(&[], mp3("320k"))
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Mp3, 32, 4_294_967_295))),
            expected(&["-ss", "4294967295"], mp3("32k"))
        );

        // The whole vector of one job, as one line.
        assert_eq!(
            strings(&transcoder.argv(&job("/music/a.flac", Target::Opus, 96, 30))).join(" "),
            "ffmpeg -nostdin -v error -protocol_whitelist file \
             -format_whitelist aac,aiff,flac,mov,mp3,ogg,wav -ss 30 -i /music/a.flac \
             -map 0:a:0 -vn -c:a libopus -b:a 96k -f ogg -"
        );
    }

    #[test]
    fn whitelists_precede_the_input_and_name_local_files_and_audio_containers() {
        assert_eq!(PROTOCOLS, "file");
        // One demuxer for each audio container that the scanner supports,
        // and no other: no playlist, no script and no network format.
        let mut demuxers: Vec<&str> = DEMUXERS.split(',').collect();
        demuxers.sort_unstable();
        assert_eq!(
            demuxers,
            ["aac", "aiff", "flac", "mov", "mp3", "ogg", "wav"]
        );

        // Both options are input options, so they stand before `-i`.
        for offset_s in [0, 30] {
            let job = job("/music/a.flac", Target::Mp3, 128, offset_s);
            let argv = transcoder("ffmpeg", 2).argv(&job);
            let position = |argument: &str| {
                argv.iter()
                    .position(|element| element == argument)
                    .unwrap_or_else(|| panic!("{argument} is absent"))
            };
            let input = position("-i");
            for (option, value) in [
                ("-protocol_whitelist", PROTOCOLS),
                ("-format_whitelist", DEMUXERS),
            ] {
                let at = position(option);
                assert!(at < input, "{option}");
                assert_eq!(argv[at + 1], value, "{option}");
            }
        }
    }

    #[test]
    fn argv_holds_the_program_and_the_path_as_one_argument_each() {
        // A program path with spaces, as `transcoding.ffmpeg` may hold it.
        let programs = [
            "/opt/my tools/ffmpeg 7/bin/ffmpeg",
            r"C:\Program Files\ffmpeg\bin\ffmpeg.exe",
            "ffmpeg -version; echo $(id)",
        ];
        // File names that a shell or ffmpeg would read as something else if
        // they were not one argument after `-i`: spaces, quotes, a leading
        // hyphen, the form of an ffmpeg protocol, and shell syntax.
        let paths = [
            "/music/Aurora Field/Glass Harbour/01 Low Tide.flac",
            "/music/it's \"quoted\" `here`.flac",
            "/music/-y.flac",
            "/music/-i /etc/passwd -f null.flac",
            "/music/concat:a.flac|b.flac",
            "/music/http://example.org/a.flac",
            "/music/a.flac; rm -rf ~ & echo $(id) | cat > x.flac",
            "/music/new\nline\ttab.flac",
            "/music/日本語 ünïcode.flac",
        ];
        for program in programs {
            for path in paths {
                let transcoder = transcoder(program, 2);
                for offset_s in [0, 30] {
                    let job = job(path, Target::Opus, 96, offset_s);
                    let argv = transcoder.argv(&job);
                    let input = argv
                        .iter()
                        .position(|argument| argument == "-i")
                        .expect("the input option");
                    assert_eq!(argv[0], program, "{program}");
                    assert_eq!(argv[input + 1], path, "{path}");
                    // The vector has the length of the fixed arguments, so
                    // no path added an argument.
                    let fixed = if offset_s == 0 { 20 } else { 22 };
                    assert_eq!(argv.len(), fixed, "{program} {path}");
                    assert_eq!(
                        argv.iter().filter(|argument| *argument == "-i").count(),
                        1,
                        "{path}"
                    );

                    // The command runs the program itself with these
                    // arguments. It is no command line of a shell.
                    let command = transcoder.command(&job);
                    let command = command.as_std();
                    assert_eq!(command.get_program(), argv[0].as_os_str(), "{program}");
                    let arguments: Vec<_> = command.get_args().collect();
                    let expected: Vec<_> = argv[1..].iter().map(OsString::as_os_str).collect();
                    assert_eq!(arguments, expected, "{path}");
                }
            }
        }
    }

    // A file name on Unix is a sequence of bytes and need not be Unicode
    // text.
    #[cfg(unix)]
    #[test]
    fn argv_holds_a_path_that_is_no_unicode_text_unchanged() {
        use std::os::unix::ffi::OsStringExt;

        let name = OsString::from_vec(b"/music/caf\xe9 \xff.flac".to_vec());
        let job = TranscodeJob {
            track: TRACK.to_owned(),
            path: PathBuf::from(&name),
            target: Target::Mp3,
            bitrate_kbps: 128,
            offset_s: 0,
        };
        let argv = transcoder("ffmpeg", 2).argv(&job);
        assert_eq!(argv[9], name);
        assert_eq!(argv.len(), 20);
    }

    #[tokio::test]
    async fn path_that_is_not_absolute_starts_no_process() {
        // The program is the test binary, so a process that started would
        // be a run of the tests. It is not started.
        let program = std::env::current_exe().expect("the test binary");
        let transcoder = transcoder(program.to_str().expect("a Unicode path"), 1);
        for path in [
            "a.flac",
            "concat:a.flac|b.flac",
            "http://example.org/a.flac",
            "-y",
            "",
        ] {
            let error = transcoder
                .spawn(job(path, Target::Opus, 96, 0))
                .await
                .expect_err("no stream");
            match error {
                TranscodeError::Spawn(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{path}");
                }
                other => panic!("{path}: {other:?}"),
            }
            assert_eq!(transcoder.semaphore.available_permits(), 1, "{path}");
        }
    }

    #[tokio::test]
    async fn dropping_the_body_kills_the_process_and_returns_the_permit() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let lock = dir.path().join("child.lock");
        let locking_child = || {
            let mut command = child(10);
            command.env(LOCK_VARIABLE, &lock);
            command
        };
        let transcoder = transcoder("unused", 1);
        let stream = transcoder
            .start(locking_child, "audio/ogg", no_report())
            .await
            .expect("the process starts");
        assert_eq!(stream.content_type, "audio/ogg");
        let mut body = stream.body;
        // The process runs, holds its lock and sleeps for ten seconds.
        let id = started(&mut body).await;
        assert!(locked(&lock));
        assert_ne!(is_child_of_this_process(id), Some(false));
        assert_eq!(transcoder.semaphore.available_permits(), 0);

        // The client disconnects: the server drops the body.
        let dropped = Instant::now();
        drop(body);
        // The process is gone: the system released its lock. A process that
        // sleeps on holds the lock for ten seconds.
        assert!(
            unlocked(&lock).await,
            "the process was not killed within {LIMIT:?}"
        );
        // The permit returns when the process was waited for, which is what
        // makes the system release the entry of the process.
        assert!(
            all_permits_back(&transcoder, 1).await,
            "the process was not waited for within {LIMIT:?}"
        );
        assert!(dropped.elapsed() < LIMIT);
        // The system has no entry for the process: it was waited for.
        assert_ne!(
            is_child_of_this_process(id),
            Some(true),
            "the process {id} has ended and was not waited for"
        );

        // A new job gets the permit and its process.
        let stream = tokio::time::timeout(
            LIMIT,
            transcoder.start(locking_child, "audio/ogg", no_report()),
        )
        .await
        .expect("the job does not wait")
        .expect("the process starts");
        let mut body = stream.body;
        started(&mut body).await;
        assert!(locked(&lock));
        drop(body);
        assert!(unlocked(&lock).await);
        assert!(all_permits_back(&transcoder, 1).await);
    }

    #[tokio::test]
    async fn job_past_the_limit_is_busy_and_starts_no_process() {
        let transcoder = transcoder("unused", 1);
        let mut first = transcoder
            .start(|| child(10), "audio/ogg", no_report())
            .await
            .expect("the process starts")
            .body;
        started(&mut first).await;

        // The limit holds as long as the body of the first job is open, and
        // not only while its process is started: the second attempt comes
        // after a whole permit wait.
        for attempt in 1..=2 {
            let built = Cell::new(false);
            let asked = Instant::now();
            let second = transcoder
                .start(
                    || {
                        built.set(true);
                        child(10)
                    },
                    "audio/ogg",
                    no_report(),
                )
                .await;
            let waited = asked.elapsed();
            assert!(
                matches!(second, Err(TranscodeError::Busy)),
                "attempt {attempt}: {second:?}"
            );
            assert!(waited >= PERMIT_WAIT, "attempt {attempt}: {waited:?}");
            assert!(waited < LIMIT, "attempt {attempt}: {waited:?}");
            // The job built no command, so it started no process.
            assert!(!built.get(), "attempt {attempt}");
        }
        assert_eq!(transcoder.semaphore.available_permits(), 0);

        // After the first body is dropped, the next job gets its permit.
        drop(first);
        let third = tokio::time::timeout(
            LIMIT,
            transcoder.start(|| child(10), "audio/ogg", no_report()),
        )
        .await
        .expect("the job gets the permit when the process is gone")
        .expect("the process starts");
        drop(third);
        assert!(all_permits_back(&transcoder, 1).await);
    }

    #[tokio::test]
    async fn two_permits_let_two_processes_run() {
        let transcoder = transcoder("unused", 2);
        let mut bodies = Vec::new();
        for _ in 0..2 {
            let stream = transcoder
                .start(|| child(10), "audio/ogg", no_report())
                .await
                .expect("the process starts");
            bodies.push(stream.body);
        }
        for body in &mut bodies {
            started(body).await;
        }
        let third = transcoder
            .start(|| child(10), "audio/ogg", no_report())
            .await;
        assert!(matches!(third, Err(TranscodeError::Busy)), "{third:?}");

        // One process ends, and one job more may start.
        bodies.pop();
        let third = tokio::time::timeout(
            LIMIT,
            transcoder.start(|| child(10), "audio/ogg", no_report()),
        )
        .await
        .expect("the job gets the permit")
        .expect("the process starts");
        drop(third);
        drop(bodies);
        assert!(all_permits_back(&transcoder, 2).await);
    }

    #[tokio::test]
    async fn process_that_ends_returns_the_permit_while_the_body_is_held() {
        let transcoder = transcoder("unused", 1);
        // The process writes its output and ends at once.
        let stream = transcoder
            .start(|| child(0), "audio/mpeg", no_report())
            .await
            .expect("the process starts");
        let mut body = stream.body;
        let mut output = Vec::new();
        let read = async {
            while let Some(frame) = body.frame().await {
                let data = frame.expect("a frame").into_data().expect("a data frame");
                output.extend_from_slice(&data);
            }
        };
        tokio::time::timeout(LIMIT, read)
            .await
            .expect("the output ends within the limit");
        assert!(child_id(&output).is_some());

        // The body is not dropped, and the permit is back: a response that
        // a client reads slowly holds no permit after its process ended.
        assert!(all_permits_back(&transcoder, 1).await);
        drop(body);
        assert_eq!(transcoder.semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn program_that_cannot_be_started_is_a_spawn_error_and_keeps_no_permit() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("no ffmpeg here");
        let transcoder = transcoder(missing.to_str().expect("a Unicode path"), 1);
        let path = dir.path().join("a.flac");
        for _ in 0..2 {
            let error = transcoder
                .spawn(job(path.to_str().unwrap(), Target::Opus, 96, 0))
                .await
                .expect_err("no stream");
            assert!(matches!(error, TranscodeError::Spawn(_)), "{error:?}");
            assert_eq!(transcoder.semaphore.available_permits(), 1);
        }
    }

    #[tokio::test]
    async fn probe_of_a_missing_program_gives_a_transcoder_that_is_unavailable() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let config = Transcoding {
            ffmpeg: dir
                .path()
                .join("no ffmpeg here")
                .to_str()
                .expect("a Unicode path")
                .to_owned(),
            max_concurrent: 1,
            ..Transcoding::default()
        };
        let transcoder = FfmpegTranscoder::probe(&config).await;
        assert_eq!(transcoder.targets, []);

        let path = dir.path().join("a.flac");
        let error = transcoder
            .spawn(job(path.to_str().unwrap(), Target::Opus, 96, 0))
            .await
            .expect_err("no stream");
        assert!(matches!(error, TranscodeError::Unavailable), "{error:?}");
        assert_eq!(error.to_string(), "ffmpeg not available");
        assert_eq!(transcoder.semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn version_is_the_first_line_of_a_program_that_succeeds() {
        // The child process writes the lines of the test harness and its
        // own line, and ends with success.
        let line = version(child(0), LIMIT).await.expect("the program runs");
        assert!(!line.is_empty());
        assert!(!line.contains('\n'), "{line}");
        assert!(line.chars().count() <= VERSION_CHARS, "{line}");
    }

    #[tokio::test]
    async fn version_reports_a_program_that_fails_is_missing_or_does_not_end() {
        // The test harness ends with a failure status for an option that it
        // does not have.
        let mut fails = Command::new(std::env::current_exe().expect("the test binary"));
        fails.arg("--no-such-option-of-the-test-harness");
        let reason = version(fails, LIMIT).await.expect_err("a failure status");
        assert!(reason.contains("ended with"), "{reason}");

        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = Command::new(dir.path().join("no ffmpeg here"));
        let reason = version(missing, LIMIT).await.expect_err("no program");
        assert!(reason.contains("cannot be run"), "{reason}");

        // A program that sleeps on is given up after the limit and killed.
        let asked = Instant::now();
        let reason = version(child(10), Duration::from_millis(200))
            .await
            .expect_err("no answer");
        assert!(reason.contains("did not end within"), "{reason}");
        assert!(asked.elapsed() < LIMIT);
    }

    /// The longest time one run of the stand-in may take in a test of the
    /// probe. It is a limit and no wait: a run takes some milliseconds.
    const PROBE_TEST_LIMIT: Duration = Duration::from_secs(10);

    /// The start of the output of `ffmpeg -hide_banner -encoders` of
    /// ffmpeg 8.0, and its lines of four audio encoders.
    const ENCODER_LIST_HEAD: &str = "Encoders:
 V..... = Video
 A..... = Audio
 S..... = Subtitle
 .F.... = Frame-level multithreading
 ..S... = Slice-level multithreading
 ...X.. = Codec is experimental
 ....B. = Supports draw_horiz_band
 .....D = Supports direct rendering method 1
 ------
 V....D a64multi             Multicolor charset for Commodore 64 (codec a64_multi)
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 A....D flac                 FLAC (Free Lossless Audio Codec)
 A..X.D opus                 Opus
";

    /// The line of the encoder libopus in that output.
    const LIBOPUS_LINE: &str = " A....D libopus              libopus Opus (codec opus)\n";

    /// The line of the encoder libmp3lame in that output.
    const LIBMP3LAME_LINE: &str =
        " A....D libmp3lame           libmp3lame MP3 (MPEG audio layer 3) (codec mp3)\n";

    /// The command of a child process that writes `output` and ends with
    /// success: a program that the probe runs.
    fn program_that_writes(output: &str) -> Command {
        let mut command = child(0);
        command.env(PRINT_VARIABLE, output);
        command
    }

    #[test]
    fn encoder_list_names_an_encoder_by_its_whole_name() {
        let both = format!("{ENCODER_LIST_HEAD}{LIBMP3LAME_LINE}{LIBOPUS_LINE}");
        assert!(lists_encoder(&both, "libopus"));
        assert!(lists_encoder(&both, "libmp3lame"));
        assert!(lists_encoder(&both, "flac"));
        // The encoder `opus` of ffmpeg itself is another encoder.
        assert!(lists_encoder(ENCODER_LIST_HEAD, "opus"));
        assert!(!lists_encoder(ENCODER_LIST_HEAD, "libopus"));
        assert!(!lists_encoder(ENCODER_LIST_HEAD, "libmp3lame"));
        let opus_only = format!("{ENCODER_LIST_HEAD}{LIBOPUS_LINE}");
        assert!(lists_encoder(&opus_only, "libopus"));
        assert!(!lists_encoder(&opus_only, "libmp3lame"));

        // A name in a description, a video encoder with the name, a line
        // of the legend and a line without flags name no audio encoder.
        for list in [
            " A....D other                wraps libopus\n",
            " V....D libopus              a video encoder\n",
            " A..... = libopus\n",
            "libopus\n",
            " libopus libopus\n",
            "",
        ] {
            assert!(!lists_encoder(list, "libopus"), "{list:?}");
        }
        // Windows line ends.
        assert!(lists_encoder(
            " A....D libopus              libopus Opus (codec opus)\r\n",
            "libopus"
        ));
    }

    #[test]
    fn encoder_of_a_target_is_the_codec_of_its_command_line() {
        for (target, name) in [(Target::Opus, "libopus"), (Target::Mp3, "libmp3lame")] {
            assert_eq!(encoder(target), name);
            let argv = transcoder("ffmpeg", 1).argv(&job("/music/a.flac", target, 96, 0));
            let codec = argv
                .iter()
                .position(|argument| argument == "-c:a")
                .expect("the codec option");
            assert_eq!(argv[codec + 1], name);
        }
        assert_eq!(TARGETS, [Target::Opus, Target::Mp3]);
    }

    #[tokio::test]
    async fn probe_finds_the_targets_whose_encoder_the_program_lists() {
        let both = format!("{ENCODER_LIST_HEAD}{LIBMP3LAME_LINE}{LIBOPUS_LINE}");
        let opus_only = format!("{ENCODER_LIST_HEAD}{LIBOPUS_LINE}");
        let mp3_only = format!("{ENCODER_LIST_HEAD}{LIBMP3LAME_LINE}");
        for (list, missing, targets) in [
            (both.as_str(), vec![], vec![Target::Opus, Target::Mp3]),
            (opus_only.as_str(), vec![Target::Mp3], vec![Target::Opus]),
            (mp3_only.as_str(), vec![Target::Opus], vec![Target::Mp3]),
            (ENCODER_LIST_HEAD, vec![Target::Opus, Target::Mp3], vec![]),
        ] {
            // The program gets the arguments of the two runs, in their order.
            let runs = std::sync::Mutex::new(Vec::new());
            let program = |arguments: &[&str]| {
                runs.lock().unwrap().push(arguments.join(" "));
                program_that_writes(list)
            };
            let found = probed(program, PROBE_TEST_LIMIT).await;
            assert_eq!(
                *runs.lock().unwrap(),
                ["-version", "-hide_banner -encoders"],
                "{missing:?}"
            );
            let Probe::Program {
                version,
                missing: found_missing,
            } = &found
            else {
                panic!("{found:?}");
            };
            assert!(!version.is_empty());
            assert_eq!(*found_missing, missing);
            assert_eq!(found.targets(), targets);
            // One warning when an encoder is missing, and none otherwise.
            assert_eq!(found.warning().is_some(), !missing.is_empty(), "{found:?}");
        }
    }

    #[tokio::test]
    async fn probe_of_a_program_whose_encoder_list_cannot_be_read_finds_no_target() {
        // The program answers `-version` and fails for the list.
        let program = |arguments: &[&str]| {
            if arguments.contains(&"-encoders") {
                failing_child(3)
            } else {
                child(0)
            }
        };
        let found = probed(program, PROBE_TEST_LIMIT).await;
        let Probe::NoProgram(reason) = &found else {
            panic!("{found:?}");
        };
        assert!(reason.contains("list of encoders"), "{reason}");
        assert!(reason.contains("ended with"), "{reason}");
        assert_eq!(found.targets(), []);

        // A program that does not run is not asked for its list.
        let runs = std::sync::Mutex::new(0);
        let dir = tempfile::tempdir().expect("a temporary directory");
        let program = |_arguments: &[&str]| {
            *runs.lock().unwrap() += 1;
            Command::new(dir.path().join("no ffmpeg here"))
        };
        let found = probed(program, PROBE_TEST_LIMIT).await;
        assert!(matches!(&found, Probe::NoProgram(reason) if reason.contains("cannot be run")));
        assert_eq!(*runs.lock().unwrap(), 1);
    }

    #[test]
    fn probe_has_one_warning_that_names_what_is_missing() {
        let found = |missing: &[Target]| Probe::Program {
            version: "ffmpeg version 8.0".to_owned(),
            missing: missing.to_vec(),
        };
        assert_eq!(found(&[]).warning(), None);

        let warning = found(&[Target::Mp3]).warning().expect("a warning");
        assert!(warning.contains("no encoder libmp3lame"), "{warning}");
        assert!(
            warning.contains("transcode to MP3 gets an error"),
            "{warning}"
        );
        assert!(warning.contains("transcode to Opus works"), "{warning}");
        assert!(!warning.contains("libopus"), "{warning}");

        let warning = found(&[Target::Opus]).warning().expect("a warning");
        assert!(warning.contains("no encoder libopus"), "{warning}");
        assert!(
            warning.contains("transcode to Opus gets an error"),
            "{warning}"
        );
        assert!(warning.contains("transcode to MP3 works"), "{warning}");
        assert!(!warning.contains("libmp3lame"), "{warning}");

        let warning = found(&[Target::Opus, Target::Mp3])
            .warning()
            .expect("a warning");
        assert!(
            warning.contains("no encoder libopus and libmp3lame"),
            "{warning}"
        );
        assert!(warning.contains("serves original files"), "{warning}");
        assert!(!warning.contains("works"), "{warning}");

        let warning = Probe::NoProgram("the program cannot be run (not found)".to_owned())
            .warning()
            .expect("a warning");
        assert!(
            warning.starts_with("ffmpeg is not available: the program cannot be run (not found)."),
            "{warning}"
        );
        assert!(warning.contains("serves original files"), "{warning}");
    }

    #[tokio::test]
    async fn target_without_its_encoder_is_unavailable_and_the_other_target_starts() {
        // The program is a path that names no program, so a job that passes
        // the check of the target fails where the process starts.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let missing = dir.path().join("no ffmpeg here");
        let bin = missing.to_str().expect("a Unicode path");
        let path = dir.path().join("a.flac");
        let path = path.to_str().expect("a Unicode path");
        for (has, lacks) in [(Target::Opus, Target::Mp3), (Target::Mp3, Target::Opus)] {
            let transcoder = transcoder_for(bin, 1, &[has]);
            let error = transcoder
                .spawn(job(path, lacks, 96, 0))
                .await
                .expect_err("no stream");
            assert!(matches!(error, TranscodeError::Unavailable), "{error:?}");
            assert_eq!(error.to_string(), "ffmpeg not available");

            let error = transcoder
                .spawn(job(path, has, 96, 0))
                .await
                .expect_err("no program");
            assert!(matches!(error, TranscodeError::Spawn(_)), "{error:?}");
            assert_eq!(transcoder.semaphore.available_permits(), 1);
        }
    }

    /// Reads `body` to its end, for [`LIMIT`] at most.
    async fn read_to_end(body: Body) -> Vec<u8> {
        let read = tokio::time::timeout(LIMIT, body.collect())
            .await
            .expect("the output ends within the limit")
            .expect("the output is read");
        read.to_bytes().to_vec()
    }

    #[tokio::test]
    async fn process_that_fails_is_reported_once_with_its_status() {
        let transcoder = transcoder("unused", 1);
        let (report, end) = recorded();
        let stream = transcoder
            .start(|| failing_child(3), "audio/ogg", report)
            .await
            .expect("the process starts");
        // The client reads the output to its end. The process then ends by
        // itself, with the status 3.
        read_to_end(stream.body).await;
        let end = end_of(end).await;
        let ProcessEnd::Exited(status) = end else {
            panic!("{end:?}");
        };
        assert_eq!(status.code(), Some(3));
        // This end is the one that the transcoder logs at `warn`.
        assert_eq!(failure(end), Some(status));
        assert!(all_permits_back(&transcoder, 1).await);
    }

    #[tokio::test]
    async fn process_that_failed_before_its_body_is_dropped_is_reported_with_its_status() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let lock = dir.path().join("child.lock");
        let transcoder = transcoder("unused", 1);
        let (report, end) = recorded();
        let stream = transcoder
            .start(
                || {
                    let mut command = failing_child(7);
                    command.env(LOCK_VARIABLE, &lock);
                    command
                },
                "audio/ogg",
                report,
            )
            .await
            .expect("the process starts");
        // The process has ended, and nobody has read its output: the lock
        // file exists, and the system released the lock.
        let ended = async {
            while !lock.exists() || locked(&lock) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(LIMIT, ended)
            .await
            .expect("the process ends within the limit");

        // The body is dropped before its end. The process is not reported
        // as killed, because it had failed by itself.
        drop(stream);
        let end = end_of(end).await;
        let ProcessEnd::Exited(status) = end else {
            panic!("{end:?}");
        };
        assert_eq!(status.code(), Some(7));
        assert_eq!(failure(end), Some(status));
        assert!(all_permits_back(&transcoder, 1).await);
    }

    #[tokio::test]
    async fn process_that_succeeds_and_process_that_the_server_kills_are_no_failures() {
        let transcoder = transcoder("unused", 1);

        // A process that ends by itself with success.
        let (report, end) = recorded();
        let stream = transcoder
            .start(|| child(0), "audio/ogg", report)
            .await
            .expect("the process starts");
        read_to_end(stream.body).await;
        let end = end_of(end).await;
        assert!(
            matches!(end, ProcessEnd::Exited(status) if status.success()),
            "{end:?}"
        );
        assert_eq!(failure(end), None);
        assert!(all_permits_back(&transcoder, 1).await);

        // A process that sleeps when the client disconnects. The status of
        // a killed process is no success, and it is no failure of ffmpeg.
        let (report, end) = recorded();
        let stream = transcoder
            .start(|| child(10), "audio/ogg", report)
            .await
            .expect("the process starts");
        let mut body = stream.body;
        started(&mut body).await;
        drop(body);
        let end = end_of(end).await;
        assert_eq!(end, ProcessEnd::Killed);
        assert_eq!(failure(end), None);
        assert!(all_permits_back(&transcoder, 1).await);
    }

    #[tokio::test]
    async fn process_whose_output_has_ended_is_waited_for_and_killed_after_the_wait() {
        let spawned = |mut command: Command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .kill_on_drop(true);
            command.spawn().expect("the process starts")
        };

        // A process that ends by itself within the wait: its status.
        let mut failing = spawned(failing_child(3));
        let end = ended_by_itself(&mut failing, LIMIT).await;
        let ProcessEnd::Exited(status) = end else {
            panic!("{end:?}");
        };
        assert_eq!(status.code(), Some(3));
        assert_eq!(failure(end), Some(status));

        let mut succeeding = spawned(child(0));
        let end = ended_by_itself(&mut succeeding, LIMIT).await;
        assert!(
            matches!(end, ProcessEnd::Exited(status) if status.success()),
            "{end:?}"
        );

        // A process that runs on after the wait is killed and waited for,
        // so that it does not hold its permit. It is no failure.
        let mut sleeping = spawned(child(10));
        let asked = Instant::now();
        let end = ended_by_itself(&mut sleeping, Duration::from_millis(100)).await;
        assert_eq!(end, ProcessEnd::Killed);
        assert_eq!(failure(end), None);
        assert!(asked.elapsed() >= Duration::from_millis(100));
        assert!(asked.elapsed() < LIMIT, "{:?}", asked.elapsed());
        let waited = sleeping.try_wait().expect("the state of the process");
        assert!(waited.is_some(), "the process was not waited for");
    }

    #[tokio::test]
    async fn transcoder_without_a_program_is_unavailable_and_starts_no_process() {
        // The program is the test binary, which runs. A transcoder that
        // started it would return a stream.
        let program = std::env::current_exe().expect("the test binary");
        let config = Transcoding {
            ffmpeg: program.to_str().expect("a Unicode path").to_owned(),
            max_concurrent: 1,
            ..Transcoding::default()
        };
        let transcoder = FfmpegTranscoder::without_program(&config);
        assert_eq!(transcoder.targets, []);

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.flac");
        for target in TARGETS {
            let error = transcoder
                .spawn(job(path.to_str().unwrap(), target, 96, 0))
                .await
                .expect_err("no stream");
            assert!(matches!(error, TranscodeError::Unavailable), "{error:?}");
            assert_eq!(error.to_string(), "ffmpeg not available");
            // No job took a permit, which a job takes before its process
            // starts.
            assert_eq!(transcoder.semaphore.available_permits(), 1);
        }
    }

    /// The directory of the committed audio fixtures.
    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audio")
    }

    /// The transcoder of the default config, which runs `ffmpeg` from
    /// `PATH`. The test fails when the probe finds no ffmpeg, or an ffmpeg
    /// without one of the two encoders.
    async fn real_transcoder() -> FfmpegTranscoder {
        let transcoder = FfmpegTranscoder::probe(&Transcoding::default()).await;
        assert_eq!(
            transcoder.targets, TARGETS,
            "this test needs ffmpeg with libopus and libmp3lame on PATH \
             (the probe of `ffmpeg -version` and `ffmpeg -encoders` found these targets)"
        );
        transcoder
    }

    /// Runs real ffmpeg on the file at `path` with the command line of the
    /// server, and returns the stream of `target` and how the process
    /// ended.
    async fn real_transcode(
        transcoder: &FfmpegTranscoder,
        path: &Path,
        target: Target,
        offset_s: u32,
    ) -> (Vec<u8>, ProcessEnd) {
        let job = TranscodeJob {
            track: TRACK.to_owned(),
            path: path.to_path_buf(),
            target,
            bitrate_kbps: 96,
            offset_s,
        };
        let (report, end) = recorded();
        let stream = transcoder.run(&job, report).await.expect("ffmpeg starts");
        assert_eq!(stream.content_type, target.content_type());
        let output = stream.body.collect().await.expect("the output is read");
        // ffmpeg is not held to the limit of the stand-in process.
        let end = tokio::time::timeout(Duration::from_secs(10), end)
            .await
            .expect("ffmpeg ends")
            .expect("the report is called");
        (output.to_bytes().to_vec(), end)
    }

    /// Converts the FLAC fixture to the file `name` in `dir` with a plain
    /// run of ffmpeg, which selects the container by the suffix of `name`.
    /// The fixtures hold no file of these containers.
    fn converted(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(fixtures().join("a.flac"))
            .arg(&path)
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "{name}: {status}");
        path
    }

    /// One of the four tests that run ffmpeg: three of this file, and one of
    /// `stream.rs`, which sends a file through `stream::transcoded`. They
    /// need ffmpeg with libopus and libmp3lame on PATH, so they are not part
    /// of the default suite:
    /// `cargo test --lib -- --ignored --nocapture real_ffmpeg`.
    ///
    /// The test transcodes a file of every audio container that the scanner
    /// supports, under the two whitelists of the command line: the five
    /// fixtures, and a WAV, an AIFF and an ADTS AAC file that ffmpeg makes
    /// from the FLAC fixture.
    #[tokio::test]
    #[ignore = "runs ffmpeg, which the default suite does not need"]
    async fn real_ffmpeg_transcodes_every_supported_container_to_opus_and_mp3() {
        let transcoder = real_transcoder().await;
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut files: Vec<PathBuf> = ["a.flac", "a.mp3", "a.m4a", "a.ogg", "a.opus"]
            .map(|name| fixtures().join(name))
            .to_vec();
        for name in ["made.wav", "made.aiff", "made.aif", "made.aac"] {
            files.push(converted(dir.path(), name));
        }

        for file in &files {
            let name = file.file_name().expect("a file name").to_string_lossy();
            let (opus, end) = real_transcode(&transcoder, file, Target::Opus, 0).await;
            assert!(
                matches!(end, ProcessEnd::Exited(status) if status.success()),
                "{name}: {end:?}"
            );
            assert!(opus.starts_with(b"OggS"), "{name}");
            assert!(
                opus.windows(8).any(|window| window == b"OpusHead"),
                "{name}: the stream holds no Opus header"
            );

            let (mp3, end) = real_transcode(&transcoder, file, Target::Mp3, 0).await;
            assert!(
                matches!(end, ProcessEnd::Exited(status) if status.success()),
                "{name}: {end:?}"
            );
            assert!(
                mp3.starts_with(&[0xff, 0xfb]) || mp3.starts_with(b"ID3"),
                "{name}: {:?}",
                &mp3[..mp3.len().min(8)]
            );
            println!("{name}: Opus {} bytes, MP3 {} bytes", opus.len(), mp3.len());
        }

        // A stream that starts inside the track is shorter. The fixture
        // lasts half a second, so the offset of one second is past its end,
        // and ffmpeg writes a stream without audio.
        let flac = fixtures().join("a.flac");
        let (whole, _) = real_transcode(&transcoder, &flac, Target::Mp3, 0).await;
        let (late, end) = real_transcode(&transcoder, &flac, Target::Mp3, 1).await;
        assert!(late.len() < whole.len(), "{} {}", late.len(), whole.len());
        println!(
            "a.flac from the second 1: MP3 {} bytes, {end:?}",
            late.len()
        );
        assert!(all_permits_back(&transcoder, 2).await);
    }

    /// Runs ffmpeg with `arguments`, without its error output, and returns
    /// what it wrote. `None` when ffmpeg cannot be run or has not ended
    /// within ten seconds.
    async fn ffmpeg_output(arguments: Vec<OsString>) -> Option<std::process::Output> {
        let mut command = Command::new("ffmpeg");
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let run = tokio::time::timeout(Duration::from_secs(10), command.output());
        run.await.ok()?.ok()
    }

    /// The job of an Opus transcode of the file at `path`.
    fn opus_job(path: &Path) -> TranscodeJob {
        TranscodeJob {
            track: TRACK.to_owned(),
            path: path.to_path_buf(),
            target: Target::Opus,
            bitrate_kbps: 96,
            offset_s: 0,
        }
    }

    /// Runs ffmpeg on the file at `path` with the command line of the
    /// server without the two whitelists, and describes what it did: the
    /// length of its output, or that it did not end within ten seconds. The
    /// test prints the text and asserts nothing about it: it shows what the
    /// ffmpeg of the machine does with the file by default.
    async fn without_the_whitelists(path: &Path) -> String {
        let mut arguments = arguments(&opus_job(path));
        for option in ["-protocol_whitelist", "-format_whitelist"] {
            let at = arguments
                .iter()
                .position(|argument| argument == option)
                .expect("the option");
            arguments.drain(at..at + 2);
        }
        match ffmpeg_output(arguments).await {
            Some(output) => format!("{} bytes and {}", output.stdout.len(), output.status),
            None => "no end within ten seconds".to_owned(),
        }
    }

    /// The number of connections that were opened to `listener` and that it
    /// has not accepted yet. The listener does not wait.
    fn connections(listener: &std::net::TcpListener) -> usize {
        let mut count = 0;
        loop {
            match listener.accept() {
                Ok(_) => count += 1,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return count,
                Err(error) => panic!("accept a connection: {error}"),
            }
        }
    }

    /// The second test that runs ffmpeg (see the first one). A file with
    /// the name of an audio file that is a playlist or a script names
    /// another local file and a URL. ffmpeg decides the format from the
    /// content, so without the whitelists it reads such a file as what it
    /// is. Under the command line of the server it fails: it writes no
    /// byte, so its output holds nothing of the other file, and it opens no
    /// connection to the URL.
    #[tokio::test]
    #[ignore = "runs ffmpeg, which the default suite does not need"]
    async fn real_ffmpeg_does_not_follow_a_playlist_with_an_audio_file_name() {
        let transcoder = real_transcoder().await;
        let dir = tempfile::tempdir().expect("a temporary directory");
        // The file that the playlists name. ffmpeg can read it.
        fs::copy(fixtures().join("a.flac"), dir.path().join("secret.flac")).expect("copy the file");
        // The URL that the playlists name leads to a listener of this test,
        // which sees every connection that is opened to it.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a loopback port");
        listener
            .set_nonblocking(true)
            .expect("a listener that does not wait");
        let url = format!(
            "http://{}/a.flac",
            listener.local_addr().expect("the address of the listener")
        );

        let concat_local = "ffconcat version 1.0\nfile 'secret.flac'\n".to_owned();
        let concat_url = format!("ffconcat version 1.0\nfile '{url}'\nfile 'secret.flac'\n");
        let hls = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\n{url}\n\
             #EXTINF:10,\nsecret.flac\n#EXT-X-ENDLIST\n"
        );
        let crafted = [
            // A script of the concat demuxer that names the other file.
            ("script.flac", &concat_local),
            // The same with the URL.
            ("script-with-url.flac", &concat_url),
            // An HLS playlist that names the URL and the other file.
            ("playlist.mp3", &hls),
            ("playlist.ogg", &hls),
        ];
        for (name, content) in crafted {
            let path = dir.path().join(name);
            fs::write(&path, content).expect("write the crafted file");

            let (output, end) = real_transcode(&transcoder, &path, Target::Opus, 0).await;
            // ffmpeg failed and wrote nothing.
            let ProcessEnd::Exited(status) = end else {
                panic!("{name}: {end:?}");
            };
            assert!(!status.success(), "{name}: {status}");
            assert_eq!(failure(end), Some(status), "{name}");
            assert_eq!(output, b"", "{name}");
            // No connection was opened to the URL.
            assert_eq!(connections(&listener), 0, "{name}");

            // What ffmpeg does with the file without the whitelists. A
            // connection of that run is not one of the next file.
            let plain = without_the_whitelists(&path).await;
            let opened = connections(&listener);
            println!(
                "{name}: with the whitelists 0 bytes and {status}; \
                 without them {plain}, {opened} connections"
            );
        }
        // The other file itself is audio, and it is transcoded.
        let secret = dir.path().join("secret.flac");
        let (output, end) = real_transcode(&transcoder, &secret, Target::Opus, 0).await;
        assert!(matches!(end, ProcessEnd::Exited(status) if status.success()));
        assert!(output.starts_with(b"OggS"));
        assert!(all_permits_back(&transcoder, 2).await);
    }

    /// The range of the first box of the kind `kind` among the boxes that
    /// `data` holds in `within`.
    fn mp4_box(
        data: &[u8],
        within: std::ops::Range<usize>,
        kind: &[u8; 4],
    ) -> std::ops::Range<usize> {
        let mut at = within.start;
        while at + 8 <= within.end {
            let size = u32::from_be_bytes(data[at..at + 4].try_into().expect("four bytes"));
            let size = usize::try_from(size).expect("a size");
            assert!(size >= 8, "a box of {size} bytes");
            if &data[at + 4..at + 8] == kind {
                return at..at + size;
            }
            at += size;
        }
        panic!("no {} box", String::from_utf8_lossy(kind));
    }

    /// The M4A file `fixture` as a file whose audio data is in another
    /// file: its data reference is a Macintosh alias record that names the
    /// file `target` in the directory of the file, in the layout that the
    /// `mov` demuxer of ffmpeg reads, and its own sample data is zeroed. The
    /// audio of such a file can only come from the file that it names.
    fn m4a_with_a_data_reference(fixture: &[u8], target: &str) -> Vec<u8> {
        let mut data = fixture.to_vec();
        let mut chain = Vec::new();
        let mut within = 0..data.len();
        for kind in [b"moov", b"trak", b"mdia", b"minf", b"dinf", b"dref"] {
            let found = mp4_box(&data, within, kind);
            within = found.start + 8..found.end;
            chain.push(found);
        }
        // The sample data precedes the `moov` box, so the offsets of the
        // samples stay as they are when that box grows.
        let mdat = mp4_box(&data, 0..data.len(), b"mdat");
        assert!(mdat.start < chain[0].start);

        let name = target.as_bytes();
        let mut file_name = [0u8; 63];
        file_name[..name.len()].copy_from_slice(name);
        let mut alias = Vec::new();
        alias.extend(b"alis");
        alias.extend([0; 4 + 10]); // the version and the flags, and a skipped part
        alias.extend([0; 1 + 27]); // the name of the volume: none
        alias.extend([0; 12]);
        alias.push(u8::try_from(name.len()).expect("a short name"));
        alias.extend(file_name);
        alias.extend([0; 16]);
        // One level up from the file and one level down to the target: a
        // reference in the directory of the file.
        alias.extend(1u16.to_be_bytes());
        alias.extend(1u16.to_be_bytes());
        alias.extend([0; 16]);
        // The record of the type 2 is the path. The type -1 ends the records.
        alias.extend(2u16.to_be_bytes());
        alias.extend(
            u16::try_from(name.len())
                .expect("a short name")
                .to_be_bytes(),
        );
        alias.extend(name);
        alias.extend(std::iter::repeat_n(0, name.len() % 2));
        alias.extend(0xffffu16.to_be_bytes());
        alias.extend(0u16.to_be_bytes());

        let size = |length: usize| u32::try_from(length).expect("a size").to_be_bytes();
        let mut dref = Vec::new();
        dref.extend(size(16 + 4 + alias.len()));
        dref.extend(b"dref");
        dref.extend([0; 4]);
        dref.extend(1u32.to_be_bytes());
        dref.extend(size(4 + alias.len()));
        dref.extend(alias);

        let old = chain[5].clone();
        let grown = dref.len() - old.len();
        data.splice(old, dref);
        for parent in &chain[..5] {
            data[parent.start..parent.start + 4].copy_from_slice(&size(parent.len() + grown));
        }
        data[mdat.start + 8..mdat.end].fill(0);
        data
    }

    /// The third test that runs ffmpeg (see the first one). An MP4 file can
    /// name a second file as the place of its audio data. The whitelists do
    /// not refuse such a file: its demuxer and the `file` protocol are on
    /// them. ffmpeg follows the reference only with the option
    /// `enable_drefs` of the demuxer, which the command line of the server
    /// does not hold. The test shows both: with the option ffmpeg reads the
    /// audio of the other file, and under the command line of the server
    /// its stream holds no audio.
    #[tokio::test]
    #[ignore = "runs ffmpeg, which the default suite does not need"]
    async fn real_ffmpeg_does_not_follow_a_data_reference_of_an_mp4_file() {
        let transcoder = real_transcoder().await;
        let dir = tempfile::tempdir().expect("a temporary directory");
        let fixture = fs::read(fixtures().join("a.m4a")).expect("read the fixture");
        let secret = dir.path().join("secret.m4a");
        fs::write(&secret, &fixture).expect("write the other file");
        let crafted = dir.path().join("crafted.m4a");
        let bytes = m4a_with_a_data_reference(&fixture, "secret.m4a");
        fs::write(&crafted, bytes).expect("write the crafted file");

        // The transcode of the other file, for the comparison.
        let (audio, end) = real_transcode(&transcoder, &secret, Target::Opus, 0).await;
        assert!(matches!(end, ProcessEnd::Exited(status) if status.success()));
        assert!(audio.len() > 4000, "{}", audio.len());

        // The crafted file is a reference that ffmpeg can follow: with the
        // option that allows it, ffmpeg reads the audio of the other file.
        let mut allowed = arguments(&opus_job(&crafted));
        let input = allowed
            .iter()
            .position(|argument| argument == "-i")
            .expect("the input option");
        allowed.splice(input..input, ["-enable_drefs", "1"].map(OsString::from));
        let followed = ffmpeg_output(allowed).await.expect("ffmpeg ends");
        assert!(
            followed.stdout.len() > audio.len() / 2,
            "ffmpeg did not follow the reference with enable_drefs: {} bytes",
            followed.stdout.len()
        );

        // Under the command line of the server ffmpeg skips the reference:
        // the stream holds the header of an Ogg Opus stream and no audio.
        let (output, end) = real_transcode(&transcoder, &crafted, Target::Opus, 0).await;
        assert!(
            output.len() < audio.len() / 4,
            "the stream holds audio of the other file: {} bytes",
            output.len()
        );
        println!(
            "crafted.m4a: the other file gives {} bytes; with enable_drefs {} bytes; \
             with the command line of the server {} bytes, {end:?}",
            audio.len(),
            followed.stdout.len(),
            output.len()
        );
        assert!(all_permits_back(&transcoder, 2).await);
    }
}
