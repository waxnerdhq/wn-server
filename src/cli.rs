//! The command line: the clap tree and the commands.
//!
//! `init` and `user set-password` handle the owner password. It is written to
//! the database and, when the command generated it, to stdout once. It is not
//! logged and is not part of an error or of a `Debug` output.

use std::env;
use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::future::Future;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use clap::builder::NonEmptyStringValueParser;
use clap::{Args, Parser, Subcommand};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use crate::auth;
use crate::config::{self, Config, ConfigError, DataDir};
use crate::db::queries::users::{self, User, UserRow};
use crate::db::{Db, DbError};
use crate::discovery;
use crate::library::scan_controller::{ScanLock, ScanLockError};
use crate::library::scanner::{self, ScanOptions, ScanSummary};
use crate::server::{self, FfmpegProbe, ServerError, ServerOptions, StopStep};

/// The environment variable that supplies the password to `init` and
/// `user set-password`.
pub const PASSWORD_ENV: &str = "WN_SERVER_PASSWORD";

/// The name of the owner account when `init` gets no `--username`.
pub const DEFAULT_USERNAME: &str = "owner";

/// The line that `init` and `user set-password` print after they store a
/// password.
const PLAIN_TEXT_NOTICE: &str =
    "The password is stored in plain text in the data directory. Do not reuse it elsewhere.";

/// The environment variable that holds the log filter. When it is set and not
/// empty it replaces `server.log_level`.
const LOG_FILTER_ENV: &str = "RUST_LOG";

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
    /// Runs one scan of the music folders and exits
    Scan(ScanArgs),
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

    /// The TCP port the server listens on. Without this option the port is
    /// 4200
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub port: Option<u16>,

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

/// The options of `wn-server scan`.
#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Reads the tags of every file again. Without this option the scan
    /// reads the files that are new or have changed
    #[arg(long)]
    pub full: bool,
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
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("the log filter `{filter}` is not valid: {reason}")]
    LogFilter { filter: String, reason: String },
    #[error("cannot start logging: {0}")]
    Logging(String),
    #[error("cannot start the runtime that runs the server: {0}")]
    ServerRuntime(io::Error),
    #[error(transparent)]
    Server(#[from] ServerError),
    #[error("{0} ended the server before its stop had finished")]
    StopCutShort(&'static str),
    #[error("cannot create the data directory {}: {source}", path.display())]
    CreateDataDir { path: PathBuf, source: io::Error },
    #[error("{} already exists; init does not overwrite it", .0.display())]
    ConfigExists(PathBuf),
    #[error(
        "{} exists; --folder and --port do not change it. Run init without them, or edit the file",
        .0.display()
    )]
    OptionsNotApplied(PathBuf),
    #[error("cannot write {}: {source}", path.display())]
    WriteFile { path: PathBuf, source: io::Error },
    #[error("cannot write to stdout: {0}")]
    Stdout(#[from] io::Error),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(
        "the database {} does not exist; `wn-server init` creates it and the owner account \
         and keeps an existing config.toml",
        .0.display()
    )]
    NoDatabase(PathBuf),
    #[error(
        "the database {} already has an owner account; init does not replace it. \
         `wn-server user set-password` changes the password",
        .0.display()
    )]
    OwnerExists(PathBuf),
    #[error(
        "the database {} has no owner account; `wn-server init` creates the account \
         and keeps the database and an existing config.toml",
        .0.display()
    )]
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
    #[error(
        "another process is scanning this library: the running server or another \
         `wn-server scan`. Nothing was scanned; run the command again when that scan has ended"
    )]
    ScanRunning,
    #[error(transparent)]
    ScanLock(#[from] ScanLockError),
    #[error("the scan recorded {}", count_of(*.0, "error"))]
    ScanErrors(usize),
}

/// `count` and `noun`, with an `s` when `count` is not 1: `1 error`,
/// `2 errors`.
fn count_of(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
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
/// fails prints its error to stderr and exits with 1.
pub fn run(cli: &Cli) -> ExitCode {
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    match execute(cli, &mut stdin, &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
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
        Command::Scan(args) => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            let config = Config::load(&data_dir.config_path())?;
            // The scanner logs the files it leaves for the next scan.
            init_tracing(&config.server.log_level)?;
            config.library.warn_unavailable_folders();
            scan(&data_dir, &config, args, out)
        }
        Command::Run => {
            let data_dir = DataDir::resolve(cli.data_dir.as_deref())?;
            serve(&data_dir)
        }
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

/// Loads the config, starts logging to stderr and runs the server in the
/// foreground on the database of `data_dir`. A missing `instance.id` is
/// written first. The server is advertised over mDNS while it runs, when
/// `discovery.mdns` is on.
///
/// A music folder of the config that does not exist or cannot be read does
/// not fail the load. `run_server` logs it once and serves the other
/// folders.
///
/// A stop signal ([`forward_stop_signals`]) stops the server in the order of
/// `docs/design.md` section 3.13, and the command then succeeds. After a
/// signal with a short limit ([`StopSignal::short`]) the stop takes the short
/// limits of the server. A second signal during the stop ends the command
/// with an error ([`run_until_stopped`]). The mDNS advertisement is withdrawn
/// before the command returns in every case.
fn serve(data_dir: &DataDir) -> Result<(), CliError> {
    let config = Config::load(&data_dir.config_path())?;
    init_tracing(&config.server.log_level)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(CliError::ServerRuntime)?;
    match write_missing_instance_id(data_dir) {
        Ok(true) => tracing::warn!(
            "{} was missing; wrote a new instance ID, so clients see a new server",
            data_dir.instance_id_path().display()
        ),
        Ok(false) => {}
        // The server runs without the file. It is the ID that discovery
        // advertises, and discovery reports that it cannot read it.
        Err(error) => tracing::warn!("{error}"),
    }
    // The advertisement starts before `run_server` binds the listener. When
    // the bind fails, it is withdrawn before the mDNS daemon has announced
    // anything.
    let advertiser = discovery::advertise(&config, &data_dir.instance_id_path());
    let withdraw = RunOnce::new(move || {
        if let Some(advertiser) = advertiser {
            // The call returns when the goodbye packet is sent.
            advertiser.stop();
            tracing::info!("the mDNS advertisement is withdrawn");
        }
    });
    let options = run_options(Box::new({
        let withdraw = withdraw.clone();
        move || withdraw.run()
    }));
    let outcome = runtime.block_on(async {
        let (signals, received) = mpsc::channel(1);
        tokio::spawn(forward_stop_signals(signals));
        serve_until_stopped(config, data_dir, options, received).await
    });
    // The stop of the server withdraws the advertisement as its first step.
    // A server that did not start has not done so, and after a second signal
    // the step may still run. The call runs the step, or waits for the call
    // that runs it, so the process never exits before the goodbye packet is
    // sent, and the step runs once.
    withdraw.run();
    // Nothing that still runs on the runtime is waited for. After a stop in
    // order nothing runs. After a second signal a scan may run: it ends with
    // the process, as the scan of a process that is killed does.
    runtime.shutdown_background();
    outcome
}

/// What the `run` command adds to its server: `withdraw`, the step that
/// withdraws the mDNS advertisement, and the probe of ffmpeg. The command is
/// the one caller of `run_server` that asks for the probe, so it is the one
/// that starts the program of `transcoding.ffmpeg`.
fn run_options(withdraw: StopStep) -> ServerOptions {
    ServerOptions {
        withdraw: Some(withdraw),
        probe: FfmpegProbe::Run,
        ..ServerOptions::default()
    }
}

/// Runs the server on the database of `data_dir` until it ends, and stops it
/// at the first stop signal that `signals` yields ([`run_until_stopped`]).
///
/// The function makes the two tokens of the stop. The server gets the token
/// of the short limits in [`ServerOptions::hurry`], in place of the one in
/// `options`, and `run_until_stopped` gets the same token. A signal with a
/// short limit therefore reaches the limits of the server, and the path
/// from the signal to the limits is the same code on every OS.
async fn serve_until_stopped(
    config: Config,
    data_dir: &DataDir,
    options: ServerOptions,
    signals: mpsc::Receiver<StopSignal>,
) -> Result<(), CliError> {
    let cancel = CancellationToken::new();
    let hurry = CancellationToken::new();
    let options = ServerOptions {
        hurry: hurry.clone(),
        ..options
    };
    let server = server::run_server(config, data_dir, cancel.clone(), options);
    run_until_stopped(server, &cancel, &hurry, signals).await
}

/// A step that blocks, that several places ask for, and that runs once.
#[derive(Clone)]
struct RunOnce(Arc<Mutex<Option<StopStep>>>);

impl RunOnce {
    fn new(step: impl FnOnce() + Send + 'static) -> Self {
        Self(Arc::new(Mutex::new(Some(Box::new(step)))))
    }

    /// Runs the step when no call has run it, and returns when the step has
    /// ended. A call that finds the step running waits for its end. The
    /// call blocks the thread, so async code calls it on a blocking thread.
    fn run(&self) {
        // A step that panicked is gone from the slot, so the lock is usable.
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // The lock is held while the step runs.
        if let Some(step) = slot.take() {
            step();
        }
    }
}

/// A stop signal that the process received.
///
/// The constants are every stop signal of every OS. They are compiled on
/// every OS, so the tests of this file check on every OS which signals have
/// a short limit. The listeners are what differs between the systems
/// ([`OtherStopSignals`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StopSignal {
    /// The name of the signal in the log and in the error of a stop that
    /// was cut short.
    name: &'static str,
    /// Whether the operating system ends the process a fixed time after the
    /// signal, whatever the process does. The stop then takes the short
    /// limits of the server (`server.shutdown_grace_short_ms`), so that it
    /// reaches its last step within that time.
    short: bool,
}

impl StopSignal {
    // A constant that the listeners of one OS send is unused in a build for
    // another OS, where it is compiled for the tests. The `allow` on such a
    // constant applies to those builds only. The build for its own OS still
    // reports the constant when no listener sends it.

    /// Ctrl-C on every OS, which is SIGINT on Unix. No system limits the
    /// time after it.
    const CTRL_C: Self = Self {
        name: "Ctrl-C",
        short: false,
    };

    /// SIGTERM. The sender decides what follows: `docker stop` and a service
    /// manager send SIGKILL after a time that their configuration sets.
    // Unix: Windows has no SIGTERM.
    #[cfg_attr(not(unix), allow(dead_code))]
    const SIGTERM: Self = Self {
        name: "SIGTERM",
        short: false,
    };

    /// The Windows console event `CTRL_BREAK_EVENT`. Windows does not limit
    /// the time after it.
    // Windows: Unix has no console events.
    #[cfg_attr(not(windows), allow(dead_code))]
    const CTRL_BREAK: Self = Self {
        name: "Ctrl-Break",
        short: false,
    };

    /// The Windows console event `CTRL_CLOSE_EVENT`: the user closed the
    /// console window or ended the task. Windows ends the process 5 seconds
    /// after the event.
    // Windows: Unix has no console events.
    #[cfg_attr(not(windows), allow(dead_code))]
    const CONSOLE_CLOSE: Self = Self {
        name: "the close of the console",
        short: true,
    };

    /// The Windows console event `CTRL_SHUTDOWN_EVENT`: the system shuts
    /// down. Windows ends the process 5 seconds after the event, and a
    /// service process 20 seconds after it. By the documentation of Windows
    /// the event reaches services only, and no process that has loaded
    /// `user32.dll`, so the server may never receive it (`docs/design.md`
    /// section 3.13).
    // Windows: Unix has no console events.
    #[cfg_attr(not(windows), allow(dead_code))]
    const SYSTEM_SHUTDOWN: Self = Self {
        name: "the shutdown of the system",
        short: true,
    };
}

/// Runs `server` to its end and returns its result. `signals` yields each
/// stop signal that the process receives.
///
/// The first signal cancels `cancel`, which asks the server to stop, and the
/// function keeps waiting for the server. When that signal has a short limit
/// ([`StopSignal::short`]), the function cancels `hurry` before `cancel`, so
/// the server that reads `hurry` when its stop begins finds it set. A second
/// signal before the server has ended is an error: the function returns at
/// once and drops `server`, so the steps of the stop that have not run are
/// skipped.
async fn run_until_stopped(
    server: impl Future<Output = Result<(), ServerError>>,
    cancel: &CancellationToken,
    hurry: &CancellationToken,
    mut signals: mpsc::Receiver<StopSignal>,
) -> Result<(), CliError> {
    let mut server = pin!(server);
    let first = tokio::select! {
        result = &mut server => return Ok(result?),
        signal = signals.recv() => match signal {
            Some(signal) => signal,
            // No signal can arrive. The server runs until it ends by itself.
            None => return Ok(server.await?),
        },
    };
    tracing::info!(
        "received {}: the server stops. A second signal ends it at once",
        first.name
    );
    if first.short {
        hurry.cancel();
    }
    cancel.cancel();
    tokio::select! {
        result = &mut server => Ok(result?),
        Some(signal) = signals.recv() => {
            tracing::warn!(
                "received {} during the stop: the server ends now",
                signal.name
            );
            Err(CliError::StopCutShort(signal.name))
        }
    }
}

/// Sends each stop signal that the process receives to `signals`, until the
/// receiver is gone. Ctrl-C is a stop signal on every OS.
/// [`OtherStopSignals`] holds the ones that differ.
///
/// The two listeners do not depend on each other. When the process cannot
/// listen for Ctrl-C, or cannot listen for the other signals, the function
/// logs one warning and listens for the rest. A signal without a listener
/// keeps the action of the system, which ends the process without the stop.
/// The function returns when it can listen for no signal.
async fn forward_stop_signals(signals: mpsc::Sender<StopSignal>) {
    // One future listens for Ctrl-C while the other signals arrive, so a
    // Ctrl-C that comes together with another signal is not lost. A new
    // future takes its place after each Ctrl-C.
    let mut ctrl_c = pin!(tokio::signal::ctrl_c());
    let mut ctrl_c_listens = true;
    let mut others = match OtherStopSignals::new() {
        Ok(others) => Some(others),
        Err(error) => {
            tracing::warn!("cannot listen for a stop signal other than Ctrl-C: {error}");
            None
        }
    };
    while ctrl_c_listens || others.is_some() {
        let signal = tokio::select! {
            result = &mut ctrl_c, if ctrl_c_listens => match result {
                Ok(()) => {
                    ctrl_c.set(tokio::signal::ctrl_c());
                    StopSignal::CTRL_C
                }
                Err(error) => {
                    tracing::warn!("cannot listen for Ctrl-C: {error}");
                    ctrl_c_listens = false;
                    continue;
                }
            },
            Some(signal) = next_signal(&mut others) => signal,
        };
        if signals.send(signal).await.is_err() {
            return;
        }
    }
    tracing::warn!("the server cannot listen for a stop signal");
}

/// Waits for the next signal of `others` and returns it. `None` without a
/// listener.
async fn next_signal(others: &mut Option<OtherStopSignals>) -> Option<StopSignal> {
    match others {
        Some(others) => Some(others.recv().await),
        None => None,
    }
}

/// The stop signals other than Ctrl-C.
///
/// Unix: SIGTERM. `kill`, systemd, launchd and `docker stop` send it to stop
/// a process. Windows has no such signal, so this type differs there.
#[cfg(unix)]
struct OtherStopSignals {
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl OtherStopSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Waits for the next signal and returns it.
    async fn recv(&mut self) -> StopSignal {
        match self.terminate.recv().await {
            Some(()) => StopSignal::SIGTERM,
            // The listener has ended and yields no signal.
            None => std::future::pending().await,
        }
    }
}

/// The stop signals other than Ctrl-C.
///
/// Windows: the console events Ctrl-Break, close and shutdown. The system
/// sends the close event when the user closes the console window and the
/// shutdown event when the system shuts down. After one of these two it
/// ends the process when the process has not exited within 5 seconds. The
/// constants of [`StopSignal`] hold that: this type only pairs each
/// listener with its constant, and it is the one part of the stop that no
/// test runs (`docs/design.md` section 3.13). Unix has no console events,
/// so this type differs there.
///
/// The process does not listen for the logoff event. That event keeps the
/// action of the system, which ends the process without the stop. By the
/// documentation of Windows the shutdown event may not reach the process
/// either ([`StopSignal::SYSTEM_SHUTDOWN`]).
#[cfg(windows)]
struct OtherStopSignals {
    ctrl_break: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl OtherStopSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};

        Ok(Self {
            ctrl_break: ctrl_break()?,
            close: ctrl_close()?,
            shutdown: ctrl_shutdown()?,
        })
    }

    /// Waits for the next event and returns its signal.
    async fn recv(&mut self) -> StopSignal {
        let event = tokio::select! {
            event = self.ctrl_break.recv() => event.map(|()| StopSignal::CTRL_BREAK),
            event = self.close.recv() => event.map(|()| StopSignal::CONSOLE_CLOSE),
            event = self.shutdown.recv() => event.map(|()| StopSignal::SYSTEM_SHUTDOWN),
        };
        match event {
            Some(signal) => signal,
            // The listener has ended and yields no event.
            None => std::future::pending().await,
        }
    }
}

/// Runs one scan of the music folders of `config` into the database of
/// `data_dir`, in this process, and prints its summary to `out`. `--full`
/// reads the tags of every file again.
///
/// The scan holds the scan lock of the data directory from its start to its
/// end. While another process scans the library, which is the running
/// server or another `wn-server scan`, the command scans nothing and fails.
/// A server that runs and does not scan keeps serving during the scan:
/// SQLite lets it read while the scan writes, and a write of the server
/// waits for one transaction of the scan at most. The server starts no scan
/// of its own while this one holds the lock.
///
/// The database must exist; only `init` creates it. The scan removes the
/// stale files of the cover cache of the data directory. After the scan the
/// command checkpoints the write-ahead log, as `init` does. The command
/// fails when the scan recorded an error, after it has printed the summary.
///
/// A music folder of `config` that does not exist or cannot be read does not
/// keep the scan from starting. The caller has logged it once. The scan
/// reads the other folders, deletes no track and records an error that
/// names the folder, so the command fails after the summary.
fn scan(
    data_dir: &DataDir,
    config: &Config,
    args: &ScanArgs,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let db = open_existing_db(data_dir)?;
    let Some(lock) = ScanLock::try_acquire(&data_dir.scan_lock_path())? else {
        return Err(CliError::ScanRunning);
    };
    let options = ScanOptions {
        covers_cache_dir: Some(data_dir.covers_cache_dir()),
        ..ScanOptions::default()
    };
    // The command has no way to cancel its scan. A process that is ended
    // during the scan keeps the batches that the scan committed, and the
    // next scan finishes the work.
    let never = CancellationToken::new();
    let scanned = scanner::scan(&db, &config.library.folders, &never, args.full, options);
    drop(lock);
    let summary = scanned?;
    // The rows of the scan are in the database file before the summary is
    // printed. A running server may keep the checkpoint from finishing, and
    // it then checkpoints the log itself.
    close_db(db)?;

    print_scan_summary(out, &summary)?;
    match summary.errors.len() {
        0 => Ok(()),
        errors => Err(CliError::ScanErrors(errors)),
    }
}

/// Prints the counts of `summary`, one `name: number` line each, and then one
/// `error: <message>` line for each error of the scan.
fn print_scan_summary(out: &mut dyn Write, summary: &ScanSummary) -> io::Result<()> {
    writeln!(out, "seen: {}", summary.seen)?;
    writeln!(out, "added: {}", summary.added)?;
    writeln!(out, "changed: {}", summary.changed)?;
    writeln!(out, "moved: {}", summary.moved)?;
    writeln!(out, "removed: {}", summary.removed)?;
    writeln!(out, "albums: {}", summary.albums)?;
    writeln!(out, "artists: {}", summary.artists)?;
    writeln!(out, "errors: {}", summary.errors.len())?;
    for error in &summary.errors {
        writeln!(out, "error: {error}")?;
    }
    Ok(())
}

/// Writes a new instance ID to `instance.id` when the file is missing and the
/// database exists. Returns whether it wrote the file.
///
/// `init` writes the file together with the database, and it refuses a data
/// directory that has a config file and an owner account. No other command
/// writes the file, so `run` restores it. A data directory without a database
/// has lost more than this file, and `init` restores that one.
fn write_missing_instance_id(data_dir: &DataDir) -> Result<bool, CliError> {
    if !data_dir.db_path().exists() {
        return Ok(false);
    }
    write_new_file(
        &data_dir.instance_id_path(),
        &format!("{}\n", new_instance_id()),
    )
}

/// The log filter: the value of `RUST_LOG` when it is set and not empty,
/// otherwise `server.log_level`.
fn log_filter(variable: Option<String>, log_level: &str) -> String {
    variable
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| log_level.to_owned())
}

/// Parses a log filter such as `info` or `warn,wn_server=debug`.
fn parse_log_filter(filter: &str) -> Result<EnvFilter, CliError> {
    EnvFilter::try_new(filter).map_err(|error| CliError::LogFilter {
        filter: filter.to_owned(),
        reason: error.to_string(),
    })
}

/// Starts logging to stderr. The filter is `RUST_LOG`, or `log_level` when
/// that variable is unset. Colours are written only when stderr is a
/// terminal.
fn init_tracing(log_level: &str) -> Result<(), CliError> {
    let filter = parse_log_filter(&log_filter(env::var(LOG_FILTER_ENV).ok(), log_level))?;
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| CliError::Logging(error.to_string()))
}

/// Creates the data directory, `config.toml`, `instance.id`, the database and
/// the owner account.
///
/// `config.toml` is the template with the folders and the port of `args`.
/// Each folder must be an existing directory and is written as an absolute
/// path. `init` refuses a folder that does not exist, although `run` and
/// `scan` start with one: a path that is typed here and does not exist is
/// most likely a typing mistake. The password comes from the source that
/// [`password_source`] picks. A generated password is printed once.
///
/// A data directory that has `config.toml` and no owner account keeps
/// `config.toml` and gets what is missing: the database when there is none,
/// and the owner account. The folders and the port of `args` are an error
/// there, because they do not change a config file that exists. The rows of a
/// database that exists stay as they are.
///
/// Nothing is created when a folder fails its check, when `config.toml`
/// exists and the database has an owner account or when no password can be
/// read. An existing owner account is an error and is left as it is. An
/// existing `instance.id` is kept. When a step after the config write fails,
/// the `config.toml` and the `instance.id` that this run wrote are removed. A
/// run that keeps `config.toml` removes no file.
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
    let text = Config::template(&folders, args.port.unwrap_or(config::DEFAULT_PORT))?;

    let config_path = data_dir.config_path();
    let db_path = data_dir.db_path();
    // A data directory with a config file and no owner account has lost its
    // database or its owner. This run keeps the config file and the database
    // that exist and creates what is missing.
    let keeps_config = config_path.exists();
    let keeps_db = db_path.exists();
    // These checks run before the password is read, so an `init` that refuses
    // does not wait for a line on stdin. The config write below refuses an
    // existing file again, and the insert refuses an existing owner again.
    if keeps_config {
        if keeps_db && has_owner(&db_path)? {
            return Err(CliError::ConfigExists(config_path));
        }
        if !args.folders.is_empty() || args.port.is_some() {
            return Err(CliError::OptionsNotApplied(config_path));
        }
    }
    let password = new_password(args.password_stdin, input)?;

    // The files this run wrote. An `instance.id` from before the run is not
    // in the list.
    let mut written = Vec::new();
    if !keeps_config {
        create_data_dir(data_dir.path()).map_err(|source| CliError::CreateDataDir {
            path: data_dir.path().to_path_buf(),
            source,
        })?;
        if !write_new_file(&config_path, &text)? {
            return Err(CliError::ConfigExists(config_path));
        }
        written.push(config_path.clone());
    }
    let (db, owner) = match create_owner(data_dir, &args.username, &password.text, &mut written) {
        Ok(created) => created,
        Err(error) => {
            // `init` does not apply `--folder` and `--port` to a config file
            // that exists. A run that wrote the config file removes the files
            // it wrote, so that `init` can run again with the same options
            // when the cause of the error is gone. A run that kept the config
            // file removes nothing. The next run keeps the `instance.id` and
            // uses the database that this run left.
            if !keeps_config {
                for path in &written {
                    let _ = fs::remove_file(path);
                }
            }
            return Err(error);
        }
    };
    // The account is committed. An error from here on leaves the config file
    // in place, and `user set-password` sets a password that was not printed.
    close_db(db)?;

    if keeps_config {
        writeln!(out, "kept {}", config_path.display())?;
        let action = if keeps_db { "kept" } else { "created" };
        writeln!(out, "{action} {}", db_path.display())?;
    } else {
        writeln!(out, "wrote {}", config_path.display())?;
        if folders.is_empty() {
            writeln!(
                out,
                "no music folder is configured; add one to library.folders in that file"
            )?;
        }
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

/// Returns whether the database at `db_path` has an owner account. The
/// file must exist. `Db::open` applies the migrations that the database
/// lacks.
fn has_owner(db_path: &Path) -> Result<bool, CliError> {
    let db = Db::open(db_path)?;
    Ok(db.with_reader(users::get_owner)?.is_some())
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
/// creates a missing file, and the `user` commands and `scan` must not
/// create a database, so a missing file is an error here.
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

/// Returns the UTC calendar date of a Unix time as `YYYY-MM-DD`. The
/// calendar computation is the one of the times that the API reports
/// (`api::model::civil_date`).
fn utc_date(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let (year, month, day) = crate::api::model::civil_date(days);
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
    use crate::db::queries::scans;
    use crate::library::scan_controller::testing::{Gate, music_folder};
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
        assert_eq!(args.port, Some(4321));
    }

    #[test]
    fn init_defaults_to_no_folder_no_port_and_the_name_owner() {
        let cli = parse(&["init"]).expect("init");
        let Command::Init(args) = cli.command else {
            panic!("expected init");
        };
        assert!(args.folders.is_empty());
        assert_eq!(args.port, None);
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
    fn scan_takes_the_full_option() {
        for (args, full) in [(&["scan"][..], false), (&["scan", "--full"], true)] {
            let cli = parse(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            let Command::Scan(scan) = cli.command else {
                panic!("expected scan");
            };
            assert_eq!(scan.full, full, "{args:?}");
        }
        assert!(parse(&["scan", "--full=maybe"]).is_err());
        assert!(parse(&["scan", "folder"]).is_err());
    }

    /// A data directory with an empty database, and the config of a server
    /// with the one music folder `folder`.
    fn scan_setup(folder: &Path) -> (tempfile::TempDir, DataDir, Config) {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
        drop(Db::open(data_dir.db_path()).expect("create the database"));
        let mut config = Config::default();
        config.library.folders = vec![folder.to_path_buf()];
        (dir, data_dir, config)
    }

    /// Runs the `scan` command and returns its result and what it printed.
    fn run_scan(data_dir: &DataDir, config: &Config, full: bool) -> (Result<(), CliError>, String) {
        let mut out = Vec::new();
        let result = scan(data_dir, config, &ScanArgs { full }, &mut out);
        (result, String::from_utf8(out).expect("UTF-8 output"))
    }

    /// The summary that `scan` prints for a scan without an error. The three
    /// fixture files carry no tags, so they are one album of one artist.
    fn summary_lines(added: u64, changed: u64) -> String {
        format!(
            "seen: 3\nadded: {added}\nchanged: {changed}\nmoved: 0\nremoved: 0\nalbums: 1\n\
             artists: 1\nerrors: 0\n"
        )
    }

    /// The rows of the `scans` table of `data_dir`.
    fn scan_rows(data_dir: &DataDir) -> Vec<scans::Scan> {
        let db = Db::open(data_dir.db_path()).expect("open the database");
        db.with_reader(scans::list).expect("read the scans")
    }

    #[test]
    fn scan_stores_the_tracks_prints_the_summary_and_frees_the_lock() {
        let folder = music_folder(3);
        let (_dir, data_dir, config) = scan_setup(folder.path());

        let (result, out) = run_scan(&data_dir, &config, false);
        result.expect("the scan succeeds");
        assert_eq!(out, summary_lines(3, 0));
        let rows = scan_rows(&data_dir);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].finished_at.is_some());
        assert_eq!(rows[0].error, None);
        assert_eq!(rows[0].counts.added, 3);

        // The lock is free, so the next scan runs. It finds nothing to do.
        let (result, out) = run_scan(&data_dir, &config, false);
        result.expect("the scan succeeds");
        assert_eq!(out, summary_lines(0, 0));

        // A full scan reads the tags of every file again.
        let (result, out) = run_scan(&data_dir, &config, true);
        result.expect("the scan succeeds");
        assert_eq!(out, summary_lines(0, 3));
        assert_eq!(scan_rows(&data_dir).len(), 3);
    }

    #[test]
    fn scan_fails_after_the_summary_when_the_scan_recorded_an_error() {
        let folder = music_folder(3);
        let (_dir, data_dir, config) = scan_setup(folder.path());
        run_scan(&data_dir, &config, false)
            .0
            .expect("the scan succeeds");

        // The folder had tracks and holds no supported file now.
        for entry in fs::read_dir(folder.path()).expect("list the folder") {
            fs::remove_file(entry.expect("an entry").path()).expect("remove the file");
        }
        let (result, out) = run_scan(&data_dir, &config, false);

        let error = result.expect_err("the scan recorded an error");
        assert!(matches!(error, CliError::ScanErrors(1)), "{error}");
        assert_eq!(error.to_string(), "the scan recorded 1 error");
        let name = folder.path().file_name().expect("a name");
        let message = format!(
            "music folder \"{}\" ({}) has no supported file; none of its tracks was deleted",
            name.to_str().expect("a UTF-8 name"),
            folder.path().display()
        );
        assert_eq!(
            out,
            format!(
                "seen: 0\nadded: 0\nchanged: 0\nmoved: 0\nremoved: 0\nalbums: 1\nartists: 1\n\
                 errors: 1\nerror: {message}\n"
            )
        );
        let rows = scan_rows(&data_dir);
        assert_eq!(rows[1].error.as_deref(), Some(message.as_str()));
    }

    #[test]
    fn scan_errors_are_counted_in_the_message() {
        assert_eq!(count_of(1, "error"), "1 error");
        assert_eq!(count_of(2, "error"), "2 errors");
        assert_eq!(
            CliError::ScanErrors(12).to_string(),
            "the scan recorded 12 errors"
        );
    }

    #[test]
    fn scan_scans_nothing_while_another_process_holds_the_scan_lock() {
        let folder = music_folder(3);
        let (_dir, data_dir, config) = scan_setup(folder.path());
        // The lock that a scan of the running server holds.
        let other = ScanLock::try_acquire(&data_dir.scan_lock_path())
            .expect("lock the file")
            .expect("the lock is free");

        for full in [false, true] {
            let (result, out) = run_scan(&data_dir, &config, full);
            let error = result.expect_err("another process scans");
            assert!(matches!(error, CliError::ScanRunning), "{error}");
            assert!(error.to_string().contains("Nothing was scanned"), "{error}");
            assert!(out.is_empty(), "{out}");
        }
        assert_eq!(scan_rows(&data_dir), []);

        // The other scan has ended.
        drop(other);
        let (result, out) = run_scan(&data_dir, &config, false);
        result.expect("the scan succeeds");
        assert_eq!(out, summary_lines(3, 0));
    }

    #[test]
    fn scan_without_a_database_names_init_and_creates_no_file() {
        let folder = music_folder(1);
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");
        let mut config = Config::default();
        config.library.folders = vec![folder.path().to_path_buf()];

        let (result, out) = run_scan(&data_dir, &config, false);
        let error = result.expect_err("no database");
        assert!(matches!(error, CliError::NoDatabase(_)), "{error}");
        assert!(error.to_string().contains("wn-server init"), "{error}");
        assert!(out.is_empty(), "{out}");
        assert!(!data_dir.db_path().exists());
        assert!(!data_dir.scan_lock_path().exists());
    }

    #[test]
    fn scan_removes_the_stale_files_of_the_cover_cache_of_the_data_directory() {
        let folder = music_folder(1);
        let cover = folder.path().join("cover.jpg");
        fs::write(&cover, "an image").expect("write the cover");
        let (_dir, data_dir, config) = scan_setup(folder.path());
        run_scan(&data_dir, &config, false)
            .0
            .expect("the scan succeeds");

        let (album_id, cover_mtime): (String, i64) = Db::open(data_dir.db_path())
            .expect("open the database")
            .with_reader(|conn| {
                Ok(
                    conn.query_row("SELECT id, cover_mtime FROM albums", [], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?,
                )
            })
            .expect("the album of the file");
        let cache = data_dir.covers_cache_dir();
        fs::create_dir_all(&cache).expect("create the cache");
        let stale = cache.join(format!("{album_id}-64-{cover_mtime}.jpg"));
        fs::write(&stale, "a resized cover").expect("write the cached cover");

        // The cover file is replaced by a newer one.
        fs::write(&cover, "another image").expect("replace the cover");
        let newer = cover_mtime + 10;
        let file = fs::File::options()
            .write(true)
            .open(&cover)
            .expect("open the cover");
        file.set_modified(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(u64::try_from(newer).expect("a time")),
        )
        .expect("set the time");
        drop(file);

        run_scan(&data_dir, &config, false)
            .0
            .expect("the scan succeeds");
        assert!(!stale.exists(), "{}", stale.display());
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

    /// The data directory `dir` with a file at the path of the database.
    fn data_dir_with_a_database(dir: &Path) -> DataDir {
        let data_dir = DataDir::resolve(Some(dir)).expect("a data directory");
        fs::write(data_dir.db_path(), b"").expect("write the database file");
        data_dir
    }

    #[test]
    fn run_writes_a_missing_instance_id_once_when_the_database_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = data_dir_with_a_database(dir.path());
        let path = data_dir.instance_id_path();

        assert!(write_missing_instance_id(&data_dir).expect("first start"));
        let id = discovery::read_instance_id(&path).expect("an ID that discovery accepts");
        assert_eq!(
            fs::read_to_string(&path).expect("read instance.id"),
            format!("{id}\n")
        );

        // The next start finds the file. It writes nothing, so `run` logs
        // nothing, and the ID stays.
        assert!(!write_missing_instance_id(&data_dir).expect("second start"));
        assert_eq!(discovery::read_instance_id(&path).expect("an ID"), id);
    }

    #[test]
    fn run_keeps_an_existing_instance_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = data_dir_with_a_database(dir.path());
        let path = data_dir.instance_id_path();
        fs::write(&path, "kept\n").expect("write instance.id");

        assert!(!write_missing_instance_id(&data_dir).expect("start"));
        assert_eq!(
            fs::read_to_string(&path).expect("read instance.id"),
            "kept\n"
        );
    }

    #[test]
    fn run_writes_no_instance_id_without_a_database() {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::resolve(Some(dir.path())).expect("a data directory");

        assert!(!write_missing_instance_id(&data_dir).expect("start"));
        assert!(!data_dir.instance_id_path().exists());
    }

    /// The longest time a test of the stop waits for an event. Each of these
    /// waits ends with its event, which needs no file and no socket, so the
    /// limit does not delay a passing run. It is 10 s, so that it only ends
    /// a test that hangs, also on a loaded machine.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// A time in which a call that must wait does not return. A call that
    /// returns in it fails the test, so a slow machine cannot fail the test.
    const SHORT: std::time::Duration = std::time::Duration::from_millis(50);

    /// A server that runs until `cancel` is cancelled and then returns
    /// `result`.
    async fn server_until(
        cancel: CancellationToken,
        result: Result<(), ServerError>,
    ) -> Result<(), ServerError> {
        cancel.cancelled().await;
        result
    }

    /// Sets its flag when it is dropped.
    struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn first_signal_asks_the_server_to_stop_and_the_command_succeeds() {
        let (cancel, hurry) = (CancellationToken::new(), CancellationToken::new());
        let (signals, received) = mpsc::channel(1);
        let server = server_until(cancel.clone(), Ok(()));
        let run = run_until_stopped(server, &cancel, &hurry, received);
        let run = tokio::time::timeout(WAIT, run);

        signals
            .send(StopSignal::SIGTERM)
            .await
            .expect("the run listens");
        run.await
            .expect("the run ends")
            .expect("the command succeeds");
        assert!(cancel.is_cancelled());
    }

    /// Runs a server that stops when it is asked to, sends it `signal`
    /// through the channel that stands for the signals of the system, and
    /// returns whether the run cancelled the stop token and whether it
    /// cancelled `hurry`. The server reads `hurry` at the moment its stop
    /// begins, as `run_server` does, and the second value is what it read.
    async fn tokens_after(signal: StopSignal) -> (bool, bool) {
        let (cancel, hurry) = (CancellationToken::new(), CancellationToken::new());
        let (signals, received) = mpsc::channel(1);
        let read_at_the_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = {
            let (cancel, hurry) = (cancel.clone(), hurry.clone());
            let read = Arc::clone(&read_at_the_stop);
            async move {
                cancel.cancelled().await;
                read.store(hurry.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        };
        let run = run_until_stopped(server, &cancel, &hurry, received);
        let run = tokio::time::timeout(WAIT, run);

        signals.send(signal).await.expect("the run listens");
        run.await
            .expect("the run ends")
            .expect("the command succeeds");
        let read = read_at_the_stop.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(read, hurry.is_cancelled(), "{signal:?}");
        (cancel.is_cancelled(), read)
    }

    #[tokio::test]
    async fn signal_with_a_short_limit_cancels_hurry_and_the_stop_token() {
        let signal = StopSignal {
            name: "a signal with a limit",
            short: true,
        };
        assert_eq!(tokens_after(signal).await, (true, true));
        // The two console events of Windows after which the system ends the
        // process.
        for signal in [StopSignal::CONSOLE_CLOSE, StopSignal::SYSTEM_SHUTDOWN] {
            assert_eq!(tokens_after(signal).await, (true, true), "{signal:?}");
        }
    }

    #[tokio::test]
    async fn signal_without_a_limit_cancels_the_stop_token_only() {
        let signal = StopSignal {
            name: "a signal without a limit",
            short: false,
        };
        assert_eq!(tokens_after(signal).await, (true, false));
        // The stop signals of Linux and macOS, and Ctrl-Break of Windows.
        // The stop after each of them has the long limits.
        for signal in [
            StopSignal::CTRL_C,
            StopSignal::SIGTERM,
            StopSignal::CTRL_BREAK,
        ] {
            assert_eq!(tokens_after(signal).await, (true, false), "{signal:?}");
        }
    }

    #[test]
    fn only_the_close_and_the_shutdown_of_a_windows_console_have_a_short_limit() {
        // Every stop signal of every OS, with its name in the log.
        let signals = [
            (StopSignal::CTRL_C, "Ctrl-C", false),
            (StopSignal::SIGTERM, "SIGTERM", false),
            (StopSignal::CTRL_BREAK, "Ctrl-Break", false),
            (StopSignal::CONSOLE_CLOSE, "the close of the console", true),
            (
                StopSignal::SYSTEM_SHUTDOWN,
                "the shutdown of the system",
                true,
            ),
        ];
        for (signal, name, short) in signals {
            assert_eq!(signal, StopSignal { name, short });
        }
    }

    #[test]
    fn run_command_asks_for_the_probe_of_ffmpeg_and_passes_its_step() {
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let options = run_options(Box::new({
            let ran = Arc::clone(&ran);
            move || ran.store(true, std::sync::atomic::Ordering::SeqCst)
        }));
        // The server of the command probes the program of
        // `transcoding.ffmpeg`. The default of the options does not.
        assert_eq!(options.probe, FfmpegProbe::Run);
        assert_eq!(ServerOptions::default().probe, FfmpegProbe::Skip);

        // The step is the one of the caller, and the options add nothing
        // else.
        let withdraw = options.withdraw.expect("the step of the command");
        withdraw();
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(options.listening.is_none());
        assert!(!options.routes.has_routes());
        assert!(options.scan.batch_size.is_none());
        assert!(options.scan.on_batch.is_none());
    }

    /// A data directory whose database holds the owner account, and the
    /// config of a server that scans the music folder `folder` at startup.
    /// The server listens on `127.0.0.1` with a port that the system
    /// chooses. Its grace is one hour and its short grace is `short`.
    async fn server_setup(
        folder: &Path,
        short: std::time::Duration,
    ) -> (tempfile::TempDir, DataDir, Config) {
        let (dir, data_dir, mut config) = scan_setup(folder);
        let db = Db::open(data_dir.db_path()).expect("open the database");
        db.write(|conn| users::insert_owner(conn, DEFAULT_USERNAME, "sesame"))
            .await
            .expect("insert the owner")
            .expect("a new database has no owner");
        config.server.bind = "127.0.0.1".parse().expect("an address");
        config.server.port = 0;
        config.server.shutdown_grace_ms = 3_600_000;
        config.server.shutdown_grace_short_ms =
            u64::try_from(short.as_millis()).expect("a short time");
        assert!(config.library.scan_on_start);
        (dir, data_dir, config)
    }

    /// Reads the `scans` rows of `data_dir` from an async test.
    async fn read_scan_rows(data_dir: &DataDir) -> Vec<scans::Scan> {
        let db = Db::open(data_dir.db_path()).expect("open the database");
        db.read(scans::list).await.expect("read the scans")
    }

    /// Waits until the one scan of `data_dir` has closed its row, and
    /// returns the error of the row.
    async fn scan_closed(data_dir: &DataDir) -> Option<String> {
        let closed = async {
            loop {
                let mut rows = read_scan_rows(data_dir).await;
                assert_eq!(rows.len(), 1);
                if rows[0].finished_at.is_some() {
                    return rows.remove(0).error;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(WAIT, closed)
            .await
            .expect("the scan ends")
    }

    #[tokio::test]
    async fn close_of_the_console_stops_the_running_server_with_its_short_limits() {
        let folder = music_folder(3);
        let short = std::time::Duration::from_millis(50);
        let (_dir, data_dir, config) = server_setup(folder.path(), short).await;
        // The scan at startup stores its first batch and then waits in its
        // hook, as a file read that does not return holds a scan.
        let (hooks, mut gate) = Gate::hooks();
        let options = ServerOptions {
            scan: hooks,
            ..ServerOptions::default()
        };
        let (signals, received) = mpsc::channel(1);
        let mut run = pin!(serve_until_stopped(config, &data_dir, options, received));
        tokio::select! {
            result = &mut run => panic!("the server ended: {result:?}"),
            () = gate.reached() => {}
        }

        // The signal of the close event arrives as the listener of Windows
        // sends it. The grace of the server is one hour, so a server that
        // ends has taken its short limit for the scan.
        signals
            .send(StopSignal::CONSOLE_CLOSE)
            .await
            .expect("the run listens");
        let began = std::time::Instant::now();
        tokio::time::timeout(WAIT, &mut run)
            .await
            .expect("the server ends long before its grace of one hour")
            .expect("the command succeeds");
        // The scan had the short limit. A wait does not end early, so a
        // slow machine cannot fail this.
        assert!(began.elapsed() >= short, "{:?}", began.elapsed());
        // The stop left the scan behind: its row has no end time.
        let rows = read_scan_rows(&data_dir).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].finished_at, None);

        // A process ends the thread of the scan by exiting. The test lets
        // the scan end, so that it does not outlive the data directory.
        gate.open();
        assert_eq!(
            scan_closed(&data_dir).await.as_deref(),
            Some(scanner::CANCELLED)
        );
    }

    #[tokio::test]
    async fn sigterm_stops_the_running_server_with_its_long_limits() {
        let folder = music_folder(3);
        // The short grace is 1 ms and is over long before the wait below
        // ends, so a server that took it would have ended.
        let short = std::time::Duration::from_millis(1);
        let (_dir, data_dir, config) = server_setup(folder.path(), short).await;
        let (hooks, mut gate) = Gate::hooks();
        let options = ServerOptions {
            scan: hooks,
            ..ServerOptions::default()
        };
        let (signals, received) = mpsc::channel(1);
        let mut run = pin!(serve_until_stopped(config, &data_dir, options, received));
        tokio::select! {
            result = &mut run => panic!("the server ended: {result:?}"),
            () = gate.reached() => {}
        }

        // The stop signal of Linux and macOS. The server waits for the
        // scan, which the hook holds.
        signals
            .send(StopSignal::SIGTERM)
            .await
            .expect("the run listens");
        let waited = tokio::time::timeout(SHORT, &mut run).await;
        assert!(waited.is_err(), "the server ended: {waited:?}");

        // The scan ends as a cancelled scan, and the server ends after it.
        gate.open();
        tokio::time::timeout(WAIT, &mut run)
            .await
            .expect("the server ends with the scan")
            .expect("the command succeeds");
        let rows = read_scan_rows(&data_dir).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].error.as_deref(), Some(scanner::CANCELLED));
    }

    #[tokio::test]
    async fn no_signal_leaves_the_server_running_and_its_error_is_the_error_of_the_command() {
        let (cancel, hurry) = (CancellationToken::new(), CancellationToken::new());
        let (_signals, received) = mpsc::channel(1);
        let server = server_until(CancellationToken::new(), Ok(()));

        // The server runs and no signal arrives.
        let run = run_until_stopped(server, &cancel, &hurry, received);
        let waited = tokio::time::timeout(SHORT, run).await;
        assert!(waited.is_err(), "the run ended without a signal");
        assert!(!cancel.is_cancelled());

        // A server that ends by itself ends the run with its result.
        let (_signals, received) = mpsc::channel(1);
        let ended = CancellationToken::new();
        ended.cancel();
        let failed = Err(ServerError::NoOwner(PathBuf::from("library.db")));
        let error = run_until_stopped(server_until(ended, failed), &cancel, &hurry, received)
            .await
            .expect_err("the server failed");
        assert!(
            matches!(error, CliError::Server(ServerError::NoOwner(_))),
            "{error}"
        );
        assert!(!cancel.is_cancelled());
        assert!(!hurry.is_cancelled());
    }

    #[tokio::test]
    async fn second_signal_during_the_stop_drops_the_server_and_is_an_error() {
        let (cancel, hurry) = (CancellationToken::new(), CancellationToken::new());
        let (signals, received) = mpsc::channel(1);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A server whose stop does not end.
        let server = {
            let flag = DropFlag(Arc::clone(&dropped));
            async move {
                let _flag = flag;
                std::future::pending::<Result<(), ServerError>>().await
            }
        };
        let mut run = pin!(run_until_stopped(server, &cancel, &hurry, received));

        // The first signal starts the stop, and the run waits for the server.
        signals
            .send(StopSignal::SIGTERM)
            .await
            .expect("the run listens");
        let waited = tokio::time::timeout(SHORT, &mut run).await;
        assert!(waited.is_err(), "the run ended after one signal");
        assert!(cancel.is_cancelled());
        assert!(!dropped.load(std::sync::atomic::Ordering::SeqCst));

        signals
            .send(StopSignal::CTRL_C)
            .await
            .expect("the run listens");
        let error = tokio::time::timeout(WAIT, &mut run)
            .await
            .expect("the run ends at the second signal")
            .expect_err("the stop was cut short");
        assert!(matches!(error, CliError::StopCutShort("Ctrl-C")), "{error}");
        assert_eq!(
            error.to_string(),
            "Ctrl-C ended the server before its stop had finished"
        );
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_without_a_signal_source_waits_for_the_server() {
        let (cancel, hurry) = (CancellationToken::new(), CancellationToken::new());
        let (signals, received) = mpsc::channel::<StopSignal>(1);
        // The process cannot listen for a signal.
        drop(signals);
        let ended = CancellationToken::new();
        let server = server_until(ended.clone(), Ok(()));
        let mut run = pin!(run_until_stopped(server, &cancel, &hurry, received));

        let waited = tokio::time::timeout(SHORT, &mut run).await;
        assert!(waited.is_err(), "the run ended while the server runs");
        ended.cancel();
        tokio::time::timeout(WAIT, &mut run)
            .await
            .expect("the run ends with the server")
            .expect("the command succeeds");
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn run_once_runs_its_step_once() {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let step = RunOnce::new({
            let runs = Arc::clone(&runs);
            move || {
                runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let clone = step.clone();

        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        step.run();
        clone.run();
        step.run();
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn run_once_makes_a_second_caller_wait_for_the_running_step() {
        use std::sync::mpsc::channel;

        let (entered, step_entered) = channel();
        let (release, released) = channel::<()>();
        let step = RunOnce::new(move || {
            entered.send(()).expect("the test listens");
            // The step runs until the test drops the sender.
            let _ = released.recv();
        });

        // The stop of the server runs the step.
        let first = std::thread::spawn({
            let step = step.clone();
            move || step.run()
        });
        step_entered.recv_timeout(WAIT).expect("the step starts");

        // The exit after a second signal asks for the step as well. It
        // returns only when the step has ended, so the process does not exit
        // during it.
        let (returned, second_returned) = channel();
        let second = std::thread::spawn(move || {
            step.run();
            returned.send(()).expect("the test listens");
        });
        assert!(second_returned.recv_timeout(SHORT).is_err());

        drop(release);
        second_returned
            .recv_timeout(WAIT)
            .expect("the second call returns when the step has ended");
        first.join().expect("the first call does not panic");
        second.join().expect("the second call does not panic");
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

    #[test]
    fn log_filter_is_the_variable_when_it_is_set() {
        assert_eq!(
            log_filter(Some("wn_server=debug".to_owned()), "info"),
            "wn_server=debug"
        );
    }

    #[test]
    fn log_filter_falls_back_to_the_config_level() {
        assert_eq!(log_filter(None, "warn"), "warn");
        assert_eq!(
            log_filter(Some(String::new()), "warn"),
            "warn",
            "an empty variable is unset"
        );
    }

    #[test]
    fn log_filter_parses_levels_and_targets() {
        for filter in [
            "info",
            "debug",
            "warn,wn_server=debug",
            "wn_server::api=trace",
        ] {
            parse_log_filter(filter).unwrap_or_else(|error| panic!("{filter}: {error}"));
        }
    }

    #[test]
    fn log_filter_that_does_not_parse_is_named() {
        let error = parse_log_filter("wn_server=loud").expect_err("an invalid level");
        assert!(matches!(error, CliError::LogFilter { .. }), "{error}");
        assert!(error.to_string().contains("wn_server=loud"), "{error}");
    }
}
