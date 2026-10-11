//! Tests of the committed audio fixtures and of `LibraryBuilder`.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use common::{LibraryBuilder, PICTURE_PNG, STANDARD_LYRICS, TestLibrary};
use image::ImageFormat;
use lofty::file::TaggedFileExt;
use lofty::picture::MimeType;
use lofty::tag::{Accessor, ItemKey, Tag};
use walkdir::WalkDir;

/// The extensions of the five fixtures.
const EXTENSIONS: [&str; 5] = ["flac", "mp3", "m4a", "ogg", "opus"];

/// One track of the standard catalogue: artist, album, title, track number,
/// disc number and extension.
type Row = (
    &'static str,
    &'static str,
    &'static str,
    u32,
    Option<u32>,
    &'static str,
);

/// The standard catalogue in the order `LibraryBuilder::standard` adds the
/// tracks.
// rustfmt would spread the longer rows over several lines.
#[rustfmt::skip]
const STANDARD: [Row; 10] = [
    ("Aurora Field", "Glass Harbour", "Low Tide", 1, None, "flac"),
    ("Aurora Field", "Glass Harbour", "Breakwater", 2, None, "flac"),
    ("Aurora Field", "Glass Harbour", "Night Ferry", 3, None, "flac"),
    ("Aurora Field", "Paper Maps", "Meridian", 1, Some(1), "mp3"),
    ("Aurora Field", "Paper Maps", "Contour", 2, Some(1), "mp3"),
    ("Aurora Field", "Paper Maps", "Legend", 1, Some(2), "mp3"),
    ("Marlow Quartet", "Slow Rivers", "Upstream", 1, None, "m4a"),
    ("Marlow Quartet", "Slow Rivers", "Delta", 2, None, "m4a"),
    ("Tessa Lund", "Summer Sampler", "Open Road", 1, None, "ogg"),
    ("Aurora Field", "Summer Sampler", "Last Train", 2, None, "opus"),
];

/// Reads the audio file at `path` with lofty and returns its primary tag.
fn read_tag(path: &Path) -> Tag {
    let file = lofty::read_from_path(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    file.primary_tag()
        .unwrap_or_else(|| panic!("{} has no primary tag", path.display()))
        .clone()
}

/// Returns the lyrics of a tag. lofty reports the lyrics of an M4A file under
/// `ItemKey::Lyrics` and those of the other formats under
/// `ItemKey::UnsyncLyrics`.
fn lyrics(tag: &Tag) -> Option<&str> {
    tag.get_string(ItemKey::UnsyncLyrics)
        .or_else(|| tag.get_string(ItemKey::Lyrics))
}

/// Returns the components of the path of track `index` below the library root.
fn components(library: &TestLibrary, index: usize) -> Vec<String> {
    library.files[index]
        .strip_prefix(library.root.path())
        .expect("a track is below the library root")
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect()
}

/// Returns the titles of the tags that `keep` accepts.
fn titles_where(tags: &[Tag], keep: impl Fn(&Tag) -> bool) -> Vec<&str> {
    tags.iter()
        .filter(|tag| keep(tag))
        .filter_map(|tag| tag.get_string(ItemKey::TrackTitle))
        .collect()
}

#[test]
fn fixtures_are_five_files_within_the_size_budget() {
    let mut sizes = Vec::new();
    for entry in fs::read_dir(common::fixtures_dir()).expect("read the fixtures directory") {
        let entry = entry.expect("read a fixtures directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        // A file manager can leave a hidden file such as `.DS_Store` here. Git
        // ignores it and so does this test.
        if name.starts_with('.') {
            continue;
        }
        let size = entry.metadata().expect("read a fixture's metadata").len();
        sizes.push((name, size));
    }
    sizes.sort();

    let names: Vec<&str> = sizes.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["a.flac", "a.m4a", "a.mp3", "a.ogg", "a.opus"]);
    for (name, size) in &sizes {
        assert!(*size < 16 * 1024, "{name} is {size} bytes");
    }
    let total: u64 = sizes.iter().map(|(_, size)| size).sum();
    assert!(total < 64 * 1024, "the fixtures total {total} bytes");
}

#[test]
fn builder_round_trips_tags_in_every_format() {
    for ext in EXTENSIONS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Full", 7, ext)
            .year(2004)
            .genre("Rock")
            .disc(2)
            .album_artist("Album Artist")
            .compilation()
            .track("Artist", "Album", "Plain", 12, ext)
            .build();

        assert_eq!(library.files.len(), 2, "{ext}");
        assert_eq!(
            components(&library, 0),
            ["Artist", "Album", &format!("07 Full.{ext}")]
        );
        assert_eq!(
            components(&library, 1),
            ["Artist", "Album", &format!("12 Plain.{ext}")]
        );

        let full = read_tag(&library.files[0]);
        assert_eq!(
            full.get_string(ItemKey::TrackArtist),
            Some("Artist"),
            "{ext}"
        );
        assert_eq!(full.get_string(ItemKey::AlbumTitle), Some("Album"), "{ext}");
        assert_eq!(full.get_string(ItemKey::TrackTitle), Some("Full"), "{ext}");
        assert_eq!(full.track(), Some(7), "{ext}");
        assert_eq!(
            full.get_string(ItemKey::RecordingDate),
            Some("2004"),
            "{ext}"
        );
        assert_eq!(full.get_string(ItemKey::Genre), Some("Rock"), "{ext}");
        assert_eq!(full.disk(), Some(2), "{ext}");
        assert_eq!(
            full.get_string(ItemKey::AlbumArtist),
            Some("Album Artist"),
            "{ext}"
        );
        assert_eq!(
            full.get_string(ItemKey::FlagCompilation),
            Some("1"),
            "{ext}"
        );

        let plain = read_tag(&library.files[1]);
        assert_eq!(
            plain.get_string(ItemKey::TrackArtist),
            Some("Artist"),
            "{ext}"
        );
        assert_eq!(
            plain.get_string(ItemKey::AlbumTitle),
            Some("Album"),
            "{ext}"
        );
        assert_eq!(
            plain.get_string(ItemKey::TrackTitle),
            Some("Plain"),
            "{ext}"
        );
        assert_eq!(plain.track(), Some(12), "{ext}");
        for key in [
            ItemKey::RecordingDate,
            ItemKey::Genre,
            ItemKey::DiscNumber,
            ItemKey::AlbumArtist,
            ItemKey::FlagCompilation,
        ] {
            assert_eq!(plain.get_string(key), None, "{ext} {key:?}");
        }
        assert_eq!(lyrics(&plain), None, "{ext}");
        assert_eq!(plain.picture_count(), 0, "{ext}");
    }
}

#[test]
fn builder_embeds_the_picture_in_every_format() {
    let decoded = image::load_from_memory_with_format(&PICTURE_PNG, ImageFormat::Png)
        .expect("decode the embedded picture");
    assert_eq!((decoded.width(), decoded.height()), (2, 2));

    for ext in EXTENSIONS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, ext)
            .picture()
            .build();
        let tag = read_tag(&library.files[0]);

        assert_eq!(tag.picture_count(), 1, "{ext}");
        let picture = &tag.pictures()[0];
        assert_eq!(picture.data(), PICTURE_PNG, "{ext}");
        assert_eq!(picture.mime_type(), Some(&MimeType::Png), "{ext}");
    }
}

#[test]
fn builder_writes_the_lyrics_in_every_format() {
    let text = "First line\nSecond line";
    for ext in EXTENSIONS {
        let library = LibraryBuilder::new()
            .track("Artist", "Album", "Title", 1, ext)
            .lyrics(text)
            .build();
        let tag = read_tag(&library.files[0]);

        assert_eq!(lyrics(&tag), Some(text), "{ext}");
    }
}

#[test]
fn cover_file_is_an_image_in_the_album_directory() {
    let covers = [
        ("cover.jpg", ImageFormat::Jpeg),
        ("folder.jpeg", ImageFormat::Jpeg),
        ("front.png", ImageFormat::Png),
        ("album.webp", ImageFormat::WebP),
    ];
    let mut builder = LibraryBuilder::new().track("Artist", "Album", "Title", 1, "flac");
    for (name, _) in covers {
        builder = builder.cover_file("Artist", "Album", name);
    }
    let library = builder.build();

    // A cover file is not a track.
    assert_eq!(library.files.len(), 1);
    let album = library.root.path().join("Artist").join("Album");
    for (name, format) in covers {
        let bytes =
            fs::read(album.join(name)).unwrap_or_else(|error| panic!("read {name}: {error}"));
        assert_eq!(image::guess_format(&bytes).ok(), Some(format), "{name}");
        let decoded = image::load_from_memory(&bytes)
            .unwrap_or_else(|error| panic!("decode {name}: {error}"));
        assert_eq!((decoded.width(), decoded.height()), (2, 2), "{name}");
    }
}

#[test]
fn standard_library_reads_back_as_the_catalogue() {
    let library = LibraryBuilder::standard().build();
    assert_eq!(library.files.len(), STANDARD.len());

    let tags: Vec<Tag> = library.files.iter().map(|path| read_tag(path)).collect();
    // Every track is at its path and has its tags. The disc numbers put the
    // tracks of one album on discs 1 and 2.
    for (index, (artist, album, title, track_no, disc, ext)) in STANDARD.into_iter().enumerate() {
        assert_eq!(
            components(&library, index),
            [artist, album, &format!("{track_no:02} {title}.{ext}")]
        );
        let tag = &tags[index];
        assert_eq!(
            tag.get_string(ItemKey::TrackArtist),
            Some(artist),
            "{title}"
        );
        assert_eq!(tag.get_string(ItemKey::AlbumTitle), Some(album), "{title}");
        assert_eq!(tag.get_string(ItemKey::TrackTitle), Some(title), "{title}");
        assert_eq!(tag.track(), Some(track_no), "{title}");
        assert_eq!(tag.disk(), disc, "{title}");
    }

    let artists: BTreeSet<&str> = tags
        .iter()
        .filter_map(|tag| tag.get_string(ItemKey::TrackArtist))
        .collect();
    assert_eq!(artists.len(), 3);
    let albums: BTreeSet<&str> = tags
        .iter()
        .filter_map(|tag| tag.get_string(ItemKey::AlbumTitle))
        .collect();
    assert_eq!(albums.len(), 4);

    // One compilation. Its tracks have the flag and no album-artist tag.
    assert_eq!(
        titles_where(&tags, |tag| tag.get_string(ItemKey::FlagCompilation)
            == Some("1")),
        ["Open Road", "Last Train"]
    );
    assert_eq!(
        titles_where(&tags, |tag| tag.get_string(ItemKey::AlbumArtist).is_some()),
        ["Upstream", "Delta"]
    );
    // One album with embedded art.
    assert_eq!(
        titles_where(&tags, |tag| tag.picture_count() > 0),
        ["Upstream", "Delta"]
    );
    // One track with lyrics.
    assert_eq!(
        titles_where(&tags, |tag| lyrics(tag).is_some()),
        ["Open Road"]
    );
    assert_eq!(
        titles_where(&tags, |tag| lyrics(tag) == Some(STANDARD_LYRICS)),
        ["Open Road"]
    );

    // One album with `cover.jpg`. It is the only file that is not a track.
    let root = library.root.path();
    let cover = root
        .join("Aurora Field")
        .join("Glass Harbour")
        .join("cover.jpg");
    let others: Vec<_> = WalkDir::new(root)
        .into_iter()
        .map(|entry| entry.expect("walk the library").into_path())
        .filter(|path| path.is_file() && !library.files.contains(path))
        .collect();
    assert_eq!(others, [cover]);
}

#[test]
fn standard_library_names_are_valid_windows_path_components() {
    let library = LibraryBuilder::standard().build();
    let mut names = BTreeSet::new();
    for path in &library.files {
        let tag = read_tag(path);
        for key in [
            ItemKey::TrackArtist,
            ItemKey::AlbumTitle,
            ItemKey::TrackTitle,
        ] {
            let name = tag
                .get_string(key)
                .unwrap_or_else(|| panic!("{} has no {key:?}", path.display()));
            names.insert(name.to_owned());
        }
    }

    for name in &names {
        assert!(!name.is_empty());
        assert!(
            !name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']),
            "{name:?} holds a character that Windows forbids"
        );
        assert!(
            !name.ends_with(['.', ' ']),
            "{name:?} ends with a dot or a space"
        );
    }
    let folded: BTreeSet<String> = names.iter().map(|name| name.to_lowercase()).collect();
    assert_eq!(folded.len(), names.len(), "two names differ only by case");
}
