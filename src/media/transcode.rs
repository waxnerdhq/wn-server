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
//! The body of the response owns the process. The process is killed when
//! the body is dropped, which is what a client that disconnects causes, and
//! when its output has ended. A task then waits for the process, so that
//! the system keeps no entry for it, and returns the permit.

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
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

/// The longest time the probe at startup waits for `ffmpeg -version`. A
/// program that has not answered by then counts as not available.
const PROBE_LIMIT: Duration = Duration::from_secs(10);

/// The most bytes the probe reads from the output of `ffmpeg -version`.
const PROBE_OUTPUT_LIMIT: u64 = 64 * 1024;

/// The longest part of the version line of ffmpeg that the log holds, in
/// characters.
const VERSION_CHARS: usize = 100;

/// The longest piece of the error output of ffmpeg that one log line holds,
/// in bytes. A longer line is logged in pieces.
const STDERR_LINE_LIMIT: u64 = 1024;

/// One transcode: the file, the format, the bitrate and the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeJob {
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
    /// The server has no program that transcodes.
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
    /// Whether the probe at startup ran the program.
    available: bool,
}

impl FfmpegTranscoder {
    /// Builds the transcoder of `config` and runs `<ffmpeg> -version` once.
    ///
    /// The result of the run is logged: the version line of the program at
    /// `info`, or one warning that says why the program is not available. A
    /// transcoder without the program starts no process, and every job of it
    /// is [`TranscodeError::Unavailable`].
    pub async fn probe(config: &Transcoding) -> Self {
        let mut command = Command::new(&config.ffmpeg);
        command.arg("-version");
        let available = match version(command, PROBE_LIMIT).await {
            Ok(version) => {
                tracing::info!(ffmpeg = %config.ffmpeg, %version, "ffmpeg is available");
                true
            }
            Err(reason) => {
                tracing::warn!(
                    ffmpeg = %config.ffmpeg,
                    "ffmpeg is not available: {reason}. The server serves original files, \
                     and a stream request that needs a transcode gets an error. \
                     Install ffmpeg or set transcoding.ffmpeg, and restart the server"
                );
                false
            }
        };
        Self::new(config, available)
    }

    /// The transcoder of `config`. `available` is the result of the probe.
    fn new(config: &Transcoding, available: bool) -> Self {
        Self {
            bin: OsString::from(&config.ffmpeg),
            semaphore: Arc::new(Semaphore::new(config.max_concurrent)),
            permit_wait: Duration::from_millis(config.permit_wait_ms),
            available,
        }
    }

    /// The argument vector of the process of `job`: the program and its
    /// arguments (`docs/design.md` section 3.9).
    ///
    /// ```text
    /// ffmpeg -nostdin -v error [-ss <offset>] -i <path> -map 0:a:0 -vn -c:a <codec> -b:a <N>k -f <format> -
    /// ```
    ///
    /// The program and the path are one element each. `-ss` precedes `-i`,
    /// so ffmpeg seeks in the input, and it is absent for a stream that
    /// starts at the start of the track. The last argument writes the stream
    /// to the standard output.
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
    async fn start(
        &self,
        command: impl FnOnce() -> Command,
        content_type: &'static str,
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
            process: Some(Process { child, permit }),
        };
        Ok(TranscodeStream {
            body: Body::from_stream(output),
            content_type,
        })
    }
}

impl Transcoder for FfmpegTranscoder {
    /// Starts ffmpeg for `job`.
    ///
    /// [`TranscodeError::Unavailable`] answers every job of a transcoder
    /// whose probe failed. [`TranscodeError::Spawn`] answers a path that is
    /// not absolute, before a permit is taken (see the module comment), and
    /// a process that the system does not start.
    fn spawn(&self, job: TranscodeJob) -> TranscodeFuture<'_> {
        Box::pin(async move {
            if !self.available {
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
            self.start(|| self.command(&job), content_type).await
        })
    }
}

/// The arguments of ffmpeg for `job`, without the program.
fn arguments(job: &TranscodeJob) -> Vec<OsString> {
    let (codec, format) = match job.target {
        Target::Opus => ("libopus", "ogg"),
        Target::Mp3 => ("libmp3lame", "mp3"),
    };
    let mut arguments: Vec<OsString> = ["-nostdin", "-v", "error"].map(OsString::from).to_vec();
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

/// Runs `command`, which is `<ffmpeg> -version`, and returns the first line
/// of its output that is not empty, cut to [`VERSION_CHARS`] characters. The error says why
/// the program is not available: it cannot be started, it ends with a
/// failure status, or it has not ended after `limit`.
///
/// The process reads nothing from its standard input, and its error output
/// is discarded. A process that has not ended after `limit` is killed.
async fn version(mut command: Command, limit: Duration) -> Result<String, String> {
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
            let text = String::from_utf8_lossy(&output);
            let line = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default();
            Ok(line.chars().take(VERSION_CHARS).collect())
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

/// A transcode process and the permit under which it runs.
struct Process {
    child: Child,
    permit: OwnedSemaphorePermit,
}

/// The standard output of a transcode process as a stream of chunks. The
/// value owns the process: see the module comment for how the process ends.
struct ChildOutput {
    stdout: ReaderStream<ChildStdout>,
    /// The process, until it is released.
    process: Option<Process>,
}

impl ChildOutput {
    /// Kills the process and hands it to a task that waits for it and then
    /// returns the permit. The first call does this, and a later call does
    /// nothing.
    ///
    /// The wait is what lets the system release the process, so the permit
    /// returns when the process is gone and not while it ends. Without a
    /// runtime there is no task: the process is killed, and the permit
    /// returns at once.
    fn release(&mut self) {
        let Some(Process { mut child, permit }) = self.process.take() else {
            return;
        };
        if let Err(error) = child.start_kill() {
            // The process has ended and was waited for.
            tracing::debug!(%error, "the transcode process was not killed");
        }
        if let Ok(runtime) = Handle::try_current() {
            runtime.spawn(async move {
                match child.wait().await {
                    Ok(status) => tracing::debug!(%status, "the transcode process has ended"),
                    Err(error) => {
                        tracing::warn!(%error, "cannot wait for the transcode process");
                    }
                }
                drop(permit);
            });
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
            self.release();
        }
        next
    }
}

impl Drop for ChildOutput {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::Path;
    use std::time::Instant;

    use http_body_util::BodyExt;

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
    /// [`LOCK_VARIABLE`] when that variable is set, writes
    /// [`CHILD_STARTED`] and sleeps for that number of seconds. A process
    /// that no test kills ends by itself after the sleep.
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
            println!("{CHILD_STARTED}{}", std::process::id());
            std::thread::sleep(Duration::from_secs(seconds));
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

    /// A transcoder that the probe found, with `max_concurrent` permits and
    /// a permit wait of [`PERMIT_WAIT`]. Its program is `bin`.
    fn transcoder(bin: &str, max_concurrent: usize) -> FfmpegTranscoder {
        let config = Transcoding {
            ffmpeg: bin.to_owned(),
            max_concurrent,
            permit_wait_ms: PERMIT_WAIT_MS,
            ..Transcoding::default()
        };
        FfmpegTranscoder::new(&config, true)
    }

    fn job(path: &str, target: Target, bitrate_kbps: u32, offset_s: u32) -> TranscodeJob {
        TranscodeJob {
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
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Opus, 96, 0))),
            [
                "ffmpeg", "-nostdin", "-v", "error", "-i", path, "-map", "0:a:0", "-vn", "-c:a",
                "libopus", "-b:a", "96k", "-f", "ogg", "-",
            ]
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Opus, 192, 30))),
            [
                "ffmpeg", "-nostdin", "-v", "error", "-ss", "30", "-i", path, "-map", "0:a:0",
                "-vn", "-c:a", "libopus", "-b:a", "192k", "-f", "ogg", "-",
            ]
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Mp3, 320, 0))),
            [
                "ffmpeg",
                "-nostdin",
                "-v",
                "error",
                "-i",
                path,
                "-map",
                "0:a:0",
                "-vn",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "320k",
                "-f",
                "mp3",
                "-",
            ]
        );
        assert_eq!(
            strings(&transcoder.argv(&job(path, Target::Mp3, 32, 4_294_967_295))),
            [
                "ffmpeg",
                "-nostdin",
                "-v",
                "error",
                "-ss",
                "4294967295",
                "-i",
                path,
                "-map",
                "0:a:0",
                "-vn",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "32k",
                "-f",
                "mp3",
                "-",
            ]
        );
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
                    let fixed = if offset_s == 0 { 16 } else { 18 };
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
            path: PathBuf::from(&name),
            target: Target::Mp3,
            bitrate_kbps: 128,
            offset_s: 0,
        };
        let argv = transcoder("ffmpeg", 2).argv(&job);
        assert_eq!(argv[5], name);
        assert_eq!(argv.len(), 16);
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
            .start(locking_child, "audio/ogg")
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
        let stream = tokio::time::timeout(LIMIT, transcoder.start(locking_child, "audio/ogg"))
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
            .start(|| child(10), "audio/ogg")
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
        let third = tokio::time::timeout(LIMIT, transcoder.start(|| child(10), "audio/ogg"))
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
                .start(|| child(10), "audio/ogg")
                .await
                .expect("the process starts");
            bodies.push(stream.body);
        }
        for body in &mut bodies {
            started(body).await;
        }
        let third = transcoder.start(|| child(10), "audio/ogg").await;
        assert!(matches!(third, Err(TranscodeError::Busy)), "{third:?}");

        // One process ends, and one job more may start.
        bodies.pop();
        let third = tokio::time::timeout(LIMIT, transcoder.start(|| child(10), "audio/ogg"))
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
            .start(|| child(0), "audio/mpeg")
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
        assert!(!transcoder.available);

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

    /// Runs real ffmpeg on the FLAC fixture and returns the first bytes of
    /// the stream of `target`.
    async fn real_transcode(target: Target, offset_s: u32) -> Vec<u8> {
        let transcoder = FfmpegTranscoder::probe(&Transcoding::default()).await;
        assert!(
            transcoder.available,
            "this test needs ffmpeg on PATH (`ffmpeg -version` did not run)"
        );
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("audio")
            .join("a.flac");
        let stream = transcoder
            .spawn(TranscodeJob {
                path: fixture,
                target,
                bitrate_kbps: 96,
                offset_s,
            })
            .await
            .expect("ffmpeg starts");
        assert_eq!(stream.content_type, target.content_type());
        let output = stream.body.collect().await.expect("the output is read");
        assert!(all_permits_back(&transcoder, 2).await);
        output.to_bytes().to_vec()
    }

    /// The one test that runs ffmpeg. It needs ffmpeg with libopus and
    /// libmp3lame on PATH, so it is not part of the default suite:
    /// `cargo test -- --ignored real_ffmpeg`.
    #[tokio::test]
    #[ignore = "runs ffmpeg, which the default suite does not need"]
    async fn real_ffmpeg_transcodes_the_flac_fixture_to_opus_and_mp3() {
        let opus = real_transcode(Target::Opus, 0).await;
        assert!(
            opus.starts_with(b"OggS"),
            "{:?}",
            &opus[..opus.len().min(8)]
        );
        assert!(
            opus.windows(8).any(|window| window == b"OpusHead"),
            "the stream holds no Opus header"
        );

        let mp3 = real_transcode(Target::Mp3, 0).await;
        assert!(
            mp3.starts_with(&[0xff, 0xfb]) || mp3.starts_with(b"ID3"),
            "{:?}",
            &mp3[..mp3.len().min(8)]
        );

        // A stream that starts inside the track is shorter. The fixture
        // lasts half a second, so the offset of one second is past its end,
        // and ffmpeg writes a stream without audio.
        let late = real_transcode(Target::Mp3, 1).await;
        assert!(late.len() < mp3.len(), "{} {}", late.len(), mp3.len());
    }
}
