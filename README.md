# wn-server

wn-server is a headless music server for OpenSubsonic clients. The design is described in [docs/design.md](docs/design.md).

## Commands

| Command | Effect |
|---|---|
| `wn-server init [--folder PATH]... [--port N]` | Creates the data directory and writes `config.toml`. |
| `wn-server config path` | Prints the path of the config file. |
| `wn-server config show` | Prints the effective config: the values of the file and the default of every key the file omits. |
| `wn-server --version` | Prints the version. |

`run`, `scan`, `user show` and `user set-password` print `not implemented yet` and exit with status 2.

Every command accepts `--data-dir DIR` before or after the subcommand. A command that fails prints `wn-server: <error>` to stderr and exits with status 1. A usage error exits with status 2.

## Data directory

The data directory holds `config.toml`. The first of these that is present names it:

1. The `--data-dir` option.
2. The `WN_SERVER_DATA_DIR` environment variable. An empty value counts as unset.
3. The platform default: `~/Library/Application Support/wn-server` on macOS, `~/.local/share/wn-server` on Linux (`$XDG_DATA_HOME/wn-server` when that variable is set), `%APPDATA%\wn-server\data` on Windows.

`wn-server config path` prints the config path that results.

## Setup

```
wn-server init --folder /path/to/music
```

`init` checks that each `--folder` is an existing directory, creates the data directory and writes `config.toml` with every key. `--folder` may be repeated. `--port` sets `server.port`.

- Folders are written as absolute paths.
- An existing `config.toml` is not overwritten. `init` fails and leaves the file as it is.
- Without `--folder`, `init` writes `folders = []`. A folder must be added to the file before another command loads it.
- On Unix, `init` creates the data directory with mode 0700. It does not change the mode of a directory that already exists.

## Configuration

`config.toml` is a TOML file. A key that the file omits has its default. An unknown key is an error.

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | `"0.0.0.0"` | IP address the server listens on |
| `server.port` | `4200` | TCP port; not 0 |
| `server.name` | the host name | instance name shown by discovery |
| `server.log_level` | `"info"` | log filter; `RUST_LOG` overrides it when set |
| `auth.disabled_unsafe` | `false` | `true` turns authentication off |
| `library.folders` | `[]` | music folders; at least one, and each is an existing directory |
| `library.scan_on_start` | `true` | scan the folders when the server starts |
| `library.rescan_interval_minutes` | `0` | minutes between periodic scans; 0 disables them |
| `transcoding.ffmpeg` | `"ffmpeg"` | ffmpeg program name or path |
| `transcoding.max_concurrent` | `2` | transcodes that run at the same time; at least 1 |
| `transcoding.opus_bitrate_kbps` | `192` | Opus bitrate when the client names none; 32 to 320 |
| `transcoding.mp3_bitrate_kbps` | `320` | MP3 bitrate when the client names none; 32 to 320 |
| `discovery.mdns` | `true` | advertise the server over mDNS |

The file is checked when a command loads it. An error names the key or the folder path.

A folder path is written as a TOML literal string, in single quotes, so that a backslash in a Windows path is kept as written:

```toml
[library]
folders = ['C:\Music', 'D:\More Music']
```

A relative folder path resolves against the working directory of the command.
