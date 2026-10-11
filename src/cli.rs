//! The command line: the clap tree and the commands that need only the
//! config, the data directory and the database.
//!
//! `init` and `user set-password` handle the owner password. It is written to
//! the database and, when the command generated it, to stdout once. It is not
//! logged and is not part of an error or of a `Debug` output.

use std::env;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand};
use thiserror::Error;

use crate::auth;
use crate::config::{self, Config, ConfigError, DataDir};
use crate::db::queries::users::{self, User, UserRow};
use crate::db::{Db, DbError};

/// The environment variable that supplies the password to `init` and
/// `user set-password`.
pub const PASSWORD_ENV: &str = "WN_SERVER_PASSWORD";

/// The name of the owner account when `init` gets no `--username`.
pub const DEFAULT_USERNAME: &str = "owner";

/// The line that `init` and `user set-password` print after they store a
/// password.
const PLAIN_TEXT_NOTICE: &str =
    "The password is stored in plain text in the data directory. Do not reuse it elsewhere.";

/// The end of the error of a `user` command that finds no database or no
/// owner account. `init` creates both and refuses a data directory that has a
/// config file, so the text names the step that makes `init` run.
const INIT_RECOVERY: &str = "`wn-server init` creates it. init does not overwrite config.toml, \
     so move that file away first when it exists";

/// A headless music server for OpenSubsonic clients.
#[derive(Debug, Parser)]
#[command(name = "wn-server", version = crate::VERSION)]
pub struct Cli {
    /// The data directory. Overrides WN_SERVER_DATA_DIR and the platform default
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands of `wn-server`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Creates the data directory, config.toml, the database and the owner account
    Init(InitArgs),
    /// Runs the server in the foreground
    Run,
    /// Runs one scan and exits
    Scan,
    /// Shows or changes the owner account
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Shows the configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// The options of `wn-server init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// A music folder. Repeat the option for each folder
    #[arg(long = "folder", value_name = "PATH")]
    pub folders: Vec<PathBuf>,

    /// The TCP port the server listens on
    #[arg(
        long,
        value_name = "N",
        default_value_t = config::DEFAULT_PORT,
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub port: u16,

    /// The name of the owner account
    #[arg(
        long,
        value_name = "NAME",
        default_value = DEFAULT_USERNAME,
        value_parser = NonEmptyStringValueParser::new()
    )]
    pub username: String,

    /// Reads the password from one line on stdin. Without this option the
    /// password is the value of WN_SERVER_PASSWORD, or is generated and
    /// printed once when that variable is not set
    #[arg(long)]
    pub password_stdin: bool,
}

/// The subcommands of `wn-server user`.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Prints the owner's name, role and creation date
    Show,
    /// Sets the owner's password
    SetPassword(SetPasswordArgs),
}

/// The options of `wn-server user set-password`.
#[derive(Debug, Args)]
pub struct SetPasswordArgs {
    /// The name of the account. Without this option the account is the owner
    #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
    pub username: Option<String>,

    /// Reads the password from one line on stdin. Without this option the
    /// password is the value of WN_SERVER_PASSWORD, or is generated and
    /// printed once when that variable is not set
    #[arg(long)]
    pub password_stdin: bool,
}

/// The subcommands of `wn-server config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Prints the resolved config path
    Path,
    /// Prints the effective config
    Show,
}

/// A command that did not complete. No variant holds a password.
#[derive(Debug, Error)]
enum CliError {
    #[error("not implemented yet")]
    NotImplemented,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot create the data directory {}: {source}", path.display())]
    CreateDataDir { path: PathBuf, source: io::Error },
    #[error("{} already exists; init does not overwrite it", .0.display())]
    ConfigExists(PathBuf),
    #[error("cannot write {}: {source}", path.display())]
    WriteFile { path: PathBuf, source: io::Error },
    #[error("cannot write to stdout: {0}")]
    Stdout(#[from] io::Error),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the database {} does not exist; {INIT_RECOVERY}", .0.display())]
    NoDatabase(PathBuf),
    #[error(
        "the database {} already has an owner account; init does not replace it. \
         `wn-server user set-password` changes the password",
        .0.display()
    )]
    OwnerExists(PathBuf),
    #[error("the database {} has no owner account; {INIT_RECOVERY}", .0.display())]
    NoOwner(PathBuf),
    #[error("no user is named {0}")]
    NoUser(String),
    #[error("--password-stdin: stdin has no password on its first line")]
    EmptyPassword,
    #[error("cannot read the password from stdin: {0}")]
    ReadPassword(io::Error),
    #[error("{PASSWORD_ENV} is not valid Unicode")]
    PasswordEnvNotUnicode,
    #[error("cannot start the runtime that closes the database: {0}")]
    Runtime(io::Error),
}

/// Where `init` and `user set-password` take the password from.
///
/// The `Debug` output leaves the password out.
#[derive(Clone, PartialEq, Eq)]
pub enum PasswordSource {
    /// One line on stdin.
    Stdin,
    /// The value of `WN_SERVER_PASSWORD`.
    Env(String),
    /// A password that the command generates.
    Generate,
}

impl fmt::Debug for PasswordSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PasswordSource::Stdin => "Stdin",
            PasswordSource::Env(_) => "Env(..)",
            PasswordSource::Generate => "Generate",
        })
    }
}

/// Picks the source of the password. `flag_stdin` is the `--password-stdin`
/// option and `env` is the value of `WN_SERVER_PASSWORD`, or `None` when the
/// variable is not set. The option wins over the variable. An empty variable
/// counts as unset. Without both the password is generated.
pub fn password_source(flag_stdin: bool, env: Option<String>) -> PasswordSource {
    match (flag_stdin, env) {
        (true, _) => PasswordSource::Stdin,
        (false, Some(password)) if !password.is_empty() => PasswordSource::Env(password),
        (false, _) => PasswordSource::Generate,
    }
}

/// A password to store, and whether the command generated it. It has no
/// `Debug` implementation, so it cannot be formatted into a log or an error.
struct NewPassword {
    text: String,
    generated: bool,
}

/// Runs the command and returns the exit code of the process. A command that
/// fails prints its error to stderr and exits with 1. `run` and `scan` print
/// `not implemented yet` to stderr and exit with 2.
pub fn run(cli: &Cli) -> ExitCode {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    match execute(cli, &mut stdin, &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error @ CliError::NotImplemented) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("wn-server: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the command and writes its output to `out`. `input` is read only by
/// a command that got `--password-stdin`.
fn execute(cli: &Cli, input: &mut dyn BufRead, out: &mut dyn Write) -> Result<(), CliError> {
    match &cli.command {
        Command::Run | Command::Scan => Err(CliError::NotImplemented),
        Command::Init(args) => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            init(&data_dir, args, input, out)
        }
        Command::User { command } => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            match command {
                UserCommand::Show => show_user(&data_dir, out),
                UserCommand::SetPassword(args) => set_password(&data_dir, args, input, out),
            }
        }
        Command::Config { command } => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            match command {
                ConfigCommand::Path => {
                    writeln!(out, "{}", data_dir.config_path().display())?;
                }
                ConfigCommand::Show => {
                    let config = Config::load(&data_dir.config_path())?;
                    out.write_all(config.to_toml()?.as_bytes())?;
                }
            }
            Ok(())
        }
    }
}

/// Creates the data directory, `config.toml`, `instance.id`, the database and
/// the owner account.
///
/// `config.toml` is the template with the folders and the port of `args`.
/// Each folder must be an existing directory and is written as an absolute
/// path. The password comes from the source that [`password_source`] picks. A
/// generated password is printed once.
///
/// Nothing is created when a folder fails its check, when `config.toml`
/// exists or when no password can be read. An existing `config.toml` and an
/// existing owner account are errors and are left as they are. An existing
/// `instance.id` is kept. When a step after the config write fails, the
/// `config.toml` and the `instance.id` that this run wrote are removed.
fn init(
    data_dir: &DataDir,
    args: &InitArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let mut folders = Vec::with_capacity(args.folders.len());
    for folder in &args.folders {
        let folder = config::absolute(folder)?;
        config::check_folder(&folder)?;
        folders.push(folder);
    }
    let text = Config::template(&folders, args.port)?;

    let config_path = data_dir.config_path();
    // This check runs before the password is read, so a second `init` does
    // not wait for a line on stdin. The write below refuses an existing file
    // again.
    if config_path.exists() {
        return Err(CliError::ConfigExists(config_path));
    }
    let password = new_password(args.password_stdin, input)?;

    create_data_dir(data_dir.path()).map_err(|source| CliError::CreateDataDir {
        path: data_dir.path().to_path_buf(),
        source,
    })?;
    if !write_new_file(&config_path, &text)? {
        return Err(CliError::ConfigExists(config_path));
    }
    // The files this run wrote. An `instance.id` from before the run is not
    // in the list.
    let mut written = vec![config_path.clone()];
    let (db, owner) = match create_owner(data_dir, &args.username, &password.text, &mut written) {
        Ok(created) => created,
        Err(error) => {
            // `init` refuses a data directory that has a config file. The
            // files this run wrote are removed, so that `init` can run again
            // when the cause of the error is gone.
            for path in &written {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
    };
    // The account is committed. An error from here on leaves the config file
    // in place, and `user set-password` sets a password that was not printed.
    close_db(db)?;

    writeln!(out, "wrote {}", config_path.display())?;
    if folders.is_empty() {
        writeln!(
            out,
            "no music folder is configured; add one to library.folders in that file"
        )?;
    }
    print_password(out, &owner.name, &password)?;
    Ok(())
}

/// Writes `instance.id` when it is missing, creates the database when it is
/// missing and inserts the owner account. A database that already has an
/// owner account is an error. The path of an `instance.id` that this call
/// wrote is added to `written`.
fn create_owner(
    data_dir: &DataDir,
    name: &str,
    password: &str,
    written: &mut Vec<PathBuf>,
) -> Result<(Db, User), CliError> {
    let instance_id_path = data_dir.instance_id_path();
    if write_new_file(&instance_id_path, &format!("{}\n", new_instance_id()))? {
        written.push(instance_id_path);
    }

    let db_path = data_dir.db_path();
    let db = Db::open(&db_path)?;
    let owner = db
        .with_writer(|conn| users::insert_owner(conn, name, password))?
        .ok_or(CliError::OwnerExists(db_path))?;
    Ok((db, owner))
}

/// Prints the name, the role and the creation date of the owner account. The
/// date is in UTC.
fn show_user(data_dir: &DataDir, out: &mut dyn Write) -> Result<(), CliError> {
    let db = open_existing_db(data_dir)?;
    let row = find_user(&db, None, data_dir)?;
    writeln!(out, "name: {}", row.user.name)?;
    writeln!(out, "role: {}", row.user.role)?;
    writeln!(out, "created: {}", utc_date(row.created_at))?;
    Ok(())
}

/// Stores a new password for the account that `args` names, or for the owner
/// when it names none. The password comes from the source that
/// [`password_source`] picks. A generated password is printed once.
fn set_password(
    data_dir: &DataDir,
    args: &SetPasswordArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let db = open_existing_db(data_dir)?;
    // The account is looked up before the password is read, so a wrong name
    // does not wait for a line on stdin.
    let user = find_user(&db, args.username.as_deref(), data_dir)?.user;
    let password = new_password(args.password_stdin, input)?;

    let changed = db.with_writer(|conn| users::set_password(conn, user.id, &password.text))?;
    if !changed {
        return Err(CliError::NoUser(user.name));
    }
    close_db(db)?;
    print_password(out, &user.name, &password)?;
    Ok(())
}

/// Opens the database of a data directory that `init` has set up. `Db::open`
/// creates a missing file, and the `user` commands must not create a
/// database, so a missing file is an error here.
fn open_existing_db(data_dir: &DataDir) -> Result<Db, CliError> {
    let db_path = data_dir.db_path();
    if !db_path.exists() {
        return Err(CliError::NoDatabase(db_path));
    }
    Ok(Db::open(&db_path)?)
}

/// Returns the row of the user named `name`, or of the owner when `name` is
/// `None`.
fn find_user(db: &Db, name: Option<&str>, data_dir: &DataDir) -> Result<UserRow, CliError> {
    match name {
        Some(name) => db
            .with_reader(|conn| users::get_by_name(conn, name))?
            .ok_or_else(|| CliError::NoUser(name.to_string())),
        None => db
            .with_reader(users::get_owner)?
            .ok_or_else(|| CliError::NoOwner(data_dir.db_path())),
    }
}

/// Moves the write-ahead log into the database file and closes the database.
///
/// SQLite does not sync the log when a transaction commits. The checkpoint
/// syncs the database file, so an account or a password that a command has
/// just stored is on disk before the command prints the password.
fn close_db(db: Db) -> Result<(), CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(CliError::Runtime)?;
    match runtime.block_on(db.close()) {
        // Another process, such as the running server, uses the database. The
        // change is committed, and that process checkpoints the log.
        Ok(()) | Err(DbError::CheckpointBusy) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Returns the password to store, from the source that [`password_source`]
/// picks.
fn new_password(flag_stdin: bool, input: &mut dyn BufRead) -> Result<NewPassword, CliError> {
    // The variable is not read when the option is given, so a value that is
    // not valid Unicode does not fail a command that ignores it.
    let variable = if flag_stdin {
        None
    } else {
        password_from_env()?
    };
    let (text, generated) = match password_source(flag_stdin, variable) {
        PasswordSource::Stdin => (read_password_line(input)?, false),
        PasswordSource::Env(password) => (password, false),
        PasswordSource::Generate => (auth::generate_password(), true),
    };
    Ok(NewPassword { text, generated })
}

/// Returns the value of `WN_SERVER_PASSWORD`, or `None` when the variable is
/// not set.
fn password_from_env() -> Result<Option<String>, CliError> {
    match env::var(PASSWORD_ENV) {
        Ok(password) => Ok(Some(password)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(CliError::PasswordEnvNotUnicode),
    }
}

/// Reads the password from the first line of `input`. The line ending, `\n`
/// or `\r\n`, is not part of the password. Every other character is, spaces
/// included. An empty line and an empty input are errors.
fn read_password_line(input: &mut dyn BufRead) -> Result<String, CliError> {
    let mut line = String::new();
    input.read_line(&mut line).map_err(CliError::ReadPassword)?;
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    if line.is_empty() {
        return Err(CliError::EmptyPassword);
    }
    Ok(line)
}

/// Prints the password when the command generated it, then the notice that
/// the password is stored in plain text. A password that the owner supplied
/// is not printed.
fn print_password(out: &mut dyn Write, name: &str, password: &NewPassword) -> io::Result<()> {
    if password.generated {
        writeln!(out, "Password for {name} (shown once): {}", password.text)?;
    }
    writeln!(out, "{PLAIN_TEXT_NOTICE}")
}

/// Returns a new instance ID: a version-4 UUID from 16 random bytes.
fn new_instance_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    format_uuid_v4(bytes)
}

/// Formats 16 bytes as a UUID in lower-case hex groups of 8, 4, 4, 4 and 12
/// characters. The version bits are set to 4 and the variant bits to the
/// RFC 9562 variant, so 122 bits of the input remain.
fn format_uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Returns the UTC calendar date of a Unix time as `YYYY-MM-DD`.
///
/// The computation counts days from 1 March of year 0 in the proleptic
/// Gregorian calendar. A year that starts in March has its leap day at the
/// end, so the month and the day follow from the day of the year without a
/// table of month lengths.
fn utc_date(unix_seconds: i64) -> String {
    /// Days from 0000-03-01 to 1970-01-01.
    const EPOCH_DAY: i64 = 719_468;
    /// Days in the 400-year cycle of the calendar.
    const CYCLE_DAYS: i64 = 146_097;

    let days = unix_seconds.div_euclid(86_400) + EPOCH_DAY;
    let cycle = days.div_euclid(CYCLE_DAYS);
    let day_of_cycle = days.rem_euclid(CYCLE_DAYS);
    let year_of_cycle = (day_of_cycle - day_of_cycle / 1_460 + day_of_cycle / 36_524
        - day_of_cycle / 146_096)
        / 365;
    let day_of_year =
        day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    // 0 is March and 11 is February.
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_cycle + cycle * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Creates `path` and its missing parents. A directory that already exists is
/// left as it is.
fn create_data_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    // Unix: the data directory holds the database with the owner password in
    // plain text, so the directories created here are mode 0700. On Windows
    // the ACL of the user profile protects the directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Writes `text` to a new file at `path` and returns `true`. Returns `false`
/// and changes nothing when a file exists at `path`.
fn write_new_file(path: &Path, text: &str) -> Result<bool, CliError> {
    let write_error = |source| CliError::WriteFile {
        path: path.to_path_buf(),
        source,
    };
    let created = OpenOptions::new().write(true).create_new(true).open(path);
    let mut file = match created {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => return Err(write_error(error)),
    };
    if let Err(error) = file.write_all(text.as_bytes()) {
        // The partial file is removed so that a second `init` does not find
        // it. Windows does not delete an open file, so the handle closes
        // first.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(write_error(error));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("wn-server").chain(args.iter().copied()))
    }

    #[test]
    fn command_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_prints_name_and_version() {
        let version = Cli::command().render_version();
        assert_eq!(version, format!("wn-server {}\n", crate::VERSION));
    }

    #[test]
    fn every_subcommand_parses() {
        for args in [
            &["init"][..],
            &["run"],
            &["scan"],
            &["user", "show"],
            &["user", "set-password"],
            &["config", "path"],
            &["config", "show"],
        ] {
            parse(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
        }
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["user"]).is_err());
        assert!(parse(&["config"]).is_err());
    }

    #[test]
    fn init_takes_folders_and_a_port() {
        let cli = parse(&["init", "--folder", "a", "--folder", "b", "--port", "4321"])
            .expect("init arguments");
        let Command::Init(args) = cli.command else {
            panic!("expected init");
        };
        assert_eq!(args.folders, [PathBuf::from("a"), PathBuf::from("b")]);
        assert_eq!(args.port, 4321);
    }

    #[test]
    fn init_defaults_to_no_folder_port_4200_and_the_name_owner() {
        let cli = parse(&["init"]).expect("init");
        let Command::Init(args) = cli.command else {
            panic!("expected init");
        };
        assert!(args.folders.is_empty());
        assert_eq!(args.port, 4200);
        assert_eq!(args.username, "owner");
        assert!(!args.password_stdin);
    }

    #[test]
    fn init_takes_a_username_and_password_stdin() {
        let cli = parse(&["init", "--username", "adnan", "--password-stdin"]).expect("init");
        let Command::Init(args) = cli.command else {
            panic!("expected init");
        };
        assert_eq!(args.username, "adnan");
        assert!(args.password_stdin);
    }

    #[test]
    fn init_rejects_port_zero() {
        let error = parse(&["init", "--port", "0"]).expect_err("port 0");
        assert!(error.to_string().contains("--port"), "{error}");
    }

    #[test]
    fn set_password_takes_a_username_and_password_stdin() {
        let cli = parse(&["user", "set-password"]).expect("set-password");
        let Command::User {
            command: UserCommand::SetPassword(args),
        } = cli.command
        else {
            panic!("expected set-password");
        };
        assert_eq!(args.username, None);
        assert!(!args.password_stdin);

        let cli = parse(&[
            "user",
            "set-password",
            "--username",
            "adnan",
            "--password-stdin",
        ])
        .expect("set-password");
        let Command::User {
            command: UserCommand::SetPassword(args),
        } = cli.command
        else {
            panic!("expected set-password");
        };
        assert_eq!(args.username.as_deref(), Some("adnan"));
        assert!(args.password_stdin);
    }

    #[test]
    fn an_empty_username_is_a_usage_error() {
        for args in [
            &["init", "--username", ""][..],
            &["user", "set-password", "--username", ""],
        ] {
            let error = parse(args).expect_err("empty username");
            assert!(error.to_string().contains("--username"), "{error}");
        }
    }

    #[test]
    fn no_command_takes_a_password_as_an_argument() {
        for args in [
            &["init", "--password", "sesame"][..],
            &["user", "set-password", "--password", "sesame"],
            &["user", "set-password", "sesame"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn data_dir_is_accepted_before_and_after_the_subcommand() {
        for args in [
            &["--data-dir", "d", "config", "path"][..],
            &["config", "--data-dir", "d", "path"],
            &["config", "path", "--data-dir", "d"],
            &["init", "--data-dir", "d", "--folder", "a"],
            &["user", "set-password", "--data-dir", "d"],
        ] {
            let cli = parse(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!(cli.data_dir, Some(PathBuf::from("d")), "{args:?}");
        }
    }

    #[test]
    fn commands_of_later_issues_are_not_implemented() {
        for args in [&["run"][..], &["scan"]] {
            let cli = parse(args).expect("arguments");
            let mut out = Vec::new();
            let error = execute(&cli, &mut io::empty(), &mut out).expect_err("not implemented");
            assert!(matches!(error, CliError::NotImplemented), "{args:?}");
            assert_eq!(error.to_string(), "not implemented yet");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn password_stdin_wins_over_the_variable() {
        assert_eq!(password_source(true, None), PasswordSource::Stdin);
        assert_eq!(
            password_source(true, Some("from-variable".to_string())),
            PasswordSource::Stdin
        );
    }

    #[test]
    fn the_variable_wins_over_generation() {
        assert_eq!(
            password_source(false, Some("from-variable".to_string())),
            PasswordSource::Env("from-variable".to_string())
        );
        // Spaces are a password.
        assert_eq!(
            password_source(false, Some(" ".to_string())),
            PasswordSource::Env(" ".to_string())
        );
    }

    #[test]
    fn a_password_is_generated_without_the_option_and_the_variable() {
        assert_eq!(password_source(false, None), PasswordSource::Generate);
    }

    #[test]
    fn an_empty_variable_counts_as_unset() {
        assert_eq!(
            password_source(false, Some(String::new())),
            PasswordSource::Generate
        );
    }

    #[test]
    fn debug_output_of_a_password_source_leaves_the_password_out() {
        let source = PasswordSource::Env("sesame-secret".to_string());
        assert_eq!(format!("{source:?}"), "Env(..)");
        assert_eq!(format!("{:?}", PasswordSource::Stdin), "Stdin");
        assert_eq!(format!("{:?}", PasswordSource::Generate), "Generate");
    }

    fn read_line(input: &str) -> Result<String, CliError> {
        read_password_line(&mut input.as_bytes())
    }

    #[test]
    fn password_line_ends_at_the_line_ending() {
        assert_eq!(read_line("sesame\n").unwrap(), "sesame");
        assert_eq!(read_line("sesame\r\n").unwrap(), "sesame");
        assert_eq!(read_line("sesame").unwrap(), "sesame");
    }

    #[test]
    fn password_line_keeps_every_other_character() {
        assert_eq!(read_line("  open sesame \n").unwrap(), "  open sesame ");
        assert_eq!(read_line("sesame\r\r\n").unwrap(), "sesame\r");
        assert_eq!(read_line("pässword ☃\n").unwrap(), "pässword ☃");
    }

    #[test]
    fn password_line_is_the_first_line_only() {
        let mut input = "first\nsecond\n".as_bytes();
        assert_eq!(read_password_line(&mut input).unwrap(), "first");
        // The rest of the input is not read.
        assert_eq!(input, b"second\n");
    }

    #[test]
    fn an_empty_password_line_is_an_error() {
        for input in ["", "\n", "\r\n", "\nsecond\n"] {
            let error = read_line(input).expect_err("empty");
            assert!(matches!(error, CliError::EmptyPassword), "{input:?}");
        }
    }

    #[test]
    fn a_password_line_that_is_not_unicode_is_an_error_without_the_bytes() {
        let mut input: &[u8] = b"sesame\xff\n";
        let error = read_password_line(&mut input).expect_err("not Unicode");
        assert!(matches!(error, CliError::ReadPassword(_)));
        let message = error.to_string();
        assert!(message.contains("stdin"), "{message}");
        assert!(!message.contains("sesame"), "{message}");
        assert!(!format!("{error:?}").contains("sesame"), "{error:?}");
    }

    #[test]
    fn a_supplied_password_is_not_printed() {
        let password = NewPassword {
            text: "sesame-secret".to_string(),
            generated: false,
        };
        let mut out = Vec::new();
        print_password(&mut out, "owner", &password).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "The password is stored in plain text in the data directory. \
             Do not reuse it elsewhere.\n"
        );
    }

    #[test]
    fn a_generated_password_is_printed_on_one_line() {
        let password = NewPassword {
            text: "abcde-fghjk-mnpqr-23456".to_string(),
            generated: true,
        };
        let mut out = Vec::new();
        print_password(&mut out, "adnan", &password).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Password for adnan (shown once): abcde-fghjk-mnpqr-23456\n\
             The password is stored in plain text in the data directory. \
             Do not reuse it elsewhere.\n"
        );
    }

    #[test]
    fn uuid_format_sets_the_version_and_the_variant() {
        assert_eq!(
            format_uuid_v4([0x00; 16]),
            "00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(
            format_uuid_v4([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        assert_eq!(
            format_uuid_v4([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef,
            ]),
            "01234567-89ab-4def-8123-456789abcdef"
        );
    }

    #[test]
    fn instance_ids_are_distinct_version_4_uuids() {
        let first = new_instance_id();
        let second = new_instance_id();
        assert_ne!(first, second);
        for id in [first, second] {
            let groups: Vec<&str> = id.split('-').collect();
            let lengths: Vec<usize> = groups.iter().map(|group| group.len()).collect();
            assert_eq!(lengths, [8, 4, 4, 4, 12], "{id}");
            assert!(
                id.chars()
                    .all(|character| matches!(character, '0'..='9' | 'a'..='f' | '-')),
                "{id}"
            );
            assert!(groups[2].starts_with('4'), "{id}");
            assert!(matches!(&groups[3][..1], "8" | "9" | "a" | "b"), "{id}");
        }
    }

    #[test]
    fn utc_date_formats_unix_seconds() {
        for (unix_seconds, date) in [
            (0, "1970-01-01"),
            (86_399, "1970-01-01"),
            (86_400, "1970-01-02"),
            (-1, "1969-12-31"),
            (-86_400, "1969-12-31"),
            (-86_401, "1969-12-30"),
            (-2_208_988_800, "1900-01-01"),
            (951_782_400, "2000-02-29"),
            (951_868_800, "2000-03-01"),
            (1_709_164_800, "2024-02-29"),
            (1_735_689_599, "2024-12-31"),
            (1_735_689_600, "2025-01-01"),
            (1_791_676_800, "2026-10-11"),
            (4_102_444_800, "2100-01-01"),
            // 2100 is not a leap year.
            (4_107_542_400, "2100-03-01"),
            (253_402_300_799, "9999-12-31"),
        ] {
            assert_eq!(utc_date(unix_seconds), date, "{unix_seconds}");
        }
    }
}
