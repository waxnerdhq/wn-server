//! Tests of `library::covers` on the standard library after a scan. The
//! service reads the cover sources that the scanner stored, and it caches
//! resized covers in the `cache/covers` directory of a data directory, from
//! which the scanner removes them.
//!
//! The cover file and the embedded picture of the standard library are 2 × 2
//! pixels, so a resized cover of 64 pixels is an enlarged image.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{LibraryBuilder, PICTURE_PNG, TestLibrary, captured_log};
use image::{ImageFormat, Rgb, RgbImage};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wn_server::config::DataDir;
use wn_server::db::Db;
use wn_server::db::queries::tracks;
use wn_server::library::covers::{Cover, Covers};
use wn_server::library::scanner::{self, ScanOptions};

/// The standard library, scanned into the database of a data directory, and
/// the cover service of that data directory.
struct Scanned {
    library: TestLibrary,
    data_dir: DataDir,
    db: Db,
    covers: Covers,
    /// The directory of the data directory. Dropping the value deletes it.
    _data: TempDir,
}

impl Scanned {
    async fn standard() -> Self {
        let library = LibraryBuilder::standard().build();
        let data = tempfile::tempdir().expect("create the data directory");
        let data_dir = DataDir::resolve(Some(data.path())).expect("a data directory");
        let db = Db::open(data_dir.db_path()).expect("open the database");
        let covers = Covers::new(data_dir.covers_cache_dir(), db.clone());
        let scanned = Self {
            library,
            data_dir,
            db,
            covers,
            _data: data,
        };
        scanned.scan().await;
        scanned
    }

    /// Scans the library as the server does: with the cover cache of the
    /// data directory.
    async fn scan(&self) {
        let db = self.db.clone();
        let folders = [self.library.root.path().to_path_buf()];
        let options = ScanOptions {
            covers_cache_dir: Some(self.data_dir.covers_cache_dir()),
            ..ScanOptions::default()
        };
        let summary = tokio::task::spawn_blocking(move || {
            scanner::scan(&db, &folders, &CancellationToken::new(), false, options)
        })
        .await
        .expect("the scan does not panic")
        .expect("the scan returns a summary");
        assert_eq!(summary.errors, [""; 0]);
    }

    /// Returns the one value of the one row that `sql` selects for `value`.
    async fn id(&self, sql: &'static str, value: &'static str) -> String {
        self.db
            .read(move |conn| Ok(conn.query_row(sql, [value], |row| row.get(0))?))
            .await
            .unwrap_or_else(|error| panic!("{sql} with {value}: {error}"))
    }

    /// Returns the ID of the album with the name `name`.
    async fn album(&self, name: &'static str) -> String {
        self.id("SELECT id FROM albums WHERE name = ?1", name).await
    }

    /// Returns the ID of the track with the title `title`.
    async fn track(&self, title: &'static str) -> String {
        self.id("SELECT id FROM tracks WHERE title = ?1", title)
            .await
    }

    /// Returns the path of the file `name` in the directory of the album
    /// `album` of `artist`.
    fn file(&self, artist: &str, album: &str, name: &str) -> PathBuf {
        self.library.root.path().join(artist).join(album).join(name)
    }

    async fn get(&self, id: &str, size: Option<u32>) -> Option<Cover> {
        self.covers.get(id, size).await.expect("no error")
    }

    /// Returns the names of the files in the cover cache in order. A
    /// directory that does not exist holds no file.
    fn cache_files(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.data_dir.covers_cache_dir()) else {
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

/// Returns the modification time of the file at `path` in Unix seconds.
fn mtime(path: &Path) -> i64 {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or_else(|error| panic!("read the time of {}: {error}", path.display()));
    let seconds = modified
        .duration_since(UNIX_EPOCH)
        .expect("a time after 1970")
        .as_secs();
    i64::try_from(seconds).expect("a time that fits")
}

/// Sets the modification time of the file at `path`.
fn set_mtime(path: &Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(time))
        .unwrap_or_else(|error| panic!("set the time of {}: {error}", path.display()));
}

/// Checks that `cover` is a JPEG and returns its width and height.
fn jpeg_dimensions(cover: &Cover) -> (u32, u32) {
    assert_eq!(cover.content_type, "image/jpeg");
    assert_eq!(
        image::guess_format(&cover.bytes).expect("a known image format"),
        ImageFormat::Jpeg
    );
    let image = image::load_from_memory(&cover.bytes).expect("decode the JPEG");
    (image.width(), image.height())
}

#[tokio::test]
async fn an_album_cover_without_a_size_is_the_cover_file_or_the_embedded_picture() {
    let scanned = Scanned::standard().await;

    // "Glass Harbour" has the file `cover.jpg`.
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let file_backed = scanned.album("Glass Harbour").await;
    let cover = scanned.get(&file_backed, None).await.expect("a cover");
    assert_eq!(cover.bytes, fs::read(&cover_file).expect("read the cover"));
    assert_eq!(cover.content_type, "image/jpeg");
    assert_eq!(cover.mtime, mtime(&cover_file));

    // "Slow Rivers" has no image file. Its cover is the picture of its
    // first track, and the time is the time of that track.
    let first_track = scanned.file("Marlow Quartet", "Slow Rivers", "01 Upstream.m4a");
    let embedded = scanned.album("Slow Rivers").await;
    let cover = scanned.get(&embedded, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    assert_eq!(cover.content_type, "image/png");
    assert_eq!(cover.mtime, mtime(&first_track));

    // A request without a size caches nothing.
    assert_eq!(scanned.cache_files(), [""; 0]);
}

#[tokio::test]
async fn a_sized_cover_is_a_jpeg_of_that_size_and_the_second_request_reads_the_cache() {
    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let first_track = scanned.file("Marlow Quartet", "Slow Rivers", "01 Upstream.m4a");
    let albums = [
        (scanned.album("Glass Harbour").await, mtime(&cover_file)),
        (scanned.album("Slow Rivers").await, mtime(&first_track)),
    ];
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);

    let mut names = Vec::new();
    for (id, source_mtime) in albums {
        let cover = scanned.get(&id, Some(64)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover), (64, 64));
        assert_eq!(cover.mtime, source_mtime);

        let name = format!("{id}-64-{source_mtime}.jpg");
        let cached = scanned.data_dir.covers_cache_dir().join(&name);
        assert_eq!(cover.bytes, fs::read(&cached).expect("read the cache file"));

        // A request that made the cover again would write a new file with
        // the present time.
        set_mtime(&cached, long_ago);
        assert_eq!(scanned.get(&id, Some(64)).await, Some(cover));
        assert_eq!(mtime(&cached), 1_000_000_000);
        names.push(name);
    }
    names.sort();
    assert_eq!(scanned.cache_files(), names);
}

#[tokio::test]
async fn a_track_cover_is_the_embedded_picture_of_the_track() {
    let scanned = Scanned::standard().await;
    let file = scanned.file("Marlow Quartet", "Slow Rivers", "02 Delta.m4a");
    let pictured = scanned.track("Delta").await;

    let cover = scanned.get(&pictured, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    assert_eq!(cover.content_type, "image/png");
    assert_eq!(cover.mtime, mtime(&file));

    let resized = scanned.get(&pictured, Some(64)).await.expect("a cover");
    assert_eq!(jpeg_dimensions(&resized), (64, 64));
    assert_eq!(
        scanned.cache_files(),
        [format!("{pictured}-64-{}.jpg", mtime(&file))]
    );

    // A track without a picture, an album without a cover file and without
    // a picture, and IDs that are not the ID of an album or a track.
    let without = [
        scanned.track("Low Tide").await,
        scanned.track("Open Road").await,
        scanned.album("Paper Maps").await,
        scanned.album("Summer Sampler").await,
        "al-00000000000000000000000000000000".to_owned(),
        "tr-00000000000000000000000000000000".to_owned(),
        "cover.jpg".to_owned(),
        "../Glass Harbour/cover.jpg".to_owned(),
        String::new(),
    ];
    for id in without {
        assert_eq!(scanned.get(&id, None).await, None, "{id:?}");
        assert_eq!(scanned.get(&id, Some(64)).await, None, "{id:?}");
    }
}

#[tokio::test]
async fn a_track_without_a_file_gives_no_picture_to_itself_or_to_its_album() {
    let scanned = Scanned::standard().await;
    let album = scanned.album("Slow Rivers").await;
    let track = scanned.track("Upstream").await;
    assert!(scanned.get(&track, None).await.is_some());
    assert!(scanned.get(&album, None).await.is_some());

    // The row gets its placeholder path, as the scanner gives it to a track
    // that it keeps without a file. The file with the picture stays on disk.
    let id = track.clone();
    scanned
        .db
        .write(move |conn| {
            let row = tracks::get_track(conn, &id)?.expect("the track");
            let placeholder = tracks::placeholder_path(&id);
            tracks::relink_track(conn, &id, row.folder_id, &placeholder, row.mtime, row.size)
        })
        .await
        .expect("give the track its placeholder path");

    for size in [None, Some(64)] {
        assert_eq!(scanned.get(&track, size).await, None);
        // The stored cover of the album still names the track.
        assert_eq!(scanned.get(&album, size).await, None);
    }
    assert_eq!(scanned.cache_files(), [""; 0]);
}

#[tokio::test]
async fn a_scan_removes_the_cached_cover_of_a_cover_file_that_changed() {
    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let album = scanned.album("Glass Harbour").await;
    let other = scanned.album("Slow Rivers").await;

    let old = scanned.get(&album, Some(64)).await.expect("a cover");
    let kept = scanned.get(&other, Some(64)).await.expect("a cover");
    let old_time = mtime(&cover_file);
    assert_eq!(jpeg_dimensions(&old), (64, 64));
    assert_eq!(scanned.cache_files().len(), 2);

    // The new image is 4 × 2 pixels, and its file is one minute newer.
    RgbImage::from_pixel(4, 2, Rgb([200, 40, 40]))
        .save(&cover_file)
        .expect("write the new cover");
    let new_time = old_time + 60;
    set_mtime(
        &cover_file,
        UNIX_EPOCH + Duration::from_secs(new_time.unsigned_abs()),
    );
    scanned.scan().await;

    // The scan removed the cached cover of the album and no other file.
    let kept_name = scanned
        .cache_files()
        .into_iter()
        .find(|name| name.starts_with(&other))
        .expect("the cached cover of the other album");
    assert_eq!(scanned.cache_files(), [kept_name.as_str()]);

    let new = scanned.get(&album, Some(64)).await.expect("a cover");
    assert_eq!(jpeg_dimensions(&new), (64, 32));
    assert_eq!(new.mtime, new_time);
    let mut names = vec![format!("{album}-64-{new_time}.jpg"), kept_name];
    names.sort();
    assert_eq!(scanned.cache_files(), names);
    assert_eq!(scanned.get(&other, Some(64)).await, Some(kept));
}

#[tokio::test]
async fn a_corrupt_cover_file_logs_a_warning_and_is_served_without_a_size_only() {
    let log = captured_log();
    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let album = scanned.album("Glass Harbour").await;
    fs::write(&cover_file, b"this is text, not a picture").expect("damage the cover");

    // The lines of the log that say that the cover of the album cannot be
    // decoded.
    let warnings = || -> Vec<String> {
        log.lines_with(&album)
            .into_iter()
            .filter(|line| line.contains("the cover image cannot be decoded"))
            .collect()
    };

    assert_eq!(scanned.get(&album, Some(64)).await, None);
    assert_eq!(scanned.cache_files(), [""; 0]);
    let lines = warnings();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    assert!(lines[0].contains("cover.jpg"), "{}", lines[0]);

    let cover = scanned.get(&album, None).await.expect("the stored bytes");
    assert_eq!(cover.bytes, &b"this is text, not a picture"[..]);
    assert_eq!(cover.content_type, "image/jpeg");
    // A request without a size decodes nothing and logs no such line.
    assert_eq!(warnings().len(), 1);
}
