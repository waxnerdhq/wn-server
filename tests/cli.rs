//! Runs the `wn-server` binary for the commands that work without a socket:
//! `init`, `user show`, `user set-password`, `config path`, `config show`, and
//! `run` up to the point where it loads the config.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;
use wn_server::auth::verify_password;
use wn_server::cli::PASSWORD_ENV;
use wn_server::config::{Config, DATA_DIR_ENV};
use wn_server::db::Db;
use wn_server::db::queries::folders::{self, Folder};
use wn_server::db::queries::users::{self, UserRow};

/// The line `init` and `user set-password` print after they store a password.
const NOTICE: &str =
    "The password is stored in plain text in the data directory. Do not reuse it elsewhere.";

/// The binary, without the data directory variable and the password variable
/// of the developer's shell.
fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wn-server"));
    command.env_remove(DATA_DIR_ENV);
    command.env_remove(PASSWORD_ENV);
    command
}

/// Runs the command with an empty stdin.
fn run(command: &mut Command) -> Output {
    command.output().expect("run wn-server")
}

/// Runs the command with `input` on stdin.
fn run_with_stdin(command: &mut Command, input: &str) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start wn-server");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // The write fails when the command exits without reading stdin. The
    // assertions of the test on the exit code and on stderr report that case.
    let _ = stdin.write_all(input.as_bytes());
    // The command sees the end of the input when the pipe closes.
    drop(stdin);
    child.wait_with_output().expect("wait for wn-server")
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

/// `init` for the data directory `data_dir` without a music folder.
fn init_command(data_dir: &Path) -> Command {
    let mut command = binary();
    command.arg("init").arg("--data-dir").arg(data_dir);
    command
}

/// `user set-password` for the data directory `data_dir`.
fn set_password_command(data_dir: &Path) -> Command {
    let mut command = binary();
    command
        .args(["user", "set-password", "--data-dir"])
        .arg(data_dir);
    command
}

/// Runs `init` for the data directory `data_dir` with the password in the
/// variable, and checks that it succeeded.
fn init_with_password(data_dir: &Path, password: &str) {
    let output = run(init_command(data_dir).env(PASSWORD_ENV, password));
    assert!(output.status.success(), "{}", stderr(&output));
}

/// The row of the owner account in the database of `data_dir`.
fn owner_row(data_dir: &Path) -> UserRow {
    let db = Db::open(data_dir.join("library.db")).expect("open the database");
    db.with_reader(users::get_owner)
        .expect("read the owner")
        .expect("an owner account")
}

/// Sets up `data_dir` with `init`, replaces the config file with text that
/// `init` does not write and removes the database. The files SQLite keeps
/// next to the database stay, as they do when the owner deletes `library.db`.
/// Returns the bytes of the config file.
fn data_dir_without_a_database(data_dir: &Path) -> Vec<u8> {
    init_with_password(data_dir, "first");
    let config_path = data_dir.join("config.toml");
    fs::write(
        &config_path,
        "# edited by the owner\n[server]\nport = 4321\n",
    )
    .expect("edit config");
    fs::remove_file(data_dir.join("library.db")).expect("remove the database");
    fs::read(&config_path).expect("read config")
}

/// The lines `init` prints before the lines about the password when it keeps
/// the config file of `data_dir`. `database` is `created` or `kept`.
fn kept_config_lines(data_dir: &Path, database: &str) -> String {
    format!(
        "kept {}\n{database} {}\n",
        data_dir.join("config.toml").display(),
        data_dir.join("library.db").display()
    )
}

/// The rows of the `folders` table in the database of `data_dir`.
fn folder_rows(data_dir: &Path) -> Vec<Folder> {
    let db = Db::open(data_dir.join("library.db")).expect("open the database");
    db.with_reader(folders::list).expect("read the folders")
}

/// The lines `init` prints for the data directory `data_dir` without a music
/// folder, before the lines about the password.
fn init_lines(data_dir: &Path) -> String {
    format!(
        "wrote {}\nno music folder is configured; add one to library.folders in that file\n",
        data_dir.join("config.toml").display()
    )
}

/// The password on the line `Password for <name> (shown once): <password>`
/// of `text`. Panics unless exactly one line starts that way.
fn printed_password(text: &str, name: &str) -> String {
    let start = format!("Password for {name} (shown once): ");
    let mut passwords = text.lines().filter_map(|line| line.strip_prefix(&start));
    let password = passwords.next().expect("a password line");
    assert!(passwords.next().is_none(), "one password line");
    password.to_string()
}

/// Checks that `password` is four groups of five characters from `a-z` and
/// `2-9` without `i`, `l` and `o`, separated by hyphens.
fn assert_generated_shape(password: &str) {
    let groups: Vec<&str> = password.split('-').collect();
    assert_eq!(groups.len(), 4);
    for group in groups {
        assert_eq!(group.len(), 5);
        assert!(
            group
                .chars()
                .all(|character| matches!(character, 'a'..='z' | '2'..='9')
                    && !matches!(character, 'i' | 'l' | 'o'))
        );
    }
}

fn unix_now() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970");
    i64::try_from(elapsed.as_secs()).expect("seconds fit")
}

#[test]
fn init_writes_a_config_that_loads() {
    let dir = temp_dir();
    let music = temp_dir();
    let data_dir = dir.path().join("data");

    let output = init(&data_dir, music.path());
    assert!(output.status.success(), "{}", stderr(&output));
    let config_path = data_dir.join("config.toml");
    let text = stdout(&output);
    assert_eq!(
        text.lines().next(),
        Some(format!("wrote {}", config_path.display()).as_str())
    );

    // On Windows the temp directory path has backslashes, so this is also the
    // round trip of a Windows path through the literal string.
    let config = Config::load(&config_path).expect("load");
    let mut expected = Config::default();
    expected.library.folders = vec![music.path().to_path_buf()];
    assert_eq!(config, expected);
    // `init` writes the fixed default name, which does not depend on the
    // machine.
    assert_eq!(config.server.name, "wn-server");
    let written = fs::read_to_string(&config_path).expect("read config.toml");
    assert!(
        written
            .lines()
            .any(|line| line.starts_with("name = \"wn-server\" ")),
        "{written}"
    );
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
fn init_creates_the_owner_in_a_database_that_has_none_and_keeps_its_rows() {
    let dir = temp_dir();
    let music = temp_dir();
    let config_path = dir.path().join("config.toml");
    fs::write(&config_path, "# kept\n").expect("write config");
    let db = Db::open(dir.path().join("library.db")).expect("create the database");
    db.with_writer(|conn| folders::sync_folders(conn, &[music.path().to_path_buf()]))
        .expect("insert a folder");
    drop(db);
    let rows = folder_rows(dir.path());
    assert_eq!(rows.len(), 1);

    // The error of the `user` commands names the command that follows.
    let show = || {
        run(binary()
            .args(["user", "show", "--data-dir"])
            .arg(dir.path()))
    };
    let output = show();
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("has no owner account; `wn-server init` creates the account"),
        "{message}"
    );

    let output = run(init_command(dir.path()).env(PASSWORD_ENV, "sesame-secret"));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{}{NOTICE}\n", kept_config_lines(dir.path(), "kept"))
    );
    assert!(stderr(&output).is_empty());
    let row = owner_row(dir.path());
    assert_eq!(row.user.name, "owner");
    assert!(verify_password(&row.password, "sesame-secret"));
    assert_eq!(folder_rows(dir.path()), rows);
    assert_eq!(
        fs::read_to_string(&config_path).expect("read config"),
        "# kept\n"
    );
    assert!(dir.path().join("instance.id").exists());
    let output = show();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("name: owner\n"));

    // The database has an owner now, and `init` refuses it.
    let output = run(init_command(dir.path()).env(PASSWORD_ENV, "second"));
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    assert!(verify_password(
        &owner_row(dir.path()).password,
        "sesame-secret"
    ));
    assert_eq!(folder_rows(dir.path()), rows);
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

// Unix only, for the same reason.
#[cfg(unix)]
#[test]
fn init_creates_a_0700_data_directory_and_a_0600_database() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(&mut init_command(&data_dir));
    assert!(output.status.success(), "{}", stderr(&output));
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&data_dir), 0o700);
    assert_eq!(mode(&data_dir.join("library.db")), 0o600);
}

#[test]
fn init_generates_a_password_prints_it_once_and_stores_it() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(&mut init_command(&data_dir));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let password = printed_password(&text, "owner");
    assert_generated_shape(&password);
    assert_eq!(
        text,
        format!(
            "{}Password for owner (shown once): {password}\n{NOTICE}\n",
            init_lines(&data_dir)
        )
    );
    assert_eq!(text.matches(&password).count(), 1);
    assert!(stderr(&output).is_empty());

    let row = owner_row(&data_dir);
    assert_eq!(row.user.name, "owner");
    assert_eq!(row.user.role, "owner");
    assert!(verify_password(&row.password, &password));
}

#[test]
fn init_stores_the_password_of_the_variable_and_does_not_print_it() {
    for password in ["x", "from the variable ☃"] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");

        let output = run(init_command(&data_dir).env(PASSWORD_ENV, password));
        assert!(output.status.success(), "{}", stderr(&output));
        // The output is the lines about the config and the notice, so no line
        // shows the password.
        assert_eq!(
            stdout(&output),
            format!("{}{NOTICE}\n", init_lines(&data_dir))
        );
        assert!(stderr(&output).is_empty());

        let row = owner_row(&data_dir);
        assert_eq!(row.password, password);
        assert!(verify_password(&row.password, password));
    }
}

#[test]
fn init_reads_the_password_from_the_first_line_of_stdin() {
    for (input, password) in [
        ("from stdin\n", "from stdin"),
        ("from stdin\r\n", "from stdin"),
        ("from stdin", "from stdin"),
        (" first \nsecond\n", " first "),
    ] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");

        let output = run_with_stdin(init_command(&data_dir).arg("--password-stdin"), input);
        assert!(output.status.success(), "{input:?}: {}", stderr(&output));
        assert_eq!(
            stdout(&output),
            format!("{}{NOTICE}\n", init_lines(&data_dir)),
            "{input:?}"
        );
        assert!(stderr(&output).is_empty(), "{input:?}");

        let row = owner_row(&data_dir);
        assert_eq!(row.password, password, "{input:?}");
        assert!(verify_password(&row.password, password), "{input:?}");
    }
}

#[test]
fn init_password_stdin_wins_over_the_variable() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run_with_stdin(
        init_command(&data_dir)
            .arg("--password-stdin")
            .env(PASSWORD_ENV, "from the variable"),
        "from stdin\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{}{NOTICE}\n", init_lines(&data_dir))
    );

    let row = owner_row(&data_dir);
    assert!(verify_password(&row.password, "from stdin"));
    assert!(!verify_password(&row.password, "from the variable"));
}

#[test]
fn init_without_a_password_on_stdin_fails_and_creates_nothing() {
    for input in ["", "\n", "\r\n"] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");

        // The variable does not replace the missing line.
        let output = run_with_stdin(
            init_command(&data_dir)
                .arg("--password-stdin")
                .env(PASSWORD_ENV, "from the variable"),
            input,
        );
        assert_eq!(output.status.code(), Some(1), "{input:?}");
        assert!(stdout(&output).is_empty(), "{input:?}");
        let message = stderr(&output);
        assert!(message.contains("--password-stdin"), "{message}");
        assert!(!message.contains("from the variable"), "{message}");
        assert!(!data_dir.exists(), "{input:?}");
    }
}

#[test]
fn init_with_an_empty_variable_generates_a_password() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(init_command(&data_dir).env(PASSWORD_ENV, ""));
    assert!(output.status.success(), "{}", stderr(&output));
    let password = printed_password(&stdout(&output), "owner");
    assert_generated_shape(&password);
    assert!(verify_password(&owner_row(&data_dir).password, &password));
}

// Unix only: a variable that is not valid Unicode is built from raw bytes,
// which only Unix environments allow.
#[cfg(unix)]
#[test]
fn init_rejects_a_variable_that_is_not_unicode_unless_stdin_supplies_the_password() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let not_unicode = OsStr::from_bytes(b"sesame\xff");
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(init_command(&data_dir).env(PASSWORD_ENV, not_unicode));
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(
        message.contains("WN_SERVER_PASSWORD is not valid Unicode"),
        "{message}"
    );
    assert!(!message.contains("sesame"), "{message}");
    assert!(!data_dir.exists());

    let output = run_with_stdin(
        init_command(&data_dir)
            .arg("--password-stdin")
            .env(PASSWORD_ENV, not_unicode),
        "from stdin\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(verify_password(
        &owner_row(&data_dir).password,
        "from stdin"
    ));
}

#[test]
fn init_names_the_owner_account_after_the_username_option() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");

    let output = run(init_command(&data_dir).args(["--username", "Adnan"]));
    assert!(output.status.success(), "{}", stderr(&output));
    let password = printed_password(&stdout(&output), "Adnan");

    let row = owner_row(&data_dir);
    assert_eq!(row.user.name, "Adnan");
    assert_eq!(row.user.role, "owner");
    assert!(verify_password(&row.password, &password));
}

#[test]
fn a_second_init_refuses_and_keeps_the_account() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    init_with_password(&data_dir, "first");
    let config_path = data_dir.join("config.toml");
    let config = fs::read_to_string(&config_path).expect("read config");
    let instance_id = fs::read_to_string(data_dir.join("instance.id")).expect("read instance.id");

    let output = run(init_command(&data_dir)
        .args(["--username", "other", "--port", "4321"])
        .env(PASSWORD_ENV, "second"));
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.contains("already exists"), "{message}");
    assert!(
        message.contains(&config_path.display().to_string()),
        "{message}"
    );
    assert!(!message.contains("second"), "{message}");

    let row = owner_row(&data_dir);
    assert_eq!(row.user.name, "owner");
    assert!(verify_password(&row.password, "first"));
    assert_eq!(
        fs::read_to_string(&config_path).expect("read config"),
        config
    );
    assert_eq!(
        fs::read_to_string(data_dir.join("instance.id")).expect("read instance.id"),
        instance_id
    );
}

#[test]
fn init_refuses_a_database_that_has_an_owner_and_removes_the_files_it_wrote() {
    for instance_id_exists in [true, false] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");
        init_with_password(&data_dir, "first");
        let config_path = data_dir.join("config.toml");
        let instance_id_path = data_dir.join("instance.id");
        let instance_id = fs::read_to_string(&instance_id_path).expect("read instance.id");
        fs::remove_file(&config_path).expect("remove config");
        if !instance_id_exists {
            fs::remove_file(&instance_id_path).expect("remove instance.id");
        }

        let output = run(init_command(&data_dir)
            .args(["--username", "other"])
            .env(PASSWORD_ENV, "second"));
        assert_eq!(output.status.code(), Some(1));
        assert!(stdout(&output).is_empty());
        let message = stderr(&output);
        assert!(
            message.contains("already has an owner account"),
            "{message}"
        );
        assert!(
            message.contains(&data_dir.join("library.db").display().to_string()),
            "{message}"
        );
        assert!(!message.contains("second"), "{message}");

        // The refused run wrote a config file, and an instance ID when the
        // directory had none. It removed what it wrote. An instance ID from
        // before the run is kept.
        assert!(!config_path.exists());
        if instance_id_exists {
            assert_eq!(
                fs::read_to_string(&instance_id_path).expect("read instance.id"),
                instance_id
            );
        } else {
            assert!(!instance_id_path.exists());
        }

        let row = owner_row(&data_dir);
        assert_eq!(row.user.name, "owner");
        assert!(verify_password(&row.password, "first"));
    }
}

#[test]
fn init_writes_a_version_4_uuid_to_instance_id() {
    let first = temp_dir();
    let second = temp_dir();
    let mut ids = Vec::new();
    for dir in [&first, &second] {
        let output = run(&mut init_command(dir.path()));
        assert!(output.status.success(), "{}", stderr(&output));
        let text = fs::read_to_string(dir.path().join("instance.id")).expect("read instance.id");
        let id = text.strip_suffix('\n').expect("a line").to_string();

        let groups: Vec<&str> = id.split('-').collect();
        let lengths: Vec<usize> = groups.iter().map(|group| group.len()).collect();
        assert_eq!(lengths, [8, 4, 4, 4, 12], "{id}");
        assert!(
            id.chars()
                .all(|character| matches!(character, '0'..='9' | 'a'..='f' | '-')),
            "{id}"
        );
        // The version digit is 4 and the variant digit is 8, 9, a or b.
        assert!(groups[2].starts_with('4'), "{id}");
        assert!(matches!(&groups[3][..1], "8" | "9" | "a" | "b"), "{id}");
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn init_keeps_an_existing_instance_id() {
    let dir = temp_dir();
    let instance_id_path = dir.path().join("instance.id");
    fs::write(&instance_id_path, "kept\n").expect("write instance.id");

    let output = run(&mut init_command(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(&instance_id_path).expect("read instance.id"),
        "kept\n"
    );
}

#[test]
fn user_show_prints_the_name_the_role_and_the_creation_date() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    let before = unix_now();
    let output = run(init_command(&data_dir)
        .args(["--username", "adnan"])
        .env(PASSWORD_ENV, "sesame-secret"));
    assert!(output.status.success(), "{}", stderr(&output));
    let after = unix_now();
    let created_at = owner_row(&data_dir).created_at;
    assert!((before..=after).contains(&created_at), "{created_at}");

    let show = || {
        let output = run(binary().args(["user", "show", "--data-dir"]).arg(&data_dir));
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stderr(&output).is_empty());
        stdout(&output)
    };
    let text = show();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(lines[0], "name: adnan");
    assert_eq!(lines[1], "role: owner");
    let date = lines[2].strip_prefix("created: ").expect("a created line");
    let digits: Vec<&str> = date.split('-').collect();
    let lengths: Vec<usize> = digits.iter().map(|part| part.len()).collect();
    assert_eq!(lengths, [4, 2, 2], "{date}");
    assert!(
        digits
            .iter()
            .all(|part| part.chars().all(|character| character.is_ascii_digit())),
        "{date}"
    );
    assert!(!text.contains("sesame-secret"), "{text}");

    // The date follows the stored creation time. 951782400 is
    // 2000-02-29T00:00:00Z.
    let db = Db::open(data_dir.join("library.db")).expect("open the database");
    db.with_writer(|conn| {
        conn.execute("UPDATE users SET created_at = 951782400", [])?;
        Ok(())
    })
    .expect("set the creation time");
    drop(db);
    assert_eq!(show(), "name: adnan\nrole: owner\ncreated: 2000-02-29\n");
}

#[test]
fn user_set_password_generates_a_password_prints_it_once_and_stores_it() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    init_with_password(&data_dir, "first");

    let output = run(&mut set_password_command(&data_dir));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let password = printed_password(&text, "owner");
    assert_generated_shape(&password);
    assert_eq!(
        text,
        format!("Password for owner (shown once): {password}\n{NOTICE}\n")
    );
    assert!(stderr(&output).is_empty());

    let row = owner_row(&data_dir);
    assert!(verify_password(&row.password, &password));
    assert!(!verify_password(&row.password, "first"));

    // Each run generates another password.
    let output = run(&mut set_password_command(&data_dir));
    assert!(output.status.success(), "{}", stderr(&output));
    let next = printed_password(&stdout(&output), "owner");
    assert_ne!(next, password);
    let row = owner_row(&data_dir);
    assert!(verify_password(&row.password, &next));
    assert!(!verify_password(&row.password, &password));
}

#[test]
fn user_set_password_stores_a_supplied_password_and_does_not_print_it() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    init_with_password(&data_dir, "first");

    let output = run(set_password_command(&data_dir).env(PASSWORD_ENV, "from the variable"));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{NOTICE}\n"));
    assert!(stderr(&output).is_empty());
    let row = owner_row(&data_dir);
    assert!(verify_password(&row.password, "from the variable"));
    assert!(!verify_password(&row.password, "first"));

    let output = run_with_stdin(
        set_password_command(&data_dir).arg("--password-stdin"),
        "from stdin\r\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{NOTICE}\n"));
    assert!(stderr(&output).is_empty());
    assert!(verify_password(
        &owner_row(&data_dir).password,
        "from stdin"
    ));

    // The option wins over the variable.
    let output = run_with_stdin(
        set_password_command(&data_dir)
            .arg("--password-stdin")
            .env(PASSWORD_ENV, "from the variable"),
        "from stdin again\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{NOTICE}\n"));
    assert!(verify_password(
        &owner_row(&data_dir).password,
        "from stdin again"
    ));
}

#[test]
fn user_set_password_finds_the_account_by_name_without_case() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    let output = run(init_command(&data_dir)
        .args(["--username", "Adnan"])
        .env(PASSWORD_ENV, "first"));
    assert!(output.status.success(), "{}", stderr(&output));

    let output = run(set_password_command(&data_dir).args(["--username", "adnan"]));
    assert!(output.status.success(), "{}", stderr(&output));
    // The line shows the name as it is stored.
    let password = printed_password(&stdout(&output), "Adnan");
    let row = owner_row(&data_dir);
    assert_eq!(row.user.name, "Adnan");
    assert!(verify_password(&row.password, &password));
}

#[test]
fn user_set_password_names_an_unknown_user_and_changes_nothing() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    init_with_password(&data_dir, "first");

    let output = run(set_password_command(&data_dir)
        .args(["--username", "nobody"])
        .env(PASSWORD_ENV, "second"));
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.contains("no user is named nobody"), "{message}");
    assert!(!message.contains("second"), "{message}");
    assert!(verify_password(&owner_row(&data_dir).password, "first"));
}

#[test]
fn user_set_password_without_a_password_on_stdin_changes_nothing() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    init_with_password(&data_dir, "first");

    let output = run_with_stdin(set_password_command(&data_dir).arg("--password-stdin"), "");
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(
        stderr(&output).contains("--password-stdin"),
        "{}",
        stderr(&output)
    );
    assert!(verify_password(&owner_row(&data_dir).password, "first"));
}

#[test]
fn user_commands_without_a_database_name_init_and_create_nothing() {
    for args in [&["user", "show"][..], &["user", "set-password"]] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");

        let output = run(binary().args(args).arg("--data-dir").arg(&data_dir));
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let message = stderr(&output);
        assert!(
            message.contains(&data_dir.join("library.db").display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(
                "does not exist; `wn-server init` creates it and the owner account \
                 and keeps an existing config.toml"
            ),
            "{message}"
        );
        assert!(!data_dir.exists(), "{args:?}");
    }
}

#[test]
fn init_with_a_config_and_no_database_creates_the_database_and_the_owner() {
    let dir = temp_dir();
    let music = temp_dir();
    let data_dir = dir.path().join("data");
    let config_path = data_dir.join("config.toml");
    let config = data_dir_without_a_database(&data_dir);

    // The error of the `user` commands names the command that follows.
    let show = || run(binary().args(["user", "show", "--data-dir"]).arg(&data_dir));
    let output = show();
    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("does not exist; `wn-server init`"),
        "{message}"
    );

    // `--folder` and `--port` do not change a config file that exists, so
    // `init` refuses them. It does so before it reads the password.
    for options in [
        &["--folder", music.path().to_str().expect("a Unicode path")][..],
        &["--port", "4000"],
    ] {
        let output = run_with_stdin(
            init_command(&data_dir)
                .args(options)
                .arg("--password-stdin"),
            "",
        );
        assert_eq!(output.status.code(), Some(1), "{options:?}");
        assert!(stdout(&output).is_empty(), "{options:?}");
        let message = stderr(&output);
        assert!(
            message.contains(&format!(
                "{} exists; --folder and --port do not change it",
                config_path.display()
            )),
            "{message}"
        );
        assert!(!data_dir.join("library.db").exists(), "{options:?}");
        assert_eq!(fs::read(&config_path).expect("read config"), config);
    }

    let output = run(init_command(&data_dir)
        .args(["--username", "Adnan"])
        .env(PASSWORD_ENV, "second"));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{}{NOTICE}\n", kept_config_lines(&data_dir, "created"))
    );
    assert!(stderr(&output).is_empty());
    assert_eq!(fs::read(&config_path).expect("read config"), config);

    let db = Db::open(data_dir.join("library.db")).expect("open the database");
    let users: i64 = db
        .with_reader(|conn| Ok(conn.query_row("SELECT count(*) FROM users", [], |row| row.get(0))?))
        .expect("count the users");
    drop(db);
    assert_eq!(users, 1);
    let row = owner_row(&data_dir);
    assert_eq!(row.user.name, "Adnan");
    assert_eq!(row.user.role, "owner");
    assert!(verify_password(&row.password, "second"));
    assert!(!verify_password(&row.password, "first"));

    let output = show();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).starts_with("name: Adnan\nrole: owner\n"));

    // The data directory has both files again, and `init` refuses it.
    let output = run(init_command(&data_dir).env(PASSWORD_ENV, "third"));
    assert_eq!(output.status.code(), Some(1));
    assert!(verify_password(&owner_row(&data_dir).password, "second"));
    assert_eq!(fs::read(&config_path).expect("read config"), config);
}

#[test]
fn init_generates_the_password_of_the_owner_it_creates_again_and_prints_it_once() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    data_dir_without_a_database(&data_dir);

    let output = run(&mut init_command(&data_dir));
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let password = printed_password(&text, "owner");
    assert_generated_shape(&password);
    assert_eq!(
        text,
        format!(
            "{}Password for owner (shown once): {password}\n{NOTICE}\n",
            kept_config_lines(&data_dir, "created")
        )
    );
    assert_eq!(text.matches(&password).count(), 1);
    assert!(stderr(&output).is_empty());
    assert!(verify_password(&owner_row(&data_dir).password, &password));
}

#[test]
fn init_reads_the_password_of_the_owner_it_creates_again_from_stdin() {
    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    let config_path = data_dir.join("config.toml");
    let config = data_dir_without_a_database(&data_dir);
    fs::remove_file(data_dir.join("instance.id")).expect("remove instance.id");

    // Nothing is created without a password. The variable does not replace
    // the missing line.
    let output = run_with_stdin(
        init_command(&data_dir)
            .arg("--password-stdin")
            .env(PASSWORD_ENV, "from the variable"),
        "\n",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.contains("--password-stdin"), "{message}");
    assert!(!message.contains("from the variable"), "{message}");
    assert!(!data_dir.join("library.db").exists());
    assert!(!data_dir.join("instance.id").exists());

    let output = run_with_stdin(
        init_command(&data_dir).arg("--password-stdin"),
        "from stdin\n",
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("{}{NOTICE}\n", kept_config_lines(&data_dir, "created"))
    );
    assert!(stderr(&output).is_empty());
    assert!(verify_password(
        &owner_row(&data_dir).password,
        "from stdin"
    ));
    assert_eq!(fs::read(&config_path).expect("read config"), config);
}

#[test]
fn init_keeps_an_instance_id_and_writes_a_missing_one_when_it_creates_the_database_again() {
    for instance_id_exists in [true, false] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");
        data_dir_without_a_database(&data_dir);
        let instance_id_path = data_dir.join("instance.id");
        let instance_id = fs::read_to_string(&instance_id_path).expect("read instance.id");
        if !instance_id_exists {
            fs::remove_file(&instance_id_path).expect("remove instance.id");
        }

        let output = run(init_command(&data_dir).env(PASSWORD_ENV, "second"));
        assert!(output.status.success(), "{}", stderr(&output));
        let written = fs::read_to_string(&instance_id_path).expect("read instance.id");
        if instance_id_exists {
            assert_eq!(written, instance_id);
        } else {
            // The file holds another UUID on one line.
            assert_ne!(written, instance_id);
            assert_eq!(written.len(), 37, "{written}");
            assert!(written.ends_with('\n'), "{written}");
        }
    }
}

// Unix only: Windows has no file modes. The user-profile ACL protects the
// directory there.
#[cfg(unix)]
#[test]
fn init_creates_the_database_again_with_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let data_dir = dir.path().join("data");
    data_dir_without_a_database(&data_dir);

    let output = run(init_command(&data_dir).env(PASSWORD_ENV, "second"));
    assert!(output.status.success(), "{}", stderr(&output));
    let mode = fs::metadata(data_dir.join("library.db"))
        .expect("metadata")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

// Unix only: the test makes the database fail to open with a directory at the
// path of the write-ahead log. SQLite removes a log that it finds next to an
// empty database, and the Unix call that removes a file fails on a directory.
#[cfg(unix)]
#[test]
fn init_that_keeps_the_config_removes_no_file_when_it_fails_and_runs_again() {
    for instance_id_exists in [true, false] {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");
        let config_path = data_dir.join("config.toml");
        let config = data_dir_without_a_database(&data_dir);
        let instance_id_path = data_dir.join("instance.id");
        if !instance_id_exists {
            fs::remove_file(&instance_id_path).expect("remove instance.id");
        }
        let log_path = data_dir.join("library.db-wal");
        if log_path.exists() {
            fs::remove_file(&log_path).expect("remove the log");
        }
        fs::create_dir(&log_path).expect("create a directory at the log path");

        let output = run(init_command(&data_dir).env(PASSWORD_ENV, "second"));
        assert_eq!(output.status.code(), Some(1));
        assert!(stdout(&output).is_empty());
        assert!(!stderr(&output).contains("second"), "{}", stderr(&output));
        assert_eq!(fs::read(&config_path).expect("read config"), config);
        // The failed run wrote a missing instance ID and left it.
        let instance_id = fs::read_to_string(&instance_id_path).expect("read instance.id");

        // The cause of the error is gone. `init` runs again and uses the
        // database file and the instance ID that the failed run left.
        fs::remove_dir(&log_path).expect("remove the directory");
        let output = run(init_command(&data_dir).env(PASSWORD_ENV, "second"));
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(
            stdout(&output),
            format!("{}{NOTICE}\n", kept_config_lines(&data_dir, "kept"))
        );
        assert!(verify_password(&owner_row(&data_dir).password, "second"));
        assert_eq!(
            fs::read_to_string(&instance_id_path).expect("read instance.id"),
            instance_id
        );
        assert_eq!(fs::read(&config_path).expect("read config"), config);
    }
}

#[test]
fn user_commands_on_a_database_without_an_owner_fail() {
    for args in [&["user", "show"][..], &["user", "set-password"]] {
        let dir = temp_dir();
        drop(Db::open(dir.path().join("library.db")).expect("create the database"));

        let output = run(binary().args(args).arg("--data-dir").arg(dir.path()));
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let message = stderr(&output);
        assert!(
            message.contains(
                "has no owner account; `wn-server init` creates the account \
                 and keeps the database and an existing config.toml"
            ),
            "{message}"
        );
    }
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
fn a_server_name_that_is_not_1_to_63_bytes_long_fails_the_commands_that_load_the_config() {
    let music = temp_dir();
    for (name, bytes) in [(String::new(), 0), ("n".repeat(64), 64)] {
        let dir = temp_dir();
        fs::write(
            dir.path().join("config.toml"),
            format!(
                "[server]\nname = \"{name}\"\n\n[library]\nfolders = ['{}']\n",
                music.path().display()
            ),
        )
        .expect("write config");

        // `run` fails when it loads the config, before it opens the database
        // or a socket.
        for command in [&["config", "show"][..], &["run"]] {
            let output = run(binary().args(command).arg("--data-dir").arg(dir.path()));
            assert_eq!(output.status.code(), Some(1), "{command:?}");
            assert!(stdout(&output).is_empty(), "{command:?}");
            assert_eq!(
                stderr(&output),
                format!(
                    "wn-server: server.name: the name is {bytes} bytes long; set `name` in the \
                     `[server]` table of config.toml to a name of 1 to 63 bytes (`wn-server \
                     config path` prints the path of the file)\n"
                ),
                "{command:?}"
            );
        }
    }
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
fn scan_prints_not_implemented_and_exits_2() {
    let output = run(binary().arg("scan"));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stderr(&output), "not implemented yet\n");
    assert!(stdout(&output).is_empty());
}

#[test]
fn run_without_a_config_names_the_config_path_and_exits_1() {
    let dir = temp_dir();

    let output = run(binary().arg("run").arg("--data-dir").arg(dir.path()));
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    assert!(message.starts_with("wn-server: "), "{message}");
    assert!(
        message.contains(&dir.path().join("config.toml").display().to_string()),
        "{message}"
    );
    assert!(message.contains("wn-server init"), "{message}");
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
