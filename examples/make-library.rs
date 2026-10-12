//! Writes the library of the end-to-end session (`examples/e2e.rs`) to a
//! directory:
//!
//! ```text
//! cargo run --example make-library -- <directory>
//! ```
//!
//! The library is the standard catalogue of the tests
//! (`LibraryBuilder::standard` in `tests/common/mod.rs`): 3 artists, 4 albums
//! and 10 tracks in the five fixture formats, with a `cover.jpg` file of 2 × 2
//! pixels in "Glass Harbour". The example adds one file to it: a `cover.png`
//! file of 600 × 400 pixels in "Paper Maps". That image is larger than the
//! first three steps of `getCoverArt`, so the session reads a cover that the
//! server scaled down.
//!
//! The directory is created when it is missing. A directory that holds
//! anything is refused, so the example writes no file next to other files.
//!
//! Exit status: 0 when the library was written, 1 when it was not, and 2 for
//! a wrong number of arguments.

// The module is the test support of `tests/`. The example uses its library
// builder and none of its servers.
#[path = "../tests/common/mod.rs"]
mod common;

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use image::{Rgb, RgbImage};

use common::LibraryBuilder;

const USAGE: &str = "usage: cargo run --example make-library -- <directory>";

/// The album directory that gets the large cover, below the library.
const LARGE_COVER_ALBUM: [&str; 2] = ["Aurora Field", "Paper Maps"];

/// The file name of the large cover. The scanner takes a file with this name
/// as the cover of the album in its directory.
const LARGE_COVER_NAME: &str = "cover.png";

/// The width and the height of the large cover in pixels.
const LARGE_COVER_SIZE: (u32, u32) = (600, 400);

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let (Some(directory), None) = (args.next(), args.next()) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let directory = PathBuf::from(directory);
    match write_library(&directory) {
        Ok(written) => {
            println!(
                "wrote {} tracks and {} cover images to {}",
                written.tracks,
                written.covers,
                written.directory.display()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("make-library: {error}");
            ExitCode::FAILURE
        }
    }
}

/// What [`write_library`] wrote.
struct Written {
    /// The absolute path of the library.
    directory: PathBuf,
    /// The number of audio files.
    tracks: usize,
    /// The number of image files.
    covers: usize,
}

/// Writes the library to `directory`, which is missing or empty.
fn write_library(directory: &Path) -> Result<Written, String> {
    let directory = std::path::absolute(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    fs::create_dir_all(&directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let holds_an_entry = fs::read_dir(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .next()
        .is_some();
    if holds_an_entry {
        return Err(format!("{} is not empty", directory.display()));
    }

    // The builder writes to a temporary directory of its own, which is
    // deleted when `library` is dropped.
    let library = LibraryBuilder::standard().build();
    let copied = copy_tree(library.root.path(), &directory)
        .map_err(|error| format!("copy the library to {}: {error}", directory.display()))?;

    let mut cover = directory.clone();
    cover.extend(LARGE_COVER_ALBUM);
    cover.push(LARGE_COVER_NAME);
    large_cover()
        .save(&cover)
        .map_err(|error| format!("{}: {error}", cover.display()))?;

    Ok(Written {
        directory,
        tracks: library.files.len(),
        covers: copied - library.files.len() + 1,
    })
}

/// Copies every file below `from` to the same relative path below `to` and
/// returns the number of files.
fn copy_tree(from: &Path, to: &Path) -> io::Result<usize> {
    let mut files = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            files += copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            files += 1;
        }
    }
    Ok(files)
}

/// The large cover: a gradient, so that the image is not one colour and a
/// scaled copy of it differs from an image of one pixel.
fn large_cover() -> RgbImage {
    let (width, height) = LARGE_COVER_SIZE;
    RgbImage::from_fn(width, height, |x, y| {
        let red = u8::try_from(x * 255 / (width - 1)).unwrap_or(u8::MAX);
        let green = u8::try_from(y * 255 / (height - 1)).unwrap_or(u8::MAX);
        Rgb([red, green, 128])
    })
}
