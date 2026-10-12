//! Tag reading: turns one audio file into the metadata that the `tracks`
//! table stores (`docs/design.md` sections 1.3 and 3.4). lofty parses the
//! file.
//!
//! The format of a file is detected from its content. The extension decides
//! the format when the content does not. The suffix and the content type of a
//! track come from the extension alone.
//!
//! Fields come from the tag of the format's primary type, or from the first
//! tag when the file has none.
//!
//! A tag read takes at most [`MAX_TAG_BYTES`] of a file (`docs/design.md`
//! section 3.10, "The tag bound"). A file whose headers declare a larger tag
//! is refused before lofty reads it, and lofty reads every other file through
//! a reader that stops at the bound. The module `bound` holds both parts.

mod bound;

use std::error::Error;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::error::FileParseError;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

use bound::Capped;
pub use bound::MAX_TAG_BYTES;

/// The supported formats: the extension in lower case and the content type.
const FORMATS: [(&str, &str); 9] = [
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("wav", "audio/wav"),
    ("aif", "audio/aiff"),
    ("aiff", "audio/aiff"),
];

/// The file extensions of the supported formats, in lower case.
pub const SUPPORTED_EXTENSIONS: [&str; FORMATS.len()] = {
    let mut extensions = [""; FORMATS.len()];
    let mut index = 0;
    while index < FORMATS.len() {
        extensions[index] = FORMATS[index].0;
        index += 1;
    }
    extensions
};

/// The artist of a file that has no artist tag.
pub const UNKNOWN_ARTIST: &str = "[Unknown Artist]";

/// The album of a file that has no album tag.
pub const UNKNOWN_ALBUM: &str = "[Unknown Album]";

/// The album artist of a compilation.
pub const VARIOUS_ARTISTS: &str = "Various Artists";

/// The content type of a picture whose image format is not known.
const UNKNOWN_PICTURE_TYPE: &str = "application/octet-stream";

/// The metadata of one audio file. The fields fill the columns of the same
/// names in the `tracks` table. A tag that is absent or blank is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMeta {
    /// The title tag. The file stem when the tag is missing.
    pub title: String,
    /// The track artist tag. [`UNKNOWN_ARTIST`] when the tag is missing.
    pub artist: String,
    /// The album artist tag. When the tag is missing, the value is
    /// [`VARIOUS_ARTISTS`] for a file with the compilation flag and `artist`
    /// for any other file. A value that equals [`VARIOUS_ARTISTS`] without
    /// regard to case is stored in that spelling.
    pub album_artist: String,
    /// The album tag. [`UNKNOWN_ALBUM`] when the tag is missing.
    pub album: String,
    /// The track number. The total in a value such as `3/12` is dropped.
    pub track_no: Option<u32>,
    /// The disc number. The total in a value such as `1/2` is dropped.
    pub disc_no: Option<u32>,
    /// The leading four-digit year of the recording date tag, or of the year
    /// tag when the date has none.
    pub year: Option<u32>,
    /// The genre tag.
    pub genre: Option<String>,
    /// Whether the compilation flag is set or `album_artist` is
    /// [`VARIOUS_ARTISTS`].
    pub compilation: bool,
    /// The duration of the audio in milliseconds.
    pub duration_ms: u64,
    /// The bitrate of the audio stream in kbps. The bitrate of the whole file
    /// when lofty does not report the stream's.
    pub bitrate_kbps: Option<u32>,
    /// The sample rate in Hz.
    pub sample_rate: Option<u32>,
    /// The bits per sample. Lossy formats have none.
    pub bit_depth: Option<u8>,
    /// The number of channels.
    pub channels: Option<u8>,
    /// Whether the tag holds a picture. It is true exactly when
    /// [`read_front_picture`] returns a picture.
    pub has_picture: bool,
    /// The extension of the file in lower case.
    pub suffix: String,
    /// The content type that belongs to `suffix`.
    pub content_type: String,
    /// Whether the format stores the audio without loss: `flac`, `wav`,
    /// `aif`, `aiff`, and `m4a` with the ALAC codec.
    pub lossless: bool,
}

/// An error from reading an audio file.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    /// The extension of the path is not one of [`SUPPORTED_EXTENSIONS`].
    #[error("{} does not have a supported audio extension", .0.display())]
    UnsupportedExtension(PathBuf),
    /// The file cannot be opened, or lofty cannot parse it.
    #[error("cannot read {}: {}", .path.display(), causes(.source))]
    Read {
        path: PathBuf,
        source: FileParseError,
    },
    /// The file is over the tag bound: its headers declare a tag of more
    /// than [`MAX_TAG_BYTES`], or reading its tags takes more bytes of the
    /// file than that. The tags were not read.
    #[error(
        "{} is refused: reading its tags takes more than {} MiB of the file",
        .0.display(),
        MAX_TAG_BYTES / (1024 * 1024)
    )]
    TooLarge(PathBuf),
}

/// Returns whether the extension of `path` is one of
/// [`SUPPORTED_EXTENSIONS`]. Case is ignored.
pub fn is_supported(path: &Path) -> bool {
    format_of(path).is_some()
}

/// Reads the tags and the audio properties of the file at `path`.
///
/// # Errors
///
/// Returns [`TagError::UnsupportedExtension`] for a path that
/// [`is_supported`] rejects, [`TagError::TooLarge`] for a file over the tag
/// bound, and [`TagError::Read`] for a file that cannot be opened or parsed.
pub fn read(path: &Path) -> Result<TrackMeta, TagError> {
    let (suffix, content_type) =
        format_of(path).ok_or_else(|| TagError::UnsupportedExtension(path.to_owned()))?;
    let parsed = parse(path, ParseOptions::new()).map_err(|error| match error {
        ParseError::TooLarge => TagError::TooLarge(path.to_owned()),
        ParseError::Read(source) => TagError::Read {
            path: path.to_owned(),
            source,
        },
    })?;
    let tag = main_tag(&parsed.file);
    let properties = parsed.file.properties();

    let title = match text(tag, ItemKey::TrackTitle) {
        Some(title) => title.to_owned(),
        None => path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let artist = text(tag, ItemKey::TrackArtist).unwrap_or(UNKNOWN_ARTIST);
    let flag = text(tag, ItemKey::FlagCompilation).is_some_and(is_set);
    let (album_artist, compilation) =
        resolve_album_artist(text(tag, ItemKey::AlbumArtist), artist, flag);

    Ok(TrackMeta {
        title,
        artist: artist.to_owned(),
        album_artist,
        album: text(tag, ItemKey::AlbumTitle)
            .unwrap_or(UNKNOWN_ALBUM)
            .to_owned(),
        track_no: text(tag, ItemKey::TrackNumber).and_then(parse_number),
        disc_no: text(tag, ItemKey::DiscNumber).and_then(parse_number),
        year: [ItemKey::RecordingDate, ItemKey::Year]
            .into_iter()
            .find_map(|key| text(tag, key).and_then(parse_year)),
        genre: text(tag, ItemKey::Genre).map(str::to_owned),
        compilation,
        duration_ms: u64::try_from(properties.duration().as_millis()).unwrap_or(u64::MAX),
        bitrate_kbps: positive(properties.audio_bitrate())
            .or_else(|| positive(properties.overall_bitrate())),
        sample_rate: positive(properties.sample_rate()),
        bit_depth: positive(properties.bit_depth()),
        channels: positive(properties.channels()),
        has_picture: front_picture(tag).is_some(),
        suffix: suffix.to_owned(),
        content_type: content_type.to_owned(),
        lossless: is_lossless(suffix, parsed.alac),
    })
}

/// Returns the bytes and the content type of the embedded picture of the file
/// at `path`. The picture is the first one of the type front cover, or the
/// first picture when no picture has that type. M4A does not store a picture
/// type, so the second rule selects the cover of an M4A file.
///
/// The content type is the one the tag names when it is JPEG, PNG, TIFF, BMP
/// or GIF. Otherwise it is detected from the first bytes of the picture, and
/// it is `application/octet-stream` when they are not a known image format.
///
/// Returns `None` when the file has no picture, cannot be read or is over the
/// tag bound. The tag of a file over the bound is not read.
pub fn read_front_picture(path: &Path) -> Option<(Vec<u8>, String)> {
    format_of(path)?;
    let parsed = parse(path, ParseOptions::new().read_properties(false)).ok()?;
    let picture = front_picture(main_tag(&parsed.file))?;
    Some((picture.data().to_vec(), picture_content_type(picture)))
}

/// A parsed audio file.
struct Parsed {
    file: TaggedFile,
    /// Whether the file is an MP4 container that holds ALAC audio. It is
    /// false when the properties were not read.
    alac: bool,
}

/// Returns the supported extension of `path` in lower case and its content
/// type. Returns `None` for any other path.
fn format_of(path: &Path) -> Option<(&'static str, &'static str)> {
    let extension = path.extension()?.to_str()?;
    FORMATS
        .into_iter()
        .find(|(supported, _)| supported.eq_ignore_ascii_case(extension))
}

/// Why a file was not parsed.
#[derive(Debug)]
enum ParseError {
    /// The file is over the tag bound.
    TooLarge,
    /// The file cannot be opened, or lofty cannot parse it.
    Read(FileParseError),
}

impl From<FileParseError> for ParseError {
    fn from(error: FileParseError) -> Self {
        Self::Read(error)
    }
}

impl From<io::Error> for ParseError {
    fn from(error: io::Error) -> Self {
        Self::Read(error.into())
    }
}

/// Opens the file at `path` and parses it with `options`, within the tag
/// bound.
fn parse(path: &Path, options: ParseOptions) -> Result<Parsed, ParseError> {
    let file = BufReader::new(File::open(path)?);
    parse_reader(file, FileType::from_path(path), options, MAX_TAG_BYTES)
}

/// Parses the file in `reader` with `options`. `by_extension` is the format
/// that the extension of the file names. At most `limit` bytes of the file
/// are read, in every step together.
///
/// Returns [`ParseError::TooLarge`] when the headers of the file declare a
/// tag of more than `limit` bytes, which is found before lofty reads the
/// file, and when a step asks the reader for a byte past `limit`.
fn parse_reader<R: Read + Seek>(
    reader: R,
    by_extension: Option<FileType>,
    options: ParseOptions,
    limit: u64,
) -> Result<Parsed, ParseError> {
    let mut reader = Capped::new(reader, limit);
    let parsed = parse_capped(&mut reader, by_extension, options, limit);
    // lofty ends some of its loops at a read that fails and returns what it
    // has, so the reader decides and not the result.
    if reader.exceeded() {
        return Err(ParseError::TooLarge);
    }
    parsed
}

/// The steps of [`parse_reader`]: detects the format, adds up what the
/// headers declare, and has lofty read the file.
fn parse_capped<R: Read + Seek>(
    reader: &mut Capped<R>,
    by_extension: Option<FileType>,
    options: ParseOptions,
    limit: u64,
) -> Result<Parsed, ParseError> {
    // The content decides the format. The extension decides it when the
    // content does not.
    let detected = Probe::new(&mut *reader)
        .options(options)
        .guess_file_type()?
        .file_type();
    let Some(file_type) = detected.or(by_extension) else {
        // lofty returns the error of a file without a format.
        let file = Probe::new(reader).options(options).read()?;
        return Ok(Parsed { file, alac: false });
    };

    if bound::declared(reader, file_type, limit) > limit {
        return Err(ParseError::TooLarge);
    }
    reader.rewind()?;

    // The properties that lofty reports for every format do not name the
    // codec. The properties of `Mp4File` do, so an MP4 file is read as one.
    if file_type == FileType::Mp4 {
        let file = Mp4File::read_from(reader, options)?;
        let alac = file.properties().codec() == Some(Mp4Codec::ALAC);
        Ok(Parsed {
            file: file.into(),
            alac,
        })
    } else {
        let probe = Probe::with_file_type(reader, file_type).options(options);
        Ok(Parsed {
            file: probe.read()?,
            alac: false,
        })
    }
}

/// Returns the tag that the fields of `file` come from: the tag of the
/// format's primary type, or the first tag when the file has none of that
/// type. Returns `None` for a file without a tag.
fn main_tag(file: &TaggedFile) -> Option<&Tag> {
    file.primary_tag().or_else(|| file.first_tag())
}

/// Returns the value of `key` in `tag` without the surrounding white space.
/// Returns `None` for a value that is absent or blank.
fn text(tag: Option<&Tag>, key: ItemKey) -> Option<&str> {
    tag?.get_string(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Returns the front cover among the pictures of `tag`, or the first picture
/// when none has that type.
fn front_picture(tag: Option<&Tag>) -> Option<&Picture> {
    let pictures = tag?.pictures();
    pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())
}

/// Returns the content type of `picture`.
fn picture_content_type(picture: &Picture) -> String {
    let named = match picture.mime_type() {
        Some(MimeType::Unknown(_)) | None => None,
        Some(known) => Some(known.as_str()),
    };
    named
        .or_else(|| {
            image::guess_format(picture.data())
                .ok()
                .map(|format| format.to_mime_type())
        })
        .unwrap_or(UNKNOWN_PICTURE_TYPE)
        .to_owned()
}

/// Applies the album artist rule of `docs/design.md` section 1.3 and returns
/// the album artist and the compilation flag. `tag` is the album artist tag,
/// `artist` is the track artist and `flag` is the compilation flag of the
/// file.
fn resolve_album_artist(tag: Option<&str>, artist: &str, flag: bool) -> (String, bool) {
    let name = tag.unwrap_or(if flag { VARIOUS_ARTISTS } else { artist });
    if name.eq_ignore_ascii_case(VARIOUS_ARTISTS) {
        (VARIOUS_ARTISTS.to_owned(), true)
    } else {
        (name.to_owned(), flag)
    }
}

/// Returns whether the value of a flag tag means that the flag is set.
fn is_set(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

/// Parses a track or disc number. In a value such as `3/12` the number
/// precedes the total.
fn parse_number(value: &str) -> Option<u32> {
    value.split('/').next()?.trim().parse().ok()
}

/// Parses the leading four-digit year of a date such as `2019` or
/// `2019-05-01`. The year `0000` marks an unknown date and yields `None`.
fn parse_year(value: &str) -> Option<u32> {
    let digits = value.get(..4)?;
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|year| *year != 0)
}

/// Returns whether a file with the extension `suffix` stores the audio
/// without loss. `alac` says whether an MP4 container holds ALAC audio.
fn is_lossless(suffix: &str, alac: bool) -> bool {
    match suffix {
        "flac" | "wav" | "aif" | "aiff" => true,
        "m4a" => alac,
        _ => false,
    }
}

/// Maps the value 0, which lofty reports for a property it cannot compute,
/// to `None`.
fn positive<T: Default + PartialEq>(value: Option<T>) -> Option<T> {
    value.filter(|value| *value != T::default())
}

/// Joins the message of `error` and the messages of its sources with `: `.
/// lofty puts the format in the error and the reason in the source. A source
/// that repeats the message before it is left out.
fn causes(error: &dyn Error) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        source = cause.source();
    }
    messages.join(": ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use std::io::Cursor;

    use lofty::properties::FileProperties;
    use lofty::tag::TagType;

    use super::bound::testing::Counting;
    use super::*;

    /// The eight bytes that start every PNG file.
    const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

    /// Writes `bytes` to the file `name` in a new temporary directory.
    /// Returns the directory, which deletes itself on drop, and the path.
    fn write_file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let path = dir.path().join(name);
        fs::write(&path, bytes).expect("write the file");
        (dir, path)
    }

    /// Returns a WAV file: 0.1 s of silence, mono, 8 kHz, 16-bit PCM.
    fn wav() -> Vec<u8> {
        let data_len = 800u32 * 2;
        let mut bytes = Vec::new();
        bytes.extend(b"RIFF");
        bytes.extend((36 + data_len).to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes()); // PCM
        bytes.extend(1u16.to_le_bytes()); // channels
        bytes.extend(8000u32.to_le_bytes()); // sample rate
        bytes.extend(16000u32.to_le_bytes()); // bytes per second
        bytes.extend(2u16.to_le_bytes()); // bytes per frame
        bytes.extend(16u16.to_le_bytes()); // bits per sample
        bytes.extend(b"data");
        bytes.extend(data_len.to_le_bytes());
        bytes.resize(bytes.len() + data_len as usize, 0);
        bytes
    }

    /// Returns an AIFF file: 0.1 s of silence, mono, 8 kHz, 16-bit PCM.
    fn aiff() -> Vec<u8> {
        let data_len = 800u32 * 2;
        let mut bytes = Vec::new();
        bytes.extend(b"FORM");
        bytes.extend((4 + 26 + 16 + data_len).to_be_bytes());
        bytes.extend(b"AIFFCOMM");
        bytes.extend(18u32.to_be_bytes());
        bytes.extend(1u16.to_be_bytes()); // channels
        bytes.extend(800u32.to_be_bytes()); // sample frames
        bytes.extend(16u16.to_be_bytes()); // bits per sample
        // The sample rate 8000 as an 80-bit extended float.
        bytes.extend([0x40, 0x0b, 0xfa, 0, 0, 0, 0, 0, 0, 0]);
        bytes.extend(b"SSND");
        bytes.extend((8 + data_len).to_be_bytes());
        bytes.extend([0; 8]); // offset and block size
        bytes.resize(bytes.len() + data_len as usize, 0);
        bytes
    }

    #[test]
    fn supported_extensions_are_the_nine_of_the_design() {
        assert_eq!(
            SUPPORTED_EXTENSIONS,
            [
                "flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "aif", "aiff"
            ]
        );
    }

    #[test]
    fn is_supported_ignores_the_case_of_the_extension() {
        for name in ["a.flac", "a.FLAC", "a.Mp3", "dir.wav/b.OPUS", "a.b.AiFf"] {
            assert!(is_supported(Path::new(name)), "{name}");
        }
        for name in [
            "a.txt",
            "a.jpg",
            "a.lrc",
            "a.wma",
            "flac",
            ".flac",
            "a.flac.bak",
            "a.",
            "",
        ] {
            assert!(!is_supported(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn content_type_follows_the_extension() {
        let expected = [
            ("a.flac", "flac", "audio/flac"),
            ("a.mp3", "mp3", "audio/mpeg"),
            ("a.m4a", "m4a", "audio/mp4"),
            ("a.aac", "aac", "audio/aac"),
            ("a.ogg", "ogg", "audio/ogg"),
            ("a.opus", "opus", "audio/ogg"),
            ("a.wav", "wav", "audio/wav"),
            ("a.aif", "aif", "audio/aiff"),
            ("a.aiff", "aiff", "audio/aiff"),
            ("A.FLAC", "flac", "audio/flac"),
        ];
        for (name, suffix, content_type) in expected {
            assert_eq!(
                format_of(Path::new(name)),
                Some((suffix, content_type)),
                "{name}"
            );
        }
    }

    #[test]
    fn lossless_formats_are_flac_wav_aiff_and_alac() {
        for suffix in ["flac", "wav", "aif", "aiff"] {
            assert!(is_lossless(suffix, false), "{suffix}");
        }
        assert!(is_lossless("m4a", true));
        assert!(!is_lossless("m4a", false));
        for suffix in ["mp3", "aac", "ogg", "opus"] {
            assert!(!is_lossless(suffix, false), "{suffix}");
            // The codec of an MP4 container counts for the suffix `m4a` only.
            assert!(!is_lossless(suffix, true), "{suffix}");
        }
    }

    #[test]
    fn number_is_the_part_before_the_total() {
        assert_eq!(parse_number("3"), Some(3));
        assert_eq!(parse_number("3/12"), Some(3));
        assert_eq!(parse_number("03"), Some(3));
        assert_eq!(parse_number("3 / 12"), Some(3));
        assert_eq!(parse_number("12/"), Some(12));
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("/12"), None);
        assert_eq!(parse_number("A1"), None);
        assert_eq!(parse_number("-3"), None);
    }

    #[test]
    fn year_is_the_leading_four_digits() {
        assert_eq!(parse_year("2019"), Some(2019));
        assert_eq!(parse_year("2019-05-01"), Some(2019));
        assert_eq!(parse_year("2019-05-01T12:00:00Z"), Some(2019));
        assert_eq!(parse_year("20190501"), Some(2019));
        assert_eq!(parse_year("0000"), None);
        assert_eq!(parse_year("201"), None);
        assert_eq!(parse_year("May 2019"), None);
        assert_eq!(parse_year("+201"), None);
        // Four full-width digits.
        assert_eq!(parse_year("\u{ff12}\u{ff10}\u{ff11}\u{ff19}"), None);
        assert_eq!(parse_year(""), None);
    }

    #[test]
    fn flag_is_set_by_one_and_by_true() {
        for value in ["1", "true", "True", "TRUE"] {
            assert!(is_set(value), "{value}");
        }
        for value in ["0", "false", "", "no", "2"] {
            assert!(!is_set(value), "{value}");
        }
    }

    #[test]
    fn album_artist_tag_is_kept() {
        assert_eq!(
            resolve_album_artist(Some("Marlow Quartet"), "Tessa Lund", false),
            ("Marlow Quartet".to_owned(), false)
        );
        assert_eq!(
            resolve_album_artist(Some("Marlow Quartet"), "Tessa Lund", true),
            ("Marlow Quartet".to_owned(), true)
        );
    }

    #[test]
    fn missing_album_artist_falls_back_to_the_artist() {
        assert_eq!(
            resolve_album_artist(None, "Tessa Lund", false),
            ("Tessa Lund".to_owned(), false)
        );
    }

    #[test]
    fn compilation_flag_without_album_artist_yields_various_artists() {
        assert_eq!(
            resolve_album_artist(None, "Tessa Lund", true),
            ("Various Artists".to_owned(), true)
        );
    }

    #[test]
    fn various_artists_in_any_case_sets_compilation() {
        for name in ["Various Artists", "various artists", "VARIOUS ARTISTS"] {
            assert_eq!(
                resolve_album_artist(Some(name), "Tessa Lund", false),
                ("Various Artists".to_owned(), true),
                "{name}"
            );
        }
        // The track artist is the album artist when the tag is missing, so
        // the rule covers it too.
        assert_eq!(
            resolve_album_artist(None, "various artists", false),
            ("Various Artists".to_owned(), true)
        );
    }

    #[test]
    fn zero_property_is_missing() {
        assert_eq!(positive(Some(44100u32)), Some(44100));
        assert_eq!(positive(Some(0u32)), None);
        assert_eq!(positive(Some(0u8)), None);
        assert_eq!(positive(None::<u32>), None);
    }

    #[test]
    fn picture_content_type_is_named_by_the_tag_or_detected() {
        let named = Picture::unchecked(vec![1, 2, 3])
            .mime_type(MimeType::Jpeg)
            .build();
        assert_eq!(picture_content_type(&named), "image/jpeg");

        let unnamed = Picture::unchecked(PNG_SIGNATURE.to_vec()).build();
        assert_eq!(picture_content_type(&unnamed), "image/png");

        let misnamed = Picture::unchecked(PNG_SIGNATURE.to_vec())
            .mime_type(MimeType::Unknown("PNG".to_owned()))
            .build();
        assert_eq!(picture_content_type(&misnamed), "image/png");

        let unknown = Picture::unchecked(vec![1, 2, 3]).build();
        assert_eq!(picture_content_type(&unknown), "application/octet-stream");
    }

    #[test]
    fn front_cover_is_preferred_over_the_first_picture() {
        let picture = |pic_type, byte: u8| {
            Picture::unchecked(vec![byte])
                .pic_type(pic_type)
                .mime_type(MimeType::Png)
                .build()
        };
        let mut without_front = Tag::new(TagType::Id3v2);
        without_front.push_picture(picture(PictureType::Artist, 1));
        without_front.push_picture(picture(PictureType::CoverBack, 2));
        let mut with_front = without_front.clone();
        with_front.push_picture(picture(PictureType::CoverFront, 3));

        assert_eq!(front_picture(None).map(Picture::data), None);
        assert_eq!(
            front_picture(Some(&Tag::new(TagType::Id3v2))).map(Picture::data),
            None
        );
        assert_eq!(
            front_picture(Some(&without_front)).map(Picture::data),
            Some(&[1u8][..])
        );
        assert_eq!(
            front_picture(Some(&with_front)).map(Picture::data),
            Some(&[3u8][..])
        );
    }

    #[test]
    fn fields_come_from_the_primary_tag_or_the_first_tag() {
        let tag_type = |file_type, tags: &[TagType]| -> Option<TagType> {
            let file = TaggedFile::new(
                file_type,
                FileProperties::default(),
                tags.iter().map(|tag_type| Tag::new(*tag_type)).collect(),
            );
            main_tag(&file).map(Tag::tag_type)
        };

        assert_eq!(
            tag_type(
                FileType::Mpeg,
                &[TagType::Id3v1, TagType::Ape, TagType::Id3v2]
            ),
            Some(TagType::Id3v2)
        );
        assert_eq!(
            tag_type(FileType::Wav, &[TagType::RiffInfo, TagType::Id3v2]),
            Some(TagType::Id3v2)
        );
        // A file without a tag of its primary type.
        assert_eq!(
            tag_type(FileType::Mpeg, &[TagType::Id3v1, TagType::Ape]),
            Some(TagType::Id3v1)
        );
        assert_eq!(tag_type(FileType::Flac, &[]), None);
    }

    #[test]
    fn blank_value_is_missing() {
        let mut tag = Tag::new(TagType::Id3v2);
        tag.insert_text(ItemKey::TrackTitle, "  Title  ".to_owned());
        tag.insert_text(ItemKey::TrackArtist, "   ".to_owned());

        assert_eq!(text(Some(&tag), ItemKey::TrackTitle), Some("Title"));
        assert_eq!(text(Some(&tag), ItemKey::TrackArtist), None);
        assert_eq!(text(Some(&tag), ItemKey::AlbumTitle), None);
        assert_eq!(text(None, ItemKey::TrackTitle), None);
    }

    #[test]
    fn causes_are_joined_without_a_repeated_message() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Middle);
        #[derive(Debug, thiserror::Error)]
        #[error("reason")]
        struct Middle(#[source] Inner);
        #[derive(Debug, thiserror::Error)]
        #[error("reason")]
        struct Inner(#[source] std::fmt::Error);

        assert_eq!(
            causes(&Outer(Middle(Inner(std::fmt::Error)))),
            format!("outer: reason: {}", std::fmt::Error)
        );
        assert_eq!(causes(&std::fmt::Error), std::fmt::Error.to_string());
    }

    /// Returns the bytes of the fixture `a.<ext>` with a title and a front
    /// cover of `picture` bytes in the tag of its primary type, as lofty
    /// writes them. `wav` and `aiff` are the files that this module builds.
    fn tagged(ext: &str, picture: usize) -> Vec<u8> {
        use lofty::config::WriteOptions;
        use lofty::tag::TagExt;

        let name = format!("a.{ext}");
        let bytes = match ext {
            "wav" => wav(),
            "aiff" => aiff(),
            _ => {
                let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/audio")
                    .join(&name);
                fs::read(&fixture).unwrap_or_else(|error| panic!("{}: {error}", fixture.display()))
            }
        };
        let (_dir, path) = write_file(&name, &bytes);
        let file_type = FileType::from_ext(ext).expect("a format of lofty");
        let mut tag = Tag::new(file_type.primary_tag_type());
        tag.insert_text(ItemKey::TrackTitle, "Tagged".to_owned());
        tag.push_picture(
            Picture::unchecked(vec![0x5a; picture])
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        tag.save_to_path(&path, WriteOptions::default())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        fs::read(&path).expect("read the tagged file")
    }

    /// The seven containers of the supported formats.
    const CONTAINERS: [&str; 7] = ["flac", "mp3", "m4a", "ogg", "opus", "wav", "aiff"];

    /// Parses `bytes` as a file with the extension `ext` and at most `limit`
    /// bytes read. Returns the result and the reader, which has counted.
    fn parse_counted(
        bytes: &[u8],
        ext: &str,
        limit: u64,
    ) -> (Result<Parsed, ParseError>, Counting<Cursor<Vec<u8>>>) {
        let mut reader = Counting::new(Cursor::new(bytes.to_vec()));
        let parsed = parse_reader(
            &mut reader,
            FileType::from_ext(ext),
            ParseOptions::new(),
            limit,
        );
        (parsed, reader)
    }

    #[test]
    fn a_tagged_file_of_each_container_is_read_within_the_bound() {
        for ext in CONTAINERS {
            let bytes = tagged(ext, 64 * 1024);
            let (parsed, reader) = parse_counted(&bytes, ext, MAX_TAG_BYTES);
            let parsed = parsed.unwrap_or_else(|error| panic!("{ext}: {error:?}"));
            let tag = main_tag(&parsed.file);
            assert_eq!(text(tag, ItemKey::TrackTitle), Some("Tagged"), "{ext}");
            let picture = front_picture(tag).unwrap_or_else(|| panic!("{ext}: no picture"));
            assert_eq!(picture.data(), vec![0x5a; 64 * 1024], "{ext}");
            // The picture is among the bytes that were read.
            assert!(reader.bytes > 64 * 1024, "{ext}: {}", reader.bytes);
        }
    }

    #[test]
    fn the_headers_of_each_container_declare_the_picture_in_its_tag() {
        for ext in CONTAINERS {
            let bytes = tagged(ext, 64 * 1024);
            let file_type = FileType::from_ext(ext).expect("a format of lofty");
            let mut reader = Counting::new(Cursor::new(&bytes));
            let declared = bound::declared(&mut reader, file_type, u64::MAX);
            // Vorbis comments hold a picture in Base64, which takes four
            // bytes for three.
            assert!(declared >= 64 * 1024, "{ext}: {declared}");
            assert!(declared <= bytes.len() as u64, "{ext}: {declared}");
            // The walk read headers and no byte of the picture.
            assert!(reader.bytes < 1024, "{ext}: {} bytes read", reader.bytes);
        }
    }

    #[test]
    fn a_file_whose_headers_declare_more_than_the_limit_is_refused_and_its_tag_is_not_read() {
        for ext in CONTAINERS {
            let bytes = tagged(ext, 64 * 1024);
            // The picture alone is four times the limit.
            let (parsed, reader) = parse_counted(&bytes, ext, 16 * 1024);
            assert!(matches!(parsed, Err(ParseError::TooLarge)), "{ext}");
            // The reader was asked for the bytes that detect the format and
            // for headers: far less than the limit and than the picture. No
            // read had room for the picture, so lofty did not read the tag.
            assert!(reader.bytes < 4096, "{ext}: {} bytes read", reader.bytes);
            assert!(reader.longest < 4096, "{ext}: a read of {}", reader.longest);
        }
    }

    #[test]
    fn the_limit_is_exact_for_each_container() {
        for ext in CONTAINERS {
            let bytes = tagged(ext, 64 * 1024);
            let file_type = FileType::from_ext(ext).expect("a format of lofty");
            let declared = bound::declared(&mut Cursor::new(&bytes), file_type, u64::MAX);
            // What a read of the file takes: the detection of the format,
            // the headers and what lofty reads.
            let (_, unlimited) = parse_counted(&bytes, ext, u64::MAX);
            let needed = unlimited.bytes;

            // A limit of one byte less refuses the file, and the reader
            // hands out no byte past the one that shows that the file goes
            // on.
            let (parsed, reader) = parse_counted(&bytes, ext, needed - 1);
            assert!(matches!(parsed, Err(ParseError::TooLarge)), "{ext}");
            assert!(reader.bytes <= needed, "{ext}: {}", reader.bytes);

            // The file is read with the smallest limit that holds what the
            // read takes and what the headers declare. The headers of an
            // MP4 file declare the sample tables too, which lofty does not
            // read.
            let (parsed, reader) = parse_counted(&bytes, ext, needed.max(declared));
            assert!(parsed.is_ok(), "{ext}: {:?}", parsed.err());
            assert_eq!(reader.bytes, needed, "{ext}");
        }
    }

    #[test]
    fn the_reader_stops_a_file_whose_headers_declare_less_than_lofty_reads() {
        // An MP3 file that starts with 64 KiB of zero bytes. No header
        // declares them, and lofty reads them one by one to find the tag
        // that follows. With a limit of 16 KiB the reader ends that.
        let mut bytes = vec![0u8; 64 * 1024];
        bytes.extend(tagged("mp3", 1024));
        let file_type = FileType::Mpeg;
        assert!(bound::declared(&mut Cursor::new(&bytes), file_type, u64::MAX) <= 128);

        let (parsed, reader) = parse_counted(&bytes, "mp3", 16 * 1024);
        assert!(matches!(parsed, Err(ParseError::TooLarge)));
        // One byte past the limit shows that the file goes on.
        assert!(reader.bytes <= 16 * 1024 + 1, "{} bytes read", reader.bytes);

        // The same file is read when the limit has room for the zero bytes.
        let (parsed, _) = parse_counted(&bytes, "mp3", 128 * 1024);
        let parsed = parsed.expect("within the limit");
        assert_eq!(
            text(main_tag(&parsed.file), ItemKey::TrackTitle),
            Some("Tagged")
        );
    }

    /// The ten bytes of an ID3v2 header that declares a tag of 256 MiB.
    const HUGE_ID3V2: [u8; 10] = [b'I', b'D', b'3', 4, 0, 0, 0x7f, 0x7f, 0x7f, 0x7f];

    /// Returns a file of a few bytes whose headers declare a tag region over
    /// [`MAX_TAG_BYTES`] for the container of `ext`. No byte of the region is
    /// in the file.
    fn declares_a_huge_tag(ext: &str) -> Vec<u8> {
        let riff = |form: &[u8; 4], kind: &[u8; 4], chunk: &[u8; 4], size: [u8; 4]| {
            [&form[..], &[0; 4], kind, chunk, &size].concat()
        };
        match ext {
            "mp3" | "aac" => HUGE_ID3V2.to_vec(),
            // An ID3v2 tag before the stream marker, which lofty reads.
            "flac" => HUGE_ID3V2.to_vec(),
            // A file type atom and the header of a `moov` atom of 2 GiB.
            "m4a" => [
                &16u32.to_be_bytes()[..],
                b"ftypM4A \0\0\0\0",
                &0x8000_0000u32.to_be_bytes(),
                b"moov",
            ]
            .concat(),
            // A page with the identification packet, and the header of a
            // page whose 255 segments are full, so that the comment packet
            // goes on after it. lofty detects the codec from the first bytes
            // of the identification packet.
            "ogg" | "opus" => {
                let mut file = b"OggS".to_vec();
                file.resize(26, 0);
                file.extend([1, 30]);
                file.extend(if ext == "ogg" {
                    &b"\x01vorbis"[..]
                } else {
                    &b"OpusHead"[..]
                });
                file.resize(27 + 1 + 30, 0);
                file.extend(b"OggS");
                file.resize(file.len() + 22, 0);
                file.push(255);
                file.resize(file.len() + 255, 255);
                file
            }
            "wav" => riff(b"RIFF", b"WAVE", b"LIST", 0x7000_0000u32.to_le_bytes()),
            "aif" | "aiff" => riff(b"FORM", b"AIFF", b"ID3 ", 0x7000_0000u32.to_be_bytes()),
            _ => panic!("no file for {ext}"),
        }
    }

    #[test]
    fn a_header_that_declares_a_tag_over_the_bound_is_refused_without_the_bytes_being_read() {
        for ext in ["mp3", "aac", "flac", "m4a", "wav", "aif", "aiff"] {
            let bytes = declares_a_huge_tag(ext);
            assert!(bytes.len() < 64, "{ext}");
            let (parsed, reader) = parse_counted(&bytes, ext, MAX_TAG_BYTES);
            assert!(matches!(parsed, Err(ParseError::TooLarge)), "{ext}");
            // Every byte that the reader handed out is a byte of the file,
            // and no read asked for more than the detection of the format
            // does. The bound is 32 MiB.
            assert!(
                reader.bytes <= 4 * bytes.len() as u64,
                "{ext}: {}",
                reader.bytes
            );
            assert!(
                reader.longest <= 8192,
                "{ext}: a read of {}",
                reader.longest
            );

            // The same file on disk, through the functions that the scanner
            // and the cover service call.
            let (_dir, path) = write_file(&format!("huge.{ext}"), &bytes);
            let error = read(&path).expect_err("over the bound");
            assert!(
                matches!(&error, TagError::TooLarge(refused) if *refused == path),
                "{ext}: {error:?}"
            );
            assert!(error.to_string().contains("more than 32 MiB"), "{error}");
            assert_eq!(read_front_picture(&path), None, "{ext}");
        }
    }

    #[test]
    fn an_ogg_comment_packet_over_the_bound_is_refused_page_by_page() {
        // The segment table of one page declares 65 025 bytes of the comment
        // packet. A file needs 517 such pages to pass 32 MiB. This file has
        // the header of each page and no content, as a file that was cut
        // would have, so the limit of the test is that of two pages.
        for ext in ["ogg", "opus"] {
            let mut bytes = declares_a_huge_tag(ext);
            let page = bytes[bytes.len() - 282..].to_vec();
            bytes.resize(bytes.len() + 255 * 255, 0);
            bytes.extend(&page);
            let (parsed, reader) = parse_counted(&bytes, ext, 100_000);
            assert!(matches!(parsed, Err(ParseError::TooLarge)), "{ext}");
            assert!(reader.bytes < 4096, "{ext}: {} bytes read", reader.bytes);
            assert!(
                reader.longest <= 8192,
                "{ext}: a read of {}",
                reader.longest
            );
        }
    }

    #[test]
    fn a_file_that_is_cut_inside_its_tag_is_an_error_and_no_panic() {
        // Each tagged file is cut at every 499th byte, which covers the
        // headers, the tag and the picture, and is read with and without
        // the properties, as the scanner and the cover service read it.
        for ext in CONTAINERS {
            let bytes = tagged(ext, 8 * 1024);
            for cut in (0..bytes.len()).step_by(499) {
                for options in [
                    ParseOptions::new(),
                    ParseOptions::new().read_properties(false),
                ] {
                    let reader = Cursor::new(&bytes[..cut]);
                    let file_type = FileType::from_ext(ext);
                    // The result differs with the place of the cut. A cut
                    // file is never over the bound.
                    let parsed = parse_reader(reader, file_type, options, MAX_TAG_BYTES);
                    assert!(
                        !matches!(parsed, Err(ParseError::TooLarge)),
                        "{ext} cut at {cut}"
                    );
                }
            }
            // The whole file is read on disk through the public functions.
            let (_dir, path) = write_file(&format!("cut.{ext}"), &bytes[..bytes.len() / 2]);
            let _ = read(&path);
            let _ = read_front_picture(&path);
        }
    }

    /// Returns a zlib stream that inflates to `prefix` followed by
    /// `runs` × 258 copies of the last byte of `prefix`, and the number of
    /// bytes that it inflates to. The stream is one Deflate block with the
    /// fixed Huffman codes: a literal for each byte of `prefix`, then `runs`
    /// matches of the length 258 at the distance 1, which take 13 bits each.
    fn zlib_of_a_run(prefix: &[u8], runs: usize) -> (Vec<u8>, usize) {
        /// Writes the codes of a Deflate block, which start at the lowest
        /// bit of each byte. A Huffman code is written from its highest bit.
        #[derive(Default)]
        struct Bits {
            bytes: Vec<u8>,
            current: u8,
            used: u8,
        }
        impl Bits {
            fn bit(&mut self, bit: bool) {
                self.current |= u8::from(bit) << self.used;
                self.used += 1;
                if self.used == 8 {
                    self.bytes.push(self.current);
                    (self.current, self.used) = (0, 0);
                }
            }
            fn code(&mut self, code: u32, len: u32) {
                for shift in (0..len).rev() {
                    self.bit((code >> shift) & 1 == 1);
                }
            }
        }

        let last = *prefix.last().expect("a byte to repeat");
        let mut inflated = prefix.to_vec();
        let mut bits = Bits::default();
        // The last block of the stream, with the fixed codes.
        bits.bit(true);
        bits.bit(true);
        bits.bit(false);
        for &literal in prefix {
            match literal {
                0..=143 => bits.code(0x30 + u32::from(literal), 8),
                _ => bits.code(0x190 + u32::from(literal) - 144, 9),
            }
        }
        for _ in 0..runs {
            bits.code(0xc5, 8); // The length 258.
            bits.code(0, 5); // The distance 1.
            inflated.resize(inflated.len() + 258, last);
        }
        bits.code(0, 7); // The end of the block.
        while bits.used != 0 {
            bits.bit(false);
        }
        // The Adler-32 checksum of the inflated bytes ends a zlib stream.
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &inflated {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        let mut stream = vec![0x78, 0x9c];
        stream.extend(bits.bytes);
        stream.extend(((b << 16) | a).to_be_bytes());
        (stream, inflated.len())
    }

    /// Returns the MP3 fixture behind an ID3v2.4 tag with one `APIC` frame
    /// whose content is `content`. `flags` are the two flag bytes of the
    /// frame, and `inflated` is the length that the frame declares for its
    /// content after decompression.
    fn mp3_with_picture_frame(content: &[u8], flags: [u8; 2], inflated: usize) -> Vec<u8> {
        let seven_bits = |number: usize| {
            [21, 14, 7, 0].map(|shift| u8::try_from((number >> shift) & 0x7f).expect("7 bits"))
        };
        let mut frame = b"APIC".to_vec();
        frame.extend(seven_bits(content.len() + 4));
        frame.extend(flags);
        frame.extend(seven_bits(inflated));
        frame.extend(content);
        let mut bytes = b"ID3\x04\0\0".to_vec();
        bytes.extend(seven_bits(frame.len()));
        bytes.extend(frame);
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audio/a.mp3");
        bytes.extend(fs::read(fixture).expect("read the fixture"));
        bytes
    }

    /// A present limit of the tag bound (`docs/design.md` section 3.10): the
    /// bound limits the bytes that lofty reads and not what lofty makes of
    /// them, and lofty inflates a compressed ID3v2 frame without a limit.
    /// The test holds the figures that the design states. It fails when
    /// lofty no longer inflates such a frame; the design then changes with
    /// it.
    #[test]
    fn a_compressed_id3v2_frame_is_inflated_past_the_limit_of_the_read() {
        // The content of a picture frame: Latin-1, the type `image/png`, a
        // front cover, no description, and a picture of one byte that the
        // stream repeats.
        let mut prefix = vec![0];
        prefix.extend(b"image/png\0");
        prefix.extend([3, 0, 0x5a]);
        let (stream, inflated) = zlib_of_a_run(&prefix, 40_000);
        // The flags of the frame: compressed, with a data length indicator.
        let bytes = mp3_with_picture_frame(&stream, [0, 0x09], inflated);
        assert_eq!(bytes.len(), 67_547);

        let limit = 64 * 1024;
        let (parsed, reader) = parse_counted(&bytes, "mp3", limit);
        let parsed = parsed.expect("the read stays within the limit");
        assert!(reader.bytes < limit, "{} bytes read", reader.bytes);
        let picture = front_picture(main_tag(&parsed.file)).expect("the inflated picture");
        // 158 times the limit of the read.
        assert_eq!(picture.data().len(), 10_320_001);
        assert!(picture.data().iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn garbage_flac_is_an_error() {
        let garbage: Vec<u8> = (0..100u8)
            .map(|index| index.wrapping_mul(37).wrapping_add(11))
            .collect();
        let (_dir, path) = write_file("garbage.flac", &garbage);

        let error = read(&path).expect_err("100 bytes of garbage are not a FLAC file");
        assert!(matches!(error, TagError::Read { .. }), "{error:?}");
        let message = error.to_string();
        assert!(
            message.contains("garbage.flac") && message.contains("Flac"),
            "{message}"
        );
        assert_eq!(read_front_picture(&path), None);
    }

    #[test]
    fn empty_file_is_an_error() {
        for name in [
            "a.flac", "a.mp3", "a.m4a", "a.aac", "a.ogg", "a.opus", "a.wav", "a.aiff",
        ] {
            let (_dir, path) = write_file(name, &[]);
            let error = read(&path).expect_err("an empty file is not an audio file");
            assert!(matches!(error, TagError::Read { .. }), "{name}: {error:?}");
        }
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().expect("create a temporary directory");
        let path = dir.path().join("absent.mp3");

        let error = read(&path).expect_err("the file does not exist");
        assert!(matches!(error, TagError::Read { .. }), "{error:?}");
        assert_eq!(read_front_picture(&path), None);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        let (_dir, path) = write_file("notes.txt", b"not audio");

        let error = read(&path).expect_err("txt is not an audio extension");
        assert!(
            matches!(&error, TagError::UnsupportedExtension(rejected) if *rejected == path),
            "{error:?}"
        );
        assert_eq!(read_front_picture(&path), None);
    }

    #[test]
    fn wav_and_aiff_are_lossless_and_have_a_bit_depth() {
        // lofty counts the eight bytes that precede the samples of an AIFF
        // sound chunk as audio, which raises the bitrate from 128 to 129.
        let files = [
            ("Silence.wav", wav(), "wav", "audio/wav", 128),
            ("Silence.aif", aiff(), "aif", "audio/aiff", 129),
            ("Silence.AIFF", aiff(), "aiff", "audio/aiff", 129),
        ];
        for (name, bytes, suffix, content_type, bitrate_kbps) in files {
            let (_dir, path) = write_file(name, &bytes);
            let meta = read(&path).unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(
                meta,
                TrackMeta {
                    title: "Silence".to_owned(),
                    artist: UNKNOWN_ARTIST.to_owned(),
                    album_artist: UNKNOWN_ARTIST.to_owned(),
                    album: UNKNOWN_ALBUM.to_owned(),
                    track_no: None,
                    disc_no: None,
                    year: None,
                    genre: None,
                    compilation: false,
                    duration_ms: 100,
                    bitrate_kbps: Some(bitrate_kbps),
                    sample_rate: Some(8000),
                    bit_depth: Some(16),
                    channels: Some(1),
                    has_picture: false,
                    suffix: suffix.to_owned(),
                    content_type: content_type.to_owned(),
                    lossless: true,
                },
                "{name}"
            );
        }
    }
}
