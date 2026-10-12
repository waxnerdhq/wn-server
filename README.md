# wn-server

wn-server is a headless music server for OpenSubsonic clients. The design is described in [docs/design.md](docs/design.md).

## Commands

| Command | Effect |
|---|---|
| `wn-server init [--folder PATH]... [--port N] [--username NAME] [--password-stdin]` | Creates the data directory, `config.toml`, `instance.id`, the database and the owner account. With a `config.toml` and no owner account, keeps `config.toml` and creates what is missing. |
| `wn-server run` | Runs the server in the foreground until Ctrl-C. |
| `wn-server scan [--full]` | Runs one scan of the music folders and prints its summary. `--full` reads the tags of every file again. See [Scanning](#scanning). |
| `wn-server user show` | Prints the name, the role and the creation date of the owner account. |
| `wn-server user set-password [--username NAME] [--password-stdin]` | Stores a new password for the owner account, or for the account `--username` names. |
| `wn-server config path` | Prints the path of the config file. |
| `wn-server config show` | Prints the effective config: the values of the file and the default of every key the file omits. |
| `wn-server --version` | Prints the version. |

Every command accepts `--data-dir DIR` before or after the subcommand. A command that fails prints `wn-server: <error>` to stderr and exits with status 1. A usage error exits with status 2.

## Data directory

The data directory holds `config.toml`, the database `library.db`, `instance.id` and the empty file `scan.lock` (see [Scanning](#scanning)). The first of these that is present names it:

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
| `server.name` | `"wn-server"` | instance name shown by discovery; 1 to 63 bytes |
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

The default `server.name` is `wn-server` on every machine. `init` writes it to `config.toml`. Edit `name` in the `[server]` table to give the server another name, for example when two servers run on one network. A name that the file holds is kept as written. The server takes no name from the host name of the machine.

`server.name` holds 1 to 63 bytes of UTF-8. A character outside ASCII takes 2 to 4 bytes. A command that loads the file fails when the name is empty or longer, and the error names `server.name` and the length of the name. Set `name` in the `[server]` table to a name within the limit, or remove the key to get the default. `wn-server config path` prints the path of the file.

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

`run` stores the music folders of `config.toml` in the database before it answers the first request, and then scans them in the background (see [Scanning](#scanning)). The server answers requests while it scans.

Logs go to stderr. `server.log_level` is the log filter, for example `info` or `warn,wn_server=debug`. The `RUST_LOG` environment variable replaces it when it is set and not empty. The log names the method, the client (`c`) and the user (`u`) of a request, each cut to 64 characters. It never holds the query string or the form body.

## Scanning

A scan reads the music folders into the database. It reads the files that are new or have changed. A full scan reads the tags of every file again. Four things start a scan:

- `wn-server run` at startup, unless `library.scan_on_start = false`.
- `wn-server run` every `library.rescan_interval_minutes`, when that key is not 0. The default is 0, so there are no periodic scans. The first one starts one interval after the server started.
- The API method `startScan`. `fullScan=true` asks for a full scan.
- The command `wn-server scan`. `--full` asks for a full scan.

One scan of a library runs at a time. Whatever asks for a scan while one runs gets none, and no scan is queued:

- The server skips a periodic scan while it scans. The next one starts one interval later.
- `startScan` during a scan of the server starts no second scan and answers with the status of the running one. A `fullScan=true` that arrives then has no effect; send it again after the scan.
- `wn-server scan` prints an error and exits with status 1 while the server scans, and while another `wn-server scan` runs. It scans nothing and does not wait.
- While `wn-server scan` scans, the server starts no scan: it skips the scan at startup and the periodic scans with a warning in the log, and `startScan` returns error 0 with the message `another process is scanning the library`.

`wn-server scan` works with and without a running server. It needs `config.toml` and `library.db`, and scans in its own process. A server that runs and does not scan keeps answering requests during the scan and shows the new tracks as the scan stores them. The process that scans holds a lock on the file `scan.lock` in the data directory. The file is empty, and the lock ends with the process.

`wn-server scan` prints the summary of its scan and then exits:

```
seen: 10
added: 10
changed: 0
moved: 0
removed: 0
albums: 4
artists: 3
errors: 0
```

`seen` is the number of supported audio files. `added`, `changed`, `moved` and `removed` count tracks. `albums` and `artists` are the numbers in the library after the scan. Each error of the scan follows as a line `error: <message>` that names the folder, the path or the file. The command exits with status 0, or with status 1 when the scan recorded an error. Warnings about files that the scan leaves for the next scan go to stderr with the log. A music folder of `config.toml` that does not exist is an error of the config: the command names the folder and exits with status 1 before it scans.

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

`auth.disabled_unsafe = true` turns authentication off. This is unsafe: every request that has `v` and `c` then acts as the owner without a password, for anyone who can reach the port. That includes any web page that is open in a browser on the network: every response carries `Access-Control-Allow-Origin: *` (see [API](#api)), so the browser lets the page read the responses. Such a page can then use the server as the owner. It can read and stream the library, and it can call every method, including the methods that change state: stars, ratings, playlists and scans. `run` logs a warning at startup while the setting is on.

## API

The server answers OpenSubsonic requests at `/rest/<method>` and `/rest/<method>.view`, for GET and POST. A POST may carry parameters in an `application/x-www-form-urlencoded` body of up to 8 MiB, the credentials included; `getOpenSubsonicExtensions` lists this as the `formPost` extension. When a parameter is in the query string and in the body, the values of the query string come first. Any other path returns HTTP 404.

- `f=json` selects JSON. `f=xml` or no `f` selects XML. Any other value, `jsonp` included, returns error 0 in XML.
- An API error is an HTTP 200 response with the status `failed` and an `error` element that holds the Subsonic error code and a message.
- The check of the user comes before the method. A request without a user gets the authentication error from every method except `getOpenSubsonicExtensions`, also from a method that the server does not serve.
- A method of the OpenSubsonic API that the server does not serve returns error 0 with the message `not supported`. A name that is not an OpenSubsonic method returns error 0 with the message `unknown endpoint`.
- `download` is disabled. A request with a user gets error 50 with the message `download is disabled on this server`, whatever its parameters. No request gets the bytes of a file. `getUser` and `getUsers` report `downloadRole` as `false`.
- `getUser` needs `username`. The value must be the name of the user of the request, in any letter case; another name returns error 50. With authentication off, every name returns the owner. `getUsers` lists the owner. The user has `scrobblingEnabled`, `adminRole`, `playlistRole` and `streamRole` as `true`, and every other role is `false`. `folder` holds the IDs of the music folders.
- `startScan` starts a scan in the background and returns `scanStatus` with `scanning` and `count`, the number of audio files the scan has finished with. `getScanStatus` returns the same for the scan of the server that runs or ran last, and adds `lastScan`, the end time of the latest scan in UTC (`2026-10-11T08:15:30Z`). `lastScan` is not part of OpenSubsonic and is absent until a scan has ended. `scanning` is `true` only for a scan of the server itself, not for one that `wn-server scan` runs. A scan that recorded errors is not an API error: the errors are in the log. See [Scanning](#scanning).
- The server has no artist or album information from other services, no similar or top songs, no list of what is playing, and no shares, podcasts, internet radio stations or bookmarks. `getArtistInfo`, `getArtistInfo2`, `getAlbumInfo`, `getAlbumInfo2`, `getSimilarSongs`, `getSimilarSongs2`, `getTopSongs`, `getNowPlaying`, `getShares`, `getPodcasts`, `getNewestPodcasts`, `getInternetRadioStations` and `getBookmarks` return an empty result. `getAvatar` returns error 70.
- `createUser`, `updateUser`, `deleteUser` and `changePassword` return error 50. The `wn-server user` commands manage the owner account.

A client that runs in a browser calls the server from a page of another origin. The server sends the CORS headers that a browser needs for that:

- Every response at `/rest/<method>` and `/rest/<method>.view` carries `Access-Control-Allow-Origin: *`. That includes an API error and an authentication error.
- An `OPTIONS` request to these paths is answered as a preflight request. It needs no user and no parameter. The answer is HTTP 204 with `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Content-Type, Range` and `Access-Control-Max-Age: 86400`.
- Every other response at these paths carries `Access-Control-Expose-Headers: Content-Range, Accept-Ranges, Content-Length`.
- No response carries `Access-Control-Allow-Credentials`. The credentials are request parameters, and the server sets no cookie.
- The server has no list of allowed origins. A response for any other path carries none of these headers.

## Discovery

While `wn-server run` is up, it advertises the server on the local network over mDNS (DNS-SD), so that a client finds the server without its address. `discovery.mdns = false` turns the advertisement off.

- Service type: `_waxnerd._tcp` in the domain `local.`
- Instance name: `server.name`. The mDNS responder appends ` (2)` when another server on the network has the same name.
- Host: `wn-server-<hex>.local.`, where `<hex>` is the first 8 hex digits of the UUID in `instance.id`. Port: `server.port`.

The host is a name of the server, not of the machine. The server publishes its address records under this name only. It never uses the host name of the machine, because a second mDNS responder for that name makes the operating system rename the machine. A client takes the host from the SRV record of the instance and resolves it to the addresses of the server.

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

`id` identifies the server to clients. It stays the same across restarts and renames. `run` writes a new `instance.id` when the file is missing and `library.db` exists, and logs one warning. Clients then see a new server, and the host of the advertisement changes with the ID.

The server runs without the advertisement when the advertisement cannot start, and logs one warning that names the cause: `instance.id` cannot be read or does not hold a UUID, or nothing was announced within 10 seconds. `RUST_LOG=info,mdns_sd=debug` shows the messages of the mDNS responder.

Ctrl-C withdraws the advertisement. On macOS, `dns-sd -B _waxnerd._tcp` lists the servers on the network.

On Windows, Windows Firewall prompts on the first start. The server needs TCP port 4200 (`server.port`) for the API and UDP port 5353 for mDNS.
