//! Configuration: the `config.toml` file with its defaults and validation, and
//! the data directory that holds it.

use std::env;
use std::ffi::OsStr;
use std::fmt::{self, Display};
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::ops::RangeInclusive;
use std::path::{self, Path, PathBuf};

use directories::ProjectDirs;
use serde::Deserialize;
use thiserror::Error;

/// The port of a config that does not set `server.port`.
pub const DEFAULT_PORT: u16 = 4200;

/// The environment variable that names the data directory.
pub const DATA_DIR_ENV: &str = "WN_SERVER_DATA_DIR";

/// The transcode bitrates a config may set, in kbps.
const BITRATE_KBPS: RangeInclusive<u32> = 32..=320;

/// The column at which a comment starts in the file `init` writes.
const COMMENT_COLUMN: usize = 32;

/// The longest DNS label, in bytes. `server.name` is one label, because it is
/// the DNS-SD instance name.
pub(crate) const LABEL_MAX_BYTES: usize = 63;

/// The `server.name` of a config that does not set the key. The name is the
/// same on every machine. No default is taken from the host name of the
/// machine.
pub const DEFAULT_NAME: &str = "wn-server";

/// A config or data directory that cannot be used.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file does not exist.
    #[error("config file {} does not exist; `wn-server init` creates it", .0.display())]
    NotFound(PathBuf),
    /// The config file exists and cannot be read.
    #[error("cannot read {}: {source}", path.display())]
    Read { path: PathBuf, source: io::Error },
    /// The config file is not valid TOML, has an unknown key, or has a value
    /// of the wrong type. The source names the line and the key. Its text ends
    /// with a line break, which the message leaves out.
    #[error("cannot parse {}: {}", path.display(), source.to_string().trim_end())]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    /// `library.folders` is empty.
    #[error("library.folders: at least one music folder is required")]
    NoFolders,
    /// A music folder that `init` was given does not exist.
    #[error("music folder {} does not exist", .0.display())]
    FolderMissing(PathBuf),
    /// A music folder that `init` was given names something that is not a
    /// directory.
    #[error("music folder {} is not a directory", .0.display())]
    FolderNotDirectory(PathBuf),
    /// A music folder that `init` was given exists and its metadata cannot
    /// be read.
    #[error("cannot read music folder {}: {source}", path.display())]
    FolderUnreadable { path: PathBuf, source: io::Error },
    /// A music folder path cannot be written to a TOML file.
    #[error("music folder {} is not valid Unicode", .0.display())]
    NonUnicodePath(PathBuf),
    /// A key has a value outside its allowed range.
    #[error("{key}: {reason}")]
    Invalid { key: &'static str, reason: String },
    /// A path cannot be made absolute.
    #[error("cannot make the path {} absolute: {source}", path.display())]
    Absolute { path: PathBuf, source: io::Error },
    /// No data directory was named and the platform has no default, because
    /// the home directory is unknown.
    #[error("no data directory: pass --data-dir or set {DATA_DIR_ENV}")]
    NoDataDir,
}

/// The contents of `config.toml`. A key that the file omits has its default.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
    pub library: Library,
    pub transcoding: Transcoding,
    pub discovery: Discovery,
}

/// The `[server]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    /// The address the HTTP listener binds.
    pub bind: IpAddr,
    /// The TCP port of the HTTP listener.
    pub port: u16,
    /// The instance name that discovery shows: 1 to 63 bytes.
    pub name: String,
    /// The log filter. `RUST_LOG` overrides it when set.
    pub log_level: String,
    /// The milliseconds that the requests in progress have to finish after
    /// the server is asked to stop, and that the running scan then has to
    /// stop. It is not a config key; tests set it.
    #[serde(skip)]
    pub shutdown_grace_ms: u64,
    /// The milliseconds that take the place of `shutdown_grace_ms` in a
    /// stop after a signal that gives the process a fixed time to exit. The
    /// close and the shutdown of a Windows console are such signals. It is
    /// not a config key; tests set it.
    #[serde(skip)]
    pub shutdown_grace_short_ms: u64,
    /// The number of original files that the responses of `stream` hold
    /// open at the same time. It is not a config key; tests set it.
    #[serde(skip)]
    pub max_streams: usize,
    /// The milliseconds a `stream` request for an original file waits for
    /// one of those files to be closed. It is not a config key; tests set
    /// it.
    #[serde(skip)]
    pub stream_permit_wait_ms: u64,
}

/// The `[auth]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    /// `true` turns authentication off.
    pub disabled_unsafe: bool,
}

/// The `[library]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Library {
    /// The music folders, as absolute paths. A folder need not exist:
    /// [`Library::unavailable_folders`] returns the ones that cannot be read
    /// now.
    pub folders: Vec<PathBuf>,
    /// Whether `run` starts a scan at startup.
    pub scan_on_start: bool,
    /// The minutes between periodic scans. 0 disables them.
    pub rescan_interval_minutes: u32,
    /// The milliseconds between two polls of a running scan. It is not a
    /// config key; tests set it.
    #[serde(skip)]
    pub scan_poll_ms: u64,
}

/// The `[transcoding]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Transcoding {
    /// The ffmpeg program name or path.
    pub ffmpeg: String,
    /// The number of transcodes that run at the same time.
    pub max_concurrent: usize,
    /// The Opus bitrate when the client names none, in kbps.
    pub opus_bitrate_kbps: u32,
    /// The MP3 bitrate when the client names none, in kbps.
    pub mp3_bitrate_kbps: u32,
    /// The milliseconds a transcode request waits for a free slot. It is not a
    /// config key; tests set it.
    #[serde(skip)]
    pub permit_wait_ms: u64,
    /// The milliseconds a transcode has to produce its first output. It is
    /// not a config key; tests set it.
    #[serde(skip)]
    pub first_output_wait_ms: u64,
    /// The milliseconds a chunk of a transcoded stream waits for the client
    /// to take the chunk before it. It is not a config key; tests set it.
    #[serde(skip)]
    pub write_timeout_ms: u64,
}

/// The `[discovery]` table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    /// Whether the server advertises itself over mDNS.
    pub mdns: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            name: DEFAULT_NAME.to_owned(),
            log_level: "info".to_owned(),
            shutdown_grace_ms: 10_000,
            shutdown_grace_short_ms: 1500,
            max_streams: 64,
            stream_permit_wait_ms: 5000,
        }
    }
}

impl Default for Library {
    fn default() -> Self {
        Self {
            folders: Vec::new(),
            scan_on_start: true,
            rescan_interval_minutes: 0,
            scan_poll_ms: 250,
        }
    }
}

impl Default for Transcoding {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".to_owned(),
            max_concurrent: 2,
            opus_bitrate_kbps: 192,
            mp3_bitrate_kbps: 320,
            permit_wait_ms: 5000,
            first_output_wait_ms: 10_000,
            write_timeout_ms: 600_000,
        }
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self { mdns: true }
    }
}

impl Config {
    /// Reads the config file at `path`. Absent keys get their defaults, the
    /// result is validated, and the folder paths are cleaned.
    ///
    /// A music folder that does not exist or cannot be read is no error
    /// here: the volume of the folder may be unplugged, or its network share
    /// may be down. `run` and `scan` start without it
    /// ([`Library::unavailable_folders`]).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ConfigError::NotFound(path.to_path_buf())
            } else {
                ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        let mut config = Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        config.validate()?;
        config.clean_folders()?;
        Ok(config)
    }

    /// Checks the rules a usable config meets: at least one folder, each
    /// folder is an absolute path that does not name a file, the port is not
    /// 0, the name is 1 to 63 bytes long, at least one transcode may run, and
    /// both bitrates are within 32 to 320 kbps. The error names the key of
    /// the first rule that fails.
    ///
    /// A folder that does not exist, and a folder whose state cannot be
    /// read, pass. Such a folder is unavailable now and may be back later.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.library.folders.is_empty() {
            return Err(ConfigError::NoFolders);
        }
        for folder in &self.library.folders {
            check_folder_value(folder)?;
        }
        if self.server.port == 0 {
            return Err(ConfigError::Invalid {
                key: "server.port",
                reason: "must not be 0".to_owned(),
            });
        }
        let name_bytes = self.server.name.len();
        if name_bytes == 0 || name_bytes > LABEL_MAX_BYTES {
            return Err(ConfigError::Invalid {
                key: "server.name",
                reason: format!(
                    "the name is {name_bytes} bytes long; set `name` in the `[server]` table \
                     of config.toml to a name of 1 to {LABEL_MAX_BYTES} bytes \
                     (`wn-server config path` prints the path of the file)"
                ),
            });
        }
        if self.transcoding.max_concurrent == 0 {
            return Err(ConfigError::Invalid {
                key: "transcoding.max_concurrent",
                reason: "must be at least 1".to_owned(),
            });
        }
        check_bitrate(
            "transcoding.opus_bitrate_kbps",
            self.transcoding.opus_bitrate_kbps,
        )?;
        check_bitrate(
            "transcoding.mp3_bitrate_kbps",
            self.transcoding.mp3_bitrate_kbps,
        )?;
        Ok(())
    }

    /// Renders the config as TOML with every key present. Folder paths are
    /// TOML literal strings, so a backslash in a Windows path is kept as
    /// written. A folder path with a single quote or a control character
    /// other than tab is written in another TOML string form and reads back
    /// unchanged.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        self.render(false)
    }

    /// Returns the commented `config.toml` that `init` writes: the defaults
    /// with `folders` and `port` set.
    pub fn template(folders: &[PathBuf], port: u16) -> Result<String, ConfigError> {
        let mut config = Self::default();
        config.library.folders = folders.to_vec();
        config.server.port = port;
        config.render(true)
    }

    /// Parses TOML text. Absent keys get their defaults. An unknown key is an
    /// error.
    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Replaces each folder path with the form that [`absolute`] gives it.
    /// The paths are absolute already ([`Config::validate`]), so nothing is
    /// resolved against the working directory. The call removes `.`
    /// components and repeated separators, and on Windows it writes every
    /// separator as `\`. Symlinks are not resolved.
    fn clean_folders(&mut self) -> Result<(), ConfigError> {
        for folder in &mut self.library.folders {
            *folder = absolute(folder)?;
        }
        Ok(())
    }

    /// Writes every table and key in the order of `docs/design.md` section
    /// 3.5. `comments` adds the explanation of each key that has one.
    fn render(&self, comments: bool) -> Result<String, ConfigError> {
        let folders = self
            .library
            .folders
            .iter()
            .map(|folder| path_value(folder))
            .collect::<Result<Vec<_>, _>>()?;
        let folders = format!("[{}]", folders.join(", "));

        let mut document = Document {
            text: String::new(),
            comments,
        };

        document.table("server");
        document.key("bind", string_value(&self.server.bind.to_string()), "");
        document.key("port", self.server.port, "");
        document.key(
            "name",
            string_value(&self.server.name),
            "instance name shown by discovery",
        );
        document.key(
            "log_level",
            string_value(&self.server.log_level),
            "RUST_LOG overrides when set",
        );

        document.table("auth");
        document.key(
            "disabled_unsafe",
            self.auth.disabled_unsafe,
            "true turns authentication off; unsafe",
        );

        document.table("library");
        document.key(
            "folders",
            folders,
            r"TOML literal strings; 'C:\Music' on Windows",
        );
        document.key("scan_on_start", self.library.scan_on_start, "");
        document.key(
            "rescan_interval_minutes",
            self.library.rescan_interval_minutes,
            "0 disables periodic rescans",
        );

        document.table("transcoding");
        document.key(
            "ffmpeg",
            string_value(&self.transcoding.ffmpeg),
            "program name or path",
        );
        document.key("max_concurrent", self.transcoding.max_concurrent, "");
        document.key(
            "opus_bitrate_kbps",
            self.transcoding.opus_bitrate_kbps,
            "used when format=opus and maxBitRate is absent",
        );
        document.key(
            "mp3_bitrate_kbps",
            self.transcoding.mp3_bitrate_kbps,
            "used when format=mp3 and maxBitRate is absent",
        );

        document.table("discovery");
        document.key("mdns", self.discovery.mdns, "");

        Ok(document.text)
    }
}

/// TOML text under construction.
struct Document {
    text: String,
    comments: bool,
}

impl Document {
    /// Starts a table. A blank line separates it from the table before it.
    fn table(&mut self, name: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(&format!("[{name}]\n"));
    }

    /// Writes one key. `value` is already TOML. A comment follows the value
    /// on the same line when comments are on and `comment` is not empty.
    fn key(&mut self, key: &str, value: impl Display, comment: &str) {
        let entry = format!("{key} = {value}");
        if self.comments && !comment.is_empty() {
            let width = COMMENT_COLUMN - 1;
            self.text
                .push_str(&format!("{entry:<width$} # {comment}\n"));
        } else {
            self.text.push_str(&entry);
            self.text.push('\n');
        }
    }
}

/// Renders `text` as a TOML string in the form the `toml` crate chooses.
fn string_value(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// Renders a folder path as a TOML literal string. A literal string cannot
/// hold a single quote or a control character other than tab, so a path with
/// one of those is rendered in the form the `toml` crate chooses.
fn path_value(path: &Path) -> Result<String, ConfigError> {
    let text = path
        .to_str()
        .ok_or_else(|| ConfigError::NonUnicodePath(path.to_path_buf()))?;
    let literal = !text
        .chars()
        .any(|c| c == '\'' || (c.is_control() && c != '\t'));
    if literal {
        Ok(format!("'{text}'"))
    } else {
        Ok(string_value(text))
    }
}

/// Makes `path` absolute without resolving symlinks. On Windows the result has
/// no `\\?\` prefix.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    path::absolute(path).map_err(|source| ConfigError::Absolute {
        path: path.to_path_buf(),
        source,
    })
}

/// Checks that the music folder `path` exists and is a directory. `init`
/// checks each `--folder` with it, because a path that is typed on the
/// command line and does not exist is most likely a typing mistake.
pub(crate) fn check_folder(path: &Path) -> Result<(), ConfigError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(ConfigError::FolderNotDirectory(path.to_path_buf())),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(ConfigError::FolderMissing(path.to_path_buf()))
        }
        Err(source) => Err(ConfigError::FolderUnreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Checks one value of `library.folders` when the config is loaded. The
/// value is malformed, and the error names the key, in two cases:
///
/// - The path is not absolute, or it is empty. A relative path names
///   another directory for each working directory, and the `folders` row of
///   a music folder is found by its path.
/// - The path names something that exists and is not a directory.
///
/// A path that does not exist and a path whose state cannot be read pass.
/// Both are the state of a folder whose volume is unplugged or whose network
/// share is down, and neither says that the value is wrong.
fn check_folder_value(path: &Path) -> Result<(), ConfigError> {
    let reason = if path.as_os_str().is_empty() {
        "a music folder is an empty path".to_owned()
    } else if !path.is_absolute() {
        format!(
            "the music folder {} is not an absolute path",
            path.display()
        )
    } else if fs::metadata(path).is_ok_and(|metadata| !metadata.is_dir()) {
        format!("the music folder {} is not a directory", path.display())
    } else {
        // A directory, a path that does not exist, or a path whose state
        // cannot be read.
        return Ok(());
    };
    Err(ConfigError::Invalid {
        key: "library.folders",
        reason,
    })
}

/// A music folder of the config that cannot be read now.
#[derive(Debug)]
pub struct UnavailableFolder {
    /// The path of the folder, as the config holds it.
    pub path: PathBuf,
    /// The error of the operating system for the folder: it does not exist,
    /// it cannot be listed, or it is not a directory.
    pub cause: io::Error,
}

impl Display for UnavailableFolder {
    /// Writes the path of the folder and the error of the operating system.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "music folder {} is unavailable: {}",
            self.path.display(),
            self.cause
        )
    }
}

impl Library {
    /// Returns the music folders that cannot be read now, in the order of
    /// the config: a folder that does not exist, and a folder that cannot be
    /// listed. The scanner treats each of them as an unread root
    /// (`docs/design.md` section 3.10). A path that the config names twice
    /// is returned once.
    ///
    /// The call opens each folder as the walk of the scanner does. It reads
    /// no entry.
    pub fn unavailable_folders(&self) -> Vec<UnavailableFolder> {
        let mut unavailable: Vec<UnavailableFolder> = Vec::new();
        for folder in &self.folders {
            if unavailable.iter().any(|known| known.path == *folder) {
                continue;
            }
            if let Err(cause) = fs::read_dir(folder) {
                unavailable.push(UnavailableFolder {
                    path: folder.clone(),
                    cause,
                });
            }
        }
        unavailable
    }

    /// Logs one warning for each music folder that cannot be read now, with
    /// its path and the error of the operating system. `run` and `scan` call
    /// it once, when they start.
    pub(crate) fn warn_unavailable_folders(&self) {
        for folder in self.unavailable_folders() {
            tracing::warn!(
                "{folder}. Its tracks stay in the library, and no scan deletes a track while \
                 the folder cannot be read"
            );
        }
    }
}

/// Checks that the bitrate under `key` is within the allowed range.
fn check_bitrate(key: &'static str, kbps: u32) -> Result<(), ConfigError> {
    if BITRATE_KBPS.contains(&kbps) {
        Ok(())
    } else {
        Err(ConfigError::Invalid {
            key,
            reason: format!(
                "{kbps} is outside {} to {}",
                BITRATE_KBPS.start(),
                BITRATE_KBPS.end()
            ),
        })
    }
}

/// The directory that holds the config file, the database, the instance ID,
/// the scan lock and the cover cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Resolves the data directory. The order is the `--data-dir` value in
    /// `flag`, then the `WN_SERVER_DATA_DIR` environment variable, then the
    /// platform default. An empty variable counts as unset. The result is an
    /// absolute path.
    pub fn resolve(flag: Option<&Path>) -> Result<Self, ConfigError> {
        let variable = env::var_os(DATA_DIR_ENV);
        Self::from_sources(flag, variable.as_deref(), platform_default())
    }

    /// Picks the first source that is present: `flag`, a non-empty
    /// `variable`, `default`.
    fn from_sources(
        flag: Option<&Path>,
        variable: Option<&OsStr>,
        default: Option<PathBuf>,
    ) -> Result<Self, ConfigError> {
        let variable = variable.filter(|value| !value.is_empty());
        let root = match (flag, variable) {
            (Some(flag), _) => flag.to_path_buf(),
            (None, Some(variable)) => PathBuf::from(variable),
            (None, None) => default.ok_or(ConfigError::NoDataDir)?,
        };
        Ok(Self {
            root: absolute(&root)?,
        })
    }

    /// The data directory itself.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// The config file, `config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The SQLite database, `library.db`.
    pub fn db_path(&self) -> PathBuf {
        self.root.join("library.db")
    }

    /// The file that holds the instance UUID, `instance.id`.
    pub fn instance_id_path(&self) -> PathBuf {
        self.root.join("instance.id")
    }

    /// The directory of resized covers, `cache/covers`.
    pub fn covers_cache_dir(&self) -> PathBuf {
        self.root.join("cache").join("covers")
    }

    /// The file that a process locks while it scans the library, `scan.lock`.
    /// The file has no content.
    pub fn scan_lock_path(&self) -> PathBuf {
        self.root.join("scan.lock")
    }
}

/// The data directory the platform assigns to the application:
/// `~/Library/Application Support/wn-server` on macOS,
/// `~/.local/share/wn-server` on Linux (or under `XDG_DATA_HOME` when set), and
/// `%APPDATA%\wn-server\data` on Windows. `None` when the home directory is
/// unknown.
fn platform_default() -> Option<PathBuf> {
    ProjectDirs::from("", "", "wn-server").map(|dirs| dirs.data_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Writes `text` as `config.toml` in a new temp directory.
    fn config_file(text: &str) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, text).expect("write config");
        (dir, path)
    }

    /// A config that passes validation: the defaults plus one existing folder.
    fn valid_config(folder: &Path) -> Config {
        let mut config = Config::default();
        config.library.folders = vec![folder.to_path_buf()];
        config
    }

    /// The key that a failed value rule names.
    fn invalid_key(config: &Config) -> &'static str {
        match config.validate() {
            Err(ConfigError::Invalid { key, .. }) => key,
            other => panic!("expected ConfigError::Invalid, got {other:?}"),
        }
    }

    #[test]
    fn defaults_match_the_design() {
        let config = Config::default();
        assert_eq!(config.server.bind.to_string(), "0.0.0.0");
        assert_eq!(config.server.port, 4200);
        assert_eq!(config.server.name, "wn-server");
        assert_eq!(config.server.log_level, "info");
        assert_eq!(config.server.shutdown_grace_ms, 10_000);
        assert_eq!(config.server.shutdown_grace_short_ms, 1500);
        assert!(!config.auth.disabled_unsafe);
        assert!(config.library.folders.is_empty());
        assert!(config.library.scan_on_start);
        assert_eq!(config.library.rescan_interval_minutes, 0);
        assert_eq!(config.library.scan_poll_ms, 250);
        assert_eq!(config.transcoding.ffmpeg, "ffmpeg");
        assert_eq!(config.transcoding.max_concurrent, 2);
        assert_eq!(config.transcoding.opus_bitrate_kbps, 192);
        assert_eq!(config.transcoding.mp3_bitrate_kbps, 320);
        assert_eq!(config.transcoding.permit_wait_ms, 5000);
        assert!(config.discovery.mdns);
    }

    #[test]
    fn the_default_name_is_wn_server() {
        assert_eq!(DEFAULT_NAME, "wn-server");
        assert_eq!(Server::default().name, "wn-server");
        assert_eq!(Config::default().server.name, "wn-server");
    }

    #[test]
    fn a_config_without_the_name_key_has_the_default_name() {
        let music = tempfile::tempdir().expect("temp dir");
        let folders = format!("[library]\nfolders = ['{}']\n", music.path().display());
        for text in [
            folders.clone(),
            format!("[server]\nport = 4321\n\n{folders}"),
        ] {
            let (_dir, path) = config_file(&text);
            let config = Config::load(&path).expect("load");
            assert_eq!(config.server.name, "wn-server", "{text}");
        }
    }

    #[test]
    fn the_template_that_init_writes_has_the_default_name() {
        let music = tempfile::tempdir().expect("temp dir");
        let text = Config::template(&[music.path().to_path_buf()], DEFAULT_PORT).expect("template");
        assert!(
            text.lines()
                .any(|line| line.starts_with("name = \"wn-server\" ")),
            "{text}"
        );
        let (_dir, path) = config_file(&text);
        assert_eq!(Config::load(&path).expect("load").server.name, "wn-server");
    }

    #[test]
    fn a_name_in_the_config_file_is_kept_as_written() {
        let music = tempfile::tempdir().expect("temp dir");
        // A name that is also the host name of a machine is kept like any
        // other name.
        for name in [
            "studio",
            "Adnans-MacBook-Pro",
            "wn-server",
            "Büro",
            "living room",
        ] {
            let (_dir, path) = config_file(&format!(
                "[server]\nname = \"{name}\"\n\n[library]\nfolders = ['{}']\n",
                music.path().display()
            ));
            let config = Config::load(&path).expect("load");
            assert_eq!(config.server.name, name);
        }
    }

    #[test]
    fn load_applies_defaults_to_absent_keys() {
        let music = tempfile::tempdir().expect("temp dir");
        let (_dir, path) = config_file(&format!(
            "[library]\nfolders = ['{}']\n",
            music.path().display()
        ));
        let config = Config::load(&path).expect("load");
        assert_eq!(config, valid_config(music.path()));
        assert_eq!(config.server.shutdown_grace_ms, 10_000);
        assert_eq!(config.server.shutdown_grace_short_ms, 1500);
        assert_eq!(config.library.scan_poll_ms, 250);
        assert_eq!(config.transcoding.permit_wait_ms, 5000);
    }

    #[test]
    fn template_round_trips_through_load() {
        let music = tempfile::tempdir().expect("temp dir");
        let folders = vec![music.path().to_path_buf()];
        let text = Config::template(&folders, 4321).expect("template");
        let (_dir, path) = config_file(&text);

        let config = Config::load(&path).expect("load");
        let mut expected = valid_config(music.path());
        expected.server.port = 4321;
        assert_eq!(config, expected);
    }

    #[test]
    fn template_is_the_commented_file_of_the_design() {
        let mut config = Config::default();
        config.server.name = "studio".to_owned();
        config.library.folders = vec![PathBuf::from("/Users/adnan/Music")];
        let expected = r#"[server]
bind = "0.0.0.0"
port = 4200
name = "studio"                 # instance name shown by discovery
log_level = "info"              # RUST_LOG overrides when set

[auth]
disabled_unsafe = false         # true turns authentication off; unsafe

[library]
folders = ['/Users/adnan/Music'] # TOML literal strings; 'C:\Music' on Windows
scan_on_start = true
rescan_interval_minutes = 0     # 0 disables periodic rescans

[transcoding]
ffmpeg = "ffmpeg"               # program name or path
max_concurrent = 2
opus_bitrate_kbps = 192         # used when format=opus and maxBitRate is absent
mp3_bitrate_kbps = 320          # used when format=mp3 and maxBitRate is absent

[discovery]
mdns = true
"#;
        assert_eq!(config.render(true).expect("render"), expected);
    }

    #[test]
    fn to_toml_has_the_same_lines_as_the_template_without_comments() {
        let config = valid_config(Path::new("/music"));
        let commented = config.render(true).expect("render");
        let stripped: Vec<&str> = commented
            .lines()
            .map(|line| line.split(" # ").next().expect("first part").trim_end())
            .collect();
        let plain = config.to_toml().expect("to_toml");
        assert_eq!(plain.lines().collect::<Vec<_>>(), stripped);
        assert!(!plain.contains('#'));
    }

    #[test]
    fn to_toml_round_trips_every_key() {
        let music = tempfile::tempdir().expect("temp dir");
        let other = tempfile::tempdir().expect("temp dir");
        // Every field is listed, so a new field does not compile until this
        // test sets it, and the comparison fails until `render` writes it.
        let config = Config {
            server: Server {
                bind: "::1".parse().expect("address"),
                port: 8080,
                name: "living \"room\"".to_owned(),
                log_level: "wn_server=debug".to_owned(),
                shutdown_grace_ms: 10_000,
                shutdown_grace_short_ms: 1500,
                max_streams: 64,
                stream_permit_wait_ms: 5000,
            },
            auth: Auth {
                disabled_unsafe: true,
            },
            library: Library {
                folders: vec![music.path().to_path_buf(), other.path().to_path_buf()],
                scan_on_start: false,
                rescan_interval_minutes: 90,
                scan_poll_ms: 250,
            },
            transcoding: Transcoding {
                ffmpeg: r"C:\Program Files\ffmpeg\bin\ffmpeg.exe".to_owned(),
                max_concurrent: 5,
                opus_bitrate_kbps: 96,
                mp3_bitrate_kbps: 128,
                permit_wait_ms: 5000,
                first_output_wait_ms: 10_000,
                write_timeout_ms: 600_000,
            },
            discovery: Discovery { mdns: false },
        };
        let text = config.to_toml().expect("to_toml");
        // The `toml` crate writes a string with a backslash as a literal
        // string, so the ffmpeg path keeps its backslashes as written.
        assert!(
            text.contains(r"ffmpeg = 'C:\Program Files\ffmpeg\bin\ffmpeg.exe'"),
            "{text}"
        );
        let (_dir, path) = config_file(&text);
        assert_eq!(Config::load(&path).expect("load"), config);
    }

    #[test]
    fn internal_timing_fields_are_not_rendered() {
        let mut config = valid_config(Path::new("/music"));
        config.server.shutdown_grace_ms = 30;
        config.server.shutdown_grace_short_ms = 40;
        config.library.scan_poll_ms = 10;
        config.transcoding.permit_wait_ms = 20;
        let text = config.to_toml().expect("to_toml");
        assert!(!text.contains("shutdown_grace_ms"));
        assert!(!text.contains("shutdown_grace_short_ms"));
        assert!(!text.contains("scan_poll_ms"));
        assert!(!text.contains("permit_wait_ms"));
    }

    #[test]
    fn folder_with_backslashes_is_a_literal_string() {
        let folders = vec![PathBuf::from(r"C:\Music"), PathBuf::from(r"D:\More\Music")];
        let text = Config::template(&folders, DEFAULT_PORT).expect("template");
        assert!(
            text.contains(r"folders = ['C:\Music', 'D:\More\Music']"),
            "{text}"
        );

        let config = Config::parse(&text).expect("parse");
        assert_eq!(config.library.folders, folders);
    }

    // Windows: `C:\Music` is absolute there, so it passes the check of a
    // folder value whether or not the directory exists, and the path that
    // `load` stores is the path the file names.
    #[cfg(windows)]
    #[test]
    fn windows_folder_survives_the_round_trip() {
        let folders = vec![PathBuf::from(r"C:\Music")];
        let text = Config::template(&folders, DEFAULT_PORT).expect("template");
        let mut config = Config::parse(&text).expect("parse");
        check_folder_value(&folders[0]).expect("an absolute path");
        config.clean_folders().expect("clean");
        assert_eq!(config.library.folders, folders);
    }

    #[test]
    fn folder_with_a_single_quote_round_trips() {
        let folders = vec![
            PathBuf::from("/music/Guns N' Roses"),
            PathBuf::from(r"C:\Music\Guns N' Roses"),
        ];
        let text = Config::template(&folders, DEFAULT_PORT).expect("template");
        // A literal string cannot hold a single quote. The first path becomes
        // a basic string. The second has backslashes too and becomes a
        // multi-line literal string, which keeps them as written.
        assert!(
            text.contains(r#"folders = ["/music/Guns N' Roses", '''C:\Music\Guns N' Roses''']"#),
            "{text}"
        );
        let config = Config::parse(&text).expect("parse");
        assert_eq!(config.library.folders, folders);
    }

    #[test]
    fn folder_with_a_control_character_round_trips() {
        let folders = vec![
            PathBuf::from("/music/bell\u{7}"),
            PathBuf::from("/music/two\nlines"),
            PathBuf::from("/music/tab\there"),
        ];
        let text = Config::template(&folders, DEFAULT_PORT).expect("template");
        // A tab is allowed in a literal string. The bell character is not,
        // and that path is a basic string with an escape. The `toml` crate
        // chooses the form of the path with a line break; the comparison
        // below checks that it reads back.
        assert!(text.contains(r#""/music/bell\u0007""#), "{text}");
        assert!(text.contains("'/music/tab\there'"), "{text}");
        let config = Config::parse(&text).expect("parse");
        assert_eq!(config.library.folders, folders);
    }

    // Unix: a path is a byte string there and need not be UTF-8. A Windows
    // path that is not valid Unicode needs an unpaired surrogate, which this
    // test does not build.
    #[cfg(unix)]
    #[test]
    fn folder_that_is_not_unicode_cannot_be_rendered() {
        use std::os::unix::ffi::OsStrExt;

        let folder = PathBuf::from(OsStr::from_bytes(b"/music/\xff"));
        let error = Config::template(&[folder], DEFAULT_PORT).expect_err("non-Unicode path");
        assert!(matches!(error, ConfigError::NonUnicodePath(_)), "{error}");
    }

    #[test]
    fn load_rejects_a_folder_that_is_not_an_absolute_path() {
        // Tests run with the package root as working directory, where `src`
        // exists. A relative path is malformed whether or not it names a
        // directory there.
        for folder in ["src", "absent", "./src", "../music"] {
            let (_dir, path) = config_file(&format!("[library]\nfolders = ['{folder}']\n"));
            let error = Config::load(&path).expect_err("a relative folder");
            assert_eq!(
                error.to_string(),
                format!("library.folders: the music folder {folder} is not an absolute path")
            );
        }
        let (_dir, path) = config_file("[library]\nfolders = ['']\n");
        let error = Config::load(&path).expect_err("an empty folder");
        assert_eq!(
            error.to_string(),
            "library.folders: a music folder is an empty path"
        );
    }

    #[test]
    fn load_cleans_an_absolute_folder_and_keeps_it_absolute() {
        let music = tempfile::tempdir().expect("temp dir");
        let spelled = music.path().join(".").join("albums");
        let (_dir, path) =
            config_file(&format!("[library]\nfolders = ['{}']\n", spelled.display()));
        let config = Config::load(&path).expect("load");
        // The `.` component is gone. The folder does not exist, which is no
        // error.
        let folder = &config.library.folders[0];
        assert_eq!(
            folder.as_os_str(),
            music.path().join("albums").as_os_str(),
            "{}",
            folder.display()
        );
    }

    #[test]
    fn load_names_a_missing_config_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let error = Config::load(&path).expect_err("missing file");
        assert!(matches!(error, ConfigError::NotFound(_)), "{error}");
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn unknown_key_fails_and_is_named() {
        let (_dir, path) = config_file("[server]\nprot = 4200\n");
        let error = Config::load(&path).expect_err("unknown key");
        assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
        assert!(error.to_string().contains("prot"), "{error}");
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn unknown_table_fails_and_is_named() {
        let (_dir, path) = config_file("[remote]\nenabled = true\n");
        let error = Config::load(&path).expect_err("unknown table");
        assert!(error.to_string().contains("remote"), "{error}");
    }

    #[test]
    fn internal_timing_fields_are_not_config_keys() {
        for text in [
            "[server]\nshutdown_grace_ms = 10\n",
            "[server]\nshutdown_grace_short_ms = 10\n",
            "[library]\nscan_poll_ms = 10\n",
            "[transcoding]\npermit_wait_ms = 10\n",
        ] {
            let (_dir, path) = config_file(text);
            let error = Config::load(&path).expect_err("internal field as key");
            assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn limits_of_stream_have_their_defaults_and_are_no_config_keys() {
        // The values of `docs/design.md` section 3.9.
        let config = Config::default();
        assert_eq!(config.server.max_streams, 64);
        assert_eq!(config.server.stream_permit_wait_ms, 5000);
        assert_eq!(config.transcoding.first_output_wait_ms, 10_000);
        assert_eq!(config.transcoding.write_timeout_ms, 600_000);

        // A config file gives them no other value, and a file that names
        // one is refused.
        let music = tempfile::tempdir().expect("temp dir");
        let (_dir, path) = config_file(&format!(
            "[library]\nfolders = ['{}']\n",
            music.path().display()
        ));
        let loaded = Config::load(&path).expect("load");
        assert_eq!(loaded.server.max_streams, 64);
        assert_eq!(loaded.server.stream_permit_wait_ms, 5000);
        assert_eq!(loaded.transcoding.first_output_wait_ms, 10_000);
        assert_eq!(loaded.transcoding.write_timeout_ms, 600_000);
        for text in [
            "[server]\nmax_streams = 1\n",
            "[server]\nstream_permit_wait_ms = 10\n",
            "[transcoding]\nfirst_output_wait_ms = 10\n",
            "[transcoding]\nwrite_timeout_ms = 10\n",
        ] {
            let (_dir, path) = config_file(text);
            let error = Config::load(&path).expect_err("internal field as key");
            assert!(matches!(error, ConfigError::Parse { .. }), "{error}");
            assert!(error.to_string().contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn limits_of_stream_are_not_rendered() {
        // `config show` prints `to_toml`, and `init` writes `render(true)`.
        let mut config = valid_config(Path::new("/music"));
        config.server.max_streams = 1;
        config.server.stream_permit_wait_ms = 50;
        config.transcoding.first_output_wait_ms = 30;
        config.transcoding.write_timeout_ms = 40;
        for text in [
            config.to_toml().expect("to_toml"),
            config.render(true).expect("render"),
        ] {
            for field in [
                "max_streams",
                "stream_permit_wait_ms",
                "first_output_wait_ms",
                "write_timeout_ms",
            ] {
                assert!(!text.contains(field), "{field}: {text}");
            }
        }
    }

    #[test]
    fn validate_accepts_the_defaults_with_one_folder() {
        let music = tempfile::tempdir().expect("temp dir");
        valid_config(music.path()).validate().expect("valid");
    }

    #[test]
    fn validate_requires_a_folder() {
        let error = Config::default().validate().expect_err("no folders");
        assert!(matches!(error, ConfigError::NoFolders), "{error}");
        assert!(error.to_string().contains("library.folders"));
    }

    #[test]
    fn validate_accepts_a_folder_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent");
        // The path of a volume that is not mounted, alone and next to a
        // folder that exists.
        valid_config(&missing)
            .validate()
            .expect("a missing folder is unavailable, not malformed");
        let mut config = valid_config(dir.path());
        config.library.folders.push(missing);
        config.validate().expect("one of two folders is missing");
    }

    #[test]
    fn validate_accepts_a_folder_below_a_path_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let below = dir.path().join("volume").join("music").join("albums");
        valid_config(&below).validate().expect("valid");
    }

    #[test]
    fn validate_rejects_a_folder_that_is_a_file_and_names_the_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("song.flac");
        fs::write(&file, b"").expect("write file");
        let config = valid_config(&file);
        assert_eq!(invalid_key(&config), "library.folders");
        assert_eq!(
            config.validate().expect_err("file as folder").to_string(),
            format!(
                "library.folders: the music folder {} is not a directory",
                file.display()
            )
        );
    }

    #[test]
    fn validate_rejects_a_relative_folder_and_names_the_key() {
        for folder in ["music", "./music", "../music"] {
            let config = valid_config(Path::new(folder));
            assert_eq!(invalid_key(&config), "library.folders", "{folder:?}");
            assert_eq!(
                config.validate().expect_err("relative folder").to_string(),
                format!("library.folders: the music folder {folder} is not an absolute path")
            );
        }
        assert_eq!(invalid_key(&valid_config(Path::new(""))), "library.folders");
    }

    // Windows: a path with a root and without a drive, and a path with a
    // drive and without a root, name another directory for each working
    // directory, so neither is absolute there.
    #[cfg(windows)]
    #[test]
    fn validate_rejects_a_windows_folder_without_a_drive_or_without_a_root() {
        for folder in [r"\Music", r"C:Music"] {
            assert_eq!(
                invalid_key(&valid_config(Path::new(folder))),
                "library.folders",
                "{folder}"
            );
        }
    }

    #[test]
    fn validate_checks_every_folder() {
        let music = tempfile::tempdir().expect("temp dir");
        let file = music.path().join("song.flac");
        fs::write(&file, b"").expect("write file");
        let mut config = valid_config(music.path());
        config.library.folders.push(music.path().join("absent"));
        config.library.folders.push(file.clone());
        let error = config.validate().expect_err("the third folder is a file");
        assert!(error.to_string().contains(&file.display().to_string()));

        config.library.folders[2] = PathBuf::from("relative");
        assert_eq!(invalid_key(&config), "library.folders");
    }

    #[test]
    fn unavailable_folders_are_the_folders_that_cannot_be_read_in_config_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (rock, jazz) = (dir.path().join("rock"), dir.path().join("jazz"));
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        fs::create_dir(&rock).expect("create a folder");
        fs::create_dir(&jazz).expect("create a folder");

        // The first folder is named twice.
        let mut config = valid_config(&rock);
        config.library.folders = vec![first.clone(), rock, second.clone(), jazz, first.clone()];
        config.validate().expect("valid");
        let unavailable = config.library.unavailable_folders();
        let paths: Vec<&Path> = unavailable
            .iter()
            .map(|folder| folder.path.as_path())
            .collect();
        assert_eq!(paths, [first.as_path(), second.as_path()]);
        for folder in &unavailable {
            assert_eq!(folder.cause.kind(), io::ErrorKind::NotFound, "{folder}");
            assert_eq!(
                folder.to_string(),
                format!(
                    "music folder {} is unavailable: {}",
                    folder.path.display(),
                    folder.cause
                )
            );
        }

        // Every folder is back.
        fs::create_dir(&first).expect("create a folder");
        fs::create_dir(&second).expect("create a folder");
        assert!(config.library.unavailable_folders().is_empty());
    }

    #[test]
    fn a_folder_that_became_a_file_after_the_load_is_unavailable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("music");
        let config = valid_config(&folder);
        config.validate().expect("valid");
        fs::write(&folder, b"").expect("write file");
        let unavailable = config.library.unavailable_folders();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].path, folder);
    }

    // File modes exist on Unix only.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_listed_passes_validation_and_is_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let folder = dir.path().join("music");
        fs::create_dir(&folder).expect("create a folder");
        let set_mode = |mode| {
            fs::set_permissions(&folder, fs::Permissions::from_mode(mode)).expect("set the mode");
        };
        set_mode(0o000);
        let config = valid_config(&folder);
        let validated = config.validate();
        let unavailable = config.library.unavailable_folders();
        // The directory gets its mode back before the assertions, so that
        // the temp directory can be removed when one fails.
        set_mode(0o755);

        validated.expect("a folder without the read permission is not malformed");
        if unavailable.is_empty() {
            // A process that runs as root lists the directory.
            eprintln!("not checked: the process lists a directory that has no read permission");
            return;
        }
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].path, folder);
        assert_eq!(unavailable[0].cause.kind(), io::ErrorKind::PermissionDenied);
        assert!(config.library.unavailable_folders().is_empty());
    }

    #[test]
    fn validate_rejects_port_zero() {
        let music = tempfile::tempdir().expect("temp dir");
        let mut config = valid_config(music.path());
        config.server.port = 0;
        assert_eq!(invalid_key(&config), "server.port");
    }

    #[test]
    fn validate_limits_the_server_name_to_1_to_63_bytes() {
        let music = tempfile::tempdir().expect("temp dir");
        let mut config = valid_config(music.path());
        // 22 characters of 3 bytes are 66 bytes.
        for name in [String::new(), "n".repeat(64), "音".repeat(22)] {
            config.server.name = name;
            assert_eq!(invalid_key(&config), "server.name");
        }
        for name in ["n".to_owned(), "n".repeat(63), "音".repeat(21)] {
            config.server.name = name;
            config.validate().expect("a name of 1 to 63 bytes is valid");
        }
    }

    #[test]
    fn validate_rejects_zero_concurrent_transcodes() {
        let music = tempfile::tempdir().expect("temp dir");
        let mut config = valid_config(music.path());
        config.transcoding.max_concurrent = 0;
        assert_eq!(invalid_key(&config), "transcoding.max_concurrent");
        config.transcoding.max_concurrent = 1;
        config.validate().expect("one transcode is valid");
    }

    #[test]
    fn validate_limits_the_opus_bitrate() {
        let music = tempfile::tempdir().expect("temp dir");
        let mut config = valid_config(music.path());
        for kbps in [0, 31, 321] {
            config.transcoding.opus_bitrate_kbps = kbps;
            assert_eq!(invalid_key(&config), "transcoding.opus_bitrate_kbps");
        }
        for kbps in [32, 320] {
            config.transcoding.opus_bitrate_kbps = kbps;
            config.validate().expect("bitrate at the limit is valid");
        }
    }

    #[test]
    fn validate_limits_the_mp3_bitrate() {
        let music = tempfile::tempdir().expect("temp dir");
        let mut config = valid_config(music.path());
        for kbps in [0, 31, 321] {
            config.transcoding.mp3_bitrate_kbps = kbps;
            assert_eq!(invalid_key(&config), "transcoding.mp3_bitrate_kbps");
        }
        for kbps in [32, 320] {
            config.transcoding.mp3_bitrate_kbps = kbps;
            config.validate().expect("bitrate at the limit is valid");
        }
    }

    #[test]
    fn load_reports_port_zero() {
        let music = tempfile::tempdir().expect("temp dir");
        let (_dir, path) = config_file(&format!(
            "[server]\nport = 0\n\n[library]\nfolders = ['{}']\n",
            music.path().display()
        ));
        let error = Config::load(&path).expect_err("port 0");
        assert_eq!(error.to_string(), "server.port: must not be 0");
    }

    #[test]
    fn load_reports_a_server_name_that_is_not_1_to_63_bytes_long() {
        let music = tempfile::tempdir().expect("temp dir");
        for (name, bytes) in [(String::new(), 0), ("n".repeat(64), 64)] {
            let (_dir, path) = config_file(&format!(
                "[server]\nname = \"{name}\"\n\n[library]\nfolders = ['{}']\n",
                music.path().display()
            ));
            let error = Config::load(&path).expect_err("a name outside 1 to 63 bytes");
            assert_eq!(
                error.to_string(),
                format!(
                    "server.name: the name is {bytes} bytes long; set `name` in the `[server]` \
                     table of config.toml to a name of 1 to 63 bytes (`wn-server config path` \
                     prints the path of the file)"
                )
            );
        }
    }

    #[test]
    fn load_accepts_a_folder_that_does_not_exist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("absent");
        let (_dir, path) = config_file(&format!(
            "[library]\nfolders = ['{}', '{}']\n",
            dir.path().display(),
            missing.display()
        ));
        let config = Config::load(&path).expect("a missing folder does not fail the load");
        assert_eq!(
            config.library.folders,
            [dir.path().to_path_buf(), missing.clone()]
        );
        let unavailable = config.library.unavailable_folders();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].path, missing);
    }

    #[test]
    fn load_rejects_an_empty_folder_list_and_a_folder_that_is_a_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("song.flac");
        fs::write(&file, b"").expect("write file");
        for (text, message) in [
            (
                "[library]\nfolders = []\n".to_owned(),
                "library.folders: at least one music folder is required".to_owned(),
            ),
            (
                "[server]\nport = 4321\n".to_owned(),
                "library.folders: at least one music folder is required".to_owned(),
            ),
            (
                format!("[library]\nfolders = ['{}']\n", file.display()),
                format!(
                    "library.folders: the music folder {} is not a directory",
                    file.display()
                ),
            ),
        ] {
            let (_dir, path) = config_file(&text);
            let error = Config::load(&path).expect_err("a malformed folder list");
            assert_eq!(error.to_string(), message, "{text}");
        }
    }

    #[test]
    fn data_dir_flag_wins_over_variable_and_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let flag = dir.path().join("flag");
        let variable = dir.path().join("variable");
        let default = dir.path().join("default");

        let resolved =
            DataDir::from_sources(Some(&flag), Some(variable.as_os_str()), Some(default))
                .expect("resolve");
        assert_eq!(resolved.path(), flag);
    }

    #[test]
    fn data_dir_variable_wins_over_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let variable = dir.path().join("variable");
        let default = dir.path().join("default");

        let resolved = DataDir::from_sources(None, Some(variable.as_os_str()), Some(default))
            .expect("resolve");
        assert_eq!(resolved.path(), variable);
    }

    #[test]
    fn data_dir_falls_back_to_the_default() {
        let dir = tempfile::tempdir().expect("temp dir");
        let default = dir.path().join("default");

        let resolved = DataDir::from_sources(None, None, Some(default.clone())).expect("resolve");
        assert_eq!(resolved.path(), default);

        let resolved = DataDir::from_sources(None, Some(OsStr::new("")), Some(default.clone()))
            .expect("resolve");
        assert_eq!(resolved.path(), default, "an empty variable is unset");
    }

    #[test]
    fn data_dir_without_any_source_is_an_error() {
        let error = DataDir::from_sources(None, None, None).expect_err("no source");
        assert!(matches!(error, ConfigError::NoDataDir), "{error}");
        assert!(error.to_string().contains(DATA_DIR_ENV));
    }

    #[test]
    fn data_dir_is_absolute() {
        let resolved =
            DataDir::from_sources(Some(Path::new("relative")), None, None).expect("resolve");
        let expected = env::current_dir()
            .expect("working directory")
            .join("relative");
        assert_eq!(resolved.path(), expected);
    }

    #[test]
    fn data_dir_names_its_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let data_dir = DataDir::from_sources(Some(dir.path()), None, None).expect("resolve");
        assert_eq!(data_dir.path(), dir.path());
        assert_eq!(data_dir.config_path(), dir.path().join("config.toml"));
        assert_eq!(data_dir.db_path(), dir.path().join("library.db"));
        assert_eq!(data_dir.instance_id_path(), dir.path().join("instance.id"));
        assert_eq!(
            data_dir.covers_cache_dir(),
            dir.path().join("cache").join("covers")
        );
        assert_eq!(data_dir.scan_lock_path(), dir.path().join("scan.lock"));
    }

    #[test]
    fn platform_default_ends_with_the_application_name() {
        // The exact path per OS is checked in `tests/cli.rs`, which runs the
        // binary with a controlled environment.
        if let Some(path) = platform_default() {
            assert!(path.is_absolute());
            assert!(
                path.components()
                    .any(|part| part.as_os_str() == "wn-server"),
                "{}",
                path.display()
            );
        }
    }
}
