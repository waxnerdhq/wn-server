//! Checks that discovery accepts the instance ID that `init` writes. No test
//! here starts the mDNS daemon or opens a socket.

use std::fs;
use std::process::Command;

use wn_server::cli::PASSWORD_ENV;
use wn_server::config::{Config, DATA_DIR_ENV};
use wn_server::discovery::{self, TxtRecord};

#[test]
fn the_instance_id_that_init_writes_is_the_txt_id() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(env!("CARGO_BIN_EXE_wn-server"))
        .env_remove(DATA_DIR_ENV)
        .env(PASSWORD_ENV, "sesame")
        .arg("init")
        .arg("--data-dir")
        .arg(dir.path())
        .output()
        .expect("run wn-server");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let path = dir.path().join("instance.id");
    let id = discovery::read_instance_id(&path).expect("an instance ID");
    assert_eq!(
        fs::read_to_string(&path).expect("read instance.id"),
        format!("{id}\n")
    );

    let record = TxtRecord::build(&Config::default(), wn_server::VERSION, &id);
    assert!(record.contains(&("id".to_owned(), id)), "{record:?}");
}
