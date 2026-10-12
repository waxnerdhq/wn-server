//! The transcoder of a [`TestServer`](super::TestServer). It starts no
//! process, so the suite needs no ffmpeg.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use wn_server::media::transcode::{
    TranscodeError, TranscodeFuture, TranscodeJob, TranscodeStream, Transcoder,
};

/// The bytes of a stream of a [`FakeTranscoder`] that a test gave no other
/// bytes.
pub const FAKE_TRANSCODE: &[u8] = b"the bytes of a transcode";

/// The size of the chunks in which a [`FakeTranscoder`] yields its bytes.
pub const FAKE_CHUNK: usize = 4096;

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

/// What a test set on a [`FakeTranscoder`], and the jobs that reached it.
struct State {
    bytes: Vec<u8>,
    failure: Option<FakeFailure>,
    stays_open: bool,
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
    open: Arc<AtomicUsize>,
}

impl Default for FakeTranscoder {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                bytes: FAKE_TRANSCODE.to_vec(),
                failure: None,
                stays_open: false,
                jobs: Vec::new(),
            }),
            open: Arc::new(AtomicUsize::new(0)),
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

    /// The jobs for which the transcoder started a stream, in their order.
    pub fn jobs(&self) -> Vec<TranscodeJob> {
        self.state().jobs.clone()
    }

    /// The number of streams that the transcoder started and that are not
    /// dropped.
    pub fn open_streams(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }
}

/// Counts one open stream until it is dropped.
struct OpenStream(Arc<AtomicUsize>);

impl OpenStream {
    fn new(open: &Arc<AtomicUsize>) -> Self {
        open.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(open))
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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

            let chunks: Vec<io::Result<Bytes>> = state
                .bytes
                .chunks(FAKE_CHUNK)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            let end: Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>> = if state.stays_open {
                Box::pin(stream::pending())
            } else {
                Box::pin(stream::empty())
            };
            let open = OpenStream::new(&self.open);
            // The closure owns the counter, and the stream owns the closure.
            let chunks = stream::iter(chunks).chain(end).map(move |chunk| {
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
