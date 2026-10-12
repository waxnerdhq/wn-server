//! Cover art: resolves a cover ID to an image, resizes the image and caches
//! the resized image on disk (`docs/design.md` section 3.11).
//!
//! A cover ID is the ID of an album or of a track. The cover of an album is
//! the source that the scanner stored in `albums.cover_source`: an image
//! file, or the embedded picture of a track. The cover of a track is the
//! embedded picture of its file.
//!
//! # Values from the client
//!
//! The ID and the size of a request come from the client. Neither is part of
//! a file path. An ID that [`ids::parse`] rejects has no cover. An accepted
//! ID selects a row, and the service opens the paths that the scanner stored
//! for that row. The name of a cached file is built from the ID that the row
//! holds, the size after [`MAX_SIZE`] is applied, and the modification time
//! that the row holds.
//!
//! # Files from the music folders
//!
//! The files come from the music folders, and a file there can be damaged,
//! or put there to make the server return another file or to exhaust its
//! memory. Two rules decide what the service returns:
//!
//! - An image file is read when its path names a regular file. A path that
//!   names a symbolic link, a directory or a device is not read. The same
//!   rule holds for a file of the cache.
//! - Bytes are returned only as an image. A cover without a size is returned
//!   when its bytes start with the header of a JPEG, PNG or WebP image that
//!   the decoder of the format accepts, and its content type is the type of
//!   that format. Any other bytes are not returned.
//!
//! # Limits
//!
//! - A source image holds at most [`MAX_SOURCE_BYTES`] bytes.
//! - An image is decoded when its width and its height are at most
//!   [`MAX_SOURCE_SIDE`] pixels and its decoded pixels take at most
//!   [`MAX_DECODE_BYTES`] bytes. The decoder reads both from the header of
//!   the image and refuses the image before it allocates the pixels.
//! - A WebP image is decoded when it is lossy and in the simple file form.
//!   The memory that the decoder takes for the other forms does not depend
//!   on the size of the image alone.
//! - The service holds the source of one image at a time
//!   ([`DECODE_PERMITS`]), to verify it or to decode it.
//! - A resized cover is made once: the requests that ask for it while it is
//!   being made wait for it and share the result.
//! - The making of a cover that waits for its turn is dropped when no
//!   request waits for the cover any more.
//!
//! `docs/design.md` section 3.11 derives the memory that the service takes
//! at most from these limits.
//!
//! Reading, verifying, decoding and encoding run on blocking threads.

use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageError, ImageFormat, ImageReader, Limits};
use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::db::queries::tracks::Track;
use crate::db::queries::{browse, tracks};
use crate::db::{self, Db, DbError};
use crate::ids::{self, Kind};
use crate::library::scanner::EMBEDDED;
use crate::library::tags;

/// The largest size of a resized cover: the length of its longer side in
/// pixels. A request for a larger size is served at this size.
pub const MAX_SIZE: u32 = 2000;

/// The largest source image that the service reads, in bytes. A larger image
/// has no cover, resized or not, because the service holds a cover in
/// memory. It is the size that lofty allows for one item of a tag, so an
/// image file has the limit of an embedded picture.
pub const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// The largest width and the largest height of an image that the service
/// decodes, in pixels.
pub const MAX_SOURCE_SIDE: u32 = 4096;

/// The most memory that the decoded pixels of one image take, in bytes. An
/// image of [`MAX_SOURCE_SIDE`] pixels on each side with four bytes for
/// each pixel takes this much.
pub const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;

/// The number of source images that the service holds at one time, to
/// verify or to decode them. With the limits above it bounds the memory
/// that the service takes for its work (`docs/design.md` section 3.11).
pub const DECODE_PERMITS: usize = 1;

/// The quality of a resized cover, on the JPEG scale of 1 to 100.
const JPEG_QUALITY: u8 = 85;

/// The content type of a resized cover.
const JPEG: &str = "image/jpeg";

/// The extensions of a cover image file, as the scanner accepts them.
const IMAGE_EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// The FourCC of the first chunk of a WebP file in the simple lossy form.
const WEBP_LOSSY_CHUNK: &[u8] = b"VP8 ";

/// A cover image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    /// The image file.
    pub bytes: Bytes,
    /// The content type of `bytes`: `image/jpeg`, `image/png` or
    /// `image/webp`.
    pub content_type: &'static str,
    /// The modification time of the source of the cover in Unix seconds, as
    /// the last scan stored it: the time of the image file, or of the audio
    /// file that holds the picture.
    pub mtime: i64,
}

/// An error of [`Covers::get`].
#[derive(Debug, thiserror::Error)]
pub enum CoverError {
    /// The database cannot be read.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The work on the cover ended without a result: the runtime shut down,
    /// or the task that made the cover ended early.
    #[error("the work on the cover ended without a result")]
    Unfinished,
}

/// The cover art service. Clones share the cache and the work in progress.
#[derive(Debug, Clone)]
pub struct Covers {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// The directory of the resized covers.
    cache_dir: PathBuf,
    db: Db,
    /// One permit for each source image that the service may hold.
    decode_permits: Arc<Semaphore>,
    /// The resized covers that are being made, by the name of their cache
    /// file.
    in_progress: Mutex<HashMap<String, Arc<watch::Sender<Progress>>>>,
    /// The number of images that the service started to decode.
    decodes: AtomicU64,
}

/// The result of the making of one resized cover: the JPEG, or `None` when
/// the source has no image that the service can read and decode.
type Outcome = Option<Bytes>;

/// The state of the making of one resized cover.
#[derive(Debug, Clone)]
enum Progress {
    Running,
    Done(Outcome),
}

/// What a cover ID resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    /// The cover ID, read from the row that the ID of the request selected.
    cover_id: String,
    /// The modification time that the row holds for the cover.
    mtime: i64,
    origin: Origin,
}

/// Where the image of a cover is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// An image file.
    File(PathBuf),
    /// The embedded picture of the audio file at the path.
    Embedded(PathBuf),
}

impl Origin {
    /// The file that holds the image.
    fn path(&self) -> &Path {
        match self {
            Origin::File(path) | Origin::Embedded(path) => path,
        }
    }
}

/// Why a file cannot be read.
#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("it is a symbolic link or another file that is not a regular file")]
    NotAFile,
    #[error("it holds {0} bytes and the limit is {MAX_SOURCE_BYTES}")]
    TooLarge(u64),
}

/// Why bytes are not served or not decoded as an image.
#[derive(Debug, thiserror::Error)]
enum ImageRefusal {
    #[error("it is not a JPEG, PNG or WebP image")]
    Format,
    #[error("it is a WebP image that is lossless or in the extended file form")]
    WebpForm,
    #[error(transparent)]
    Image(#[from] ImageError),
}

/// One resized cover to make.
#[derive(Debug)]
struct Job {
    cover_id: String,
    origin: Origin,
    /// The length of the longer side in pixels, 1 to [`MAX_SIZE`].
    size: u32,
    /// The name of the cache file.
    name: String,
}

/// The entry of a job in the covers in progress. Dropping the value removes
/// the entry, which also happens when the task of the job ends early.
struct Registration {
    inner: Arc<Inner>,
    /// The name of the cache file, which is the key of the entry.
    name: String,
    /// The sender of the entry. The requests that wait for the job hold its
    /// receivers.
    progress: Arc<watch::Sender<Progress>>,
}

impl Registration {
    /// Removes the entry when no request waits for the job, and returns
    /// whether it did.
    ///
    /// A request subscribes to a job while it holds the lock of the covers
    /// in progress, and this function counts the requests under the same
    /// lock. A request therefore either waits for this job, which then goes
    /// on, or finds no entry and starts a job of its own.
    fn abandon(&self) -> bool {
        let mut in_progress = self.inner.in_progress();
        if self.progress.receiver_count() > 0 {
            return false;
        }
        in_progress.remove(&self.name);
        true
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut in_progress = self.inner.in_progress();
        // After `abandon` the name can be the key of a later job.
        let own = in_progress
            .get(&self.name)
            .is_some_and(|progress| Arc::ptr_eq(progress, &self.progress));
        if own {
            in_progress.remove(&self.name);
        }
    }
}

impl Covers {
    /// Returns the service that caches resized covers in `cache_dir` and
    /// reads the library from `db`. `cache_dir` is `cache/covers` of the data
    /// directory. The service creates the directory when it caches the first
    /// cover.
    pub fn new(cache_dir: impl Into<PathBuf>, db: Db) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache_dir: cache_dir.into(),
                db,
                decode_permits: Arc::new(Semaphore::new(DECODE_PERMITS)),
                in_progress: Mutex::default(),
                decodes: AtomicU64::new(0),
            }),
        }
    }

    /// Returns the cover with the ID `id`, which is the ID of an album or of
    /// a track.
    ///
    /// Without `size`, and with a `size` of 0, the result is the image as its
    /// source stores it: the bytes of the image file or of the embedded
    /// picture. The header of the image is verified and the image is not
    /// decoded. The content type is the type of the format that the header
    /// has: `image/jpeg`, `image/png` or `image/webp`.
    ///
    /// With another `size` the result is a JPEG whose longer side is at most
    /// `size` pixels, and at most [`MAX_SIZE`] pixels when `size` is larger.
    /// An image whose sides are within the size keeps its size: no image is
    /// enlarged. The resized cover is read from the cache directory, and
    /// made and stored there when the cache does not hold it. Requests that
    /// ask for the same resized cover while it is being made share one
    /// result, and the cover is made once. A cover whose image is being
    /// decoded is finished and cached also when every request for it has
    /// ended. A cover that still waits for its turn is dropped then.
    ///
    /// Returns `None` in these cases:
    ///
    /// - `id` is not the ID of an album or of a track in the form that
    ///   [`ids::parse`] accepts, or no row has the ID.
    /// - The album has no cover. The track has no embedded picture or no
    ///   file.
    /// - The source is gone, cannot be read or is larger than
    ///   [`MAX_SOURCE_BYTES`]. The image file is a symbolic link or another
    ///   file that is not a regular file.
    /// - The bytes of the source do not start with the header of a JPEG,
    ///   PNG or WebP image. The service logs a warning.
    /// - A size is given and the image cannot be decoded: it is damaged, it
    ///   is over [`MAX_SOURCE_SIDE`] or [`MAX_DECODE_BYTES`], or it is a
    ///   WebP image that is lossless or in the extended file form. The
    ///   service logs a warning.
    ///
    /// A resized cover that cannot be written to the cache is returned
    /// without being cached, and the service logs a warning.
    ///
    /// # Errors
    ///
    /// Returns [`CoverError::Db`] when the database cannot be read and
    /// [`CoverError::Unfinished`] when the runtime shuts down.
    pub async fn get(&self, id: &str, size: Option<u32>) -> Result<Option<Cover>, CoverError> {
        let Some((kind, _)) = ids::parse(id) else {
            return Ok(None);
        };
        let id = id.to_owned();
        let source = self
            .inner
            .db
            .read(move |conn| resolve(conn, kind, &id))
            .await?;
        let Some(source) = source else {
            return Ok(None);
        };
        match effective_size(size) {
            None => self.original(source).await,
            Some(size) => self.resized(source, size).await,
        }
    }

    /// Returns the image of `source` as its source stores it, when its
    /// header is the header of an image.
    ///
    /// The request waits for a decode permit, because the service holds the
    /// source of one image at a time. A request that ends while it waits
    /// leaves the queue.
    async fn original(&self, source: Source) -> Result<Option<Cover>, CoverError> {
        let Source {
            cover_id,
            mtime,
            origin,
        } = source;
        let permit = Arc::clone(&self.inner.decode_permits)
            .acquire_owned()
            .await
            .map_err(|_| CoverError::Unfinished)?;
        let verified = run_blocking(move || {
            // The permit is held until the bytes are verified. The copies
            // that the verification makes are freed by then.
            let _permit = permit;
            let bytes = load(&cover_id, &origin)?;
            match verify(&bytes) {
                Ok(format) => Some((bytes, format.to_mime_type())),
                Err(error) => {
                    tracing::warn!(
                        cover = cover_id,
                        path = %origin.path().display(),
                        %error,
                        "the source of the cover is not an image that the service serves; \
                         the cover is not served"
                    );
                    None
                }
            }
        })
        .await?
        .flatten();
        Ok(verified.map(|(bytes, content_type)| Cover {
            bytes: Bytes::from(bytes),
            content_type,
            mtime,
        }))
    }

    /// Returns the image of `source` as a JPEG whose longer side is at most
    /// `size` pixels, from the cache or made now.
    async fn resized(&self, source: Source, size: u32) -> Result<Option<Cover>, CoverError> {
        let Source {
            cover_id,
            mtime,
            origin,
        } = source;
        let name = cache_name(&cover_id, size, mtime);
        let path = self.inner.cache_dir.join(&name);
        let cached = run_blocking(move || read_cached(&path)).await?.flatten();
        let bytes = match cached {
            Some(bytes) => Some(Bytes::from(bytes)),
            None => {
                let job = Job {
                    cover_id,
                    origin,
                    size,
                    name,
                };
                let mut progress = self.join_or_start(job);
                let state = progress
                    .wait_for(|state| matches!(state, Progress::Done(_)))
                    .await
                    .map_err(|_| CoverError::Unfinished)?;
                match &*state {
                    Progress::Done(bytes) => bytes.clone(),
                    Progress::Running => None,
                }
            }
        };
        Ok(bytes.map(|bytes| Cover {
            bytes,
            content_type: JPEG,
            mtime,
        }))
    }

    /// Returns a receiver of the progress of the job that makes the cache
    /// file of `job`. Starts `job` when no such job runs.
    ///
    /// The job runs in a task of its own, so that it does not end with the
    /// request that started it while other requests wait for it.
    fn join_or_start(&self, job: Job) -> watch::Receiver<Progress> {
        let mut in_progress = self.inner.in_progress();
        if let Some(progress) = in_progress.get(&job.name) {
            return progress.subscribe();
        }
        let progress = Arc::new(watch::Sender::new(Progress::Running));
        let receiver = progress.subscribe();
        in_progress.insert(job.name.clone(), Arc::clone(&progress));
        drop(in_progress);

        // The task owns the registration from the start, so the entry is
        // removed also when the runtime drops the task before it runs.
        let registration = Registration {
            inner: Arc::clone(&self.inner),
            name: job.name.clone(),
            progress,
        };
        tokio::spawn(async move {
            // A job that stops without an outcome sends nothing. When the
            // job was not abandoned, the requests that wait find its sender
            // dropped and return `CoverError::Unfinished`.
            if let Some(outcome) = registration.inner.make(job, &registration).await {
                registration.progress.send_replace(Progress::Done(outcome));
            }
        });
        receiver
    }

    /// The number of images that the service started to decode.
    #[cfg(test)]
    fn decodes(&self) -> u64 {
        self.inner.decodes.load(Ordering::Relaxed)
    }

    /// The number of requests that wait for the job of the cache file
    /// `name`. It is 0 when no such job runs.
    #[cfg(test)]
    fn waiting(&self, name: &str) -> usize {
        self.inner
            .in_progress()
            .get(name)
            .map_or(0, |progress| progress.receiver_count())
    }
}

impl Inner {
    /// Locks the covers in progress. The code that holds the lock changes
    /// the map with single calls, so the map of a poisoned lock is intact.
    fn in_progress(&self) -> MutexGuard<'_, HashMap<String, Arc<watch::Sender<Progress>>>> {
        self.in_progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the resized cover of `job` and stores it in the cache.
    ///
    /// Returns `None` when the job stops without an outcome: no request
    /// waits for it any more while it waits for a decode permit, or the
    /// runtime shuts down.
    async fn make(self: &Arc<Self>, job: Job, registration: &Registration) -> Option<Outcome> {
        // A job that ended after the request looked in the cache has written
        // the file. The request that then starts this job finds it here.
        let path = self.cache_dir.join(&job.name);
        let cached = run_blocking(move || read_cached(&path)).await.ok()?;
        if let Some(bytes) = cached.flatten() {
            return Some(Some(Bytes::from(bytes)));
        }

        let permit = self.decode_permit(registration).await?;
        let inner = Arc::clone(self);
        // The blocking thread owns the permit. A blocking task runs to its
        // end also when the runtime drops this future, and the permit is
        // held for as long as the source and the pixels are in memory.
        let jpeg = run_blocking(move || {
            let jpeg = {
                let _permit = permit;
                inner.resize(&job)?
            };
            inner.store(&job.name, &jpeg);
            Some(jpeg)
        });
        Some(jpeg.await.ok()?.flatten().map(Bytes::from))
    }

    /// Waits for a decode permit for the job of `registration`.
    ///
    /// Returns `None` when no request waits for the job any more. The entry
    /// of the job is removed then, and the job decodes nothing. A client
    /// that ends its requests therefore leaves no work behind in the queue.
    async fn decode_permit(&self, registration: &Registration) -> Option<OwnedSemaphorePermit> {
        loop {
            tokio::select! {
                biased;
                () = registration.progress.closed() => {
                    if registration.abandon() {
                        return None;
                    }
                    // A request subscribed after the last one left.
                }
                permit = Arc::clone(&self.decode_permits).acquire_owned() => {
                    return permit.ok();
                }
            }
        }
    }

    /// Reads the image of `job` and returns it as a JPEG of the size of the
    /// job. Returns `None`, with a warning, when the image cannot be decoded.
    fn resize(&self, job: &Job) -> Option<Vec<u8>> {
        let bytes = load(&job.cover_id, &job.origin)?;
        self.decodes.fetch_add(1, Ordering::Relaxed);
        match resize(bytes, job.size, decode_limits()) {
            Ok(jpeg) => Some(jpeg),
            Err(error) => {
                tracing::warn!(
                    cover = %job.cover_id,
                    path = %job.origin.path().display(),
                    %error,
                    "the cover image cannot be decoded; no resized cover is served"
                );
                None
            }
        }
    }

    /// Writes `jpeg` to the cache file `name`. A failure is logged, and the
    /// request is answered with the cover all the same.
    fn store(&self, name: &str, jpeg: &[u8]) {
        if let Err(error) = write_cache_file(&self.cache_dir, name, jpeg) {
            tracing::warn!(
                path = %self.cache_dir.join(name).display(),
                %error,
                "the resized cover cannot be written to the cache"
            );
        }
    }
}

/// Returns the size that a request for `size` is served at: `None` for a
/// request without a size and for the size 0, which both ask for the image
/// as it is stored, and at most [`MAX_SIZE`] otherwise.
fn effective_size(size: Option<u32>) -> Option<u32> {
    size.filter(|size| *size != 0)
        .map(|size| size.min(MAX_SIZE))
}

/// Returns the name of the cache file of a resized cover:
/// `<cover ID>-<size>-<mtime>.jpg` (`docs/design.md` section 3.5). The
/// caller passes the ID and the modification time that a row holds and a
/// size that [`effective_size`] returned.
fn cache_name(cover_id: &str, size: u32, mtime: i64) -> String {
    format!("{cover_id}-{size}-{mtime}.jpg")
}

/// Returns what the cover ID `id` of the kind `kind` resolves to, or `None`
/// when the ID has no cover.
fn resolve(conn: &Connection, kind: Kind, id: &str) -> db::Result<Option<Source>> {
    match kind {
        Kind::Album => album_source(conn, id),
        Kind::Track => Ok(picture_file(conn, id)?.map(|(track, path)| Source {
            cover_id: track.id,
            mtime: track.mtime,
            origin: Origin::Embedded(path),
        })),
        Kind::Artist | Kind::Playlist => Ok(None),
    }
}

/// Returns the cover that the last scan stored for the album `album_id`.
fn album_source(conn: &Connection, album_id: &str) -> db::Result<Option<Source>> {
    let Some(album) = browse::album(conn, album_id)? else {
        return Ok(None);
    };
    let (Some(source), Some(mtime)) = (album.cover_source, album.cover_mtime) else {
        return Ok(None);
    };
    let origin = match source.strip_prefix(EMBEDDED) {
        Some(track_id) => {
            if !matches!(ids::parse(track_id), Some((Kind::Track, _))) {
                tracing::warn!(
                    album = %album.id,
                    "the stored cover of the album names no track; the album has no cover"
                );
                return Ok(None);
            }
            match picture_file(conn, track_id)? {
                Some((_, path)) => Origin::Embedded(path),
                None => return Ok(None),
            }
        }
        None => {
            let path = PathBuf::from(&source);
            if !path.is_absolute() || !has_image_extension(&path) {
                tracing::warn!(
                    album = %album.id,
                    path = %path.display(),
                    "the stored cover of the album is not an absolute path of an image file; \
                     the album has no cover"
                );
                return Ok(None);
            }
            Origin::File(path)
        }
    };
    Ok(Some(Source {
        cover_id: album.id,
        mtime,
        origin,
    }))
}

/// Returns the track `track_id` and the path of its file when the track has
/// a file and an embedded picture.
///
/// A track without a file has a placeholder path as its `rel_path`
/// (`docs/design.md` section 3.4). `browse::file_path` returns `None` for
/// such a track before it builds a path, so no file is read for it.
fn picture_file(conn: &Connection, track_id: &str) -> db::Result<Option<(Track, PathBuf)>> {
    let Some(track) = tracks::get_track(conn, track_id)? else {
        return Ok(None);
    };
    if !track.has_picture {
        return Ok(None);
    }
    Ok(browse::file_path(conn, &track)?.map(|path| (track, path)))
}

/// Returns whether the extension of `path` is one of [`IMAGE_EXTENSIONS`],
/// without regard to case.
fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// Reads the bytes of the image at `origin`. Returns `None` when there is no
/// image to serve: the track has no picture, the file is gone, or the image
/// cannot be read. The last case is logged as a warning.
fn load(cover_id: &str, origin: &Origin) -> Option<Vec<u8>> {
    match read_origin(origin) {
        Ok(bytes) => bytes,
        Err(LoadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(
                cover = cover_id,
                path = %origin.path().display(),
                "the source of the cover is gone"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                cover = cover_id,
                path = %origin.path().display(),
                %error,
                "the source of the cover cannot be read; the cover is not served"
            );
            None
        }
    }
}

/// Reads the bytes of the image at `origin`. Returns `None` when `origin` is
/// an audio file without an embedded picture.
///
/// An image file is read by [`read_regular_file`], which follows no
/// symbolic link. An audio file is read through a link, as the scanner reads
/// it (`docs/design.md` section 1.3): the result is the picture that the tag
/// reader finds in the tag of the file, and never the bytes of the file.
fn read_origin(origin: &Origin) -> Result<Option<Vec<u8>>, LoadError> {
    match origin {
        Origin::File(path) => read_regular_file(path).map(Some),
        Origin::Embedded(path) => {
            // Opening a file that is not a regular file can block: a named
            // pipe without a writer never opens.
            if !fs::metadata(path)?.is_file() {
                return Err(LoadError::NotAFile);
            }
            let Some((bytes, _)) = tags::read_front_picture(path) else {
                return Ok(None);
            };
            let len = bytes.len() as u64;
            if len > MAX_SOURCE_BYTES {
                return Err(LoadError::TooLarge(len));
            }
            Ok(Some(bytes))
        }
    }
}

/// Reads the file at `path` when the path names a regular file of at most
/// [`MAX_SOURCE_BYTES`] bytes.
///
/// A symbolic link at `path` is not followed: the state of the path is read
/// without following a link, and a link is not a regular file. The file
/// that is then opened must be the file whose state was read, so a path
/// that is changed to a link between the two steps is not read either. A
/// directory in the path can be a link.
fn read_regular_file(path: &Path) -> Result<Vec<u8>, LoadError> {
    let named = fs::symlink_metadata(path)?;
    if !named.is_file() {
        return Err(LoadError::NotAFile);
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(&named, &opened) {
        return Err(LoadError::NotAFile);
    }
    if opened.len() > MAX_SOURCE_BYTES {
        return Err(LoadError::TooLarge(opened.len()));
    }
    // The file can grow after its size was read, so the read itself stops
    // one byte past the limit.
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    file.take(MAX_SOURCE_BYTES + 1).read_to_end(&mut bytes)?;
    let len = bytes.len() as u64;
    if len > MAX_SOURCE_BYTES {
        return Err(LoadError::TooLarge(len));
    }
    Ok(bytes)
}

/// Returns whether `named`, the state of a path, and `opened`, the state of
/// an open file, are the states of one file: the same device and the same
/// inode.
#[cfg(unix)]
fn same_file(named: &Metadata, opened: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    named.dev() == opened.dev() && named.ino() == opened.ino()
}

/// The standard library gives no identity of a file on Windows, so the test
/// of the path before the file is opened is the only one there. Creating a
/// symbolic link on Windows needs a privilege that an account does not have
/// by default.
#[cfg(not(unix))]
fn same_file(_named: &Metadata, _opened: &Metadata) -> bool {
    true
}

/// Reads the cache file at `path`. Returns `None` when the cache does not
/// hold the file, and when the file cannot be read, which is logged. A
/// symbolic link in the place of a cache file is not read.
fn read_cached(path: &Path) -> Option<Vec<u8>> {
    match read_regular_file(path) {
        Ok(bytes) => Some(bytes),
        Err(LoadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "the cached cover cannot be read; the cover is made again"
            );
            None
        }
    }
}

/// Returns the format of the image in `bytes` when the bytes start with the
/// header of a JPEG, PNG or WebP image that the decoder of the format
/// accepts.
///
/// The decoder reads the header and no pixel. The service returns no bytes
/// of a source that this function refuses, so a file that is no image is
/// not returned under the name of a cover.
fn verify(bytes: &[u8]) -> Result<ImageFormat, ImageRefusal> {
    let mut reader = guessed(bytes)?;
    let format = match reader.format() {
        Some(format @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)) => format,
        _ => return Err(ImageRefusal::Format),
    };
    reader.limits(header_limits());
    reader.into_dimensions()?;
    Ok(format)
}

/// Returns a reader of the image in `bytes` whose format is detected from
/// the first bytes.
fn guessed(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, ImageRefusal> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| ImageRefusal::Image(ImageError::IoError(error)))
}

/// The limits of [`verify`]: the memory that a decoder takes while it reads
/// a header is at most [`MAX_DECODE_BYTES`]. The size of the image has no
/// limit, because the image is not decoded.
fn header_limits() -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

/// The limits of [`decode`] for a cover: [`MAX_SOURCE_SIDE`] and
/// [`MAX_DECODE_BYTES`].
fn decode_limits() -> Limits {
    let mut limits = header_limits();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits
}

/// Decodes the image in `bytes`, which is a JPEG, a PNG or a lossy WebP in
/// the simple file form. The format is detected from the content.
///
/// The decoder reads the width and the height from the header of the image.
/// It returns an error before it allocates the pixels when a side is over
/// the limit of `limits`, and when the decoded image is larger than
/// `limits.max_alloc`.
///
/// A WebP image that is lossless, or that is in the extended file form,
/// which holds an alpha channel, an animation or metadata, is refused. The
/// decoder builds tables and planes for these forms whose size the limits
/// do not bound.
fn decode(bytes: &[u8], limits: Limits) -> Result<DynamicImage, ImageRefusal> {
    let mut reader = guessed(bytes)?;
    match reader.format() {
        Some(ImageFormat::Jpeg | ImageFormat::Png) => {}
        Some(ImageFormat::WebP) if is_simple_lossy_webp(bytes) => {}
        Some(ImageFormat::WebP) => return Err(ImageRefusal::WebpForm),
        _ => return Err(ImageRefusal::Format),
    }
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// Returns whether `bytes`, which start as a WebP file, are in the simple
/// lossy form: the first chunk of the file is the lossy image. The decoder
/// then reads that chunk and no other.
fn is_simple_lossy_webp(bytes: &[u8]) -> bool {
    // The header of the file is `RIFF`, four bytes of length and `WEBP`.
    bytes.get(12..16) == Some(WEBP_LOSSY_CHUNK)
}

/// Decodes the image in `bytes` within `limits` and returns it as a JPEG
/// whose longer side is at most `size` pixels. An image with a longer side
/// is scaled down to `size` in its proportions, and its shorter side is at
/// least one pixel. An image whose sides are within `size` keeps its size.
/// An image with an alpha channel loses the channel, because JPEG has none.
fn resize(bytes: Vec<u8>, size: u32, limits: Limits) -> Result<Vec<u8>, ImageRefusal> {
    let image = decode(&bytes, limits)?;
    // Each buffer is freed when the next one is made, so the source, the
    // pixels and the resized pixels are not all in memory at one time.
    drop(bytes);
    let image = if image.width() > size || image.height() > size {
        let smaller = image.thumbnail(size, size);
        drop(image);
        smaller
    } else {
        // `thumbnail` enlarges a smaller image to the size.
        image
    };
    let pixels = image.into_rgb8();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).encode_image(&pixels)?;
    Ok(jpeg)
}

/// Writes `bytes` to the file `name` in the directory `cache_dir`, which is
/// created when it is missing.
///
/// The bytes are written to a new file with a random name in the same
/// directory, flushed to the disk, and the file is then renamed to `name`.
/// A reader therefore opens either no file or the whole file. The file is
/// flushed before the rename, so that a power failure leaves no file with
/// the final name and without its bytes. The directory is not flushed: a
/// power failure can undo the rename, and the cover is then made again.
fn write_cache_file(cache_dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(cache_dir)?;
    let temp = cache_dir.join(temp_name());
    let written = write_new_file(&temp, bytes);
    let renamed = written.and_then(|()| fs::rename(&temp, cache_dir.join(name)));
    settle_rename(renamed, &temp, &cache_dir.join(name))
}

/// Creates the file at `path`, writes `bytes` to it and flushes it to the
/// disk. A file that exists at `path` is an error and stays as it is.
fn write_new_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Returns the result of writing the cache file `dest` through the
/// temporary file `temp`, given `renamed`, the result of the write and the
/// rename.
///
/// A failure counts as success when `dest` exists. On Windows a rename
/// fails while another process reads the destination, and the destination
/// then holds the same cover (`docs/design.md` section 2). After a failure
/// the temporary file is removed.
fn settle_rename(renamed: io::Result<()>, temp: &Path, dest: &Path) -> io::Result<()> {
    let Err(error) = renamed else {
        return Ok(());
    };
    // The temporary file may not exist, when creating it was the failure.
    let _ = fs::remove_file(temp);
    if dest.is_file() { Ok(()) } else { Err(error) }
}

/// Returns a new name for a temporary file in the cache directory: `tmp.`
/// and 32 random hex characters.
///
/// The scanner removes the files of the cache whose name starts with the ID
/// of an album and a hyphen (`docs/design.md` section 3.10). This name has no
/// hyphen, so it starts with no cover ID.
fn temp_name() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    format!("tmp.{}", hex::encode(bytes))
}

/// Runs `f` on a blocking thread.
///
/// Returns `None` when `f` panicked. A tag reader or an image decoder that
/// panics on a damaged file then ends one request and not the server.
async fn run_blocking<T, F>(f: F) -> Result<Option<T>, CoverError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.is_panic() => {
            tracing::warn!("reading a cover panicked; the cover is not served");
            Ok(None)
        }
        Err(_) => Err(CoverError::Unfinished),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use image::{Luma, LumaA, Rgb, Rgba};
    use tempfile::TempDir;

    use super::*;
    use crate::db::queries::tracks::TrackContent;
    use crate::db::queries::{albums, artists, folders};
    use crate::library::tags::TrackMeta;

    /// The modification time of the covers that the tests store.
    const MTIME: i64 = 1_700_000_000;

    /// A library in a temporary directory: a music folder, a memory database
    /// that holds the folder and one artist, and the cover service. The cache
    /// directory, `cache/covers`, does not exist until the service writes to
    /// it.
    struct Library {
        dir: TempDir,
        db: Db,
        covers: Covers,
        folder_id: i64,
        artist_id: String,
    }

    impl Library {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let music = dir.path().join("music");
            fs::create_dir(&music).expect("create the music folder");
            let db = Db::open_memory().expect("a memory database");
            let (folder_id, artist_id) = db
                .write(move |conn| {
                    let folder = folders::sync_folders(conn, &[music])?.remove(0);
                    Ok((folder.id, artists::upsert_by_key(conn, "Aurora Field")?))
                })
                .await
                .expect("insert the folder and the artist");
            let covers = Covers::new(dir.path().join("cache").join("covers"), db.clone());
            Self {
                dir,
                db,
                covers,
                folder_id,
                artist_id,
            }
        }

        fn music(&self) -> PathBuf {
            self.dir.path().join("music")
        }

        fn cache_dir(&self) -> PathBuf {
            self.dir.path().join("cache").join("covers")
        }

        /// Inserts an album without a cover and returns its ID.
        async fn album(&self, title: &str) -> String {
            let (artist_id, title) = (self.artist_id.clone(), title.to_owned());
            self.db
                .write(move |conn| {
                    albums::upsert_by_key(conn, &artist_id, &title, "Aurora Field", false)
                })
                .await
                .expect("insert the album")
        }

        /// Stores `source` and `mtime` as the cover of the album `album_id`.
        async fn set_cover(&self, album_id: &str, source: &str, mtime: i64) {
            let (album_id, source) = (album_id.to_owned(), source.to_owned());
            self.db
                .write(move |conn| albums::set_cover(conn, &album_id, Some(&source), Some(mtime)))
                .await
                .expect("store the cover");
        }

        /// Writes `bytes` to the file `<title>/<file>` in the music folder
        /// and inserts the album `title` with that file as its cover and
        /// [`MTIME`] as the time of the cover. Returns the ID of the album
        /// and the path of the file.
        async fn album_with_cover(
            &self,
            title: &str,
            file: &str,
            bytes: &[u8],
        ) -> (String, PathBuf) {
            let dir = self.music().join(title);
            fs::create_dir(&dir).expect("create the album directory");
            let path = dir.join(file);
            fs::write(&path, bytes).expect("write the cover file");
            let id = self.album(title).await;
            self.set_cover(&id, path.to_str().expect("a Unicode path"), MTIME)
                .await;
            (id, path)
        }

        /// Inserts a track of the album `album_id` at `rel_path` with the
        /// modification time `mtime` and returns its ID. No file is written.
        async fn track(
            &self,
            album_id: &str,
            rel_path: &str,
            has_picture: bool,
            mtime: i64,
        ) -> String {
            let id = ids::new_id(Kind::Track);
            let meta = TrackMeta {
                title: rel_path.to_owned(),
                artist: "Aurora Field".to_owned(),
                album_artist: "Aurora Field".to_owned(),
                album: "Album".to_owned(),
                track_no: None,
                disc_no: None,
                year: None,
                genre: None,
                compilation: false,
                duration_ms: 1000,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                has_picture,
                has_lyrics: false,
                suffix: "flac".to_owned(),
                content_type: "audio/flac".to_owned(),
                lossless: true,
            };
            let (track_id, folder_id) = (id.clone(), self.folder_id);
            let (artist_id, album_id) = (self.artist_id.clone(), album_id.to_owned());
            let rel_path = rel_path.to_owned();
            self.db
                .write(move |conn| {
                    let content = TrackContent {
                        fingerprint: "100:aa",
                        mtime,
                        size: 100,
                        meta: &meta,
                        artist_id: &artist_id,
                        album_id: &album_id,
                    };
                    tracks::insert_track(conn, &track_id, folder_id, &rel_path, &content)
                })
                .await
                .expect("insert the track");
            id
        }

        /// Gives the track `id` its placeholder path, so that it has no
        /// file.
        async fn detach(&self, id: &str) {
            let (id, folder_id) = (id.to_owned(), self.folder_id);
            self.db
                .write(move |conn| {
                    let placeholder = tracks::placeholder_path(&id);
                    tracks::relink_track(conn, &id, folder_id, &placeholder, 100, 100)
                })
                .await
                .expect("detach the track");
        }

        /// Returns what the cover ID `id` resolves to.
        async fn resolve(&self, id: &str) -> Option<Source> {
            let (kind, _) = ids::parse(id).expect("a well-formed ID");
            let id = id.to_owned();
            self.db
                .read(move |conn| resolve(conn, kind, &id))
                .await
                .expect("read the database")
        }

        async fn get(&self, id: &str, size: Option<u32>) -> Option<Cover> {
            self.covers.get(id, size).await.expect("no error")
        }

        /// Returns the names of the files in the cache directory in order.
        /// A directory that does not exist holds no file.
        fn cache_files(&self) -> Vec<String> {
            let Ok(entries) = fs::read_dir(self.cache_dir()) else {
                return Vec::new();
            };
            let mut names: Vec<String> = entries
                .map(|entry| entry.expect("a directory entry").file_name())
                .map(|name| name.into_string().expect("a Unicode name"))
                .collect();
            names.sort();
            names
        }
    }

    /// Returns `image` in the encoding `format`.
    fn encoded(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image
            .into()
            .write_to(&mut bytes, format)
            .expect("encode the image");
        bytes.into_inner()
    }

    /// Returns an 8-bit RGB image of `width` × `height` pixels whose colour
    /// changes from pixel to pixel.
    fn picture(width: u32, height: u32) -> image::RgbImage {
        image::RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        })
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        encoded(picture(width, height), ImageFormat::Png)
    }

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        encoded(picture(width, height), ImageFormat::Jpeg)
    }

    /// Returns a JPEG whose header declares 65535 × 65535 pixels, the
    /// largest size of the format. The file holds the data of 8 × 8 pixels,
    /// so it is a few hundred bytes long.
    fn jpeg_that_declares_the_largest_size() -> Vec<u8> {
        let mut bytes = jpeg(8, 8);
        // The frame header starts with the marker FF C0. Two length bytes
        // and the precision byte follow, then the height and the width.
        let marker = bytes
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .expect("a baseline frame header");
        bytes[marker + 5..marker + 9].fill(0xff);
        bytes
    }

    /// A lossy WebP image of 8 × 4 pixels in the simple file form. The
    /// `image` crate writes lossless WebP only, so the bytes are those that
    /// `cwebp -q 20` wrote for a gradient.
    const LOSSY_WEBP: [u8; 72] = [
        0x52, 0x49, 0x46, 0x46, 0x40, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38,
        0x20, 0x34, 0x00, 0x00, 0x00, 0xf0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x08, 0x00, 0x04, 0x00,
        0x05, 0x40, 0x7c, 0x25, 0xa8, 0x02, 0x74, 0x6b, 0x80, 0xb7, 0xff, 0x05, 0x3c, 0x00, 0x00,
        0xfe, 0xe9, 0xfc, 0xb7, 0xa9, 0x4b, 0x83, 0xa8, 0x3e, 0x53, 0x3d, 0x75, 0x38, 0xe9, 0x28,
        0x3e, 0x30, 0x6c, 0x90, 0xba, 0xdc, 0x27, 0xb4, 0xb5, 0x00, 0x00, 0x00,
    ];

    /// Returns [`LOSSY_WEBP`] in the extended file form: the file starts
    /// with a `VP8X` chunk that sets no flag, and the lossy image follows.
    fn extended_webp() -> Vec<u8> {
        let lossy_chunk = &LOSSY_WEBP[12..];
        let mut bytes = b"RIFF".to_vec();
        bytes.extend((4 + 18 + lossy_chunk.len() as u32).to_le_bytes());
        bytes.extend(b"WEBPVP8X");
        bytes.extend(10u32.to_le_bytes());
        // The flags, three reserved bytes, the width less one and the height
        // less one in three bytes each.
        bytes.extend([0, 0, 0, 0, 7, 0, 0, 3, 0, 0]);
        bytes.extend(lossy_chunk);
        bytes
    }

    /// Returns a PNG that has a header and no pixel: the signature, the
    /// `IHDR` chunk with these values, an empty `IDAT` chunk and the `IEND`
    /// chunk. The file is 57 bytes long whatever size it declares.
    fn png_without_pixels(width: u32, height: u32, bit_depth: u8, color_type: u8) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend(width.to_be_bytes());
        header.extend(height.to_be_bytes());
        header.extend([bit_depth, color_type, 0, 0, 0]);
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        for (kind, data) in [(b"IHDR", header.as_slice()), (b"IDAT", &[]), (b"IEND", &[])] {
            bytes.extend((data.len() as u32).to_be_bytes());
            let checked = bytes.len();
            bytes.extend(kind);
            bytes.extend(data);
            let crc = crc32(&bytes[checked..]);
            bytes.extend(crc.to_be_bytes());
        }
        bytes
    }

    /// Returns the CRC-32 of `bytes`, as a PNG chunk carries it.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Checks that `error` says that an image is over a limit of the
    /// decoder.
    fn assert_over_a_limit(error: &ImageRefusal) {
        assert!(
            matches!(error, ImageRefusal::Image(ImageError::Limits(_))),
            "{error}"
        );
    }

    /// Checks that `bytes` is a JPEG and returns its width and height.
    fn jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(
            image::guess_format(bytes).expect("a known image format"),
            ImageFormat::Jpeg
        );
        let image = image::load_from_memory(bytes).expect("decode the JPEG");
        (image.width(), image.height())
    }

    /// Waits until `done` returns true and fails after one second.
    async fn wait_until(mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "the condition did not hold within one second"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Takes every decode permit of `covers`. No image is read, verified or
    /// decoded until the returned permit is dropped.
    async fn hold_decodes(covers: &Covers) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&covers.inner.decode_permits)
            .acquire_many_owned(DECODE_PERMITS as u32)
            .await
            .expect("the semaphore is open")
    }

    #[test]
    fn a_size_of_zero_is_no_size_and_a_size_over_the_maximum_is_the_maximum() {
        assert_eq!(effective_size(None), None);
        assert_eq!(effective_size(Some(0)), None);
        assert_eq!(effective_size(Some(1)), Some(1));
        assert_eq!(effective_size(Some(64)), Some(64));
        assert_eq!(effective_size(Some(MAX_SIZE)), Some(2000));
        assert_eq!(effective_size(Some(MAX_SIZE + 1)), Some(2000));
        assert_eq!(effective_size(Some(u32::MAX)), Some(2000));
    }

    #[test]
    fn the_name_of_a_cache_file_holds_the_id_the_size_and_the_time() {
        let id = "al-0123456789abcdef0123456789abcdef";
        assert_eq!(
            cache_name(id, 300, 1_700_000_000),
            "al-0123456789abcdef0123456789abcdef-300-1700000000.jpg"
        );
        // A file from before 1970 has a negative time.
        assert_eq!(
            cache_name(id, 64, -5),
            "al-0123456789abcdef0123456789abcdef-64--5.jpg"
        );
    }

    #[test]
    fn the_name_of_a_temporary_file_has_no_hyphen_and_is_new_each_time() {
        let name = temp_name();
        assert!(name.starts_with("tmp."), "{name}");
        assert_eq!(name.len(), "tmp.".len() + 32);
        assert!(!name.contains('-'), "{name}");
        assert_ne!(name, temp_name());
    }

    #[test]
    fn a_cover_image_file_has_one_of_four_extensions() {
        for name in ["cover.jpg", "cover.jpeg", "Folder.PNG", "front.WebP"] {
            assert!(has_image_extension(Path::new(name)), "{name}");
        }
        for name in [
            "cover.gif",
            "cover.jpg.bak",
            "cover",
            "library.db",
            "01 Low Tide.flac",
        ] {
            assert!(!has_image_extension(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_wide_png_and_a_tall_jpeg_resize_to_their_proportions() {
        let resized = |bytes: Vec<u8>, size: u32| {
            jpeg_dimensions(&resize(bytes, size, decode_limits()).expect("resize the image"))
        };
        assert_eq!(resized(png(4, 2), 2), (2, 1));
        assert_eq!(resized(jpeg(2, 4), 2), (1, 2));
        assert_eq!(resized(png(64, 32), 16), (16, 8));
        assert_eq!(resized(jpeg(32, 64), 16), (8, 16));
        assert_eq!(resized(png(3, 3), 2), (2, 2));
        // The shorter side is at least one pixel.
        assert_eq!(resized(png(400, 1), 4), (4, 1));
        assert_eq!(resized(png(1, 400), 4), (1, 4));
    }

    #[test]
    fn an_image_within_the_size_is_not_enlarged() {
        let resized = |bytes: Vec<u8>, size: u32| {
            jpeg_dimensions(&resize(bytes, size, decode_limits()).expect("resize the image"))
        };
        // The result is a JPEG of the size of the image.
        assert_eq!(resized(png(4, 2), 64), (4, 2));
        assert_eq!(resized(jpeg(2, 4), 64), (2, 4));
        assert_eq!(resized(png(2, 2), MAX_SIZE), (2, 2));
        // The longer side is the size itself.
        assert_eq!(resized(png(4, 2), 4), (4, 2));
        assert_eq!(resized(jpeg(2, 4), 4), (2, 4));
        // One pixel over the size is scaled down.
        assert_eq!(resized(png(5, 2), 4), (4, 2));
        assert_eq!(resized(png(2, 5), 4), (2, 4));
    }

    #[test]
    fn every_pixel_format_resizes_to_an_rgb_jpeg() {
        let sources = [
            encoded(
                image::RgbaImage::from_pixel(8, 4, Rgba([10, 20, 30, 128])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayImage::from_pixel(8, 4, Luma([90])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayAlphaImage::from_pixel(8, 4, LumaA([90, 128])),
                ImageFormat::Png,
            ),
            encoded(
                image::ImageBuffer::<Rgb<u16>, _>::from_pixel(8, 4, Rgb([9000, 500, 65000])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayImage::from_pixel(8, 4, Luma([90])),
                ImageFormat::Jpeg,
            ),
            LOSSY_WEBP.to_vec(),
        ];
        // The same sources are JPEGs of their own size when the size is not
        // smaller than they are.
        for (size, dimensions) in [(4, (4, 2)), (64, (8, 4))] {
            for source in &sources {
                let jpeg = resize(source.clone(), size, decode_limits()).expect("resize");
                assert_eq!(jpeg_dimensions(&jpeg), dimensions);
                let decoded = image::load_from_memory(&jpeg).expect("decode the JPEG");
                assert_eq!(decoded.color(), image::ColorType::Rgb8);
            }
        }
    }

    #[test]
    fn a_lossy_webp_in_the_simple_form_is_decoded() {
        assert_eq!(&LOSSY_WEBP[12..16], b"VP8 ");
        assert!(is_simple_lossy_webp(&LOSSY_WEBP));
        let image = decode(&LOSSY_WEBP, decode_limits()).expect("decode the image");
        assert_eq!((image.width(), image.height()), (8, 4));
    }

    #[test]
    fn a_webp_that_is_lossless_or_in_the_extended_form_is_not_decoded() {
        let lossless = encoded(picture(8, 4), ImageFormat::WebP);
        assert_eq!(&lossless[12..16], b"VP8L");
        let extended = extended_webp();
        assert_eq!(&extended[12..16], b"VP8X");

        for bytes in [lossless, extended] {
            assert!(!is_simple_lossy_webp(&bytes));
            // The header is the header of a WebP image, so the bytes are
            // served without a size.
            assert_eq!(verify(&bytes).expect("a WebP header"), ImageFormat::WebP);
            let error = decode(&bytes, decode_limits()).expect_err("not decoded");
            assert!(matches!(error, ImageRefusal::WebpForm), "{error}");
        }
    }

    #[test]
    fn the_header_of_an_image_is_verified_without_decoding_the_image() {
        let verified = |bytes: &[u8]| verify(bytes).expect("the header of an image");
        assert_eq!(verified(&jpeg(4, 2)), ImageFormat::Jpeg);
        assert_eq!(verified(&png(4, 2)), ImageFormat::Png);
        assert_eq!(verified(&LOSSY_WEBP), ImageFormat::WebP);
        // The size of the image has no limit here, and its pixels are not
        // read: these files hold none, or the data of 8 × 8 pixels.
        assert_eq!(
            verified(&png_without_pixels(60_000, 60_000, 8, 2)),
            ImageFormat::Png
        );
        assert_eq!(
            verified(&jpeg_that_declares_the_largest_size()),
            ImageFormat::Jpeg
        );
    }

    #[test]
    fn bytes_that_are_no_jpeg_png_or_webp_image_are_not_verified() {
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
        let bmp = b"BM\x46\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00\x00\x00";
        let tiff = b"II*\x00\x08\x00\x00\x00\x00\x00";
        let other: [&[u8]; 8] = [
            b"",
            b"not an image",
            b"SQLite format 3\x00 and the pages of a database",
            b"[server]\nport = 4200\n",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            gif,
            bmp,
            tiff,
        ];
        for bytes in other {
            for refusal in [
                verify(bytes).expect_err("no image"),
                decode(bytes, decode_limits()).expect_err("no image"),
            ] {
                assert!(
                    matches!(refusal, ImageRefusal::Format),
                    "{bytes:?}: {refusal}"
                );
            }
        }

        // The first bytes of a format without the rest of its header.
        let cut_png = &png(4, 2)[..20];
        let cut_jpeg = &jpeg(4, 2)[..20];
        let cut_webp = &LOSSY_WEBP[..22];
        for bytes in [cut_png, cut_jpeg, cut_webp] {
            let refusal = verify(bytes).expect_err("a header that ends early");
            assert!(matches!(refusal, ImageRefusal::Image(_)), "{refusal}");
        }
    }

    #[test]
    fn an_image_with_a_side_over_the_limit_is_not_decoded() {
        let over = |bytes: &[u8]| {
            assert_over_a_limit(&decode(bytes, decode_limits()).expect_err("over a limit"));
        };
        over(&png(MAX_SOURCE_SIDE + 1, 1));
        over(&png(1, MAX_SOURCE_SIDE + 1));
        over(&jpeg(MAX_SOURCE_SIDE + 1, 1));

        let widest = decode(&png(MAX_SOURCE_SIDE, 1), decode_limits()).expect("at the limit");
        assert_eq!((widest.width(), widest.height()), (MAX_SOURCE_SIDE, 1));
    }

    #[test]
    fn an_image_that_declares_more_pixels_than_the_memory_limit_is_not_decoded() {
        // 65535 × 65535 pixels of three bytes are 12 GiB. The decoder reads
        // the size from the header and returns before it allocates them.
        let bomb = jpeg_that_declares_the_largest_size();
        assert!(bomb.len() < 4096, "{}", bomb.len());
        let start = Instant::now();
        assert_over_a_limit(&decode(&bomb, decode_limits()).expect_err("over a limit"));
        assert!(start.elapsed() < Duration::from_secs(1));

        // The memory limit alone refuses the image, without the limit on
        // the sides.
        assert_over_a_limit(&decode(&bomb, header_limits()).expect_err("over the limit"));

        // An image within the sides whose pixels are just over the memory
        // limit: 3345 × 3344 pixels of six bytes are 5 KiB more than 64 MiB.
        let sixteen_bit = png_without_pixels(3345, 3344, 16, 2);
        assert_eq!(3345 * 3344 * 6 - MAX_DECODE_BYTES, 5216);
        assert_over_a_limit(&decode(&sixteen_bit, decode_limits()).expect_err("over the limit"));

        // An image at both limits is accepted. This one holds no pixel, so
        // the decoder then fails for the missing data.
        let largest = png_without_pixels(MAX_SOURCE_SIDE, MAX_SOURCE_SIDE, 8, 6);
        let error = decode(&largest, decode_limits()).expect_err("no pixel data");
        assert!(
            matches!(&error, ImageRefusal::Image(image) if !matches!(image, ImageError::Limits(_))),
            "{error}"
        );

        // Four RGB pixels are 12 bytes.
        let mut eleven_bytes = decode_limits();
        eleven_bytes.max_alloc = Some(11);
        assert_over_a_limit(&decode(&png(2, 2), eleven_bytes).expect_err("over the limit"));
        assert!(decode(&png(2, 2), decode_limits()).is_ok());
    }

    #[test]
    fn the_limits_are_the_ones_that_the_design_derives_the_memory_from() {
        let limits = decode_limits();
        assert_eq!(limits.max_image_width, Some(4096));
        assert_eq!(limits.max_image_height, Some(4096));
        assert_eq!(limits.max_alloc, Some(4096 * 4096 * 4));
        assert_eq!(MAX_DECODE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_SOURCE_BYTES, 16 * 1024 * 1024);
        assert_eq!(DECODE_PERMITS, 1);
        assert_eq!(MAX_SIZE, 2000);

        let header = header_limits();
        assert_eq!(header.max_image_width, None);
        assert_eq!(header.max_image_height, None);
        assert_eq!(header.max_alloc, Some(MAX_DECODE_BYTES));
    }

    #[tokio::test]
    async fn an_album_cover_without_a_size_is_the_file_with_the_type_of_its_format() {
        let library = Library::new().await;
        let lossless_webp = encoded(picture(4, 2), ImageFormat::WebP);
        let files = [
            ("Glass Harbour", "cover.jpg", jpeg(4, 2), "image/jpeg"),
            ("Paper Maps", "Folder.PNG", png(4, 2), "image/png"),
            ("Slow Rivers", "front.webp", lossless_webp, "image/webp"),
            ("Open Road", "album.webp", LOSSY_WEBP.to_vec(), "image/webp"),
            // The type is the type of the content, also when the extension
            // names another format.
            ("Summer Sampler", "cover.jpg", png(2, 4), "image/png"),
        ];
        for (title, file, bytes, content_type) in files {
            let (id, _) = library.album_with_cover(title, file, &bytes).await;
            let cover = library.get(&id, None).await.expect("the album has a cover");
            assert_eq!(cover.bytes, bytes, "{title}");
            assert_eq!(cover.content_type, content_type, "{title}");
            assert_eq!(cover.mtime, MTIME);
            // A size of 0 asks for the same.
            assert_eq!(library.get(&id, Some(0)).await, Some(cover));
        }
        // Nothing was decoded and nothing was cached.
        assert_eq!(library.covers.decodes(), 0);
        assert!(!library.cache_dir().exists());
    }

    #[tokio::test]
    async fn a_file_that_is_no_image_is_not_returned_as_a_cover() {
        let library = Library::new().await;
        let database = library.dir.path().join("library.db");
        fs::write(&database, b"SQLite format 3\x00 and the rows of a database")
            .expect("write the file");

        // A hard link is a regular file. Only its content tells that it is
        // no image.
        let dir = library.music().join("Glass Harbour");
        fs::create_dir(&dir).expect("create the album directory");
        let linked = dir.join("cover.jpg");
        fs::hard_link(&database, &linked).expect("link the file");
        let hard_linked = library.album("Glass Harbour").await;
        library
            .set_cover(
                &hard_linked,
                linked.to_str().expect("a Unicode path"),
                MTIME,
            )
            .await;

        let (text, _) = library
            .album_with_cover("Paper Maps", "cover.png", b"password = sesame\n")
            .await;
        let (empty, _) = library
            .album_with_cover("Slow Rivers", "cover.webp", b"")
            .await;
        let (gif, _) = library
            .album_with_cover(
                "Summer Sampler",
                "cover.jpg",
                b"GIF89a\x01\x00\x01\x00\x00\x00\x00;",
            )
            .await;

        for id in [hard_linked, text, empty, gif] {
            for size in [None, Some(0), Some(64), Some(MAX_SIZE)] {
                assert_eq!(library.get(&id, size).await, None, "{id} {size:?}");
            }
        }
        assert_eq!(library.cache_files(), [""; 0]);
    }

    #[tokio::test]
    async fn a_damaged_image_with_a_whole_header_is_served_without_a_size_only() {
        let library = Library::new().await;
        // The header of the PNG is whole, and the end of its data is cut.
        let whole = png(64, 64);
        let damaged = &whole[..whole.len() - 40];
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", damaged)
            .await;

        assert_eq!(library.get(&id, Some(32)).await, None);
        assert_eq!(library.get(&id, Some(32)).await, None);
        // Each request with a size tried the image once.
        assert_eq!(library.covers.decodes(), 2);
        assert_eq!(library.cache_files(), [""; 0]);

        let cover = library.get(&id, None).await.expect("the stored bytes");
        assert_eq!(cover.bytes, damaged);
        assert_eq!(cover.content_type, "image/png");
    }

    #[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
    #[tokio::test]
    async fn a_cover_file_that_is_a_symbolic_link_is_not_read() {
        use std::os::unix::fs::symlink;

        let library = Library::new().await;
        // The target is an image, so only the link keeps it from being
        // served.
        let outside = library.dir.path().join("outside.jpg");
        fs::write(&outside, jpeg(4, 2)).expect("write the image");
        let (id, cover_file) = library
            .album_with_cover("Glass Harbour", "cover.jpg", &jpeg(4, 2))
            .await;
        assert!(library.get(&id, None).await.is_some());

        let link_to = |target: &Path| {
            fs::remove_file(&cover_file).expect("remove the cover file");
            symlink(target, &cover_file).expect("create the link");
        };
        let second_link = library.dir.path().join("link.jpg");
        symlink(&outside, &second_link).expect("create the link");
        let targets = [
            outside.clone(),
            second_link,
            library.dir.path().join("missing.jpg"),
            library.music(),
        ];
        for target in &targets {
            link_to(target);
            for size in [None, Some(0), Some(2), Some(64)] {
                assert_eq!(library.get(&id, size).await, None, "{target:?} {size:?}");
            }
        }
        assert_eq!(library.covers.decodes(), 0);
        assert_eq!(library.cache_files(), [""; 0]);

        // A directory in the path can be a link. The image file in it is a
        // regular file and is read.
        let linked_dir = library.music().join("Linked");
        symlink(library.dir.path(), &linked_dir).expect("link the directory");
        let through_link = library.album("Paper Maps").await;
        let path = linked_dir.join("outside.jpg");
        library
            .set_cover(&through_link, path.to_str().expect("a Unicode path"), MTIME)
            .await;
        let cover = library.get(&through_link, None).await.expect("a cover");
        assert_eq!(cover.bytes, fs::read(&outside).expect("read the image"));
    }

    #[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
    #[tokio::test]
    async fn a_symbolic_link_in_the_place_of_a_cache_file_is_not_read() {
        use std::os::unix::fs::symlink;

        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let target = library.dir.path().join("outside.txt");
        fs::write(&target, b"not a cover").expect("write the file");
        let cached = library.cache_dir().join(cache_name(&id, 2, MTIME));
        fs::create_dir_all(library.cache_dir()).expect("create the cache directory");
        symlink(&target, &cached).expect("create the link");

        // The cover is made from its source. The rename puts a regular file
        // in the place of the link, and the target of the link stays.
        let cover = library.get(&id, Some(2)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (2, 1));
        assert_eq!(library.covers.decodes(), 1);
        let state = fs::symlink_metadata(&cached).expect("the state of the cache file");
        assert!(state.is_file());
        assert_eq!(fs::read(&target).expect("read the file"), b"not a cover");

        assert_eq!(library.get(&id, Some(2)).await, Some(cover));
        assert_eq!(library.covers.decodes(), 1);
    }

    #[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
    #[test]
    fn a_file_is_read_when_its_path_names_the_regular_file_that_is_opened() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("a temporary directory");
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        fs::write(&first, b"first").expect("write the file");
        fs::write(&second, b"second").expect("write the file");
        let link = dir.path().join("link");
        symlink(&first, &link).expect("create the link");

        assert_eq!(read_regular_file(&first).expect("a regular file"), b"first");
        for path in [link.as_path(), dir.path()] {
            let error = read_regular_file(path).expect_err("no regular file");
            assert!(matches!(error, LoadError::NotAFile), "{error}");
        }
        let error = read_regular_file(&dir.path().join("missing")).expect_err("no file");
        assert!(
            matches!(&error, LoadError::Io(io) if io.kind() == io::ErrorKind::NotFound),
            "{error}"
        );

        // The state of a path is compared with the state of the file that
        // was opened at the path.
        let opened = |path: &Path| {
            let file = File::open(path).expect("open the file");
            file.metadata().expect("the state of the open file")
        };
        let named = fs::symlink_metadata(&first).expect("the state of the path");
        assert!(same_file(&named, &opened(&first)));
        assert!(same_file(&named, &opened(&link)));
        assert!(!same_file(&named, &opened(&second)));
    }

    #[tokio::test]
    async fn a_sized_cover_is_a_jpeg_that_is_cached_and_not_decoded_again() {
        let library = Library::new().await;
        let (id, source) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(128, 96))
            .await;

        let cover = library.get(&id, Some(64)).await.expect("a resized cover");
        assert_eq!(cover.content_type, "image/jpeg");
        assert_eq!(cover.mtime, MTIME);
        assert_eq!(jpeg_dimensions(&cover.bytes), (64, 48));
        assert_eq!(library.covers.decodes(), 1);

        // The cache holds the cover under its name and no other file.
        let name = format!("{id}-64-{MTIME}.jpg");
        assert_eq!(library.cache_files(), [name.as_str()]);
        let cached = fs::read(library.cache_dir().join(&name)).expect("read the cache file");
        assert_eq!(cover.bytes, cached);

        // The second request reads the cache file. It decodes nothing, and
        // it does not read the source, which is gone here.
        fs::remove_file(&source).expect("remove the source");
        assert_eq!(library.get(&id, Some(64)).await, Some(cover));
        assert_eq!(library.covers.decodes(), 1);
        assert_eq!(library.cache_files(), [name.as_str()]);

        // Another size is another cover, and its source is gone.
        assert_eq!(library.get(&id, Some(32)).await, None);
        assert_eq!(library.get(&id, None).await, None);
    }

    #[tokio::test]
    async fn a_sized_cover_of_a_smaller_image_has_the_size_of_the_image() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;

        let cover = library.get(&id, Some(64)).await.expect("a cover");
        assert_eq!(cover.content_type, "image/jpeg");
        assert_eq!(jpeg_dimensions(&cover.bytes), (4, 2));
        // Each size has its cache file, also when the covers are the same.
        let largest = library.get(&id, Some(MAX_SIZE)).await.expect("a cover");
        assert_eq!(jpeg_dimensions(&largest.bytes), (4, 2));
        assert_eq!(
            library.cache_files(),
            [
                format!("{id}-2000-{MTIME}.jpg"),
                format!("{id}-64-{MTIME}.jpg")
            ]
        );
    }

    #[tokio::test]
    async fn the_cache_file_is_served_as_it_is() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        fs::create_dir_all(library.cache_dir()).expect("create the cache directory");
        let name = format!("{id}-64-{MTIME}.jpg");
        fs::write(library.cache_dir().join(name), b"cached earlier").expect("write the file");

        let cover = library.get(&id, Some(64)).await.expect("the cached cover");
        assert_eq!(cover.bytes, &b"cached earlier"[..]);
        assert_eq!(cover.content_type, "image/jpeg");
        assert_eq!(library.covers.decodes(), 0);
    }

    #[tokio::test]
    async fn a_new_modification_time_of_the_cover_is_a_new_cache_file() {
        let library = Library::new().await;
        let (id, path) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let first = library.get(&id, Some(2)).await.expect("a resized cover");

        fs::write(&path, png(2, 4)).expect("replace the cover file");
        library
            .set_cover(&id, path.to_str().expect("a Unicode path"), MTIME + 60)
            .await;
        let second = library.get(&id, Some(2)).await.expect("a resized cover");

        assert_eq!(jpeg_dimensions(&first.bytes), (2, 1));
        assert_eq!(jpeg_dimensions(&second.bytes), (1, 2));
        assert_eq!(second.mtime, MTIME + 60);
        assert_eq!(library.covers.decodes(), 2);
        // The scanner removes the file of the old time (`tests/covers.rs`).
        assert_eq!(
            library.cache_files(),
            [
                format!("{id}-2-{MTIME}.jpg"),
                format!("{id}-2-{}.jpg", MTIME + 60)
            ]
        );
    }

    #[tokio::test]
    async fn a_size_over_the_maximum_is_served_and_cached_at_the_maximum() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4000, 2))
            .await;

        let cover = library.get(&id, Some(5000)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (2000, 1));
        let name = format!("{id}-2000-{MTIME}.jpg");
        assert_eq!(library.cache_files(), [name.as_str()]);

        // Every size over the maximum is the same cover and the same file.
        for size in [2000, 2001, 1_000_000, u32::MAX] {
            assert_eq!(library.get(&id, Some(size)).await.as_ref(), Some(&cover));
        }
        assert_eq!(library.covers.decodes(), 1);
        assert_eq!(library.cache_files(), [name.as_str()]);
    }

    #[tokio::test]
    async fn a_hostile_id_has_no_cover_and_reaches_no_file() {
        let library = Library::new().await;
        let (album, cover_file) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let hex = &album["al-".len()..];
        // A file that a path built from an ID would name.
        fs::write(library.dir.path().join("secret.jpg"), jpeg(4, 2)).expect("write the file");

        let hostile = [
            String::new(),
            "al-".to_owned(),
            "../secret".to_owned(),
            "../../secret.jpg".to_owned(),
            "al-../../../secret".to_owned(),
            "al-/etc/passwd".to_owned(),
            r"al-..\..\secret".to_owned(),
            cover_file.to_str().expect("a Unicode path").to_owned(),
            format!("{album}/../../secret"),
            format!("{album}/"),
            format!("{album}\0"),
            format!("{album}-64-{MTIME}.jpg"),
            format!(" {album}"),
            format!("{album}\n"),
            format!("AL-{hex}"),
            format!("al-{}", hex.to_uppercase()),
            format!("al_{hex}"),
            format!("al-{}", &hex[1..]),
            format!("al-{hex}0"),
            format!("embedded:{album}"),
            "al-' OR '1'='1".to_owned(),
            "al-%".to_owned(),
            // IDs of the right form that no cover has.
            "al-00000000000000000000000000000000".to_owned(),
            "tr-00000000000000000000000000000000".to_owned(),
            format!("tr-{hex}"),
            format!("ar-{hex}"),
            format!("pl-{hex}"),
            library.artist_id.clone(),
        ];
        for id in &hostile {
            for size in [None, Some(0), Some(64), Some(u32::MAX)] {
                assert_eq!(library.get(id, size).await, None, "{id:?} {size:?}");
            }
        }
        // No request decoded an image or wrote a file.
        assert_eq!(library.covers.decodes(), 0);
        assert!(!library.cache_dir().exists());
        let mut entries: Vec<_> = fs::read_dir(library.dir.path())
            .expect("list the directory")
            .map(|entry| entry.expect("a directory entry").file_name())
            .collect();
        entries.sort();
        assert_eq!(entries, ["music", "secret.jpg"]);
    }

    #[tokio::test]
    async fn an_image_over_the_decode_limits_has_no_sized_cover() {
        let library = Library::new().await;
        let sources = [
            (
                "Glass Harbour",
                "cover.jpg",
                jpeg_that_declares_the_largest_size(),
            ),
            ("Paper Maps", "cover.png", png(MAX_SOURCE_SIDE + 1, 1)),
            ("Slow Rivers", "cover.png", png(1, MAX_SOURCE_SIDE + 1)),
            (
                "Open Road",
                "cover.png",
                png_without_pixels(3345, 3344, 16, 2),
            ),
        ];
        for (title, file, bytes) in sources {
            let (id, _) = library.album_with_cover(title, file, &bytes).await;
            let start = Instant::now();
            assert_eq!(library.get(&id, Some(64)).await, None, "{title}");
            assert_eq!(library.get(&id, Some(MAX_SIZE)).await, None, "{title}");
            assert!(start.elapsed() < Duration::from_secs(1), "{title}");
            // The image is not decoded for a request without a size.
            let cover = library.get(&id, None).await.expect("the stored bytes");
            assert_eq!(cover.bytes, bytes, "{title}");
        }
        assert_eq!(library.cache_files(), [""; 0]);

        // An image at the limit of one side is resized.
        let (id, _) = library
            .album_with_cover("Summer Sampler", "cover.png", &png(MAX_SOURCE_SIDE, 1))
            .await;
        let cover = library.get(&id, Some(64)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (64, 1));
    }

    #[tokio::test]
    async fn a_webp_that_is_not_decoded_has_no_sized_cover() {
        let library = Library::new().await;
        let lossless = encoded(picture(8, 4), ImageFormat::WebP);
        for (title, bytes) in [("Glass Harbour", lossless), ("Paper Maps", extended_webp())] {
            let (id, _) = library.album_with_cover(title, "cover.webp", &bytes).await;
            assert_eq!(library.get(&id, Some(4)).await, None, "{title}");
            let cover = library.get(&id, None).await.expect("the stored bytes");
            assert_eq!(cover.bytes, bytes, "{title}");
            assert_eq!(cover.content_type, "image/webp");
        }
        assert_eq!(library.cache_files(), [""; 0]);

        let (id, _) = library
            .album_with_cover("Slow Rivers", "cover.webp", &LOSSY_WEBP)
            .await;
        let cover = library.get(&id, Some(4)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (4, 2));
    }

    #[tokio::test]
    async fn an_image_file_over_the_byte_limit_has_no_cover() {
        let library = Library::new().await;
        let (id, path) = library
            .album_with_cover("Glass Harbour", "cover.jpg", &jpeg(4, 2))
            .await;
        // The file gets its length without its bytes being written.
        let grow = |len: u64| {
            let file = OpenOptions::new().write(true).open(&path).expect("open");
            file.set_len(len).expect("set the length");
        };

        assert_eq!(
            LoadError::TooLarge(MAX_SOURCE_BYTES + 1).to_string(),
            "it holds 16777217 bytes and the limit is 16777216"
        );
        grow(MAX_SOURCE_BYTES + 1);
        assert_eq!(library.get(&id, None).await, None);
        assert_eq!(library.get(&id, Some(64)).await, None);
        assert_eq!(library.covers.decodes(), 0);
        assert!(!library.cache_dir().exists());

        // A file of exactly the limit is read.
        grow(MAX_SOURCE_BYTES);
        let cover = library.get(&id, None).await.expect("the stored bytes");
        assert_eq!(cover.bytes.len() as u64, MAX_SOURCE_BYTES);
    }

    #[tokio::test]
    async fn a_source_that_is_gone_or_no_regular_file_has_no_cover() {
        let library = Library::new().await;
        let (gone, path) = library
            .album_with_cover("Glass Harbour", "cover.jpg", &jpeg(4, 2))
            .await;
        fs::remove_file(path).expect("remove the cover file");

        let directory = library.music().join("Paper Maps").join("cover.jpg");
        fs::create_dir_all(&directory).expect("create a directory with the name of a cover");
        let not_a_file = library.album("Paper Maps").await;
        library
            .set_cover(
                &not_a_file,
                directory.to_str().expect("a Unicode path"),
                MTIME,
            )
            .await;

        for id in [gone, not_a_file] {
            assert_eq!(library.get(&id, None).await, None);
            assert_eq!(library.get(&id, Some(64)).await, None);
        }
        assert_eq!(library.covers.decodes(), 0);
        assert!(!library.cache_dir().exists());
    }

    #[tokio::test]
    async fn an_album_resolves_to_its_stored_image_file() {
        let library = Library::new().await;
        let (id, path) = library
            .album_with_cover("Glass Harbour", "Cover.JPG", &jpeg(4, 2))
            .await;
        assert_eq!(
            library.resolve(&id).await,
            Some(Source {
                cover_id: id.clone(),
                mtime: MTIME,
                origin: Origin::File(path),
            })
        );

        // An album without a cover.
        let bare = library.album("Paper Maps").await;
        assert_eq!(library.resolve(&bare).await, None);

        // A stored source that is not the absolute path of an image file is
        // not opened.
        let music = library.music();
        let audio = music.join("Glass Harbour").join("01 Low Tide.flac");
        for source in [
            "cover.jpg",
            "Glass Harbour/cover.jpg",
            "../cover.jpg",
            audio.to_str().expect("a Unicode path"),
            music.to_str().expect("a Unicode path"),
            "",
        ] {
            library.set_cover(&id, source, MTIME).await;
            assert_eq!(library.resolve(&id).await, None, "{source:?}");
            assert_eq!(library.get(&id, None).await, None, "{source:?}");
        }
    }

    #[tokio::test]
    async fn a_track_resolves_to_the_picture_of_its_file() {
        let library = Library::new().await;
        let album = library.album("Glass Harbour").await;
        let pictured = library
            .track(&album, "Glass Harbour/01 Low Tide.flac", true, 500)
            .await;
        let plain = library
            .track(&album, "Glass Harbour/02 Breakwater.flac", false, 600)
            .await;

        let file = library
            .music()
            .join("Glass Harbour")
            .join("01 Low Tide.flac");
        let embedded = Source {
            cover_id: pictured.clone(),
            mtime: 500,
            origin: Origin::Embedded(file.clone()),
        };
        assert_eq!(library.resolve(&pictured).await, Some(embedded));
        // A track without a picture has no cover.
        assert_eq!(library.resolve(&plain).await, None);

        // The cover of an album can be the picture of a track. The album
        // has its own ID and the time that the scan stored for it.
        library
            .set_cover(&album, &format!("{EMBEDDED}{pictured}"), 700)
            .await;
        assert_eq!(
            library.resolve(&album).await,
            Some(Source {
                cover_id: album.clone(),
                mtime: 700,
                origin: Origin::Embedded(file),
            })
        );

        // A stored source that names a track without a picture, no track or
        // no ID of a track has no cover.
        for source in [
            format!("{EMBEDDED}{plain}"),
            format!("{EMBEDDED}tr-00000000000000000000000000000000"),
            format!("{EMBEDDED}{album}"),
            format!("{EMBEDDED}../../secret"),
            EMBEDDED.to_owned(),
        ] {
            library.set_cover(&album, &source, 700).await;
            assert_eq!(library.resolve(&album).await, None, "{source}");
        }

        // The file of the track does not exist, so the track has no cover.
        assert_eq!(library.get(&pictured, None).await, None);
        assert_eq!(library.get(&pictured, Some(64)).await, None);
    }

    #[tokio::test]
    async fn a_track_without_a_file_gives_no_picture() {
        let library = Library::new().await;
        let album = library.album("Glass Harbour").await;
        let track = library
            .track(&album, "Glass Harbour/01 Low Tide.flac", true, 500)
            .await;
        library
            .set_cover(&album, &format!("{EMBEDDED}{track}"), 500)
            .await;
        assert!(library.resolve(&track).await.is_some());
        assert!(library.resolve(&album).await.is_some());

        // The row keeps `has_picture` and gets the placeholder path. No path
        // is built from it: the track and the album resolve to nothing.
        library.detach(&track).await;
        assert_eq!(library.resolve(&track).await, None);
        assert_eq!(library.resolve(&album).await, None);
        for size in [None, Some(64)] {
            assert_eq!(library.get(&track, size).await, None);
            assert_eq!(library.get(&album, size).await, None);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_for_one_uncached_cover_at_one_time_decode_it_once() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let name = cache_name(&id, 2, MTIME);

        // While the test holds the permit, the job waits before it decodes
        // and every request finds it in progress.
        let permits = hold_decodes(&library.covers).await;
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let (covers, id) = (library.covers.clone(), id.clone());
                tokio::spawn(async move { covers.get(&id, Some(2)).await })
            })
            .collect();
        wait_until(|| library.covers.waiting(&name) == requests.len()).await;
        assert_eq!(library.covers.decodes(), 0);
        assert_eq!(library.cache_files(), [""; 0]);

        drop(permits);
        let mut covers = Vec::new();
        for request in requests {
            let cover = request.await.expect("the request ends");
            covers.push(cover.expect("no error").expect("a resized cover"));
        }
        assert_eq!(jpeg_dimensions(&covers[0].bytes), (2, 1));
        assert!(covers.iter().all(|cover| *cover == covers[0]));
        assert_eq!(library.covers.decodes(), 1);
        assert_eq!(library.cache_files(), [name.as_str()]);
        // The job leaves no entry behind.
        wait_until(|| library.covers.inner.in_progress().is_empty()).await;

        // A later request reads the cache.
        assert_eq!(library.get(&id, Some(2)).await.as_ref(), Some(&covers[0]));
        assert_eq!(library.covers.decodes(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_for_one_corrupt_cover_at_one_time_try_it_once() {
        let library = Library::new().await;
        let whole = png(64, 64);
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &whole[..whole.len() - 40])
            .await;
        let name = cache_name(&id, 32, MTIME);

        let permits = hold_decodes(&library.covers).await;
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let (covers, id) = (library.covers.clone(), id.clone());
                tokio::spawn(async move { covers.get(&id, Some(32)).await })
            })
            .collect();
        wait_until(|| library.covers.waiting(&name) == requests.len()).await;
        drop(permits);
        for request in requests {
            let cover = request.await.expect("the request ends");
            assert_eq!(cover.expect("no error"), None);
        }
        assert_eq!(library.covers.decodes(), 1);
        assert_eq!(library.cache_files(), [""; 0]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn covers_of_different_sizes_are_jobs_of_their_own_that_wait_for_the_permit() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(8, 4))
            .await;

        let permits = hold_decodes(&library.covers).await;
        let sizes = [1, 2, 3, 4, 5, 6];
        let requests: Vec<_> = sizes
            .map(|size| {
                let (covers, id) = (library.covers.clone(), id.clone());
                tokio::spawn(async move { covers.get(&id, Some(size)).await })
            })
            .into_iter()
            .collect();
        // Each size is a job of its own, and none decodes without a permit.
        wait_until(|| library.covers.inner.in_progress().len() == sizes.len()).await;
        assert_eq!(library.covers.decodes(), 0);
        assert_eq!(library.covers.inner.decode_permits.available_permits(), 0);

        drop(permits);
        for (request, size) in requests.into_iter().zip(sizes) {
            let cover = request.await.expect("the request ends");
            let cover = cover.expect("no error").expect("a resized cover");
            assert_eq!(jpeg_dimensions(&cover.bytes).0, size);
        }
        assert_eq!(library.covers.decodes(), sizes.len() as u64);
        assert_eq!(library.cache_files().len(), sizes.len());
        wait_until(|| library.covers.inner.in_progress().is_empty()).await;
        assert_eq!(
            library.covers.inner.decode_permits.available_permits(),
            DECODE_PERMITS
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_without_a_size_waits_for_the_permit_too() {
        let library = Library::new().await;
        let bytes = jpeg(4, 2);
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.jpg", &bytes)
            .await;

        // The service holds the source of one image at a time, also when it
        // only verifies the header.
        let permits = hold_decodes(&library.covers).await;
        let mut request = {
            let (covers, id) = (library.covers.clone(), id.clone());
            tokio::spawn(async move { covers.get(&id, None).await })
        };
        let waiting = tokio::time::timeout(Duration::from_millis(50), &mut request).await;
        assert!(waiting.is_err(), "the request ended without a permit");

        drop(permits);
        let cover = request.await.expect("the request ends").expect("no error");
        assert_eq!(cover.expect("a cover").bytes, bytes);
        assert_eq!(
            library.covers.inner.decode_permits.available_permits(),
            DECODE_PERMITS
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_that_no_request_waits_for_is_dropped_before_it_decodes() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let name = cache_name(&id, 2, MTIME);

        let permits = hold_decodes(&library.covers).await;
        let request = {
            let (covers, id) = (library.covers.clone(), id.clone());
            tokio::spawn(async move { covers.get(&id, Some(2)).await })
        };
        wait_until(|| library.covers.waiting(&name) == 1).await;
        request.abort();
        let ended = request.await.expect_err("the request was ended");
        assert!(ended.is_cancelled());

        // The job leaves while the test still holds the permit.
        wait_until(|| library.covers.inner.in_progress().is_empty()).await;
        drop(permits);
        assert_eq!(library.covers.decodes(), 0);
        assert_eq!(library.cache_files(), [""; 0]);

        // A later request makes the cover.
        let cover = library.get(&id, Some(2)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (2, 1));
        assert_eq!(library.covers.decodes(), 1);
        assert_eq!(library.cache_files(), [name.as_str()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_job_goes_on_while_one_request_waits_for_it() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        let name = cache_name(&id, 2, MTIME);

        let permits = hold_decodes(&library.covers).await;
        let mut requests: Vec<_> = (0..8)
            .map(|_| {
                let (covers, id) = (library.covers.clone(), id.clone());
                tokio::spawn(async move { covers.get(&id, Some(2)).await })
            })
            .collect();
        wait_until(|| library.covers.waiting(&name) == requests.len()).await;

        // Every request but one ends, the first one among them.
        let last = requests.pop().expect("eight requests");
        for request in requests {
            request.abort();
            assert!(request.await.expect_err("ended").is_cancelled());
        }
        assert_eq!(library.covers.waiting(&name), 1);

        drop(permits);
        let cover = last.await.expect("the request ends").expect("no error");
        assert_eq!(jpeg_dimensions(&cover.expect("a cover").bytes), (2, 1));
        assert_eq!(library.covers.decodes(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_that_comes_when_the_last_one_left_gets_its_cover() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(64, 32))
            .await;
        let get = |size: u32| {
            let (covers, id) = (library.covers.clone(), id.clone());
            tokio::spawn(async move { covers.get(&id, Some(size)).await })
        };

        // The second request finds the job of the first one, which leaves
        // at that moment, or no job. In both cases it gets the cover, and
        // the cover is made once.
        let sizes = 1..=8;
        for size in sizes.clone() {
            let permits = hold_decodes(&library.covers).await;
            let first = get(size);
            wait_until(|| library.covers.waiting(&cache_name(&id, size, MTIME)) == 1).await;
            first.abort();
            let second = get(size);
            drop(permits);

            let cover = second.await.expect("the request ends").expect("no error");
            assert_eq!(
                jpeg_dimensions(&cover.expect("a cover").bytes).0,
                size,
                "size {size}"
            );
        }
        assert_eq!(library.covers.decodes(), sizes.count() as u64);
        wait_until(|| library.covers.inner.in_progress().is_empty()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cover_whose_decoding_started_is_cached_when_its_request_ends() {
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(512, 256))
            .await;
        let name = cache_name(&id, 64, MTIME);

        let request = {
            let (covers, id) = (library.covers.clone(), id.clone());
            tokio::spawn(async move { covers.get(&id, Some(64)).await })
        };
        // The request is ended while its image is decoded, or after it was
        // answered when the decoding was faster than this test. The cover
        // is in the cache in both cases.
        wait_until(|| library.covers.decodes() == 1).await;
        request.abort();
        let _ = request.await;

        wait_until(|| library.cache_files() == [name.as_str()]).await;
        wait_until(|| library.covers.inner.in_progress().is_empty()).await;
        let cover = library.get(&id, Some(64)).await.expect("the cached cover");
        assert_eq!(jpeg_dimensions(&cover.bytes), (64, 32));
        assert_eq!(library.covers.decodes(), 1);
    }

    #[tokio::test]
    async fn a_cover_that_cannot_be_cached_is_served_all_the_same() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let library = Library::new().await;
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.png", &png(4, 2))
            .await;
        // The cache directory cannot be created below a file.
        let blocked = dir.path().join("cache");
        fs::write(&blocked, b"a file").expect("write the file");
        let covers = Covers::new(blocked.join("covers"), library.db.clone());

        for decodes in [1, 2] {
            let cover = covers.get(&id, Some(2)).await.expect("no error");
            assert_eq!(jpeg_dimensions(&cover.expect("a cover").bytes), (2, 1));
            assert_eq!(covers.decodes(), decodes);
        }
        assert_eq!(fs::read(&blocked).expect("read the file"), b"a file");
    }

    #[test]
    fn a_cache_file_is_written_whole_and_replaces_the_file_of_its_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let cache_dir = dir.path().join("cache").join("covers");
        let names = || {
            let mut names: Vec<_> = fs::read_dir(&cache_dir)
                .expect("list the cache directory")
                .map(|entry| entry.expect("a directory entry").file_name())
                .collect();
            names.sort();
            names
        };

        // The directory is created, and no temporary file stays.
        write_cache_file(&cache_dir, "a.jpg", b"first").expect("write the file");
        assert_eq!(names(), ["a.jpg"]);
        assert_eq!(fs::read(cache_dir.join("a.jpg")).expect("read"), b"first");

        write_cache_file(&cache_dir, "a.jpg", b"second").expect("replace the file");
        write_cache_file(&cache_dir, "b.jpg", b"other").expect("write the file");
        assert_eq!(names(), ["a.jpg", "b.jpg"]);
        assert_eq!(fs::read(cache_dir.join("a.jpg")).expect("read"), b"second");
    }

    #[test]
    fn a_new_file_does_not_replace_a_file_at_its_path() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("tmp.0");
        write_new_file(&path, b"first").expect("create the file");
        let error = write_new_file(&path, b"second").expect_err("the file exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).expect("read"), b"first");
    }

    #[test]
    fn a_failed_rename_is_a_success_when_the_destination_exists() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let temp = dir.path().join("tmp.0");
        let dest = dir.path().join("a.jpg");
        let failure = || Err(io::Error::from(io::ErrorKind::PermissionDenied));

        // The rename worked: nothing is left to do.
        fs::write(&dest, b"renamed").expect("write the destination");
        settle_rename(Ok(()), &temp, &dest).expect("the rename worked");
        assert_eq!(fs::read(&dest).expect("read"), b"renamed");

        // The rename failed and another writer has made the destination:
        // the cover is cached, and the temporary file is removed.
        fs::write(&temp, b"mine").expect("write the temporary file");
        settle_rename(failure(), &temp, &dest).expect("the destination exists");
        assert!(!temp.exists());
        assert_eq!(fs::read(&dest).expect("read"), b"renamed");

        // The rename failed and no destination exists: the error is
        // returned, and the temporary file is removed.
        fs::remove_file(&dest).expect("remove the destination");
        fs::write(&temp, b"mine").expect("write the temporary file");
        let error = settle_rename(failure(), &temp, &dest).expect_err("no destination");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!temp.exists());
        assert!(!dest.exists());

        // The failure can be that the temporary file was not created.
        let error = settle_rename(failure(), &temp, &dest).expect_err("no destination");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
