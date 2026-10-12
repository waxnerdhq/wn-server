# wn-server

wn-server is a headless music server for OpenSubsonic clients. The design is described in [docs/design.md](docs/design.md).

## Commands

| Command | Effect |
|---|---|
| `wn-server init [--folder PATH]... [--port N] [--username NAME] [--password-stdin]` | Creates the data directory, `config.toml`, `instance.id`, the database and the owner account. With a `config.toml` and no owner account, keeps `config.toml` and creates what is missing. |
| `wn-server run` | Runs the server in the foreground until Ctrl-C. |
| `wn-server user show` | Prints the name, the role and the creation date of the owner account. |
| `wn-server user set-password [--username NAME] [--password-stdin]` | Stores a new password for the owner account, or for the account `--username` names. |
| `wn-server config path` | Prints the path of the config file. |
| `wn-server config show` | Prints the effective config: the values of the file and the default of every key the file omits. |
| `wn-server --version` | Prints the version. |

`scan` prints `not implemented yet` and exits with status 2.

Every command accepts `--data-dir DIR` before or after the subcommand. A command that fails prints `wn-server: <error>` to stderr and exits with status 1. A usage error exits with status 2.

## Data directory

The data directory holds `config.toml`, the database `library.db` and `instance.id`. The first of these that is present names it:

1. The `--data-dir` option.
2. The `WN_SERVER_DATA_DIR` environment variable. An empty value counts as unset.
3. The platform default: `~/Library/Application Support/wn-server` on macOS, `~/.local/share/wn-server` on Linux (`$XDG_DATA_HOME/wn-server` when that variable is set), `%APPDATA%\wn-server\data` on Windows.

`wn-server config path` prints the config path that results.

## Setup

```
wn-server init --folder /path/to/music
```

`init` checks that each `--folder` is an existing directory, creates the data directory and writes `config.toml` with every key. `--folder` may be repeated. `--port` sets `server.port`. `init` then writes `instance.id`, creates the database `library.db` and creates the owner account.

- Folders are written as absolute paths.
- An existing `config.toml` is not overwritten. When `library.db` has an owner account as well, `init` fails and changes nothing. When `library.db` is missing or has no owner account, `init` creates what is missing (see [Owner account](#owner-account)).
- An existing owner account is not replaced. `init` fails when `library.db` already has one, and removes the `config.toml` and the `instance.id` it wrote in that run.
- `instance.id` holds a random UUID that identifies this server to clients on the network. An existing `instance.id` is kept.
- Without `--folder`, `init` writes `folders = []`. A folder must be added to the file before another command loads it.
- On Unix, `init` creates the data directory with mode 0700 and `library.db` with mode 0600. It does not change the mode of a directory that already exists.

## Owner account

The server has one account, the owner. A client signs in with the name and the password of that account. `--username` sets the name at `init`. The default name is `owner`.

`init` takes the password from the first of these that is present:

1. The `--password-stdin` option. The password is the first line of stdin, without the line ending. An empty line is an error.
2. The `WN_SERVER_PASSWORD` environment variable. An empty value counts as unset.
3. Neither. `init` generates a password and prints it once:

```
Password for owner (shown once): xxxxx-xxxxx-xxxxx-xxxxx
```

A generated password is four groups of five characters from the lower-case letters without `i`, `l` and `o` and the digits `2` to `9`. No command shows a password again. A password that the owner supplied is not printed. There is no interactive prompt.

```
wn-server init --folder /path/to/music --password-stdin < password-file
```

`wn-server user set-password` stores a new password. It takes the password from the same three sources and prints a generated password once. `--username` names the account. Without it the account is the owner.

`wn-server user show` prints the name, the role and the creation date of the owner account. The date is in UTC.

The `user` commands need `library.db` and its owner account, and only `init` creates them. When `config.toml` exists and `library.db` is missing or has no owner account, `wn-server init` creates what is missing: the database, the owner account and `instance.id`. It leaves `config.toml` as it is. `--folder` and `--port` are errors in that case, because `init` does not change an existing `config.toml`; edit the file to change them. `init` takes the password from the same three sources, and `--username` names the account. A database that exists keeps everything else in it. A database that `init` creates is empty; nothing of a lost database is restored.

The password is stored in plain text in `library.db`. Subsonic token authentication needs the password itself, so the server cannot store a hash. Anyone who can read the data directory can read the password. Do not use a password that protects anything else. On Windows the access control list of the user profile protects the data directory.

## Configuration

`config.toml` is a TOML file. A key that the file omits has its default. An unknown key is an error.

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | `"0.0.0.0"` | IP address the server listens on |
| `server.port` | `4200` | TCP port; not 0 |
| `server.name` | the host name | instance name shown by discovery |
| `server.log_level` | `"info"` | log filter; `RUST_LOG` overrides it when set |
| `auth.disabled_unsafe` | `false` | `true` turns authentication off; unsafe, see [Authentication](#authentication) |
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

## Running

```
wn-server run
```

`run` loads `config.toml`, listens on `server.bind` and `server.port` (`0.0.0.0:4200` by default) and logs the address. Ctrl-C stops the server. Requests that are in progress are answered first.

`run` opens `library.db` and needs the owner account in it. `run` creates neither and fails at startup when one is missing. `wn-server init` then creates what is missing and keeps `config.toml` (see [Owner account](#owner-account)).

Logs go to stderr. `server.log_level` is the log filter, for example `info` or `warn,wn_server=debug`. The `RUST_LOG` environment variable replaces it when it is set and not empty. The log names the method, the client (`c`) and the user (`u`) of a request, each cut to 64 characters. It never holds the query string or the form body.

## Authentication

Every API method needs a user, except `getOpenSubsonicExtensions`. The server checks the request before the method runs. A client sends the name of the owner account in `u` and one of these credentials:

- `p`: the password, as it is or as `enc:` followed by its hex encoding.
- `t` and `s`: a token and its salt. The token is the MD5 digest of the password followed by the salt, in lower-case hex.

`v` and `c` are required. The value of `v` is not compared. The name in `u` is matched without regard to ASCII case.

| Request | Error |
|---|---|
| `v`, `c` or `u` is absent, or the request has no credential, or it has `t` without `s` or `s` without `t` | 10, naming the parameter |
| the user is unknown, or the password or the token is wrong | 40 |
| `apiKey` alone; the server has no API keys | 42 |
| `apiKey` with `u`, `p`, `t` or `s`, or `p` with `t` or `s` | 43 |

The response does not tell an unknown user from a wrong password. A failed attempt is logged at the `info` level with the values of `u` and `c`, each cut to 64 characters, and the error code. A password, a token or a salt is never logged and is never part of an error message.

`auth.disabled_unsafe = true` turns authentication off. This is unsafe: every request that has `v` and `c` then acts as the owner without a password, for anyone who can reach the port. `run` logs a warning at startup while the setting is on.

## API

The server answers OpenSubsonic requests at `/rest/<method>` and `/rest/<method>.view`, for GET and POST. A POST may carry parameters in an `application/x-www-form-urlencoded` body of up to 8 MiB, the credentials included; `getOpenSubsonicExtensions` lists this as the `formPost` extension. When a parameter is in the query string and in the body, the values of the query string come first. Any other path returns HTTP 404.

- `f=json` selects JSON. `f=xml` or no `f` selects XML. Any other value, `jsonp` included, returns error 0 in XML.
- An API error is an HTTP 200 response with the status `failed` and an `error` element that holds the Subsonic error code and a message.
- The check of the user comes before the method. A request without a user gets the authentication error from every method except `getOpenSubsonicExtensions`, also from a method that the server does not serve.
- A method of the OpenSubsonic API that the server does not serve returns error 0 with the message `not supported`. A name that is not an OpenSubsonic method returns error 0 with the message `unknown endpoint`.

## Discovery

While `wn-server run` is up, it advertises the server on the local network over mDNS (DNS-SD), so that a client finds the server without its address. `discovery.mdns = false` turns the advertisement off.

- Service type: `_waxnerd._tcp` in the domain `local.`
- Instance name: `server.name`. The mDNS responder appends ` (2)` when another server on the network has the same name.
- Host: the first label of the system host name, followed by `.local.`. Port: `server.port`.

The advertised addresses follow `server.bind`:

| `server.bind` | Advertised addresses |
|---|---|
| `0.0.0.0` | every IPv4 address of the machine |
| `::` | every IPv4 and IPv6 address of the machine |
| another address | that address only, on the network it belongs to |

A server on `127.0.0.1` is advertised to clients on the same machine only. On Windows a listener on `::` accepts IPv6 connections only, so the IPv4 addresses that a server on `::` advertises do not accept connections there.

The TXT record holds these keys, in this order:

| Key | Value |
|---|---|
| `txtvers` | `1`, the version of this set of keys |
| `api` | `opensubsonic` |
| `path` | `/rest` |
| `id` | the UUID in `instance.id`: 36 characters, lower-case hex digits in groups of 8, 4, 4, 4 and 12, separated by hyphens |
| `name` | `server.name` |
| `auth` | `password`, or `none` when `auth.disabled_unsafe = true` |
| `tls` | `0` |
| `ver` | the Cargo version of the server, for example `0.1.0`, or `0.2.0-beta.1` for a pre-release |

Every device on the network can read the record. It holds no password and no account name. Section 3.12 of [docs/design.md](docs/design.md) states what a client does with each key.

`id` identifies the server to clients. It stays the same across restarts and renames. `run` writes a new `instance.id` when the file is missing and `library.db` exists, and logs one warning. Clients then see a new server.

The server runs without the advertisement when the advertisement cannot start, and logs one warning that names the cause: `instance.id` cannot be read or does not hold a UUID, `server.name` is empty or longer than 63 bytes, or nothing was announced within 10 seconds. `RUST_LOG=info,mdns_sd=debug` shows the messages of the mDNS responder.

Ctrl-C withdraws the advertisement. On macOS, `dns-sd -B _waxnerd._tcp` lists the servers on the network.

On Windows, Windows Firewall prompts on the first start. The server needs TCP port 4200 (`server.port`) for the API and UDP port 5353 for mDNS.
