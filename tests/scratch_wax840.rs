//! Scratch file for WAX-840. It uses `std::os::unix` without `#[cfg(unix)]`,
//! so it compiles for Linux and macOS and does not compile for Windows. The
//! commit after this one removes it.

use std::os::unix::fs::PermissionsExt;

#[test]
fn the_temp_directory_has_a_unix_mode() {
    let metadata = std::fs::metadata(std::env::temp_dir()).expect("the temp directory exists");
    assert_ne!(metadata.permissions().mode(), 0);
}
