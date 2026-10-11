//! Tests of `library::tags` on the audio fixtures. `LibraryBuilder` writes the
//! tags that the reader reads back.

mod common;

use std::fs;
use std::path::Path;

use common::{LibraryBuilder, PICTURE_PNG, STANDARD_LYRICS};
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, Tag, TagExt};
use wn_server::library::tags::{self, TrackMeta, UNKNOWN_ALBUM, UNKNOWN_ARTIST, VARIOUS_ARTISTS};

/// One fixture format: the extension and the properties of `a.<ext>`.
struct Format {
    ext: &'static str,
    content_type: &'static str,
    lossless: bool,
    duration_ms: u64,
    bitrate_kbps: u32,
    sample_rate: u32,
    bit_depth: Option<u8>,
}

/// The five fixture formats. Every fixture is mono and holds 0.5 s of audio.
/// lofty reports 575 ms for the MP3 fixture and 546 ms for the M4A fixture.
// rustfmt would spread each row over nine lines.
#[rustfmt::skip]
const FORMATS: [Format; 5] = [
    Format { ext: "flac", content_type: "audio/flac", lossless: true, duration_ms: 500, bitrate_kbps: 58, sample_rate: 22050, bit_depth: Some(16) },
    Format { ext: "mp3", content_type: "audio/mpeg", lossless: false, duration_ms: 575, bitrate_kbps: 34, sample_rate: 22050, bit_depth: None },
    Format { ext: "m4a", content_type: "audio/mp4", lossless: false, duration_ms: 546, bitrate_kbps: 34, sample_rate: 22050, bit_depth: None },
    Format { ext: "ogg", content_type: "audio/ogg", lossless: false, duration_ms: 500, bitrate_kbps: 24, sample_rate: 22050, bit_depth: None },
    Format { ext: "opus", content_type: "audio/ogg", lossless: false, duration_ms: 500, bitrate_kbps: 28, sample_rate: 24000, bit_depth: None },
];

/// A lyrics file with LRC time stamps.
const LRC: &str = "[00:01.00]First line\n[00:02.50]Second line\n";

/// Reads the metadata of the audio file at `path`.
fn read(path: &Path) -> TrackMeta {
    tags::read(path).unwrap_or_else(|error| panic!("{error}"))
}

/// Returns the metadata of a track that `LibraryBuilder::track` added with
/// the artist "Artist" and the album "Album" and that has no optional tag.
fn plain(format: &Format, title: &str, track_no: u32) -> TrackMeta {
    TrackMeta {
        title: title.to_owned(),
        artist: "Artist".to_owned(),
        album_artist: "Artist".to_owned(),
        album: "Album".to_owned(),
        track_no: Some(track_no),
        disc_no: None,
        year: None,
        genre: None,
        compilation: false,
        duration_ms: format.duration_ms,
        bitrate_kbps: Some(format.bitrate_kbps),
        sample_rate: Some(format.sample_rate),
        bit_depth: format.bit_depth,
        channels: Some(1),
        has_picture: false,
        has_lyrics: false,
        suffix: format.ext.to_owned(),
        content_type: format.content_type.to_owned(),
        lossless: format.lossless,
    }
}

/// Changes the primary tag of the audio file at `path` with `edit` and saves
/// it.
fn retag(path: &Path, edit: impl FnOnce(&mut Tag)) {
    let mut file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let tag = file
        .primary_tag_mut()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()));
    edit(tag);
    tag.save_to_path(path, WriteOptions::default())
        .unwrap_or_else(|error| panic!("write the tag of {}: {error}", path.display()));
}

/// Returns whether the file at `path` holds the bytes of `text`. A test uses
/// it to check that a tag is stored in the form the test is about.
fn holds(path: &Path, text: &str) -> bool {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

#[test]
fn read_returns_every_field_in_each_format() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Full", 7, format.ext)
            .year(2004)
            .genre("Rock")
            .disc(2)
            .album_artist("Album Artist")
            .picture()
            .lyrics("First line\nSecond line")
            .track("Artist", "Album", "Plain", 12, format.ext)
            .build();

        assert_eq!(
            read(&library.files[0]),
            TrackMeta {
                album_artist: "Album Artist".to_owned(),
                disc_no: Some(2),
                year: Some(2004),
                genre: Some("Rock".to_owned()),
                has_picture: true,
                has_lyrics: true,
                ..plain(format, "Full", 7)
            },
            "{}",
            format.ext
        );
        // A missing album artist is the artist.
        assert_eq!(
            read(&library.files[1]),
            plain(format, "Plain", 12),
            "{}",
            format.ext
        );
    }
}

#[test]
fn untagged_file_yields_the_fallbacks_and_a_duration() {
    let dir = tempfile::tempdir().expect("create a temporary directory");
    for format in &FORMATS {
        let fixture = common::fixtures_dir().join(format!("a.{}", format.ext));
        let path = dir.path().join(format!("Field Recording.{}", format.ext));
        fs::copy(&fixture, &path).expect("copy the fixture");

        let meta = read(&path);
        assert_eq!(
            meta,
            TrackMeta {
                artist: UNKNOWN_ARTIST.to_owned(),
                album_artist: UNKNOWN_ARTIST.to_owned(),
                album: UNKNOWN_ALBUM.to_owned(),
                track_no: None,
                ..plain(format, "Field Recording", 0)
            },
            "{}",
            format.ext
        );
        assert!(meta.duration_ms > 0, "{}", format.ext);
    }
}

#[test]
fn compilation_flag_without_album_artist_yields_various_artists() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, format.ext)
            .compilation()
            .build();

        assert_eq!(
            read(&library.files[0]),
            TrackMeta {
                album_artist: VARIOUS_ARTISTS.to_owned(),
                compilation: true,
                ..plain(format, "Title", 1)
            },
            "{}",
            format.ext
        );
    }
}

#[test]
fn compilation_flag_keeps_the_album_artist_tag() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, format.ext)
            .album_artist("Album Artist")
            .compilation()
            .build();

        assert_eq!(
            read(&library.files[0]),
            TrackMeta {
                album_artist: "Album Artist".to_owned(),
                compilation: true,
                ..plain(format, "Title", 1)
            },
            "{}",
            format.ext
        );
    }
}

#[test]
fn album_artist_various_artists_in_any_case_sets_compilation() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, format.ext)
            .album_artist("VARIOUS artists")
            .build();

        assert_eq!(
            read(&library.files[0]),
            TrackMeta {
                album_artist: "Various Artists".to_owned(),
                compilation: true,
                ..plain(format, "Title", 1)
            },
            "{}",
            format.ext
        );
    }
}

#[test]
fn has_picture_agrees_with_read_front_picture() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "With", 1, format.ext)
            .picture()
            .track("Artist", "Album", "Without", 2, format.ext)
            .build();

        assert!(read(&library.files[0]).has_picture, "{}", format.ext);
        assert_eq!(
            tags::read_front_picture(&library.files[0]),
            Some((PICTURE_PNG.to_vec(), "image/png".to_owned())),
            "{}",
            format.ext
        );
        assert!(!read(&library.files[1]).has_picture, "{}", format.ext);
        assert_eq!(
            tags::read_front_picture(&library.files[1]),
            None,
            "{}",
            format.ext
        );
    }
}

#[test]
fn lyrics_tag_sets_has_lyrics() {
    let text = "First line\nSecond line";
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "With", 1, format.ext)
            .lyrics(text)
            .track("Artist", "Album", "Without", 2, format.ext)
            .build();

        assert!(read(&library.files[0]).has_lyrics, "{}", format.ext);
        assert_eq!(
            tags::read_lyrics(&library.files[0]).as_deref(),
            Some(text),
            "{}",
            format.ext
        );
        assert!(!read(&library.files[1]).has_lyrics, "{}", format.ext);
        assert_eq!(tags::read_lyrics(&library.files[1]), None, "{}", format.ext);
    }
}

#[test]
fn sidecar_lrc_sets_has_lyrics() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Sidecar", 1, format.ext)
            .track("Artist", "Album", "Both", 2, format.ext)
            .lyrics("Tag line")
            .track("Artist", "Album", "Neither", 3, format.ext)
            .build();
        for path in &library.files[..2] {
            fs::write(path.with_extension("lrc"), LRC).expect("write the lyrics file");
        }

        assert!(read(&library.files[0]).has_lyrics, "{}", format.ext);
        assert_eq!(
            tags::read_lyrics(&library.files[0]).as_deref(),
            Some(LRC),
            "{}",
            format.ext
        );
        // The lyrics file is read before the tag.
        assert!(read(&library.files[1]).has_lyrics, "{}", format.ext);
        assert_eq!(
            tags::read_lyrics(&library.files[1]).as_deref(),
            Some(LRC),
            "{}",
            format.ext
        );
        // The lyrics file of another track does not count.
        assert!(!read(&library.files[2]).has_lyrics, "{}", format.ext);
        assert_eq!(tags::read_lyrics(&library.files[2]), None, "{}", format.ext);
    }
}

#[test]
fn sidecar_lrc_is_read_without_the_byte_order_mark() {
    let library = LibraryBuilder::new()
        .track("Artist", "Album", "Title", 1, "flac")
        .build();
    let sidecar = library.files[0].with_extension("lrc");

    fs::write(&sidecar, format!("\u{feff}{LRC}")).expect("write the lyrics file");
    assert_eq!(tags::read_lyrics(&library.files[0]).as_deref(), Some(LRC));

    // A byte that is not valid UTF-8 becomes the replacement character.
    fs::write(&sidecar, b"[00:01.00]Caf\xe9\n").expect("write the lyrics file");
    assert_eq!(
        tags::read_lyrics(&library.files[0]).as_deref(),
        Some("[00:01.00]Caf\u{fffd}\n")
    );
}

#[test]
fn number_with_a_total_and_a_full_date_are_reduced() {
    for format in &FORMATS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, format.ext)
            .build();
        let path = &library.files[0];
        let vorbis_comments = matches!(format.ext, "flac" | "ogg" | "opus");
        retag(path, |tag| {
            if vorbis_comments {
                // A Vorbis comment holds the number and the total in one
                // value.
                tag.insert_text(ItemKey::TrackNumber, "3/12".to_owned());
                tag.insert_text(ItemKey::DiscNumber, "1/2".to_owned());
            } else {
                // lofty writes the pair as the text `3/12` into the `TRCK`
                // frame of an MP3 file and as two integers into the `trkn`
                // atom of an M4A file.
                tag.insert_text(ItemKey::TrackNumber, "3".to_owned());
                tag.insert_text(ItemKey::TrackTotal, "12".to_owned());
                tag.insert_text(ItemKey::DiscNumber, "1".to_owned());
                tag.insert_text(ItemKey::DiscTotal, "2".to_owned());
            }
            tag.insert_text(ItemKey::RecordingDate, "2019-05-01".to_owned());
        });
        if format.ext != "m4a" {
            for text in ["3/12", "1/2", "2019-05-01"] {
                assert!(holds(path, text), "{}: {text}", format.ext);
            }
        }

        assert_eq!(
            read(path),
            TrackMeta {
                disc_no: Some(1),
                year: Some(2019),
                ..plain(format, "Title", 3)
            },
            "{}",
            format.ext
        );
    }
}

#[test]
fn year_tag_is_read_when_the_date_has_no_year() {
    let library = LibraryBuilder::new()
        .track("Artist", "Album", "Year Only", 1, "flac")
        .track("Artist", "Album", "Bad Date", 2, "flac")
        .year(2004)
        .build();
    retag(&library.files[0], |tag| {
        tag.insert_text(ItemKey::Year, "1999".to_owned());
    });
    retag(&library.files[1], |tag| {
        tag.insert_text(ItemKey::RecordingDate, "unknown".to_owned());
        tag.insert_text(ItemKey::Year, "1999".to_owned());
    });

    assert!(holds(&library.files[0], "YEAR=1999"));
    assert!(!holds(&library.files[0], "DATE="));
    assert!(holds(&library.files[1], "YEAR=1999"));
    assert!(holds(&library.files[1], "DATE=unknown"));

    assert_eq!(read(&library.files[0]).year, Some(1999));
    assert_eq!(read(&library.files[1]).year, Some(1999));
}

#[test]
fn id3v1_tag_fills_the_fields_the_id3v2_tag_lacks() {
    let library = LibraryBuilder::new()
        .track("Artist", "Album", "Title", 1, "mp3")
        .build();
    let path = &library.files[0];

    // An ID3v1.1 tag is the last 128 bytes of the file.
    let mut trailer = b"TAG".to_vec();
    for field in ["Old Title", "Old Artist", "Old Album"] {
        let mut bytes = [0u8; 30];
        bytes[..field.len()].copy_from_slice(field.as_bytes());
        trailer.extend(bytes);
    }
    trailer.extend(b"1988");
    trailer.extend([0u8; 28]); // comment
    trailer.extend([0, 9]); // track number
    trailer.push(17); // genre: Rock
    assert_eq!(trailer.len(), 128);
    let mut bytes = fs::read(path).expect("read the track");
    bytes.extend(trailer);
    fs::write(path, bytes).expect("write the track");

    // ID3v2 is the primary tag of an MP3 file, so its fields win.
    let meta = read(path);
    assert_eq!(
        (
            meta.title.as_str(),
            meta.artist.as_str(),
            meta.album.as_str()
        ),
        ("Title", "Artist", "Album")
    );
    assert_eq!(meta.track_no, Some(1));
    assert_eq!(meta.year, Some(1988));
    assert_eq!(meta.genre.as_deref(), Some("Rock"));
}

#[test]
fn opus_stream_in_an_ogg_file_is_read() {
    let opus = &FORMATS[4];
    let dir = tempfile::tempdir().expect("create a temporary directory");
    let path = dir.path().join("Take.ogg");
    fs::copy(common::fixtures_dir().join("a.opus"), &path).expect("copy the fixture");

    // The content decides the format. The extension decides the suffix.
    assert_eq!(
        read(&path),
        TrackMeta {
            artist: UNKNOWN_ARTIST.to_owned(),
            album_artist: UNKNOWN_ARTIST.to_owned(),
            album: UNKNOWN_ALBUM.to_owned(),
            track_no: None,
            suffix: "ogg".to_owned(),
            ..plain(opus, "Take", 0)
        }
    );
}

#[test]
fn standard_library_reads_back_as_the_catalogue() {
    let library = LibraryBuilder::standard().build();
    let metas: Vec<TrackMeta> = library.files.iter().map(|path| read(path)).collect();

    // Title, artist, album artist, album, track, disc, year, genre,
    // compilation, suffix.
    type Row<'a> = (
        &'a str,
        &'a str,
        &'a str,
        &'a str,
        Option<u32>,
        Option<u32>,
        Option<u32>,
        Option<&'a str>,
        bool,
        &'a str,
    );
    let rows: Vec<Row> = metas
        .iter()
        .map(|meta| {
            (
                meta.title.as_str(),
                meta.artist.as_str(),
                meta.album_artist.as_str(),
                meta.album.as_str(),
                meta.track_no,
                meta.disc_no,
                meta.year,
                meta.genre.as_deref(),
                meta.compilation,
                meta.suffix.as_str(),
            )
        })
        .collect();
    // rustfmt would spread each row over twelve lines.
    #[rustfmt::skip]
    let expected: [Row; 10] = [
        ("Low Tide", "Aurora Field", "Aurora Field", "Glass Harbour", Some(1), None, Some(2019), Some("Electronic"), false, "flac"),
        ("Breakwater", "Aurora Field", "Aurora Field", "Glass Harbour", Some(2), None, Some(2019), Some("Electronic"), false, "flac"),
        ("Night Ferry", "Aurora Field", "Aurora Field", "Glass Harbour", Some(3), None, Some(2019), Some("Electronic"), false, "flac"),
        ("Meridian", "Aurora Field", "Aurora Field", "Paper Maps", Some(1), Some(1), Some(2021), Some("Electronic"), false, "mp3"),
        ("Contour", "Aurora Field", "Aurora Field", "Paper Maps", Some(2), Some(1), Some(2021), Some("Electronic"), false, "mp3"),
        ("Legend", "Aurora Field", "Aurora Field", "Paper Maps", Some(1), Some(2), Some(2021), Some("Electronic"), false, "mp3"),
        ("Upstream", "Marlow Quartet", "Marlow Quartet", "Slow Rivers", Some(1), None, Some(2015), Some("Jazz"), false, "m4a"),
        ("Delta", "Marlow Quartet", "Marlow Quartet", "Slow Rivers", Some(2), None, Some(2015), Some("Jazz"), false, "m4a"),
        ("Open Road", "Tessa Lund", "Various Artists", "Summer Sampler", Some(1), None, Some(2020), Some("Pop"), true, "ogg"),
        ("Last Train", "Aurora Field", "Various Artists", "Summer Sampler", Some(2), None, Some(2020), Some("Pop"), true, "opus"),
    ];
    assert_eq!(rows, expected);

    let titles_where = |keep: fn(&TrackMeta) -> bool| -> Vec<&str> {
        metas
            .iter()
            .filter(|meta| keep(meta))
            .map(|meta| meta.title.as_str())
            .collect()
    };
    assert_eq!(titles_where(|meta| meta.has_picture), ["Upstream", "Delta"]);
    assert_eq!(titles_where(|meta| meta.has_lyrics), ["Open Road"]);
    assert_eq!(
        titles_where(|meta| meta.lossless),
        ["Low Tide", "Breakwater", "Night Ferry"]
    );
    assert_eq!(
        tags::read_lyrics(&library.files[8]).as_deref(),
        Some(STANDARD_LYRICS)
    );
    // The cover file of "Glass Harbour" is not an embedded picture.
    assert_eq!(tags::read_front_picture(&library.files[0]), None);
}
