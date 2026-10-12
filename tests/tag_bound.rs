//! Tests of the tag bound (`docs/design.md` section 3.10, "The tag bound")
//! through a scan and through the API: what a scan does with a file whose
//! tags are over the bound, and what `getCoverArt` and `stream` answer for a
//! track that has such a file.
//!
//! A file over the bound is a file of a few bytes whose header declares a
//! tag of 256 MiB. No test writes a large file. The unit tests in
//! `src/library/tags.rs` and `src/library/tags/bound.rs` cover each
//! container and show, with a reader that counts, that such a file is
//! refused without its tag being read.

mod common;

use std::fs;
use std::path::Path;

use serde_json::Value;
use wn_server::db::queries::scans;
use wn_server::library::scanner::ScanSummary;
use wn_server::library::tags::{self, MAX_TAG_BYTES, TagError};

use common::{LibraryBuilder, ScannedServer, TestServer};

/// The header of an ID3v2 tag that declares 256 MiB of frames. lofty reads
/// such a tag at the start of an MP3 file and before the stream marker of a
/// FLAC file.
const HUGE_ID3V2: [u8; 10] = [b'I', b'D', b'3', 4, 0, 0, 0x7f, 0x7f, 0x7f, 0x7f];

/// The relative path of the first track of [`library`].
const FIRST: &str = "Tessa Lund/Night Windows/01 Amber.mp3";

/// The relative path of the second track of [`library`].
const SECOND: &str = "Tessa Lund/Night Windows/02 Cobalt.flac";

/// A server over a scanned library of one album with two tracks. Both have
/// an embedded picture, and the album has no cover file, so the cover of the
/// album is the picture of its first track.
async fn library() -> ScannedServer {
    let library = LibraryBuilder::new()
        .track("Tessa Lund", "Night Windows", "Amber", 1, "mp3")
        .picture()
        .track("Tessa Lund", "Night Windows", "Cobalt", 2, "flac")
        .picture()
        .build();
    TestServer::scanned(library).await
}

/// Puts [`HUGE_ID3V2`] before the bytes of the file at `path`. The file is
/// ten bytes longer, so a scan sees that it changed.
fn declare_a_huge_tag(path: &Path) {
    let bytes = fs::read(path).expect("read the track");
    fs::write(path, [&HUGE_ID3V2[..], &bytes].concat()).expect("write the track");
}

/// The error that a scan records for the file at `rel_path` of `server`
/// when the file is over the tag bound.
fn refusal(server: &ScannedServer, rel_path: &str) -> String {
    let root = server.root();
    let name = root.file_name().expect("a name").to_string_lossy();
    format!(
        "\"{rel_path}\" in music folder \"{name}\" ({}) is refused: reading its tags takes more \
         than 32 MiB of the file; it gets no track, and a track that it has stays as it is",
        root.display()
    )
}

/// The error of the last scan in the `scans` table.
async fn stored_error(server: &ScannedServer) -> Option<String> {
    server
        .db()
        .read(scans::latest)
        .await
        .expect("read the scans")
        .expect("a scan has run")
        .error
}

/// The titles of the tracks that the library holds, in the order of the
/// catalogue.
async fn titles(server: &ScannedServer) -> Vec<String> {
    let catalogue = server.catalogue().await;
    catalogue
        .songs
        .iter()
        .map(|song| song.track.title.clone())
        .collect()
}

/// The error of an API response: its code and its message.
fn error_of(response: &Value) -> (i64, String) {
    assert_eq!(response["status"], "failed", "{response}");
    (
        response["error"]["code"].as_i64().expect("a code"),
        response["error"]["message"]
            .as_str()
            .expect("a message")
            .to_owned(),
    )
}

/// Checks that `summary` is the summary of a scan whose one problem is the
/// refused file at `rel_path`.
fn assert_one_refusal(server: &ScannedServer, summary: &ScanSummary, rel_path: &str) {
    assert_eq!(summary.errors, [refusal(server, rel_path)]);
}

#[test]
fn the_bound_of_the_messages_is_the_bound_of_the_reader() {
    assert_eq!(MAX_TAG_BYTES, 32 * 1024 * 1024);
}

#[tokio::test]
async fn a_new_file_over_the_bound_gets_no_track_and_one_error_in_each_scan() {
    let server = library().await;
    let rel_path = "Tessa Lund/Night Windows/03 Huge.mp3";
    fs::write(server.path(rel_path), HUGE_ID3V2).expect("write the file");

    // The tag reader refuses the file before it reads a tag.
    let error = tags::read(&server.path(rel_path)).expect_err("over the bound");
    assert!(matches!(error, TagError::TooLarge(_)), "{error:?}");

    let summary = server.rescan().await;
    // One error, which names the path below its folder. The scan read the
    // tags of the file once.
    assert_one_refusal(&server, &summary, rel_path);
    assert_eq!((summary.seen, summary.tags_read), (3, 1));
    assert_eq!(
        (
            summary.added,
            summary.changed,
            summary.moved,
            summary.removed
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(titles(&server).await, ["Amber", "Cobalt"]);
    assert_eq!(
        stored_error(&server).await,
        Some(refusal(&server, rel_path))
    );

    // The file has no row, so the next scan finds it again and records the
    // same one error. Nothing is recorded between the scans.
    let summary = server.rescan().await;
    assert_one_refusal(&server, &summary, rel_path);
    assert_eq!(titles(&server).await, ["Amber", "Cobalt"]);

    // When the file is gone, the scan has no error.
    fs::remove_file(server.path(rel_path)).expect("remove the file");
    let summary = server.rescan().await;
    assert_eq!(summary.errors, [""; 0]);
    assert_eq!(stored_error(&server).await, None);
}

#[tokio::test]
async fn a_file_over_the_bound_holds_no_deletion_back() {
    let server = library().await;
    let rel_path = "Tessa Lund/Night Windows/03 Huge.flac";
    fs::write(server.path(rel_path), HUGE_ID3V2).expect("write the file");
    fs::remove_file(server.path(SECOND)).expect("remove a track");

    // The scan read the state and the fingerprint of the refused file, so it
    // knows that the file is not the file of the track that is gone. The
    // track is deleted, and the scan has the one error of the refused file:
    // no message says that a track was kept.
    let summary = server.rescan().await;
    assert_one_refusal(&server, &summary, rel_path);
    assert_eq!(summary.removed, 1);
    assert_eq!(titles(&server).await, ["Amber"]);
}

#[tokio::test]
async fn a_track_whose_file_grows_over_the_bound_keeps_its_row_and_what_is_attached_to_it() {
    let server = library().await;
    let before = server.catalogue().await;
    let amber = before.song("Amber").track.id.clone();
    let cobalt = before.song("Cobalt").track.id.clone();
    let album = before.album("Night Windows").id.clone();

    for (method, params) in [
        ("star", vec![("id", amber.as_str())]),
        ("setRating", vec![("id", amber.as_str()), ("rating", "4")]),
        ("scrobble", vec![("id", amber.as_str())]),
    ] {
        let response = server.get_json(method, &params).await;
        assert_eq!(response["status"], "ok", "{method}: {response}");
    }
    let song = server.get_json("getSong", &[("id", &amber)]).await["song"].clone();
    assert_eq!(song["userRating"], 4, "{song}");
    assert_eq!(song["playCount"], 1, "{song}");
    assert!(song["starred"].is_string(), "{song}");
    // The picture of the first track is the cover of the album.
    let cover = server.get_raw("getCoverArt", &[("id", &album)]).await;
    assert_eq!(common::content_type(&cover), "image/png");

    // The tag of the file grows over the bound at its path.
    declare_a_huge_tag(&server.path(FIRST));
    let summary = server.rescan().await;
    assert_one_refusal(&server, &summary, FIRST);
    assert_eq!((summary.changed, summary.removed), (0, 0));

    // The row is the row of before the scan, with its ID, its tags and its
    // annotations.
    let after = server.catalogue().await;
    assert_eq!(after.song("Amber").track, before.song("Amber").track);
    let kept = server.get_json("getSong", &[("id", &amber)]).await["song"].clone();
    assert_eq!(kept, song);

    // The picture of the file is not read: the track and the album answer
    // as an item without a cover does.
    let missing = error_of(
        &server
            .get_json("getCoverArt", &[("id", "al-00000000000000000000000000")])
            .await,
    );
    assert_eq!(missing.0, 70);
    for id in [&amber, &album] {
        let response = server.get_json("getCoverArt", &[("id", id)]).await;
        assert_eq!(error_of(&response), missing, "{id}");
    }
    // The row holds the size of the file that the last scan read, so the
    // file that is there now is not served.
    let stream = server.get_json("stream", &[("id", &amber)]).await;
    assert_eq!(error_of(&stream).0, 70);

    // The other track is read as before.
    let cover = server.get_raw("getCoverArt", &[("id", &cobalt)]).await;
    assert_eq!(common::content_type(&cover), "image/png");

    // A second scan changes nothing and records the error once more.
    let summary = server.rescan().await;
    assert_one_refusal(&server, &summary, FIRST);
    assert_eq!(
        server.catalogue().await.song("Amber").track,
        before.song("Amber").track
    );
}
