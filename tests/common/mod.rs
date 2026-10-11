//! Test support shared by the integration tests. A test file includes it with
//! `mod common;`.

use std::fs;
use std::path::{Path, PathBuf};

use image::{Rgb, RgbImage};
use lofty::config::WriteOptions;
use lofty::file::FileType;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, Tag, TagExt};
use tempfile::TempDir;

/// The picture that [`LibraryBuilder::picture`] embeds: a 2×2 PNG, 8-bit RGB,
/// with one red, one green, one blue and one white pixel.
pub const PICTURE_PNG: [u8; 75] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a,
    0x73, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xc0,
    0x00, 0xc2, 0x0c, 0xff, 0x81, 0x00, 0x00, 0x1f, 0xee, 0x05, 0xfb, 0xf1, 0xab, 0xba, 0x77, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// The lyrics of the track "Open Road" in [`LibraryBuilder::standard`].
pub const STANDARD_LYRICS: &str = "The road is open\nThe morning is clear";

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
    lyrics: Option<String>,
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
    ///   by Tessa Lund, which has lyrics, and one Opus track by Aurora Field.
    ///   Both tracks have the compilation flag and no album-artist tag.
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
            .lyrics(STANDARD_LYRICS)
            .track("Aurora Field", "Summer Sampler", "Last Train", 2, "opus")
            .year(2020)
            .genre("Pop")
            .compilation()
    }

    /// Adds a track. `ext` selects the fixture and is one of `flac`, `mp3`,
    /// `m4a`, `ogg` and `opus`. The methods `year`, `genre`, `disc`,
    /// `album_artist`, `compilation`, `picture` and `lyrics` change the track
    /// that was added last.
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
            lyrics: None,
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

    /// Sets the unsynchronised lyrics of the last track. The text is stored
    /// in a `USLT` frame in MP3, in the `©lyr` atom in M4A and in an
    /// `UNSYNCEDLYRICS` comment in FLAC, Ogg Vorbis and Opus. lofty reads it
    /// back under `ItemKey::UnsyncLyrics`, and under `ItemKey::Lyrics` from
    /// M4A.
    pub fn lyrics(mut self, text: &str) -> Self {
        self.last().lyrics = Some(text.to_owned());
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
    if let Some(lyrics) = &track.lyrics {
        set(ItemKey::UnsyncLyrics, lyrics.clone());
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
