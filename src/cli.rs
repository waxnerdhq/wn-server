//! The command line: the clap tree and the commands that need only the config
//! and the data directory.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use thiserror::Error;

use crate::config::{self, Config, ConfigError, DataDir};

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
    /// Creates the data directory and writes config.toml
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
}

/// The subcommands of `wn-server user`.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Prints the owner's name and creation date
    Show,
    /// Sets the owner's password
    SetPassword,
}

/// The subcommands of `wn-server config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Prints the resolved config path
    Path,
    /// Prints the effective config
    Show,
}

/// A command that did not complete.
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
    WriteConfig { path: PathBuf, source: io::Error },
    #[error("cannot write to stdout: {0}")]
    Stdout(#[from] io::Error),
}

/// Runs the command and returns the exit code of the process. A command that
/// fails prints its error to stderr and exits with 1. `run`, `scan` and `user`
/// print `not implemented yet` to stderr and exit with 2.
pub fn run(cli: &Cli) -> ExitCode {
    let mut stdout = io::stdout().lock();
    match execute(cli, &mut stdout) {
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

/// Runs the command and writes its output to `out`.
fn execute(cli: &Cli, out: &mut dyn Write) -> Result<(), CliError> {
    match &cli.command {
        Command::Run | Command::Scan | Command::User { .. } => Err(CliError::NotImplemented),
        Command::Init(args) => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            init(&data_dir, args, out)
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

/// Creates the data directory and writes `config.toml` from the template with
/// the folders and the port of `args`. Each folder must be an existing
/// directory and is written as an absolute path. Nothing is created when a
/// folder fails that check. An existing `config.toml` is an error and is left
/// as it is.
fn init(data_dir: &DataDir, args: &InitArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let mut folders = Vec::with_capacity(args.folders.len());
    for folder in &args.folders {
        let folder = config::absolute(folder)?;
        config::check_folder(&folder)?;
        folders.push(folder);
    }
    let text = Config::template(&folders, args.port)?;

    create_data_dir(data_dir.path()).map_err(|source| CliError::CreateDataDir {
        path: data_dir.path().to_path_buf(),
        source,
    })?;
    let config_path = data_dir.config_path();
    write_new_file(&config_path, &text)?;

    writeln!(out, "wrote {}", config_path.display())?;
    if folders.is_empty() {
        writeln!(
            out,
            "no music folder is configured; add one to library.folders in that file"
        )?;
    }
    Ok(())
}

/// Creates `path` and its missing parents. A directory that already exists is
/// left as it is.
fn create_data_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    // Unix: the data directory will hold the database with the owner password
    // in plain text, so the directories created here are mode 0700. On Windows
    // the ACL of the user profile protects the directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Writes `text` to a new file at `path`. A file that already exists is an
/// error and is not changed.
fn write_new_file(path: &Path, text: &str) -> Result<(), CliError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                CliError::ConfigExists(path.to_path_buf())
            } else {
                CliError::WriteConfig {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    if let Err(source) = file.write_all(text.as_bytes()) {
        // The partial file is removed so that a second `init` is not refused.
        // Windows does not delete an open file, so the handle closes first.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(CliError::WriteConfig {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
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
    fn init_defaults_to_no_folder_and_port_4200() {
        let cli = parse(&["init"]).expect("init");
        let Command::Init(args) = cli.command else {
            panic!("expected init");
        };
        assert!(args.folders.is_empty());
        assert_eq!(args.port, 4200);
    }

    #[test]
    fn init_rejects_port_zero() {
        let error = parse(&["init", "--port", "0"]).expect_err("port 0");
        assert!(error.to_string().contains("--port"), "{error}");
    }

    #[test]
    fn data_dir_is_accepted_before_and_after_the_subcommand() {
        for args in [
            &["--data-dir", "d", "config", "path"][..],
            &["config", "--data-dir", "d", "path"],
            &["config", "path", "--data-dir", "d"],
            &["init", "--data-dir", "d", "--folder", "a"],
        ] {
            let cli = parse(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!(cli.data_dir, Some(PathBuf::from("d")), "{args:?}");
        }
    }

    #[test]
    fn commands_of_later_issues_are_not_implemented() {
        for args in [
            &["run"][..],
            &["scan"],
            &["user", "show"],
            &["user", "set-password"],
        ] {
            let cli = parse(args).expect("arguments");
            let mut out = Vec::new();
            let error = execute(&cli, &mut out).expect_err("not implemented");
            assert!(matches!(error, CliError::NotImplemented), "{args:?}");
            assert_eq!(error.to_string(), "not implemented yet");
            assert!(out.is_empty());
        }
    }
}
