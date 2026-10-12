//! Tests of `library::covers` on the standard library after a scan. The
//! service reads the cover sources that the scanner stored, and it caches
//! resized covers in the `cache/covers` directory of a data directory, from
//! which the scanner removes them.
//!
//! The cover file and the embedded picture of the standard library are 2 × 2
//! pixels. No cover is enlarged, so a test that needs a smaller cover writes
//! a larger image first. A size is served at its step, and the smallest step
//! is 80 pixels.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{LibraryBuilder, PICTURE_PNG, TestLibrary, captured_log};
use image::{ImageFormat, Rgb, RgbImage};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::TagExt;
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
        Self::of(LibraryBuilder::standard().build()).await
    }

    /// Scans `library` into the database of a new data directory.
    async fn of(library: TestLibrary) -> Self {
        let data = tempfile::tempdir().expect("create the data directory");
        let data_dir = DataDir::resolve(Some(data.path())).expect("a data directory");
        let db = Db::open(data_dir.db_path()).expect("open the database");
        let folders = vec![library.root.path().to_path_buf()];
        let covers = Covers::new(data_dir.covers_cache_dir(), db.clone(), folders);
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

    /// Returns the stored cover source of the album with the name `name`.
    async fn cover_source(&self, name: &'static str) -> Option<String> {
        let sql = "SELECT cover_source FROM albums WHERE name = ?1";
        self.db
            .read(move |conn| Ok(conn.query_row(sql, [name], |row| row.get(0))?))
            .await
            .unwrap_or_else(|error| panic!("the cover source of {name}: {error}"))
    }

    /// Returns the path of the file `name` in the directory of the album
    /// `album` of `artist`.
    fn file(&self, artist: &str, album: &str, name: &str) -> PathBuf {
        file(&self.library, artist, album, name)
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

/// Returns the path of the file `name` in the directory of the album `album`
/// of `artist` in `library`.
fn file(library: &TestLibrary, artist: &str, album: &str, name: &str) -> PathBuf {
    library.root.path().join(artist).join(album).join(name)
}

/// Writes an image of `width` × `height` pixels to `path`. The extension of
/// the path selects the encoding.
fn write_image(path: &Path, width: u32, height: u32) {
    RgbImage::from_fn(width, height, |x, y| Rgb([x as u8, y as u8, 128]))
        .save(path)
        .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// Adds a front cover with the content `bytes` to the tag of the audio file
/// at `path`. The tag names `mime` as the type of the picture.
fn embed_picture(path: &Path, bytes: &[u8], mime: MimeType) {
    let mut file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let tag = file
        .primary_tag_mut()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()));
    tag.push_picture(
        Picture::unchecked(bytes.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(mime)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
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

    // The sources are the ones that the scan stored.
    let stored = scanned.cover_source("Glass Harbour").await;
    assert_eq!(stored.map(PathBuf::from), Some(cover_file));
    let track = scanned.track("Upstream").await;
    assert_eq!(
        scanned.cover_source("Slow Rivers").await,
        Some(format!("embedded:{track}"))
    );

    // A request without a size caches nothing.
    assert_eq!(scanned.cache_files(), [""; 0]);
}

#[tokio::test]
async fn a_sized_cover_is_a_jpeg_of_at_most_its_step_and_the_second_request_reads_the_cache() {
    let library = LibraryBuilder::standard().build();
    let cover_file = file(&library, "Aurora Field", "Glass Harbour", "cover.jpg");
    write_image(&cover_file, 160, 80);
    let scanned = Scanned::of(library).await;
    let first_track = scanned.file("Marlow Quartet", "Slow Rivers", "01 Upstream.m4a");
    // The size 64 is served at the step 80. The cover file is larger than
    // 80 pixels and is scaled down. The embedded picture is 2 × 2 pixels and
    // keeps its size.
    let albums = [
        ("Glass Harbour", mtime(&cover_file), (80, 40)),
        ("Slow Rivers", mtime(&first_track), (2, 2)),
    ];
    let long_ago = UNIX_EPOCH + Duration::from_secs(1_000_000_000);

    let mut names = Vec::new();
    for (name, source_mtime, dimensions) in albums {
        let id = scanned.album(name).await;
        let cover = scanned.get(&id, Some(64)).await.expect("a resized cover");
        assert_eq!(jpeg_dimensions(&cover), dimensions, "{name}");
        assert_eq!(cover.mtime, source_mtime);

        let name = format!("{id}-80-{source_mtime}.jpg");
        let cached = scanned.data_dir.covers_cache_dir().join(&name);
        assert_eq!(cover.bytes, fs::read(&cached).expect("read the cache file"));

        // A request that made the cover again would write a new file with
        // the present time. Another size of the step reads the same file.
        set_mtime(&cached, long_ago);
        assert_eq!(scanned.get(&id, Some(64)).await.as_ref(), Some(&cover));
        assert_eq!(scanned.get(&id, Some(80)).await, Some(cover));
        assert_eq!(mtime(&cached), 1_000_000_000);
        names.push(name);
    }
    names.sort();
    assert_eq!(scanned.cache_files(), names);
}

#[tokio::test]
async fn requests_for_the_sizes_290_300_and_310_create_one_cache_file() {
    let library = LibraryBuilder::standard().build();
    let cover_file = file(&library, "Aurora Field", "Glass Harbour", "cover.jpg");
    write_image(&cover_file, 640, 480);
    let scanned = Scanned::of(library).await;
    let id = scanned.album("Glass Harbour").await;

    let first = scanned.get(&id, Some(290)).await.expect("a resized cover");
    assert_eq!(jpeg_dimensions(&first), (320, 240));
    for size in [300, 310] {
        assert_eq!(scanned.get(&id, Some(size)).await.as_ref(), Some(&first));
    }
    assert_eq!(
        scanned.cache_files(),
        [format!("{id}-320-{}.jpg", mtime(&cover_file))]
    );
}

#[tokio::test]
async fn a_size_above_the_largest_step_is_the_largest_step_or_the_size_of_the_image() {
    // The cover file of one album is larger than the largest step. The
    // picture of the other album is 2 × 2 pixels.
    let library = LibraryBuilder::standard().build();
    let cover_file = file(&library, "Aurora Field", "Glass Harbour", "cover.png");
    write_image(&cover_file, 2400, 12);
    fs::remove_file(file(&library, "Aurora Field", "Glass Harbour", "cover.jpg"))
        .expect("remove the cover file");
    let scanned = Scanned::of(library).await;
    let large = scanned.album("Glass Harbour").await;
    let small = scanned.album("Slow Rivers").await;

    for size in [2001, 2400, 5000, u32::MAX] {
        let cover = scanned.get(&large, Some(size)).await.expect("a cover");
        assert_eq!(jpeg_dimensions(&cover), (2000, 10), "{size}");
        let cover = scanned.get(&small, Some(size)).await.expect("a cover");
        assert_eq!(jpeg_dimensions(&cover), (2, 2), "{size}");
    }
    // Each cover has the one file of the largest step.
    let files = scanned.cache_files();
    assert_eq!(files.len(), 2, "{files:?}");
    for id in [large, small] {
        let prefix = format!("{id}-2000-");
        assert!(
            files.iter().any(|name| name.starts_with(&prefix)),
            "{files:?}"
        );
    }
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

    // The picture is 2 × 2 pixels, and a sized cover is not enlarged.
    let resized = scanned.get(&pictured, Some(64)).await.expect("a cover");
    assert_eq!(jpeg_dimensions(&resized), (2, 2));
    assert_eq!(
        scanned.cache_files(),
        [format!("{pictured}-80-{}.jpg", mtime(&file))]
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
    assert_eq!(jpeg_dimensions(&old), (2, 2));
    assert_eq!(scanned.cache_files().len(), 2);

    // The new image is 256 × 128 pixels, and its file is one minute newer.
    write_image(&cover_file, 256, 128);
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
    assert_eq!(jpeg_dimensions(&new), (80, 40));
    assert_eq!(new.mtime, new_time);
    let mut names = vec![format!("{album}-80-{new_time}.jpg"), kept_name];
    names.sort();
    assert_eq!(scanned.cache_files(), names);
    assert_eq!(scanned.get(&other, Some(64)).await, Some(kept));
}

#[tokio::test]
async fn a_scan_removes_the_cached_covers_of_a_track_whose_row_it_deleted() {
    let scanned = Scanned::standard().await;
    let delta_file = scanned.file("Marlow Quartet", "Slow Rivers", "02 Delta.m4a");
    let delta = scanned.track("Delta").await;
    let upstream = scanned.track("Upstream").await;
    let album = scanned.album("Slow Rivers").await;
    let other = scanned.album("Glass Harbour").await;

    // Two steps of the track that the test deletes, and one cover of the
    // other track, of its album and of another album.
    for (id, size) in [
        (&delta, 80),
        (&delta, 320),
        (&upstream, 80),
        (&album, 80),
        (&other, 80),
    ] {
        scanned.get(id, Some(size)).await.expect("a cover");
    }
    let cache = scanned.data_dir.covers_cache_dir();
    // Files that an earlier version of the server left: a size that is no
    // step for a track that has its row, and for a track and an album that
    // have none.
    let gone_track = "tr-00000000000000000000000000000001";
    let gone_album = "al-00000000000000000000000000000001";
    let left = [
        format!("{upstream}-300-1000.jpg"),
        format!("{gone_track}-300-1000.jpg"),
        format!("{gone_track}-64-1000.jpg"),
        format!("{gone_album}-300-1000.jpg"),
    ];
    for name in &left {
        fs::write(cache.join(name), b"a resized cover").expect("write the file");
    }
    let before = scanned.cache_files();
    assert_eq!(before.len(), 9, "{before:?}");
    let of = |id: &str| -> Vec<String> {
        let prefix = format!("{id}-");
        scanned
            .cache_files()
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .collect()
    };
    assert_eq!(of(&delta).len(), 2);

    // The file of the track is gone, and the scan deletes its row.
    fs::remove_file(&delta_file).expect("remove the audio file");
    scanned.scan().await;
    assert_eq!(scanned.get(&delta, None).await, None);

    // The files of the deleted track and of the track without a row are
    // removed. The files of the track that has its row stay, and so does
    // every file of an album.
    assert_eq!(of(&delta), [""; 0]);
    assert_eq!(of(gone_track), [""; 0]);
    let mut expected: Vec<String> = before
        .into_iter()
        .filter(|name| !name.starts_with(&delta) && !name.starts_with(gone_track))
        .collect();
    expected.sort();
    assert_eq!(scanned.cache_files(), expected);
    assert_eq!(expected.len(), 5);
    assert_eq!(of(&upstream).len(), 2);
    assert_eq!(of(&album).len(), 1);
    assert_eq!(of(&other).len(), 1);
    assert_eq!(of(gone_album).len(), 1);

    // The music folder holds the files that it held, less the one that the
    // test removed.
    let slow_rivers = delta_file.parent().expect("the directory of the album");
    let names: Vec<_> = fs::read_dir(slow_rivers)
        .expect("read the directory")
        .map(|entry| entry.expect("a directory entry").file_name())
        .collect();
    assert_eq!(names, ["01 Upstream.m4a"]);
}

#[tokio::test]
async fn a_source_that_cannot_be_read_is_logged_at_info_and_a_source_that_is_gone_at_debug() {
    let log = captured_log();
    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let album = scanned.album("Glass Harbour").await;
    let lines = |text: &str| -> Vec<String> {
        log.lines_with(&album)
            .into_iter()
            .filter(|line| line.contains(text))
            .collect()
    };
    let unreadable = "the source of the cover cannot be read";
    let gone = "the source of the cover is gone";

    // A directory has the path of the cover file, so the source cannot be
    // read. Each request writes one line, with and without a size.
    fs::remove_file(&cover_file).expect("remove the cover file");
    fs::create_dir(&cover_file).expect("create a directory");
    for (requests, size) in [(1, None), (2, Some(64))] {
        assert_eq!(scanned.get(&album, size).await, None);
        let found = lines(unreadable);
        assert_eq!(found.len(), requests, "{found:?}");
        for line in &found {
            assert!(line.contains("INFO"), "{line}");
            assert!(line.contains("cover.jpg"), "{line}");
        }
    }

    // Nothing has the path of the cover file.
    fs::remove_dir(&cover_file).expect("remove the directory");
    assert_eq!(scanned.get(&album, None).await, None);
    let found = lines(gone);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("DEBUG"), "{}", found[0]);

    // No line about the cover is a warning.
    let warnings: Vec<String> = log
        .lines_with(&album)
        .into_iter()
        .filter(|line| line.contains("WARN") || line.contains("ERROR"))
        .collect();
    assert_eq!(warnings, [""; 0]);
    assert_eq!(scanned.cache_files(), [""; 0]);
}

#[tokio::test]
async fn a_cover_file_that_is_no_image_logs_a_warning_and_is_not_served() {
    let log = captured_log();
    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let album = scanned.album("Glass Harbour").await;
    fs::write(&cover_file, b"this is text, not a picture").expect("replace the cover");

    // The lines of the log about the cover of the album that hold `text`.
    let warnings = |text: &str| -> Vec<String> {
        log.lines_with(&album)
            .into_iter()
            .filter(|line| line.contains(text))
            .collect()
    };
    let decoded = "the cover image cannot be decoded";
    let served = "is not an image that the service serves";

    // With a size the bytes are not decoded.
    assert_eq!(scanned.get(&album, Some(64)).await, None);
    assert_eq!(scanned.cache_files(), [""; 0]);
    let lines = warnings(decoded);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    assert!(lines[0].contains("cover.jpg"), "{}", lines[0]);
    assert_eq!(warnings(served).len(), 0);

    // Without a size the bytes are not returned: they are no image.
    assert_eq!(scanned.get(&album, None).await, None);
    let lines = warnings(served);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("WARN"), "{}", lines[0]);
    assert!(lines[0].contains("cover.jpg"), "{}", lines[0]);
    assert_eq!(warnings(decoded).len(), 1);
}

#[tokio::test]
async fn an_embedded_picture_is_served_when_it_is_a_jpeg_png_or_webp_image() {
    // Three FLAC tracks of "Glass Harbour" get a picture. The tag of each
    // names JPEG as the type.
    let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";
    let library = LibraryBuilder::standard().build();
    let track_file = |name: &str| file(&library, "Aurora Field", "Glass Harbour", name);
    embed_picture(
        &track_file("01 Low Tide.flac"),
        b"this is text, not a picture",
        MimeType::Jpeg,
    );
    embed_picture(&track_file("02 Breakwater.flac"), gif, MimeType::Jpeg);
    embed_picture(
        &track_file("03 Night Ferry.flac"),
        &PICTURE_PNG,
        MimeType::Jpeg,
    );
    let scanned = Scanned::of(library).await;

    // Bytes that are no image, and an image in a format that the service
    // does not serve, are not returned.
    for title in ["Low Tide", "Breakwater"] {
        let track = scanned.track(title).await;
        for size in [None, Some(0), Some(64)] {
            assert_eq!(scanned.get(&track, size).await, None, "{title} {size:?}");
        }
    }
    assert_eq!(scanned.cache_files(), [""; 0]);

    // The type of a picture is the type of its content, not the type that
    // the tag names.
    let track = scanned.track("Night Ferry").await;
    let cover = scanned.get(&track, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    assert_eq!(cover.content_type, "image/png");
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn a_cover_file_that_is_a_symbolic_link_is_no_cover() {
    use std::os::unix::fs::symlink;

    // An image outside the music folder.
    let outside = tempfile::tempdir().expect("create a directory");
    let image = outside.path().join("elsewhere.jpg");
    write_image(&image, 8, 8);

    // "Glass Harbour" gets a link in the place of its cover file. "Slow
    // Rivers", which has an embedded picture, gets a link as a cover file.
    let library = LibraryBuilder::standard().build();
    let replaced = file(&library, "Aurora Field", "Glass Harbour", "cover.jpg");
    fs::remove_file(&replaced).expect("remove the cover file");
    symlink(&image, &replaced).expect("create the link");
    let added = file(&library, "Marlow Quartet", "Slow Rivers", "cover.jpg");
    symlink(&image, &added).expect("create the link");
    let scanned = Scanned::of(library).await;

    // The scanner stores no link as a cover, so the first album has no
    // cover and the second one keeps its embedded picture.
    assert_eq!(scanned.cover_source("Glass Harbour").await, None);
    let album = scanned.album("Glass Harbour").await;
    for size in [None, Some(64)] {
        assert_eq!(scanned.get(&album, size).await, None);
    }
    let track = scanned.track("Upstream").await;
    assert_eq!(
        scanned.cover_source("Slow Rivers").await,
        Some(format!("embedded:{track}"))
    );
    let album = scanned.album("Slow Rivers").await;
    let cover = scanned.get(&album, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    assert_eq!(scanned.cache_files(), [""; 0]);
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn a_cover_file_that_becomes_a_symbolic_link_after_the_scan_is_not_read() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().expect("create a directory");
    let image = outside.path().join("elsewhere.jpg");
    write_image(&image, 8, 8);

    let scanned = Scanned::standard().await;
    let cover_file = scanned.file("Aurora Field", "Glass Harbour", "cover.jpg");
    let album = scanned.album("Glass Harbour").await;
    assert!(scanned.get(&album, None).await.is_some());

    // The stored source still names the path. No scan has run since the
    // file became a link.
    fs::remove_file(&cover_file).expect("remove the cover file");
    symlink(&image, &cover_file).expect("create the link");
    assert!(scanned.cover_source("Glass Harbour").await.is_some());
    for size in [None, Some(64)] {
        assert_eq!(scanned.get(&album, size).await, None);
    }
    assert_eq!(scanned.cache_files(), [""; 0]);

    // The next scan clears the stored source.
    scanned.scan().await;
    assert_eq!(scanned.cover_source("Glass Harbour").await, None);
    assert_eq!(scanned.get(&album, None).await, None);
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn a_link_in_the_place_of_the_cover_file_of_a_parent_directory_is_not_read() {
    use std::os::unix::fs::symlink;

    // An image outside the music folder.
    let outside = tempfile::tempdir().expect("create a directory");
    let image = outside.path().join("elsewhere.jpg");
    write_image(&image, 8, 8);

    // Each disc of "Long Play" has a directory of its own, and the cover
    // file is in their parent directory. The tracks hold a picture.
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Long Play", "Side A", 1, "flac")
        .disc(1)
        .picture()
        .track("Aurora Field", "Long Play", "Side C", 1, "flac")
        .disc(2)
        .picture()
        .build();
    let album_dir = library.root.path().join("Aurora Field").join("Long Play");
    for (name, disc) in [("01 Side A.flac", "CD1"), ("01 Side C.flac", "CD2")] {
        fs::create_dir(album_dir.join(disc)).expect("create the disc directory");
        fs::rename(album_dir.join(name), album_dir.join(disc).join(name)).expect("move the track");
    }
    let cover_file = album_dir.join("cover.jpg");
    write_image(&cover_file, 16, 8);
    let scanned = Scanned::of(library).await;

    // The scan stored the file of the parent directory, and the service
    // reads it.
    let album = scanned.album("Long Play").await;
    let stored = cover_file.to_str().expect("a Unicode path").to_owned();
    assert_eq!(scanned.cover_source("Long Play").await, Some(stored));
    let cover = scanned.get(&album, None).await.expect("a cover");
    assert_eq!(cover.bytes, fs::read(&cover_file).expect("read the cover"));

    // The file becomes a link to the image outside the folder. The stored
    // source still names the path. No scan has run since.
    fs::remove_file(&cover_file).expect("remove the cover file");
    symlink(&image, &cover_file).expect("create the link");
    assert!(scanned.cover_source("Long Play").await.is_some());
    for size in [None, Some(4)] {
        assert_eq!(scanned.get(&album, size).await, None);
    }
    assert_eq!(scanned.cache_files(), [""; 0]);

    // The next scan stores no link. The cover is the embedded picture of
    // the first track.
    scanned.scan().await;
    let track = scanned.track("Side A").await;
    assert_eq!(
        scanned.cover_source("Long Play").await,
        Some(format!("embedded:{track}"))
    );
    let cover = scanned.get(&album, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn an_audio_file_that_is_a_symbolic_link_gives_its_embedded_picture() {
    use std::os::unix::fs::symlink;

    // The audio file is in a directory of the music folder that the scanner
    // does not walk. The scanner follows a link to it and stores its tags.
    // The service reads the picture of the same file.
    let library = LibraryBuilder::standard().build();
    let linked = file(&library, "Marlow Quartet", "Slow Rivers", "02 Delta.m4a");
    let store = library.root.path().join(".store");
    fs::create_dir(&store).expect("create a directory");
    let target = store.join("delta.m4a");
    fs::copy(&linked, &target).expect("copy the audio file");
    fs::remove_file(&linked).expect("remove the audio file");
    symlink(&target, &linked).expect("create the link");
    let scanned = Scanned::of(library).await;

    let track = scanned.track("Delta").await;
    let cover = scanned.get(&track, None).await.expect("a cover");
    assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    assert_eq!(cover.content_type, "image/png");
}

/// Moves the directory `dir` of a music folder to `outside` and puts a
/// symbolic link to its new place at its path, as a link does that changed
/// after the scan. The files below `dir` keep their paths in the library.
#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
fn link_out_of_the_folder(dir: &Path, outside: &Path) {
    use std::os::unix::fs::symlink;

    let moved = outside.join(dir.file_name().expect("the name of the directory"));
    fs::rename(dir, &moved).expect("move the directory");
    symlink(&moved, dir).expect("create the link");
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn a_cover_file_below_a_directory_link_out_of_the_folder_is_not_served() {
    let log = captured_log();
    let outside = tempfile::tempdir().expect("create a directory");
    let library = LibraryBuilder::standard().build();
    let cover_file = file(&library, "Aurora Field", "Glass Harbour", "cover.jpg");
    write_image(&cover_file, 160, 80);
    let scanned = Scanned::of(library).await;
    let album = scanned.album("Glass Harbour").await;
    let stored = fs::read(&cover_file).expect("read the cover");

    // The cover is served while its file is in the music folder, and the
    // cache holds the cover of one step.
    let original = scanned.get(&album, None).await.expect("a cover");
    assert_eq!(original.bytes, stored);
    let resized = scanned
        .get(&album, Some(80))
        .await
        .expect("a resized cover");
    assert_eq!(jpeg_dimensions(&resized), (80, 40));
    let cached = scanned.cache_files();
    assert_eq!(cached.len(), 1, "{cached:?}");

    // The directory of the album becomes a link to a directory outside the
    // music folder. The stored source names the same path, and the file at
    // that path is the same regular file.
    let album_dir = cover_file.parent().expect("the directory of the album");
    link_out_of_the_folder(album_dir, outside.path());
    assert_eq!(
        fs::read(&cover_file).expect("read through the link"),
        stored
    );
    let state = fs::symlink_metadata(&cover_file).expect("the state of the cover file");
    assert!(state.is_file());

    // The cover is not served: not the file, not a cover of a step that the
    // cache does not hold, and not the cover that the cache holds.
    for size in [None, Some(0), Some(320), Some(80), Some(64), Some(5000)] {
        assert_eq!(scanned.get(&album, size).await, None, "{size:?}");
    }
    // No cache file was written.
    assert_eq!(scanned.cache_files(), cached);

    // Each request wrote one line at the level `info`, as `stream` does.
    let lines: Vec<String> = log
        .lines_with(&album)
        .into_iter()
        .filter(|line| line.contains("is outside the music folders"))
        .collect();
    assert_eq!(lines.len(), 6, "{lines:?}");
    for line in &lines {
        assert!(line.contains("INFO"), "{line}");
    }
}

#[cfg(unix)] // Creating a symbolic link on Windows needs a privilege.
#[tokio::test]
async fn an_embedded_picture_below_a_directory_link_out_of_the_folder_is_not_served() {
    let outside = tempfile::tempdir().expect("create a directory");
    let scanned = Scanned::standard().await;
    let album = scanned.album("Slow Rivers").await;
    let track = scanned.track("Delta").await;
    for id in [&album, &track] {
        let cover = scanned.get(id, None).await.expect("a cover");
        assert_eq!(cover.bytes, &PICTURE_PNG[..]);
    }

    // The audio files of the album are regular files below a link that
    // leaves the music folder.
    let audio = scanned.file("Marlow Quartet", "Slow Rivers", "02 Delta.m4a");
    let album_dir = audio.parent().expect("the directory of the album");
    link_out_of_the_folder(album_dir, outside.path());
    assert!(audio.is_file());

    // The picture of the track is not read, for the ID of the track and as
    // the cover of the album.
    for id in [&album, &track] {
        for size in [None, Some(80)] {
            assert_eq!(scanned.get(id, size).await, None, "{id} {size:?}");
        }
    }
    assert_eq!(scanned.cache_files(), [""; 0]);
}
