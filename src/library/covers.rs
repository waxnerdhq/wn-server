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
//! # Limits
//!
//! The images come from the music folders, and a file there can be damaged
//! or made to exhaust memory. The service therefore bounds its work:
//!
//! - It reads a source image of at most [`MAX_SOURCE_BYTES`] bytes.
//! - It decodes an image whose width and height are at most
//!   [`MAX_SOURCE_SIDE`] pixels and whose decoded form takes at most
//!   [`MAX_DECODE_BYTES`] bytes. The decoder reads both values from the
//!   header of the image and refuses the image before it allocates the
//!   pixels.
//! - It decodes at most [`DECODE_PERMITS`] images at one time.
//! - It makes one resized cover once: the requests that ask for the same
//!   resized cover while it is being made wait for it and share the result.
//! - It drops the making of a cover that waits for its turn to be decoded
//!   when no request waits for the cover any more.
//!
//! Reading, decoding and encoding run on blocking threads.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader, ImageResult, Limits};
use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::db::queries::tracks::Track;
use crate::db::queries::{albums, folders, tracks};
use crate::db::{self, Db, DbError};
use crate::ids::{self, Kind};
use crate::library::scanner::EMBEDDED;
use crate::library::tags;

/// The largest size of a resized cover: the length of its longer side in
/// pixels. A request for a larger size is served at this size.
pub const MAX_SIZE: u32 = 2000;

/// The largest source image that the service reads, in bytes. A larger image
/// file has no cover, resized or not, because the service holds a cover in
/// memory.
pub const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// The largest width and the largest height of an image that the service
/// decodes, in pixels. A scan of a 12-inch record sleeve at 600 dpi is 7200
/// pixels wide.
pub const MAX_SOURCE_SIDE: u32 = 8192;

/// The most memory that the decoder takes for the pixels of one image, in
/// bytes. An image of [`MAX_SOURCE_SIDE`] pixels on each side with four
/// bytes for each pixel takes this much.
pub const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// The number of images that are decoded at one time. With the limits above
/// it bounds the memory that resizing takes.
pub const DECODE_PERMITS: usize = 2;

/// The quality of a resized cover, on the JPEG scale of 1 to 100.
const JPEG_QUALITY: u8 = 85;

/// The content type of a resized cover.
const JPEG: &str = "image/jpeg";

/// The content type of an embedded picture whose image format is not known.
const UNKNOWN_TYPE: &str = "application/octet-stream";

/// The extensions of a cover image file and the content type of each.
const IMAGE_TYPES: [(&str, &str); 4] = [
    ("jpg", JPEG),
    ("jpeg", JPEG),
    ("png", "image/png"),
    ("webp", "image/webp"),
];

/// A cover image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    /// The image file.
    pub bytes: Bytes,
    /// The content type of `bytes`.
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
    /// The runtime shut down while the cover was read or resized.
    #[error("the runtime shut down before the cover was ready")]
    Interrupted,
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
    /// One permit for each image that may be decoded at one time.
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
    /// An image file and the content type of its extension.
    File {
        path: PathBuf,
        content_type: &'static str,
    },
    /// The embedded picture of the audio file at the path.
    Embedded(PathBuf),
}

impl Origin {
    /// The file that holds the image.
    fn path(&self) -> &Path {
        match self {
            Origin::File { path, .. } | Origin::Embedded(path) => path,
        }
    }
}

/// The image of a cover as its source stores it.
struct Original {
    bytes: Vec<u8>,
    content_type: &'static str,
}

/// Why the image of a cover cannot be read.
#[derive(Debug, thiserror::Error)]
enum LoadError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("it is not a regular file")]
    NotAFile,
    #[error("it holds {0} bytes and the limit is {MAX_SOURCE_BYTES}")]
    TooLarge(u64),
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
    /// source stores it: the bytes of the image file with the content type of
    /// its extension, or the embedded picture with its content type. The
    /// image is not decoded.
    ///
    /// With another `size` the result is a JPEG whose longer side is `size`
    /// pixels, or [`MAX_SIZE`] pixels when `size` is larger. A smaller image
    /// is enlarged. The resized cover is read from the cache directory, and
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
    /// - The source is gone, cannot be read, is not a regular file or is
    ///   larger than [`MAX_SOURCE_BYTES`].
    /// - A size is given and the image cannot be decoded: it is damaged, it
    ///   is in a format other than JPEG, PNG and WebP, or it is over
    ///   [`MAX_SOURCE_SIDE`] or [`MAX_DECODE_BYTES`]. The service logs a
    ///   warning.
    ///
    /// A resized cover that cannot be written to the cache is returned
    /// without being cached, and the service logs a warning.
    ///
    /// # Errors
    ///
    /// Returns [`CoverError::Db`] when the database cannot be read and
    /// [`CoverError::Interrupted`] when the runtime shuts down.
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

    /// Returns the image of `source` as its source stores it.
    async fn original(&self, source: Source) -> Result<Option<Cover>, CoverError> {
        let Source {
            cover_id,
            mtime,
            origin,
        } = source;
        let original = run_blocking(move || load(&cover_id, &origin)).await?;
        Ok(original.flatten().map(|original| Cover {
            bytes: Bytes::from(original.bytes),
            content_type: original.content_type,
            mtime,
        }))
    }

    /// Returns the image of `source` as a JPEG whose longer side is `size`
    /// pixels, from the cache or made now.
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
                    .map_err(|_| CoverError::Interrupted)?;
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
            // runtime interrupted it, the requests that wait find its sender
            // dropped and return the interruption.
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
        // held for as long as the pixels are in memory.
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
        let original = load(&job.cover_id, &job.origin)?;
        self.decodes.fetch_add(1, Ordering::Relaxed);
        match resize(&original.bytes, job.size, decode_limits()) {
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
    let Some(stored) = albums::cover(conn, album_id)? else {
        return Ok(None);
    };
    let origin = match stored.source.strip_prefix(EMBEDDED) {
        Some(track_id) => {
            if !matches!(ids::parse(track_id), Some((Kind::Track, _))) {
                tracing::warn!(
                    album = %stored.album_id,
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
            let path = PathBuf::from(&stored.source);
            let content_type = image_content_type(&path).filter(|_| path.is_absolute());
            let Some(content_type) = content_type else {
                tracing::warn!(
                    album = %stored.album_id,
                    path = %path.display(),
                    "the stored cover of the album is not an absolute path of an image file; \
                     the album has no cover"
                );
                return Ok(None);
            };
            Origin::File { path, content_type }
        }
    };
    Ok(Some(Source {
        cover_id: stored.album_id,
        mtime: stored.mtime,
        origin,
    }))
}

/// Returns the track `track_id` and the path of its file when the track has
/// a file and an embedded picture.
///
/// A track without a file has a placeholder path as its `rel_path`
/// (`docs/design.md` section 3.4). The function returns `None` for such a
/// track before it builds a path, so no file is read for it.
fn picture_file(conn: &Connection, track_id: &str) -> db::Result<Option<(Track, PathBuf)>> {
    let Some(track) = tracks::get_track(conn, track_id)? else {
        return Ok(None);
    };
    if !track.has_file() || !track.has_picture {
        return Ok(None);
    }
    let folder = folders::list(conn)?
        .into_iter()
        .find(|folder| folder.id == track.folder_id);
    let Some(folder) = folder else {
        return Ok(None);
    };
    let mut path = folder.path;
    path.extend(track.rel_path.split('/'));
    Ok(Some((track, path)))
}

/// Returns the content type of the image file at `path` from its extension,
/// without regard to case. Returns `None` for an extension that is not the
/// extension of a cover image file.
fn image_content_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    IMAGE_TYPES
        .into_iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(extension))
        .map(|(_, content_type)| content_type)
}

/// Returns the content type `named`, which `tags::read_front_picture`
/// reports for an embedded picture, as a static string. A type that is not
/// the type of an image format is [`UNKNOWN_TYPE`].
fn picture_content_type(named: &str) -> &'static str {
    ImageFormat::all()
        .map(|format| format.to_mime_type())
        .find(|known| *known == named)
        .unwrap_or(UNKNOWN_TYPE)
}

/// Reads the image at `origin`. Returns `None` when there is no image to
/// serve: the track has no picture, the file is gone, or the image cannot be
/// read. The last case is logged as a warning.
fn load(cover_id: &str, origin: &Origin) -> Option<Original> {
    match read_origin(origin) {
        Ok(original) => original,
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

/// Reads the image at `origin`. Returns `None` when `origin` is an audio
/// file without an embedded picture.
fn read_origin(origin: &Origin) -> Result<Option<Original>, LoadError> {
    let path = origin.path();
    // Opening a file that is not a regular file can block: a named pipe
    // without a writer never opens.
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(LoadError::NotAFile);
    }
    match origin {
        Origin::File { content_type, .. } => {
            if metadata.len() > MAX_SOURCE_BYTES {
                return Err(LoadError::TooLarge(metadata.len()));
            }
            // The file can grow after its size was read, so the read itself
            // stops one byte past the limit.
            let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
            File::open(path)?
                .take(MAX_SOURCE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            let len = bytes.len() as u64;
            if len > MAX_SOURCE_BYTES {
                return Err(LoadError::TooLarge(len));
            }
            Ok(Some(Original {
                bytes,
                content_type,
            }))
        }
        Origin::Embedded(_) => {
            let Some((bytes, content_type)) = tags::read_front_picture(path) else {
                return Ok(None);
            };
            let len = bytes.len() as u64;
            if len > MAX_SOURCE_BYTES {
                return Err(LoadError::TooLarge(len));
            }
            Ok(Some(Original {
                bytes,
                content_type: picture_content_type(&content_type),
            }))
        }
    }
}

/// Reads the cache file at `path`. Returns `None` when the cache does not
/// hold the file, and when the file cannot be read, which is logged.
fn read_cached(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
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

/// The limits of [`decode`] for a cover: [`MAX_SOURCE_SIDE`] and
/// [`MAX_DECODE_BYTES`].
fn decode_limits() -> Limits {
    let mut limits = Limits::no_limits();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

/// Decodes the image in `bytes`. The format is detected from the content.
///
/// The decoder reads the width and the height from the header of the image.
/// It returns an error before it allocates the pixels when a side is over
/// the limit of `limits`, and when the decoded image is larger than
/// `limits.max_alloc`.
fn decode(bytes: &[u8], limits: Limits) -> ImageResult<DynamicImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits);
    reader.decode()
}

/// Decodes the image in `bytes` within `limits` and returns it as a JPEG
/// whose longer side is `size` pixels. The proportions of the image are
/// kept, and the shorter side is at least one pixel. An image with an alpha
/// channel loses the channel, because JPEG has none.
fn resize(bytes: &[u8], size: u32, limits: Limits) -> ImageResult<Vec<u8>> {
    let resized = decode(bytes, limits)?.thumbnail(size, size).into_rgb8();
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY).encode_image(&resized)?;
    Ok(jpeg)
}

/// Writes `bytes` to the file `name` in the directory `cache_dir`, which is
/// created when it is missing.
///
/// The bytes are written to a new file with a random name in the same
/// directory, flushed to the disk, and the file is then renamed to `name`.
/// A reader therefore opens either no file or the whole file, also after a
/// power failure.
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
        Err(_) => Err(CoverError::Interrupted),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use image::{ImageError, Luma, LumaA, Rgb, Rgba};
    use tempfile::TempDir;

    use super::*;
    use crate::db::queries::artists;
    use crate::db::queries::tracks::TrackContent;
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

    /// Takes every decode permit of `covers`. No image is decoded until the
    /// returned permit is dropped.
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
    fn the_content_type_of_an_image_file_follows_its_extension() {
        let content_type = |name: &str| image_content_type(Path::new(name));
        assert_eq!(content_type("cover.jpg"), Some("image/jpeg"));
        assert_eq!(content_type("cover.jpeg"), Some("image/jpeg"));
        assert_eq!(content_type("Folder.PNG"), Some("image/png"));
        assert_eq!(content_type("front.WebP"), Some("image/webp"));
        assert_eq!(content_type("cover.gif"), None);
        assert_eq!(content_type("cover.jpg.bak"), None);
        assert_eq!(content_type("cover"), None);
        assert_eq!(content_type("01 Low Tide.flac"), None);
    }

    #[test]
    fn the_content_type_of_a_picture_is_one_of_the_image_types_or_unknown() {
        for known in [
            "image/jpeg",
            "image/png",
            "image/tiff",
            "image/bmp",
            "image/gif",
            "image/webp",
        ] {
            assert_eq!(picture_content_type(known), known);
        }
        assert_eq!(picture_content_type(UNKNOWN_TYPE), UNKNOWN_TYPE);
        assert_eq!(picture_content_type("text/html"), UNKNOWN_TYPE);
        assert_eq!(picture_content_type(""), UNKNOWN_TYPE);
    }

    #[test]
    fn a_wide_png_and_a_tall_jpeg_resize_to_their_proportions() {
        let resized = |bytes: &[u8], size: u32| {
            jpeg_dimensions(&resize(bytes, size, decode_limits()).expect("resize the image"))
        };
        // The longer side takes the size, also when the image is smaller.
        assert_eq!(resized(&png(4, 2), 2), (2, 1));
        assert_eq!(resized(&jpeg(2, 4), 2), (1, 2));
        assert_eq!(resized(&png(4, 2), 64), (64, 32));
        assert_eq!(resized(&jpeg(2, 4), 64), (32, 64));
        assert_eq!(resized(&png(4, 2), 4), (4, 2));
        assert_eq!(resized(&png(3, 3), 2), (2, 2));
        // The shorter side is at least one pixel.
        assert_eq!(resized(&png(400, 1), 4), (4, 1));
        assert_eq!(resized(&png(1, 400), 4), (1, 4));
    }

    #[test]
    fn every_pixel_format_and_webp_resize_to_an_rgb_jpeg() {
        let sources = [
            encoded(
                image::RgbaImage::from_pixel(6, 4, Rgba([10, 20, 30, 128])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayImage::from_pixel(6, 4, Luma([90])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayAlphaImage::from_pixel(6, 4, LumaA([90, 128])),
                ImageFormat::Png,
            ),
            encoded(
                image::ImageBuffer::<Rgb<u16>, _>::from_pixel(6, 4, Rgb([9000, 500, 65000])),
                ImageFormat::Png,
            ),
            encoded(
                image::GrayImage::from_pixel(6, 4, Luma([90])),
                ImageFormat::Jpeg,
            ),
            encoded(picture(6, 4), ImageFormat::WebP),
        ];
        for source in sources {
            let jpeg = resize(&source, 3, decode_limits()).expect("resize the image");
            assert_eq!(jpeg_dimensions(&jpeg), (3, 2));
            let decoded = image::load_from_memory(&jpeg).expect("decode the JPEG");
            assert_eq!(decoded.color(), image::ColorType::Rgb8);
        }
    }

    #[test]
    fn an_image_with_a_side_over_the_limit_is_not_decoded() {
        let limits_error = |bytes: &[u8]| {
            let error = decode(bytes, decode_limits()).expect_err("the image is over a limit");
            assert!(matches!(error, ImageError::Limits(_)), "{error}");
        };
        limits_error(&png(MAX_SOURCE_SIDE + 1, 1));
        limits_error(&png(1, MAX_SOURCE_SIDE + 1));
        limits_error(&jpeg(MAX_SOURCE_SIDE + 1, 1));

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
        let error = decode(&bomb, decode_limits()).expect_err("the image is over a limit");
        assert!(matches!(error, ImageError::Limits(_)), "{error}");
        assert!(start.elapsed() < Duration::from_secs(1));

        // The memory limit alone refuses the image, without the limit on
        // the sides.
        let mut memory_only = Limits::no_limits();
        memory_only.max_alloc = Some(MAX_DECODE_BYTES);
        let error = decode(&bomb, memory_only).expect_err("the image is over the limit");
        assert!(matches!(error, ImageError::Limits(_)), "{error}");

        // Four RGB pixels are 12 bytes.
        let mut eleven_bytes = decode_limits();
        eleven_bytes.max_alloc = Some(11);
        let error = decode(&png(2, 2), eleven_bytes).expect_err("the image is over the limit");
        assert!(matches!(error, ImageError::Limits(_)), "{error}");
        assert!(decode(&png(2, 2), decode_limits()).is_ok());
    }

    #[test]
    fn the_limits_of_a_cover_hold_an_image_of_the_largest_sides() {
        let limits = decode_limits();
        assert_eq!(limits.max_image_width, Some(8192));
        assert_eq!(limits.max_image_height, Some(8192));
        assert_eq!(limits.max_alloc, Some(8192 * 8192 * 4));
        assert_eq!(MAX_SOURCE_BYTES, 32 * 1024 * 1024);
    }

    #[test]
    fn bytes_that_are_no_image_are_an_error_of_the_decoder() {
        for bytes in [&b""[..], b"not an image", &png(4, 2)[..20]] {
            let error = decode(bytes, decode_limits()).expect_err("no image");
            assert!(!matches!(error, ImageError::Limits(_)), "{error}");
        }
    }

    #[tokio::test]
    async fn an_album_cover_without_a_size_is_the_file_with_the_type_of_its_extension() {
        let library = Library::new().await;
        let files = [
            ("Glass Harbour", "cover.jpg", jpeg(4, 2), "image/jpeg"),
            ("Paper Maps", "Folder.PNG", png(4, 2), "image/png"),
            (
                "Slow Rivers",
                "front.webp",
                encoded(picture(4, 2), ImageFormat::WebP),
                "image/webp",
            ),
        ];
        for (title, file, bytes, content_type) in files {
            let (id, _) = library.album_with_cover(title, file, &bytes).await;
            let cover = library.get(&id, None).await.expect("the album has a cover");
            assert_eq!(cover.bytes, bytes, "{file}");
            assert_eq!(cover.content_type, content_type, "{file}");
            assert_eq!(cover.mtime, MTIME);
            // A size of 0 asks for the same.
            assert_eq!(library.get(&id, Some(0)).await, Some(cover));
        }
        // Nothing was decoded and nothing was cached.
        assert_eq!(library.covers.decodes(), 0);
        assert!(!library.cache_dir().exists());
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
        assert_eq!(library.cache_files(), [name]);

        // Another size is another cover, and its source is gone.
        assert_eq!(library.get(&id, Some(32)).await, None);
        assert_eq!(library.get(&id, None).await, None);
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
        // The scanner removes the file of the old time (`scanner` tests).
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
        assert_eq!(library.cache_files(), [name]);
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
    async fn a_corrupt_image_has_no_sized_cover_and_is_served_without_a_size() {
        let library = Library::new().await;
        let truncated = &jpeg(64, 64)[..200];
        let sources = [
            ("Glass Harbour", &b"this is text, not a picture"[..]),
            ("Paper Maps", truncated),
            ("Slow Rivers", &b""[..]),
        ];
        for (index, (title, bytes)) in sources.into_iter().enumerate() {
            let (id, _) = library.album_with_cover(title, "cover.jpg", bytes).await;
            assert_eq!(library.get(&id, Some(64)).await, None, "{title}");
            assert_eq!(library.get(&id, Some(64)).await, None, "{title}");
            // Each sized request tried the image once.
            assert_eq!(library.covers.decodes(), 2 * (index as u64 + 1));

            let cover = library.get(&id, None).await.expect("the stored bytes");
            assert_eq!(cover.bytes, bytes, "{title}");
            assert_eq!(cover.content_type, "image/jpeg");
        }
        assert_eq!(library.cache_files(), [""; 0]);
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
            "it holds 33554433 bytes and the limit is 33554432"
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
                origin: Origin::File {
                    path,
                    content_type: "image/jpeg",
                },
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

        // While the test holds the permits, the job waits before it decodes
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
        let (id, _) = library
            .album_with_cover("Glass Harbour", "cover.jpg", b"not a picture")
            .await;
        let name = cache_name(&id, 64, MTIME);

        let permits = hold_decodes(&library.covers).await;
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let (covers, id) = (library.covers.clone(), id.clone());
                tokio::spawn(async move { covers.get(&id, Some(64)).await })
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
    async fn covers_of_different_sizes_are_made_side_by_side_within_the_permits() {
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

        // The job leaves while the test still holds the permits.
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
        let sizes = 1..=32;
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
