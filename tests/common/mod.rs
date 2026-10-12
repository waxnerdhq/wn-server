//! Test support shared by the integration tests. A test file includes it with
//! `mod common;`.

// Each test binary compiles this module separately and uses a subset of it.
// The items one binary does not use are not dead code.
#![allow(dead_code)]

pub mod transcoder;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use image::{Rgb, RgbImage};
use lofty::config::WriteOptions;
use lofty::file::FileType;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, Tag, TagExt};
use md5::{Digest, Md5};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use transcoder::FakeTranscoder;
use wn_server::api::{self, AppState};
use wn_server::config::Config;
use wn_server::db::Db;
use wn_server::db::queries::users;
use wn_server::library::scan_controller::{ScanController, ScanHooks, ScanSettings, StartOutcome};

/// The picture that [`LibraryBuilder::picture`] embeds: a 2×2 PNG, 8-bit RGB,
/// with one red, one green, one blue and one white pixel.
pub const PICTURE_PNG: [u8; 75] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a,
    0x73, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xc0,
    0x00, 0xc2, 0x0c, 0xff, 0x81, 0x00, 0x00, 0x1f, 0xee, 0x05, 0xfb, 0xf1, 0xab, 0xba, 0x77, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Returns the directory of the committed audio fixtures,
/// `tests/fixtures/audio`.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("audio")
}

/// One track of a library that is being built.
struct TrackSpec {
    artist: String,
    album: String,
    title: String,
    track_no: u32,
    ext: String,
    year: Option<u32>,
    genre: Option<String>,
    disc: Option<u32>,
    album_artist: Option<String>,
    compilation: bool,
    picture: bool,
}

/// One cover image file of a library that is being built.
struct CoverSpec {
    artist: String,
    album: String,
    name: String,
}

/// Builds a music library in a temporary directory. Each track is a copy of
/// the fixture that has the track's extension. It is stored as
/// `<artist>/<album>/<nn> <title>.<ext>`, where `nn` is the track number in
/// two digits, and it is tagged with lofty.
pub struct LibraryBuilder {
    root: TempDir,
    tracks: Vec<TrackSpec>,
    covers: Vec<CoverSpec>,
}

/// A library that [`LibraryBuilder::build`] wrote. Dropping the value deletes
/// the directory.
pub struct TestLibrary {
    /// The directory that holds the library.
    pub root: TempDir,
    /// The absolute path of each track, in the order the tracks were added.
    pub files: Vec<PathBuf>,
}

impl LibraryBuilder {
    /// Creates an empty library in a new temporary directory.
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("create the library directory");
        Self {
            root,
            tracks: Vec::new(),
            covers: Vec::new(),
        }
    }

    /// Returns the builder of the standard catalogue: 3 artists, 4 albums and
    /// 10 tracks in the five fixture formats.
    ///
    /// - Aurora Field, "Glass Harbour" (2019, Electronic): 3 FLAC tracks and
    ///   a `cover.jpg` file.
    /// - Aurora Field, "Paper Maps" (2021, Electronic): 3 MP3 tracks, two on
    ///   disc 1 and one on disc 2.
    /// - Marlow Quartet, "Slow Rivers" (2015, Jazz): 2 M4A tracks with an
    ///   album-artist tag and an embedded picture.
    /// - "Summer Sampler" (2020, Pop): a compilation of one Ogg Vorbis track
    ///   by Tessa Lund and one Opus track by Aurora Field. Both tracks have
    ///   the compilation flag and no album-artist tag.
    ///
    /// The tags name three track artists. By the album identity rule in
    /// `docs/design.md` §1.3 the album artists are Aurora Field, Marlow
    /// Quartet and Various Artists.
    ///
    /// Every name is a valid Windows path component and no two names differ
    /// only by case.
    pub fn standard() -> Self {
        Self::new()
            .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "flac")
            .year(2019)
            .genre("Electronic")
            .track("Aurora Field", "Glass Harbour", "Breakwater", 2, "flac")
            .year(2019)
            .genre("Electronic")
            .track("Aurora Field", "Glass Harbour", "Night Ferry", 3, "flac")
            .year(2019)
            .genre("Electronic")
            .cover_file("Aurora Field", "Glass Harbour", "cover.jpg")
            .track("Aurora Field", "Paper Maps", "Meridian", 1, "mp3")
            .year(2021)
            .genre("Electronic")
            .disc(1)
            .track("Aurora Field", "Paper Maps", "Contour", 2, "mp3")
            .year(2021)
            .genre("Electronic")
            .disc(1)
            .track("Aurora Field", "Paper Maps", "Legend", 1, "mp3")
            .year(2021)
            .genre("Electronic")
            .disc(2)
            .track("Marlow Quartet", "Slow Rivers", "Upstream", 1, "m4a")
            .year(2015)
            .genre("Jazz")
            .album_artist("Marlow Quartet")
            .picture()
            .track("Marlow Quartet", "Slow Rivers", "Delta", 2, "m4a")
            .year(2015)
            .genre("Jazz")
            .album_artist("Marlow Quartet")
            .picture()
            .track("Tessa Lund", "Summer Sampler", "Open Road", 1, "ogg")
            .year(2020)
            .genre("Pop")
            .compilation()
            .track("Aurora Field", "Summer Sampler", "Last Train", 2, "opus")
            .year(2020)
            .genre("Pop")
            .compilation()
    }

    /// Adds a track. `ext` selects the fixture and is one of `flac`, `mp3`,
    /// `m4a`, `ogg` and `opus`. The methods `year`, `genre`, `disc`,
    /// `album_artist`, `compilation` and `picture` change the track that was
    /// added last.
    pub fn track(
        mut self,
        artist: &str,
        album: &str,
        title: &str,
        track_no: u32,
        ext: &str,
    ) -> Self {
        self.tracks.push(TrackSpec {
            artist: artist.to_owned(),
            album: album.to_owned(),
            title: title.to_owned(),
            track_no,
            ext: ext.to_owned(),
            year: None,
            genre: None,
            disc: None,
            album_artist: None,
            compilation: false,
            picture: false,
        });
        self
    }

    /// Sets the recording year of the last track.
    pub fn year(mut self, year: u32) -> Self {
        self.last().year = Some(year);
        self
    }

    /// Sets the genre of the last track.
    pub fn genre(mut self, genre: &str) -> Self {
        self.last().genre = Some(genre.to_owned());
        self
    }

    /// Sets the disc number of the last track.
    pub fn disc(mut self, disc: u32) -> Self {
        self.last().disc = Some(disc);
        self
    }

    /// Sets the album artist of the last track.
    pub fn album_artist(mut self, album_artist: &str) -> Self {
        self.last().album_artist = Some(album_artist.to_owned());
        self
    }

    /// Sets the compilation flag of the last track.
    pub fn compilation(mut self) -> Self {
        self.last().compilation = true;
        self
    }

    /// Embeds [`PICTURE_PNG`] in the last track as the front cover. M4A does
    /// not store a picture type, so lofty reads the picture of an M4A file
    /// back with the type `Other`.
    pub fn picture(mut self) -> Self {
        self.last().picture = true;
        self
    }

    /// Adds the image file `name` to the directory `<artist>/<album>`. The
    /// extension of `name` selects the encoding and is one of `jpg`, `jpeg`,
    /// `png` and `webp`. The image is 2×2 pixels of one colour.
    pub fn cover_file(mut self, artist: &str, album: &str, name: &str) -> Self {
        self.covers.push(CoverSpec {
            artist: artist.to_owned(),
            album: album.to_owned(),
            name: name.to_owned(),
        });
        self
    }

    /// Writes the tracks and the cover files and returns the library.
    pub fn build(self) -> TestLibrary {
        let mut files = Vec::with_capacity(self.tracks.len());
        for track in &self.tracks {
            let dir = self.album_dir(&track.artist, &track.album);
            let name = format!("{:02} {}.{}", track.track_no, track.title, track.ext);
            let path = dir.join(name);
            assert!(!path.exists(), "two tracks map to {}", path.display());
            let fixture = fixtures_dir().join(format!("a.{}", track.ext));
            fs::copy(&fixture, &path).unwrap_or_else(|error| {
                panic!("copy {} to {}: {error}", fixture.display(), path.display())
            });
            write_tags(&path, track);
            files.push(path);
        }
        for cover in &self.covers {
            let path = self
                .album_dir(&cover.artist, &cover.album)
                .join(&cover.name);
            RgbImage::from_pixel(2, 2, Rgb([32, 96, 160]))
                .save(&path)
                .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        }
        TestLibrary {
            root: self.root,
            files,
        }
    }

    /// Returns the track that was added last.
    fn last(&mut self) -> &mut TrackSpec {
        self.tracks
            .last_mut()
            .expect("a track is added before one of its fields is set")
    }

    /// Creates the directory `<artist>/<album>` when it is missing and returns
    /// its path.
    fn album_dir(&self, artist: &str, album: &str) -> PathBuf {
        let dir = self.root.path().join(artist).join(album);
        fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
        dir
    }
}

/// Replaces the tag of the audio file at `path` with the fields of `track`.
/// The tag has the primary type of the file format: Vorbis comments for FLAC,
/// Ogg Vorbis and Opus, ID3v2 for MP3 and an `ilst` atom for M4A.
fn write_tags(path: &Path, track: &TrackSpec) {
    let file_type = FileType::from_ext(&track.ext)
        .unwrap_or_else(|| panic!("lofty has no file type for the extension {}", track.ext));
    let mut tag = Tag::new(file_type.primary_tag_type());
    let mut set = |key: ItemKey, value: String| {
        let stored = tag.insert_text(key, value);
        assert!(stored, "{key:?} has no mapping in {file_type:?}");
    };
    set(ItemKey::TrackArtist, track.artist.clone());
    set(ItemKey::AlbumTitle, track.album.clone());
    set(ItemKey::TrackTitle, track.title.clone());
    set(ItemKey::TrackNumber, track.track_no.to_string());
    if let Some(year) = track.year {
        set(ItemKey::RecordingDate, year.to_string());
    }
    if let Some(genre) = &track.genre {
        set(ItemKey::Genre, genre.clone());
    }
    if let Some(disc) = track.disc {
        set(ItemKey::DiscNumber, disc.to_string());
    }
    if let Some(album_artist) = &track.album_artist {
        set(ItemKey::AlbumArtist, album_artist.clone());
    }
    if track.compilation {
        set(ItemKey::FlagCompilation, "1".to_owned());
    }
    if track.picture {
        tag.push_picture(
            Picture::unchecked(PICTURE_PNG.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
    }
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
}

// A test binary that names none of these types has no use for the re-export.
#[allow(unused_imports)]
pub use scanned::{
    Catalogue, ScannedServer, StandardAlbums, StandardArtists, StandardServer, StandardTracks,
    body_bytes, get_request,
};

/// The servers over a scanned library, and what the tests of the methods
/// that read the library share.
///
/// The module is one block of this file with its own imports, so that a
/// change to another part of the file does not touch its lines.
mod scanned {
    use std::ops::Deref;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError, Weak};

    use axum::body::Body;
    use axum::http::Request;
    use axum::response::Response;
    use tokio_util::sync::CancellationToken;
    use wn_server::config::Config;
    use wn_server::db::Db;
    use wn_server::db::queries::browse::{self, Album, Artist, Folder, Song};
    use wn_server::library::scanner::{self, ScanOptions, ScanSummary};

    use super::{AuthDefaults, LibraryBuilder, TestLibrary, TestServer, urlencode};

    /// A [`TestServer`] over a library that was scanned into its database.
    /// The config of the server names the directory of the library as its
    /// one music folder.
    ///
    /// The value dereferences to the [`TestServer`], so `get_json` and the
    /// other request methods are called on it.
    pub struct ScannedServer {
        server: TestServer,
        library: Arc<TestLibrary>,
        /// The ID of the music folder row of the library.
        pub folder_id: i64,
    }

    impl Deref for ScannedServer {
        type Target = TestServer;

        fn deref(&self) -> &TestServer {
            &self.server
        }
    }

    impl ScannedServer {
        /// The directory of the library, which is the music folder.
        pub fn root(&self) -> &Path {
            self.library.root.path()
        }

        /// The absolute path of `rel_path`, a path below the music folder
        /// with `/` between its components, as `tracks.rel_path` holds it.
        pub fn path(&self, rel_path: &str) -> PathBuf {
            let mut path = self.root().to_path_buf();
            path.extend(rel_path.split('/'));
            path
        }

        /// The absolute path of each track of the library, in the order the
        /// tracks were added to the builder.
        pub fn files(&self) -> &[PathBuf] {
            &self.library.files
        }

        /// Scans the music folder again and returns the summary of the scan.
        /// A test calls it after it changed a file of a library of its own.
        /// The scan is a call of the scanner and not of the scan controller
        /// of the server, so `scanning` and `count` of `getScanStatus` do
        /// not report it.
        pub async fn rescan(&self) -> ScanSummary {
            scan(self.db(), self.root()).await
        }

        /// Reads the rows of the library as the database holds them now.
        pub async fn catalogue(&self) -> Catalogue {
            self.db()
                .read(|conn| {
                    let artists = browse::list_artists(conn, None)?;
                    let mut albums = Vec::new();
                    for artist in &artists {
                        albums.extend(browse::albums_of_artist(conn, &artist.id)?);
                    }
                    let mut songs = Vec::new();
                    for album in &albums {
                        songs.extend(browse::tracks_of_album(conn, &album.id)?);
                    }
                    Ok(Catalogue {
                        folders: browse::folders(conn)?,
                        artists,
                        albums,
                        songs,
                    })
                })
                .await
                .expect("read the library")
        }
    }

    /// The rows of a scanned library: the music folders in ID order, the
    /// artists in the order of `browse::list_artists`, the albums of each
    /// artist in the order of `browse::albums_of_artist`, and the tracks of
    /// each album in the order of `browse::tracks_of_album`.
    pub struct Catalogue {
        pub folders: Vec<Folder>,
        pub artists: Vec<Artist>,
        pub albums: Vec<Album>,
        pub songs: Vec<Song>,
    }

    impl Catalogue {
        /// The artist with the name `name`. Panics unless one artist has it.
        pub fn artist(&self, name: &str) -> &Artist {
            only(
                self.artists.iter().filter(|row| row.name == name),
                "artist",
                name,
            )
        }

        /// The album with the title `title`. Panics unless one album has it.
        pub fn album(&self, title: &str) -> &Album {
            only(
                self.albums.iter().filter(|row| row.name == title),
                "album",
                title,
            )
        }

        /// The track with the title `title`. Panics unless one track has it.
        pub fn song(&self, title: &str) -> &Song {
            let titled = self.songs.iter().filter(|row| row.track.title == title);
            only(titled, "track", title)
        }
    }

    /// The one item of `rows`. Panics when `rows` holds none or several.
    fn only<'a, T>(mut rows: impl Iterator<Item = &'a T>, kind: &str, name: &str) -> &'a T {
        let first = rows
            .next()
            .unwrap_or_else(|| panic!("the library has no {kind} named {name:?}"));
        assert!(
            rows.next().is_none(),
            "the library has several {kind}s named {name:?}"
        );
        first
    }

    /// The IDs of the artists of [`LibraryBuilder::standard`]. They are the
    /// album artists: "Tessa Lund" is a track artist only and has no row.
    pub struct StandardArtists {
        pub aurora_field: String,
        pub marlow_quartet: String,
        pub various_artists: String,
    }

    /// The IDs of the albums of [`LibraryBuilder::standard`].
    pub struct StandardAlbums {
        /// By Aurora Field, 2019: three FLAC tracks and the file `cover.jpg`.
        pub glass_harbour: String,
        /// By Aurora Field, 2021: three MP3 tracks on two discs, no cover.
        pub paper_maps: String,
        /// By Marlow Quartet, 2015: two M4A tracks with an embedded picture.
        pub slow_rivers: String,
        /// By Various Artists, 2020: an Ogg Vorbis and an Opus track, no
        /// cover.
        pub summer_sampler: String,
    }

    /// The IDs of the tracks of [`LibraryBuilder::standard`], in the order
    /// the builder adds them. Each field has the title of its track.
    pub struct StandardTracks {
        /// "Glass Harbour", track 1, FLAC.
        pub low_tide: String,
        /// "Glass Harbour", track 2, FLAC.
        pub breakwater: String,
        /// "Glass Harbour", track 3, FLAC.
        pub night_ferry: String,
        /// "Paper Maps", disc 1, track 1, MP3.
        pub meridian: String,
        /// "Paper Maps", disc 1, track 2, MP3.
        pub contour: String,
        /// "Paper Maps", disc 2, track 1, MP3.
        pub legend: String,
        /// "Slow Rivers", track 1, M4A.
        pub upstream: String,
        /// "Slow Rivers", track 2, M4A.
        pub delta: String,
        /// "Summer Sampler", track 1, Ogg Vorbis, by Tessa Lund.
        pub open_road: String,
        /// "Summer Sampler", track 2, Opus, by Aurora Field.
        pub last_train: String,
    }

    /// A [`ScannedServer`] over the library of [`LibraryBuilder::standard`],
    /// with the IDs that the scan gave its artists, albums and tracks.
    ///
    /// The value dereferences to the [`ScannedServer`] and through it to the
    /// [`TestServer`].
    pub struct StandardServer {
        server: ScannedServer,
        /// The IDs of the three artists.
        pub artist: StandardArtists,
        /// The IDs of the four albums.
        pub album: StandardAlbums,
        /// The IDs of the ten tracks.
        pub track: StandardTracks,
    }

    impl Deref for StandardServer {
        type Target = ScannedServer;

        fn deref(&self) -> &ScannedServer {
            &self.server
        }
    }

    impl TestServer {
        /// A server over the scanned standard library
        /// ([`LibraryBuilder::standard`]). Every call returns a server with
        /// a database of its own, so a test writes annotations, playlists
        /// and other rows without an effect on another test.
        ///
        /// The files of the library are shared: the tests of one test binary
        /// that hold a server at the same time read the same directory,
        /// which is written once for them. A test therefore does not change,
        /// add or remove a file below [`ScannedServer::root`]. A test that
        /// does so calls [`TestServer::standard_with_own_files`].
        pub async fn standard() -> StandardServer {
            standard_server(shared_standard_library()).await
        }

        /// A server over the scanned standard library, as
        /// [`TestServer::standard`] returns it, with a copy of the files that
        /// no other test reads. The test changes the files and calls
        /// [`ScannedServer::rescan`].
        pub async fn standard_with_own_files() -> StandardServer {
            standard_server(Arc::new(LibraryBuilder::standard().build())).await
        }

        /// A server over `library`, scanned. The scan must report no error.
        /// A test of names or files that the standard library does not have
        /// builds its library with [`LibraryBuilder`] and finds the IDs with
        /// [`ScannedServer::catalogue`].
        pub async fn scanned(library: TestLibrary) -> ScannedServer {
            scanned_server(Arc::new(library), |_| {}).await
        }

        /// As [`TestServer::scanned`], with the config that `adjust`
        /// changes. A test of a limit of `stream` sets the fields of the
        /// config that are no config keys.
        pub async fn scanned_with(
            library: TestLibrary,
            adjust: impl FnOnce(&mut Config),
        ) -> ScannedServer {
            scanned_server(Arc::new(library), adjust).await
        }

        /// Sends `GET /rest/<method>` with `params` and the auth defaults in
        /// the query string, and with `headers` as request headers. A test
        /// of `Range` or `If-None-Match` uses it.
        pub async fn get_with_headers(
            &self,
            method: &str,
            params: &[(&str, &str)],
            headers: &[(&str, &str)],
        ) -> Response {
            let auth = AuthDefaults::new();
            let request = get_request(method, &auth.after(params), headers);
            self.request(request).await
        }
    }

    /// A `GET /rest/<method>` request with `params` in the query string,
    /// with `headers` as its headers and with no other parameter.
    pub fn get_request(
        method: &str,
        params: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Request<Body> {
        let path = format!("/rest/{method}?{}", urlencode(params));
        let mut request = Request::get(&path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("GET {path}: {error}"))
    }

    /// Reads the body of `response` as bytes. A test of a method that
    /// returns a file or an image uses it.
    pub async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the response body")
            .to_vec()
    }

    /// The standard library that the servers of [`TestServer::standard`]
    /// share. The library is written when no server holds it, and its
    /// directory is deleted when the last server that holds it is dropped.
    /// The tests of a binary run on several threads and overlap, so the
    /// library is written once for a run of the binary in most runs, and no
    /// directory is left behind when the process ends.
    fn shared_standard_library() -> Arc<TestLibrary> {
        static SHARED: Mutex<Weak<TestLibrary>> = Mutex::new(Weak::new());
        // A test that panicked while it held the lock left the value intact.
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(library) = shared.upgrade() {
            return library;
        }
        let library = Arc::new(LibraryBuilder::standard().build());
        *shared = Arc::downgrade(&library);
        library
    }

    async fn standard_server(library: Arc<TestLibrary>) -> StandardServer {
        let server = scanned_server(library, |_| {}).await;
        let catalogue = server.catalogue().await;
        let artist = |name: &str| catalogue.artist(name).id.clone();
        let album = |title: &str| catalogue.album(title).id.clone();
        let track = |title: &str| catalogue.song(title).track.id.clone();
        StandardServer {
            artist: StandardArtists {
                aurora_field: artist("Aurora Field"),
                marlow_quartet: artist("Marlow Quartet"),
                various_artists: artist("Various Artists"),
            },
            album: StandardAlbums {
                glass_harbour: album("Glass Harbour"),
                paper_maps: album("Paper Maps"),
                slow_rivers: album("Slow Rivers"),
                summer_sampler: album("Summer Sampler"),
            },
            track: StandardTracks {
                low_tide: track("Low Tide"),
                breakwater: track("Breakwater"),
                night_ferry: track("Night Ferry"),
                meridian: track("Meridian"),
                contour: track("Contour"),
                legend: track("Legend"),
                upstream: track("Upstream"),
                delta: track("Delta"),
                open_road: track("Open Road"),
                last_train: track("Last Train"),
            },
            server,
        }
    }

    async fn scanned_server(
        library: Arc<TestLibrary>,
        adjust: impl FnOnce(&mut Config),
    ) -> ScannedServer {
        let root = library.root.path().to_path_buf();
        let mut config = Config::default();
        config.library.folders = vec![root.clone()];
        adjust(&mut config);
        let server = TestServer::bare(config).await;

        let summary = scan(server.db(), &root).await;
        assert_eq!(
            summary.errors,
            [] as [&str; 0],
            "the scan of {}",
            root.display()
        );
        let folders = server
            .db()
            .read(browse::folders)
            .await
            .expect("read the music folders");
        assert_eq!(folders.len(), 1, "the scan stores one music folder");
        ScannedServer {
            server,
            library,
            folder_id: folders[0].id,
        }
    }

    /// Scans the music folder `root` into `db` on a blocking thread.
    async fn scan(db: &Db, root: &Path) -> ScanSummary {
        let db = db.clone();
        let folders = vec![root.to_path_buf()];
        tokio::task::spawn_blocking(move || {
            let cancel = CancellationToken::new();
            scanner::scan(&db, &folders, &cancel, false, ScanOptions::default())
        })
        .await
        .expect("the scan does not panic")
        .expect("the scan reads and writes the database")
    }
}

/// The name of the owner account of a [`TestServer`].
pub const OWNER: &str = "owner";

/// The password of the owner account of a [`TestServer`].
pub const OWNER_PASSWORD: &str = "sesame";

/// The value of `v` in the auth defaults of a [`TestServer`].
pub const API_VERSION: &str = "1.16.1";

/// The value of `c` in the auth defaults of a [`TestServer`].
pub const CLIENT: &str = "wn-server-tests";

/// The longest time [`TestServer::wait_for_scan`] polls. The limit ends a
/// test whose scan does not end. It does not delay a passing run: a scan of
/// the standard library takes some tens of milliseconds.
const SCAN_WAIT: Duration = Duration::from_secs(1);

/// The Subsonic token of `password` for `salt`: the MD5 digest of the
/// password followed by the salt, as lower-case hex.
pub fn token(password: &str, salt: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(password.as_bytes());
    hasher.update(salt.as_bytes());
    hex::encode(hasher.finalize())
}

/// The parameters that authenticate one request as the owner of a
/// [`TestServer`]: `u`, a token `t` with its salt `s`, `v` and `c`. Every
/// value of this type has another salt.
struct AuthDefaults {
    salt: String,
    token: String,
}

impl AuthDefaults {
    fn new() -> Self {
        static REQUESTS: AtomicU64 = AtomicU64::new(0);
        let salt = format!("salt{:08x}", REQUESTS.fetch_add(1, Ordering::Relaxed));
        let token = token(OWNER_PASSWORD, &salt);
        Self { salt, token }
    }

    /// `params` followed by the defaults. A default whose name `params`
    /// holds is left out.
    fn after<'a>(&'a self, params: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let defaults = [
            ("u", OWNER),
            ("t", self.token.as_str()),
            ("s", self.salt.as_str()),
            ("v", API_VERSION),
            ("c", CLIENT),
        ];
        let mut merged = params.to_vec();
        for (name, value) in defaults {
            if !params.iter().any(|(given, _)| *given == name) {
                merged.push((name, value));
            }
        }
        merged
    }
}

/// The server under test: the router of `wn_server::api`, called in the test
/// process. No socket is opened.
///
/// The database is in memory and holds the owner [`OWNER`] with the password
/// [`OWNER_PASSWORD`]. [`TestServer::db`] returns it, so a test writes the
/// rows that its requests read.
///
/// `get_raw`, `get_json`, `get_xml` and `post_form` add the auth defaults to
/// the parameters of the caller: `u`, a token `t` with a new salt `s`, `v` and
/// `c`. A default whose name the caller passes is left out. `get_raw_noauth`,
/// `get_json_noauth`, `get_path` and `request` add no auth default. A test
/// that sends a password, an API key or no credential uses them.
///
/// The server starts no scan by itself. [`TestServer::scan_and_wait`] and
/// the `startScan` method scan the music folders of the config.
///
/// The transcoder of the server is a [`FakeTranscoder`], so no request
/// starts ffmpeg. [`TestServer::transcoder`] returns it.
pub struct TestServer {
    /// The directory of the cover cache. Dropping the server deletes it.
    _cache_dir: TempDir,
    router: Router,
    db: Db,
    scan: ScanController,
    scan_poll_ms: u64,
    transcoder: Arc<FakeTranscoder>,
}

impl TestServer {
    /// A server with the state that `config` gives, a database that holds
    /// the owner account, and nothing else. The music folders of `config`
    /// are not read before a scan starts.
    pub async fn bare(config: Config) -> Self {
        Self::with_scan(config, |_| {}).await
    }

    /// As [`TestServer::bare`], with the settings of the scans that `adjust`
    /// changes. Without a change the scans take no scan lock, remove no
    /// cached cover and have no test hook, because the server has no data
    /// directory.
    pub async fn with_scan(config: Config, adjust: impl FnOnce(&mut ScanSettings)) -> Self {
        let cache_dir = tempfile::tempdir().expect("create the cover cache directory");
        let db = Db::open_memory().expect("open a memory database");
        db.write(|conn| users::insert_owner(conn, OWNER, OWNER_PASSWORD))
            .await
            .expect("insert the owner")
            .expect("a new database has no owner");

        let mut settings = ScanSettings {
            db: db.clone(),
            folders: config.library.folders.clone(),
            covers_cache_dir: None,
            lock_path: None,
            cancel: CancellationToken::new(),
            hooks: ScanHooks::default(),
        };
        adjust(&mut settings);
        let scan = ScanController::new(settings);
        let scan_poll_ms = config.library.scan_poll_ms;
        let transcoder = Arc::new(FakeTranscoder::default());
        let folders = config.library.folders.clone();
        let streams = Arc::new(Semaphore::new(config.server.max_streams));

        // The fields follow the layout rule in the module comment of
        // `wn_server::api`.
        #[rustfmt::skip]
        let state = AppState {
            config: Arc::new(config),

            db: db.clone(),

            covers: wn_server::library::covers::Covers::new(cache_dir.path(), db.clone(), folders),

            transcoder: transcoder.clone(),

            streams,

            scan: scan.clone(),

            cancel: CancellationToken::new(),

            started_at: Instant::now(),
        };
        Self {
            _cache_dir: cache_dir,
            router: api::build_router(state),
            db,
            scan,
            scan_poll_ms,
            transcoder,
        }
    }

    /// The database of the server.
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The transcoder of the server. It starts no process: a test sets the
    /// bytes of its streams and reads the jobs that reached it.
    pub fn transcoder(&self) -> &FakeTranscoder {
        &self.transcoder
    }

    /// The controller of the scans of the server.
    pub fn scan(&self) -> &ScanController {
        &self.scan
    }

    /// Starts a scan of the music folders of the config that is not full,
    /// and waits until it has ended. No other scan may run.
    pub async fn scan_and_wait(&self) {
        let outcome = self.scan.start(false).expect("start the scan");
        assert_eq!(outcome, StartOutcome::Started);
        self.wait_for_scan().await;
    }

    /// Waits until no scan of the server runs. Polls the status of the
    /// controller every `library.scan_poll_ms` of the config, which a test
    /// sets to 10, for one second at most.
    pub async fn wait_for_scan(&self) {
        let poll = Duration::from_millis(self.scan_poll_ms);
        let idle = async {
            while self.scan.status().scanning {
                tokio::time::sleep(poll).await;
            }
        };
        tokio::time::timeout(SCAN_WAIT, idle)
            .await
            .expect("the scan ends within one second");
    }

    /// Sends `request` to the router and returns the response.
    pub async fn request(&self, request: Request<Body>) -> Response {
        match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    /// Sends `GET <path>`. `path` is the path and the query string as the
    /// request line holds them.
    pub async fn get_path(&self, path: &str) -> Response {
        let request = Request::get(path)
            .body(Body::empty())
            .unwrap_or_else(|error| panic!("GET {path}: {error}"));
        self.request(request).await
    }

    /// Sends `GET /rest/<method>` with `params` and the auth defaults in the
    /// query string.
    pub async fn get_raw(&self, method: &str, params: &[(&str, &str)]) -> Response {
        let auth = AuthDefaults::new();
        self.get_raw_noauth(method, &auth.after(params)).await
    }

    /// Sends `GET /rest/<method>` with `params` in the query string and no
    /// other parameter.
    pub async fn get_raw_noauth(&self, method: &str, params: &[(&str, &str)]) -> Response {
        let query = urlencode(params);
        if query.is_empty() {
            self.get_path(&format!("/rest/{method}")).await
        } else {
            self.get_path(&format!("/rest/{method}?{query}")).await
        }
    }

    /// Sends `GET /rest/<method>` with `params`, the auth defaults and
    /// `f=json`. Checks that the response is HTTP 200 with the JSON content
    /// type. Returns the value of `subsonic-response`.
    pub async fn get_json(&self, method: &str, params: &[(&str, &str)]) -> Value {
        let auth = AuthDefaults::new();
        self.get_json_noauth(method, &auth.after(params)).await
    }

    /// Sends `GET /rest/<method>` with `params` and `f=json`, and no other
    /// parameter. Checks that the response is HTTP 200 with the JSON content
    /// type. Returns the value of `subsonic-response`.
    pub async fn get_json_noauth(&self, method: &str, params: &[(&str, &str)]) -> Value {
        let mut params = params.to_vec();
        params.push(("f", "json"));
        let response = self.get_raw_noauth(method, &params).await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        assert_eq!(content_type(&response), "application/json", "{method}");
        subsonic_response(&body_string(response).await)
    }

    /// Sends `GET /rest/<method>` with `params` and the auth defaults. Checks
    /// that the response is HTTP 200 with the XML content type. Returns the
    /// body.
    pub async fn get_xml(&self, method: &str, params: &[(&str, &str)]) -> String {
        let response = self.get_raw(method, params).await;
        assert_eq!(response.status(), StatusCode::OK, "{method}");
        assert_eq!(content_type(&response), "application/xml", "{method}");
        body_string(response).await
    }

    /// Sends `POST /rest/<method>` with `params` and the auth defaults as the
    /// form body.
    pub async fn post_form(&self, method: &str, params: &[(&str, &str)]) -> Response {
        let auth = AuthDefaults::new();
        let request = form_request(&format!("/rest/{method}"), &auth.after(params));
        self.request(request).await
    }
}

/// A `POST <path>` request with `params` as the form body and no other
/// parameter. `path` is the path and the query string as the request line
/// holds them.
pub fn form_request(path: &str, params: &[(&str, &str)]) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(urlencode(params)))
        .unwrap_or_else(|error| panic!("POST {path}: {error}"))
}

/// The value of the `Content-Type` header of `response`. An empty string when
/// the header is absent.
pub fn content_type(response: &Response) -> String {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().expect("an ASCII content type").to_owned())
        .unwrap_or_default()
}

/// Reads the body of `response` as UTF-8 text.
pub async fn body_string(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the response body");
    String::from_utf8(bytes.to_vec()).expect("the response body is UTF-8")
}

/// Parses a JSON response body and returns the value of `subsonic-response`,
/// which is the only key of the document.
pub fn subsonic_response(body: &str) -> Value {
    let document: Value =
        serde_json::from_str(body).unwrap_or_else(|error| panic!("{error}: {body}"));
    let Value::Object(mut document) = document else {
        panic!("the body is not a JSON object: {body}");
    };
    assert_eq!(
        document.keys().collect::<Vec<_>>(),
        ["subsonic-response"],
        "{body}"
    );
    document["subsonic-response"].take()
}

/// Encodes `params` as `application/x-www-form-urlencoded` data, which is
/// also the form of a query string. Every byte outside the unreserved set of
/// RFC 3986 is written as a percent escape.
pub fn urlencode(params: &[(&str, &str)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    };
    params
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The log of the test process. [`captured_log`] returns it.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    /// Everything that was logged so far.
    pub fn text(&self) -> String {
        let bytes = self.0.lock().expect("the log buffer lock");
        String::from_utf8(bytes.clone()).expect("the log is UTF-8")
    }

    /// The lines logged so far that contain `text`.
    pub fn lines_with(&self, text: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|line| line.contains(text))
            .map(str::to_owned)
            .collect()
    }
}

impl io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("the log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Captures the log of the test process, down to the `trace` level and
/// without colours, and returns the buffer that holds it. The first call
/// starts the capture. Every call returns the same buffer.
///
/// The subscriber is the global default of the process, so the buffer holds
/// the log of every test of the test binary. A test finds its own lines by a
/// value that only it sends.
///
/// A subscriber that is the default of one thread
/// (`tracing::subscriber::set_default`) is not used, because it loses events
/// when tests run on several threads. tracing caches for each log statement
/// whether a subscriber wants it. While one subscriber exists, tracing asks
/// the default of the thread that reaches the statement first. When that is a
/// thread of another test, it has no subscriber, and the statement stays off
/// for the thread that captures.
pub fn captured_log() -> LogBuffer {
    static LOG: OnceLock<LogBuffer> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("the test process has no other global subscriber");
        buffer
    })
    .clone()
}

// A test binary that names none of these items has no use for the re-export.
#[allow(unused_imports)]
pub use rows::{
    ATTACHED, Dump, ROWID, attached_rows, dump, dump_of, forbid_writes, table_names, without_scans,
};

/// What the tests of the migrations and of the scanner share to check that a
/// step kept the rows of a database or wrote none: the rows of every table,
/// and triggers that fail a write.
///
/// The module is one block of this file with its own imports, as `scanned`
/// is.
mod rows {
    use std::collections::BTreeMap;

    use rusqlite::Connection;
    use rusqlite::types::Value;
    use wn_server::db::Db;

    /// The name that [`dump`] gives the rowid of a row.
    pub const ROWID: &str = "_rowid";

    /// The tables whose rows a user or a scan attaches to the tracks, the albums
    /// and the artists.
    pub const ATTACHED: [&str; 6] = [
        "annotations",
        "play_queue",
        "play_queue_tracks",
        "playlist_tracks",
        "playlists",
        "users",
    ];

    /// The rows of every table by the name of the table: for each row in rowid
    /// order, the rowid under the name [`ROWID`], and the name and the value of
    /// every column.
    pub type Dump = BTreeMap<String, Vec<Vec<(String, Value)>>>;

    /// The names of the tables of the database of `conn`, in the order of the
    /// names.
    pub fn table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
        conn.prepare(
            "SELECT name FROM main.sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect()
    }

    /// The rows of every table of the database of `conn`.
    pub fn dump(conn: &Connection) -> rusqlite::Result<Dump> {
        let mut tables = Dump::new();
        for table in table_names(conn)? {
            let mut select = conn.prepare(&format!(
                "SELECT rowid AS {ROWID}, * FROM main.{table} ORDER BY rowid"
            ))?;
            let names: Vec<String> = select
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect();
            let rows = select
                .query_map([], |row| {
                    names
                        .iter()
                        .enumerate()
                        .map(|(index, name)| Ok((name.clone(), row.get(index)?)))
                        .collect()
                })?
                .collect::<rusqlite::Result<_>>()?;
            tables.insert(table, rows);
        }
        Ok(tables)
    }

    /// The rows of every table of `db`, read on the calling thread.
    pub fn dump_of(db: &Db) -> Dump {
        db.with_reader(|conn| Ok(dump(conn)?)).unwrap()
    }

    /// The rows of the tables of [`ATTACHED`].
    pub fn attached_rows(dump: &Dump) -> Vec<&Vec<Vec<(String, Value)>>> {
        ATTACHED.iter().map(|table| &dump[*table]).collect()
    }

    /// Returns `dump` without the table `scans`, which gets a row with every
    /// scan.
    pub fn without_scans(dump: &Dump) -> Dump {
        let mut dump = dump.clone();
        dump.remove("scans");
        dump
    }

    /// Fails every statement on the write connection, which the scanner uses,
    /// that writes a row of the library or a row that is attached to it.
    pub fn forbid_writes(db: &Db) {
        db.with_writer(|conn| {
            for table in ["tracks", "albums", "artists"].iter().chain(&ATTACHED) {
                for (event, verb) in [
                    ("INSERT", "inserted"),
                    ("UPDATE", "updated"),
                    ("DELETE", "deleted"),
                ] {
                    conn.execute_batch(&format!(
                        "CREATE TEMP TRIGGER no_{table}_{verb} BEFORE {event} ON {table}
                         BEGIN SELECT RAISE(ABORT, 'a row of {table} was {verb}'); END;"
                    ))?;
                }
            }
            Ok(())
        })
        .unwrap();
    }
}
