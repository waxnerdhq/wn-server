//! The transcoder of a [`TestServer`](super::TestServer). It starts no
//! process, so the suite needs no ffmpeg.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::{mpsc, watch};
use wn_server::media::transcode::{
    TranscodeError, TranscodeFuture, TranscodeJob, TranscodeStream, Transcoder,
};

/// The bytes of a stream of a [`FakeTranscoder`] that a test gave no other
/// bytes.
pub const FAKE_TRANSCODE: &[u8] = b"the bytes of a transcode";

/// The size of the chunks in which a [`FakeTranscoder`] yields its bytes.
pub const FAKE_CHUNK: usize = 4096;

/// A value of `transcoding.ffmpeg` that names no program: a path in the
/// directory of the audio fixtures, which holds no such file.
///
/// A test that runs `run_server` sets it. The server then probes this path
/// and starts no program, so the test does not depend on whether the
/// machine has ffmpeg, and the default suite starts no ffmpeg.
pub fn no_ffmpeg() -> String {
    super::fixtures_dir()
        .join("no-ffmpeg")
        .to_str()
        .expect("a Unicode path")
        .to_owned()
}

/// An error that a [`FakeTranscoder`] returns in place of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeFailure {
    /// [`TranscodeError::Busy`]: every transcode slot is taken.
    Busy,
    /// [`TranscodeError::Unavailable`]: the server has no ffmpeg.
    Unavailable,
    /// [`TranscodeError::Spawn`]: the process does not start.
    Spawn,
}

/// The chunks of a stream that a test feeds ([`FakeTranscoder::feed`]).
type Feed = mpsc::UnboundedReceiver<Vec<u8>>;

/// What a test set on a [`FakeTranscoder`], and the jobs that reached it.
struct State {
    bytes: Vec<u8>,
    failure: Option<FakeFailure>,
    stays_open: bool,
    known_length: bool,
    feed: Option<Feed>,
    jobs: Vec<TranscodeJob>,
}

/// A transcoder that returns the bytes a test gave it and records its jobs.
///
/// A stream yields the bytes in chunks of [`FAKE_CHUNK`] bytes and has the
/// content type of the target of its job. A job that the transcoder answers
/// with an error is not recorded, as no process starts for it.
pub struct FakeTranscoder {
    state: Mutex<State>,
    /// The number of streams that exist.
    open: Arc<watch::Sender<usize>>,
}

impl Default for FakeTranscoder {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                bytes: FAKE_TRANSCODE.to_vec(),
                failure: None,
                stays_open: false,
                known_length: false,
                feed: None,
                jobs: Vec::new(),
            }),
            open: Arc::new(watch::Sender::new(0)),
        }
    }
}

impl FakeTranscoder {
    fn state(&self) -> MutexGuard<'_, State> {
        // A test that panicked while it held the lock left the value intact.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the bytes of the streams that start from now on.
    pub fn set_bytes(&self, bytes: impl Into<Vec<u8>>) {
        self.state().bytes = bytes.into();
    }

    /// Makes every job from now on end with `failure`, or start a stream
    /// again with `None`.
    pub fn fail_with(&self, failure: Option<FakeFailure>) {
        self.state().failure = failure;
    }

    /// Makes the streams that start from now on stay open after their
    /// bytes, as the output of a process that runs does. Such a stream ends
    /// when it is dropped.
    pub fn stay_open(&self) {
        self.state().stays_open = true;
    }

    /// Makes the streams that start from now on bodies of a known length:
    /// one piece of the bytes, as `Body::from` builds it. A layer of the
    /// server adds `Content-Length` to a response with such a body, unless
    /// the handler builds the body of the response anew. Such a stream is
    /// not counted by [`FakeTranscoder::open_streams`].
    pub fn known_length(&self) {
        self.state().known_length = true;
    }

    /// Makes the next stream yield the chunks that the test sends to the
    /// returned sender, each when it is sent. The stream has no output
    /// before the first chunk is sent, as a process that has written
    /// nothing. It ends when the sender is dropped, and it stays open as
    /// long as the test holds the sender.
    pub fn feed(&self) -> mpsc::UnboundedSender<Vec<u8>> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.state().feed = Some(receiver);
        sender
    }

    /// The jobs for which the transcoder started a stream, in their order.
    pub fn jobs(&self) -> Vec<TranscodeJob> {
        self.state().jobs.clone()
    }

    /// The number of streams that the transcoder started and that are not
    /// dropped.
    pub fn open_streams(&self) -> usize {
        *self.open.borrow()
    }

    /// Waits until `count` streams exist: the streams that the transcoder
    /// started and that are not dropped. The wait ends with the start or
    /// the drop of a stream, so a test waits for the event and polls
    /// nothing.
    pub async fn streams_open(&self, count: usize) {
        let mut open = self.open.subscribe();
        open.wait_for(|open| *open == count)
            .await
            .expect("the transcoder exists");
    }
}

/// Counts one open stream until it is dropped.
struct OpenStream(Arc<watch::Sender<usize>>);

impl OpenStream {
    fn new(open: &Arc<watch::Sender<usize>>) -> Self {
        open.send_modify(|open| *open += 1);
        Self(Arc::clone(open))
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.send_modify(|open| *open -= 1);
    }
}

impl Transcoder for FakeTranscoder {
    fn spawn(&self, job: TranscodeJob) -> TranscodeFuture<'_> {
        Box::pin(async move {
            let mut state = self.state();
            match state.failure {
                Some(FakeFailure::Busy) => return Err(TranscodeError::Busy),
                Some(FakeFailure::Unavailable) => return Err(TranscodeError::Unavailable),
                Some(FakeFailure::Spawn) => {
                    let error = io::Error::new(io::ErrorKind::NotFound, "no program");
                    return Err(TranscodeError::Spawn(error));
                }
                None => {}
            }
            let content_type = job.target.content_type();
            state.jobs.push(job);
            if state.known_length {
                return Ok(TranscodeStream {
                    body: Body::from(state.bytes.clone()),
                    content_type,
                });
            }

            type Chunks = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;
            let chunks: Chunks = if let Some(mut feed) = state.feed.take() {
                Box::pin(
                    stream::poll_fn(move |cx| feed.poll_recv(cx)).map(|chunk| Ok(chunk.into())),
                )
            } else {
                let chunks: Vec<io::Result<Bytes>> = state
                    .bytes
                    .chunks(FAKE_CHUNK)
                    .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                    .collect();
                let end: Chunks = if state.stays_open {
                    Box::pin(stream::pending())
                } else {
                    Box::pin(stream::empty())
                };
                Box::pin(stream::iter(chunks).chain(end))
            };
            let open = OpenStream::new(&self.open);
            // The closure owns the counter, and the stream owns the closure.
            let chunks = chunks.map(move |chunk| {
                let _owned = &open;
                chunk
            });
            Ok(TranscodeStream {
                body: Body::from_stream(chunks),
                content_type,
            })
        })
    }
}
