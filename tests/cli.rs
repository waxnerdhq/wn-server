//! Runs the `wn-server` binary for the commands that work without a database:
//! `init`, `config path` and `config show`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;
use wn_server::config::{Config, DATA_DIR_ENV};

/// The binary, without the data directory variable of the developer's shell.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wn-server"));
    command.env_remove(DATA_DIR_ENV);
    command
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run wn-server")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn temp_dir() -> TempDir {
    tempfile::tempdir().expect("temp dir")
}

/// The line `config path` prints for the data directory `data_dir`.
fn config_path_line(data_dir: &Path) -> String {
    format!("{}\n", data_dir.join("config.toml").display())
}

/// Runs `init` for the data directory `data_dir` with one music folder.
fn init(data_dir: &Path, folder: &Path) -> Output {
    run(binary()
        .arg("init")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--folder")
        .arg(folder))
}

#[test]
fn init_writes_a_config_that_loads() {
    let dir = temp_dir();
    let music = temp_dir();
    let data_dir = dir.path().join("data");

    let output = init(&data_dir, music.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let config_path = data_dir.join("config.toml");
    assert_eq!(
        stdout(&output),
        format!("wrote {}\n", config_path.display())
    );

    // On Windows the temp directory path has backslashes, so this is also the
    // round trip of a Windows path through the literal string.
    let config = Config::load(&config_path).expect("load");
    let mut expected = Config::default();
    expected.library.folders = vec![music.path().to_path_buf()];
    assert_eq!(config, expected);
}

#[test]
fn init_writes_every_folder_and_the_port() {
    let dir = temp_dir();
    let first = temp_dir();
    let second = temp_dir();

    let output = run(binary()
        .arg("init")
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--folder")
        .arg(first.path())
        .arg("--folder")
        .arg(second.path())
        .args(["--port", "4321"]));
    assert!(output.status.success(), "{}", stderr(&output));

    let config = Config::load(&dir.path().join("config.toml")).expect("load");
    assert_eq!(
        config.library.folders,
        [first.path().to_path_buf(), second.path().to_path_buf()]
    );
    assert_eq!(config.server.port, 4321);
}

#[test]
fn init_writes_the_commented_template() {
    let dir = temp_dir();
    let music = temp_dir();

    let output = init(dir.path(), music.path());
    assert!(output.status.success(), "{}", stderr(&output));

    let text = fs::read_to_string(dir.path().join("config.toml")).expect("read config");
    assert!(text.starts_with("[server]\n"), "{text}");
    assert!(
        text.contains(&format!("folders = ['{}']", music.path().display())),
        "{text}"
    );
    assert!(
        text.contains("# true turns authentication off; unsafe"),
        "{text}"
    );
}

#[test]
fn init_does_not_overwrite_an_existing_config() {
    let dir = temp_dir();
    let music = temp_dir();
    let config_path = dir.path().join("config.toml");
    fs::write(&config_path, "# kept\n").expect("write config");

    let output = init(dir.path(), music.path());
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.contains("already exists"), "{message}");
    assert!(
        message.contains(&config_path.display().to_string()),
        "{message}"
    );
    assert_eq!(
        fs::read_to_string(&config_path).expect("read config"),
        "# kept\n"
    );
}

#[test]
fn init_names_a_missing_folder_and_writes_nothing() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    let missing = dir.path().join("absent");

    let output = init(&data_dir, &missing);
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("does not exist"), "{message}");
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );
    assert!(!data_dir.exists());
}

#[test]
fn init_without_a_folder_writes_a_config_that_needs_one() {
    let dir = temp_dir();

    let output = run(binary().arg("init").arg("--data-dir").arg(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("library.folders"));

    let text = fs::read_to_string(dir.path().join("config.toml")).expect("read config");
    assert!(text.contains("folders = []"), "{text}");

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("library.folders"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn init_rejects_port_zero() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(binary()
        .arg("init")
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["--port", "0"]));
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--port"), "{}", stderr(&output));
    assert!(!data_dir.exists());
}

// Unix only: Windows has no file modes. The user-profile ACL protects the
// directory there.
#[cfg(unix)]
#[test]
fn init_creates_the_data_directory_with_mode_0700() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let music = temp_dir();
    let parent = dir.path().join("parent");
    let data_dir = parent.join("data");

    let output = init(&data_dir, music.path());
    assert!(output.status.success(), "{}", stderr(&output));
    for created in [&parent, &data_dir] {
        let mode = fs::metadata(created)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{}", created.display());
    }
}

// Unix only, for the same reason.
#[cfg(unix)]
#[test]
fn init_keeps_the_mode_of_an_existing_data_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let music = temp_dir();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o750)).expect("set mode");

    let output = init(dir.path(), music.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let mode = fs::metadata(dir.path())
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o750);
}

#[test]
fn config_show_prints_the_effective_config_with_defaults() {
    let dir = temp_dir();
    let music = temp_dir();
    fs::write(
        dir.path().join("config.toml"),
        format!("[library]\nfolders = ['{}']\n", music.path().display()),
    )
    .expect("write config");

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for line in [
        "[server]",
        "bind = \"0.0.0.0\"",
        "port = 4200",
        "log_level = \"info\"",
        "disabled_unsafe = false",
        "scan_on_start = true",
        "rescan_interval_minutes = 0",
        "ffmpeg = \"ffmpeg\"",
        "max_concurrent = 2",
        "opus_bitrate_kbps = 192",
        "mp3_bitrate_kbps = 320",
        "mdns = true",
    ] {
        assert!(
            text.lines().any(|printed| printed == line),
            "{line}: {text}"
        );
    }
    assert!(
        text.contains(&format!("folders = ['{}']", music.path().display())),
        "{text}"
    );

    // The output is a config file with the same meaning.
    let printed = temp_dir();
    let printed_path = printed.path().join("config.toml");
    fs::write(&printed_path, &text).expect("write printed config");
    let mut expected = Config::default();
    expected.library.folders = vec![music.path().to_path_buf()];
    assert_eq!(Config::load(&printed_path).expect("load"), expected);
}

#[test]
fn config_show_shows_the_values_of_the_file() {
    let dir = temp_dir();
    let music = temp_dir();

    let output = run(binary()
        .arg("init")
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--folder")
        .arg(music.path())
        .args(["--port", "4321"]));
    assert!(output.status.success(), "{}", stderr(&output));

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.lines().any(|line| line == "port = 4321"), "{text}");
    assert!(!text.contains('#'), "{text}");
}

#[test]
fn config_show_names_an_unknown_key() {
    let dir = temp_dir();
    let config_path = dir.path().join("config.toml");
    fs::write(&config_path, "[server]\nprot = 4200\n").expect("write config");

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.contains("prot"), "{message}");
    assert!(
        message.contains(&config_path.display().to_string()),
        "{message}"
    );
}

#[test]
fn config_show_names_a_missing_folder() {
    let dir = temp_dir();
    let missing = dir.path().join("absent");
    fs::write(
        dir.path().join("config.toml"),
        format!("[library]\nfolders = ['{}']\n", missing.display()),
    )
    .expect("write config");

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains(&missing.display().to_string()),
        "{message}"
    );
}

#[test]
fn config_show_rejects_port_zero() {
    let dir = temp_dir();
    let music = temp_dir();
    fs::write(
        dir.path().join("config.toml"),
        format!(
            "[server]\nport = 0\n\n[library]\nfolders = ['{}']\n",
            music.path().display()
        ),
    )
    .expect("write config");

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("server.port"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn config_show_without_a_config_names_the_path_and_init() {
    let dir = temp_dir();

    let output = run(binary()
        .args(["config", "show", "--data-dir"])
        .arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains(&dir.path().join("config.toml").display().to_string()),
        "{message}"
    );
    assert!(message.contains("wn-server init"), "{message}");
}

#[test]
fn config_path_prints_the_path_under_the_data_dir_flag() {
    let dir = temp_dir();

    let output = run(binary()
        .args(["config", "path", "--data-dir"])
        .arg(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), config_path_line(dir.path()));
    assert!(
        !dir.path().join("config.toml").exists(),
        "config path creates nothing"
    );
}

#[test]
fn data_dir_variable_overrides_the_platform_default() {
    let dir = temp_dir();
    let from_variable = dir.path().join("variable");

    let output = run(binary()
        .args(["config", "path"])
        .env(DATA_DIR_ENV, &from_variable));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), config_path_line(&from_variable));
}

#[test]
fn data_dir_flag_overrides_the_variable() {
    let dir = temp_dir();
    let from_variable = dir.path().join("variable");
    let from_flag = dir.path().join("flag");

    let output = run(binary()
        .args(["config", "path", "--data-dir"])
        .arg(&from_flag)
        .env(DATA_DIR_ENV, &from_variable));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), config_path_line(&from_flag));
}

// macOS: the default is under the Library directory of the home directory,
// which the `directories` crate reads from HOME.
#[cfg(target_os = "macos")]
#[test]
fn platform_default_is_application_support_on_macos() {
    let home = temp_dir();

    let output = run(binary().args(["config", "path"]).env("HOME", home.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    let data_dir = home
        .path()
        .join("Library")
        .join("Application Support")
        .join("wn-server");
    assert_eq!(stdout(&output), config_path_line(&data_dir));
}

// Linux: the default follows the XDG base directory specification, which is
// `~/.local/share` when XDG_DATA_HOME is unset.
#[cfg(target_os = "linux")]
#[test]
fn platform_default_is_local_share_on_linux() {
    let home = temp_dir();

    let output = run(binary()
        .args(["config", "path"])
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME"));
    assert!(output.status.success(), "{}", stderr(&output));
    let data_dir = home.path().join(".local").join("share").join("wn-server");
    assert_eq!(stdout(&output), config_path_line(&data_dir));
}

// Windows: the default is under the roaming application data folder. Windows
// reports that folder through the known-folder API, not through a variable a
// test can set, so the test compares with the APPDATA value of its own
// environment.
#[cfg(windows)]
#[test]
fn platform_default_is_roaming_app_data_on_windows() {
    let app_data = std::env::var_os("APPDATA").expect("APPDATA is set");

    let output = run(binary().args(["config", "path"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let data_dir = Path::new(&app_data).join("wn-server").join("data");
    assert_eq!(stdout(&output), config_path_line(&data_dir));
}

#[test]
fn commands_of_later_issues_print_not_implemented_and_exit_2() {
    for args in [
        &["run"][..],
        &["scan"],
        &["user", "show"],
        &["user", "set-password"],
    ] {
        let output = run(binary().args(args));
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(stderr(&output), "not implemented yet\n", "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
    }
}

#[test]
fn version_flag_prints_the_version() {
    let output = run(binary().arg("--version"));
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        format!("wn-server {}\n", wn_server::VERSION)
    );
}
