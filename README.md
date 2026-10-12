# wn-server

wn-server is a headless music server for the local network. It scans the music folders of one machine into a SQLite database and serves the library to OpenSubsonic clients over HTTP. It is one program with a command line and a TOML configuration file, and it runs on Linux, macOS and Windows.

This file describes how to install, configure and run the server, and what a client sees of it. [docs/design.md](docs/design.md) describes the design.

## Requirements

To build the server:

- Rust 1.98.1. `rust-toolchain.toml` pins that version for a toolchain that `rustup` manages.
- A C compiler. The build compiles the SQLite library that the server contains.
  - Linux: a C compiler such as `gcc`.
  - macOS: the Xcode command line tools.
  - Windows: the Visual Studio Build Tools for the MSVC toolchain of Rust, or mingw-w64 for the GNU toolchain.

To run the server:

- No other program. The server contains its database engine.
- For transcoding only: ffmpeg with the encoders libopus and libmp3lame. The server runs the program `ffmpeg` from `PATH`, which is `ffmpeg.exe` on Windows, or the program that `transcoding.ffmpeg` names. Without ffmpeg the server serves the original files (see [Streaming and transcoding](#streaming-and-transcoding)).

## Install

The server is built from source. No binary release exists.

```
git clone https://github.com/waxnerdhq/wn-server.git
cd wn-server
cargo install --locked --path .
```

`cargo install` builds the program and copies it to the `bin` directory of cargo: `~/.cargo/bin/wn-server`, or `%USERPROFILE%\.cargo\bin\wn-server.exe` on Windows. `--locked` takes the versions of the dependencies from `Cargo.lock`. `wn-server --version` prints the version.

`cargo build --release --locked` builds the same program and leaves it at `target/release/wn-server`. The program needs no other file of the source tree.

## First run

### 1. Create the data directory and the owner account

```
wn-server init --folder /path/to/music
```

On Windows:

```
wn-server init --folder C:\Music
```

`init` creates the [data directory](#data-directory), writes `config.toml` with the music folder, and creates the database and the owner account. The owner is the one account of the server. `--folder` may be repeated. `init` generates the password of the owner and prints it once:

```
wrote /home/user/.local/share/wn-server/config.toml
Password for owner (shown once): xxxxx-xxxxx-xxxxx-xxxxx
The password is stored in plain text in the data directory. Do not reuse it elsewhere.
```

`wn-server user set-password` sets a new password.

The password is stored in plain text in the data directory; anyone who can read that directory can read it, so do not reuse it elsewhere. Subsonic token authentication needs the password itself, so the server cannot store a hash.

`init` takes the password from the first of these that is present:

1. The `--password-stdin` option. The password is the first line of stdin, without the line ending. An empty line is an error.
2. The `WN_SERVER_PASSWORD` environment variable. An empty value counts as unset.
3. Neither. `init` generates a password and prints it once, as above.

A generated password is four groups of five characters from the lower-case letters without `i`, `l` and `o` and the digits `2` to `9`. No command shows a password again. A password that the owner supplied is not printed. There is no interactive prompt.

```
wn-server init --folder /path/to/music --password-stdin < password-file
```

### 2. Start the server

```
wn-server run
```

`run` listens on port 4200 of every IPv4 address of the machine. It scans the music folders in the background and answers requests while it scans. It runs in the foreground and logs to stderr. Ctrl-C stops it (see [Running](#running)). On Windows, Windows Firewall prompts on the first start.

### 3. Check that the server answers

Put the password in the place of `PASSWORD`:

```
curl "http://localhost:4200/rest/ping?u=owner&p=PASSWORD&v=1.16.1&c=curl&f=json"
```

```
{"subsonic-response":{"status":"ok","version":"1.16.1","type":"wn-server","serverVersion":"0.1.0","openSubsonic":true}}
```

`getScanStatus` shows the scan of the music folders:

```
curl "http://localhost:4200/rest/getScanStatus?u=owner&p=PASSWORD&v=1.16.1&c=curl&f=json"
```

```
{"subsonic-response":{"status":"ok","version":"1.16.1","type":"wn-server","serverVersion":"0.1.0","openSubsonic":true,"scanStatus":{"count":5,"lastScan":"2026-10-12T08:15:30Z","scanning":false}}}
```

`scanning` is `false` when the scan has ended, and `count` is the number of audio files that it read.

### 4. Connect a client

Enter three values in an OpenSubsonic client: the address `http://<address of the machine>:4200`, the user name `owner` and the password. [Connecting a client](#connecting-a-client) describes what the client finds.

## Configuration

`config.toml` in the [data directory](#data-directory) is a TOML file. `init` writes it with every key. A key that the file omits has its default. An unknown key is an error. The file is checked when a command loads it, and an error names the key. `run` reads the file when it starts, so a change takes effect at the next start.

`wn-server config path` prints the path of the file. `wn-server config show` prints the values in effect.

The name of a key in this table is its table in the file, a dot, and its name there: `server.port` is the key `port` in the table `[server]`.

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | `"0.0.0.0"` | IP address the server listens on; `"0.0.0.0"` is every IPv4 address of the machine |
| `server.port` | `4200` | TCP port; 1 to 65535 |
| `server.name` | `"wn-server"` | instance name shown by discovery; 1 to 63 bytes (see [Server name](#server-name)) |
| `server.log_level` | `"info"` | log filter; `RUST_LOG` replaces it when set (see [Logs](#logs)) |
| `auth.disabled_unsafe` | `false` | `true` turns authentication off; unsafe, see [Authentication](#authentication) |
| `library.folders` | `[]` | music folders; at least one, and each is the absolute path of a directory (see [Music folders](#music-folders)) |
| `library.scan_on_start` | `true` | scan the folders when the server starts |
| `library.rescan_interval_minutes` | `0` | minutes between periodic scans; 0 disables them (see [Scanning](#scanning)) |
| `transcoding.ffmpeg` | `"ffmpeg"` | ffmpeg program name or path |
| `transcoding.max_concurrent` | `2` | transcodes that run at the same time; at least 1 |
| `transcoding.opus_bitrate_kbps` | `192` | Opus bitrate when the client names none; 32 to 320 |
| `transcoding.mp3_bitrate_kbps` | `320` | MP3 bitrate when the client names none; 32 to 320 |
| `discovery.mdns` | `true` | advertise the server over mDNS (see [Discovery](#discovery)) |

`init` writes this file, with the folders of `--folder` and the port of `--port`:

```toml
[server]
bind = "0.0.0.0"
port = 4200
name = "wn-server"              # instance name shown by discovery
log_level = "info"              # RUST_LOG overrides when set

[auth]
disabled_unsafe = false         # true turns authentication off; unsafe

[library]
folders = ['/path/to/music']    # TOML literal strings; 'C:\Music' on Windows
scan_on_start = true
rescan_interval_minutes = 0     # 0 disables periodic rescans

[transcoding]
ffmpeg = "ffmpeg"               # program name or path
max_concurrent = 2
opus_bitrate_kbps = 192         # used when format=opus and maxBitRate is absent
mp3_bitrate_kbps = 320          # used when format=mp3 and maxBitRate is absent

[discovery]
mdns = true
```

A Windows path is written as a TOML literal string, in single quotes, so that a backslash is kept as written. That holds for the paths of `library.folders` and for a path in `transcoding.ffmpeg`:

```toml
[library]
folders = ['C:\Music', 'D:\More Music']

[transcoding]
ffmpeg = 'C:\Program Files\ffmpeg\bin\ffmpeg.exe'
```

### Server name

The default `server.name` is `wn-server` on every machine. `init` writes it to `config.toml`. Edit `name` in the `[server]` table to give the server another name, for example when two servers run on one network. A name that the file holds is kept as written. The server takes no name from the host name of the machine.

`server.name` holds 1 to 63 bytes of UTF-8. A character outside ASCII takes 2 to 4 bytes. A command that loads the file fails when the name is empty or longer, and the error names `server.name` and the length of the name. Set `name` in the `[server]` table to a name within the limit, or remove the key to get the default.

## Commands

| Command | Effect |
|---|---|
| `wn-server init [--folder PATH]... [--port N] [--username NAME] [--password-stdin]` | Creates the data directory, `config.toml`, `instance.id`, the database and the owner account. With a `config.toml` and no owner account, keeps `config.toml` and creates what is missing. |
| `wn-server run` | Runs the server in the foreground until a stop signal, such as Ctrl-C. |
| `wn-server scan [--full]` | Runs one scan of the music folders and prints its summary. `--full` reads the tags of every file again. See [Scanning](#scanning). |
| `wn-server user show` | Prints the name, the role and the creation date of the owner account. |
| `wn-server user set-password [--username NAME] [--password-stdin]` | Stores a new password for the owner account, or for the account `--username` names. |
| `wn-server config path` | Prints the path of the config file. |
| `wn-server config show` | Prints the effective config: the values of the file and the default of every key the file omits. |
| `wn-server --version` | Prints the version. |

`wn-server --help` and `wn-server <command> --help` print the options of a command.

Every command accepts `--data-dir DIR` before or after the subcommand. A command that fails prints `wn-server: <error>` to stderr and exits with status 1. A usage error exits with status 2.

Three environment variables are read:

| Variable | Meaning |
|---|---|
| `WN_SERVER_DATA_DIR` | the data directory, for a command without `--data-dir` (see [Data directory](#data-directory)) |
| `WN_SERVER_PASSWORD` | the password for `init` and `user set-password`, for a command without `--password-stdin` (see [First run](#first-run)) |
| `RUST_LOG` | the log filter of `run` and `scan`, in place of `server.log_level` (see [Logs](#logs)) |

### `init`

`init` checks that each `--folder` is an existing directory, creates the data directory and writes `config.toml` with every key. `--folder` may be repeated. `--port` sets `server.port`. `init` then writes `instance.id`, creates the database `library.db` and creates the owner account.

- Folders are written as absolute paths.
- An existing `config.toml` is not overwritten. When `library.db` has an owner account as well, `init` fails and changes nothing. When `library.db` is missing or has no owner account, `init` creates what is missing (see [Owner account](#owner-account)).
- An existing owner account is not replaced. `init` fails when `library.db` already has one, and removes the `config.toml` and the `instance.id` it wrote in that run.
- An existing `instance.id` is kept.
- Without `--folder`, `init` writes `folders = []`. A folder must be added to the file before another command loads it.

### Owner account

The server has one account, the owner. A client signs in with the name and the password of that account. `--username` sets the name at `init`. The default name is `owner`.

`wn-server user set-password` stores a new password. It takes the password from the three sources of `init` (see [First run](#first-run)) and prints a generated password once. `--username` names the account. Without it the account is the owner.

`wn-server user show` prints the name, the role and the creation date of the owner account. The date is in UTC.

The `user` commands need `library.db` and its owner account, and only `init` creates them. When `config.toml` exists and `library.db` is missing or has no owner account, `wn-server init` creates what is missing: the database, the owner account and `instance.id`. It leaves `config.toml` as it is. `--folder` and `--port` are errors in that case, because `init` does not change an existing `config.toml`; edit the file to change them. `init` takes the password from its three sources, and `--username` names the account. A database that exists keeps everything else in it. A database that `init` creates is empty; nothing of a lost database is restored.

## Data directory

The data directory holds what the server stores:

| Entry | Content |
|---|---|
| `config.toml` | the [configuration](#configuration) |
| `library.db` | the SQLite database: the library, the owner account with its password, the stars, the ratings, the play counts, the playlists and the play queue. SQLite keeps the files `library.db-wal` and `library.db-shm` next to it. |
| `instance.id` | a random UUID that identifies this server to clients on the network (see [Discovery](#discovery)) |
| `scan.lock` | an empty file that the process that scans holds a lock on (see [Scanning](#scanning)); the first scan creates it |
| `cache/covers` | resized covers (see [Cover art](#cover-art)); the first resized cover creates the directory |

The first of these that is present names the data directory:

1. The `--data-dir` option.
2. The `WN_SERVER_DATA_DIR` environment variable. An empty value counts as unset.
3. The platform default: `~/Library/Application Support/wn-server` on macOS, `~/.local/share/wn-server` on Linux (`$XDG_DATA_HOME/wn-server` when that variable is set), `%APPDATA%\wn-server\data` on Windows.

`wn-server config path` prints the config path that results.

On Unix, `init` creates the data directory with mode 0700 and `library.db` with mode 0600. It does not change the mode of a directory that already exists. On Windows the access control list of the user profile protects the data directory. The password of the owner is in `library.db` in plain text (see [First run](#first-run)).

## Music folders

`library.folders` names the directories that hold the music. `init` writes the folders of `--folder` there, and the owner edits the list in `config.toml`.

A folder path is absolute, and a Windows path is written as a TOML literal string (see [Configuration](#configuration)). A command that loads the file fails when `library.folders` is empty, when a path is relative, or when a path names a file, and the error names `library.folders`.

A scan indexes the files with the extensions `flac`, `mp3`, `m4a`, `aac`, `ogg`, `opus`, `wav`, `aif` and `aiff`, in any letter case. It skips every file and directory whose name starts with `.`, and every directory with the name `System Volume Information`, `$RECYCLE.BIN` or `lost+found`.

### A folder that is unavailable

A folder that does not exist or cannot be read is not an error of the file. The drive of the folder may be unplugged, or its network share may be down. `run` and `scan` start without it (see [Running](#running) and [Scanning](#scanning)). `init` is stricter: it refuses a `--folder` that does not exist.

While a folder is unavailable, the server behaves in this way:

- `run` starts and serves the music of the other folders. It logs one warning at startup that names the folder: `music folder <path> is unavailable: <cause>. Its tracks stay in the library, and no scan deletes a track while the folder cannot be read`.
- The folder stays in the list of `getMusicFolders`. Its tracks stay in the library with their stars, ratings and playlist entries. Their files cannot be read until the folder is back, and `stream` returns error 70 for them.
- A scan reads the other folders and stores their new, changed and moved files. A file that was moved out of the unavailable folder is stored as a new track.
- A scan deletes no track, in any folder. It keeps the tracks of the unavailable folder. It also keeps a track of another folder whose file is gone, because that file may have moved to the folder that the scan could not read. Deleting the track would lose its ID, its stars, its rating, its play count and its playlist entries. A track whose file was deleted therefore stays listed during the outage and cannot be played: `stream` returns error 70 for it. This holds for as long as the folder is unavailable.

The API does not report an unavailable folder. `getMusicFolders` lists the folder as before, and `getScanStatus` holds no error of a scan. The log and `wn-server scan` report it:

- The log of `run` has the warning at startup. Each scan of the server adds one warning: `music folder "<name>" (<path>) cannot be read: <cause>; no track was deleted`.
- `wn-server scan` logs both warnings, prints the error of the folder after its summary and exits with status 1 (see [Scanning](#scanning)).

An outage ends in one of two ways:

- The folder comes back. The server does not watch for the folder. The next scan after the folder is back reads it, finds its tracks under the same IDs, and removes the tracks whose files are gone.
- The folder is taken out of `library.folders`, and the server is started again. A scan that reads every remaining folder in full, and finds music in each, then removes the tracks of that folder for good, with their stars, ratings, play counts and playlist entries.

### A disk mounted below a music folder

A disk that is mounted on a directory below a music folder is not protected. While the disk is not mounted, that directory is empty. A scan reads the empty directory without an error and takes the files of the disk for deleted. It removes their tracks with their stars, ratings, play counts and playlist entries. When the disk is mounted again, a scan stores its files as new tracks. The rule for an unavailable folder does not apply, because the music folder itself can be read.

Add each disk as a music folder of its own in `library.folders`, and do not mount a disk below a music folder.

A music folder that is the mount point of a disk keeps its tracks while the disk is not mounted. A scan deletes no track of a music folder that had tracks and now holds no audio file, and it records an error that names the folder. This rule protects that folder only: the scan removes a track of another folder whose file is gone.

### Symbolic links

The server indexes and serves only the files inside the folders of `library.folders`. A scan resolves each symbolic link that it finds in a music folder and compares the target of the link with the music folders.

- A link to a file or to a directory outside every music folder is not followed. The files behind it are not in the library. Such a link is no error of the scan. The scan logs each one at the `debug` level, and the number of them in each folder at the `info` level.
- A link to a file or to a directory inside a music folder adds no second track. A file that the scan also finds at its own path has its track there. A file that the scan finds through a link only has its track at the path of the link. That is a file in a directory that the scan skips, such as a directory whose name starts with `.`.
- A music folder can itself be a symbolic link.
- A link that cannot be resolved is an error of the scan: its target is gone, or the server may not read the path to it. The scan cannot tell what is behind the link, so it deletes no track while the link is there. Repair the link or remove it.

To add the music of another disk, name its directory in `library.folders`. Do not link it into a music folder.

A library can hold tracks whose files are behind a link to a place outside the music folders. The next scan that reads every folder in full, and finds music in each, removes these tracks, with their stars, ratings, play counts and playlist entries. To keep them, add the directory that the link points to to `library.folders` before that scan. The tracks then move to the new folder and keep what is attached to them. When the tracks are already removed, adding the folder creates new tracks without those values. The same holds for a track at a path through a link when the scan finds the file at its own path too: the track at the own path of the file stays, and the track at the path through the link is removed.

A hard link is no symbolic link. The server cannot tell a hard link from the file, so a hard link in a music folder to a file outside the folders is indexed and served.

## Scanning

A scan reads the music folders into the database. It reads the files that are new or have changed. A full scan reads the tags of every file again, except a file that was renamed or moved since the last scan and did not change otherwise. Four things start a scan:

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

`seen` is the number of supported audio files. `added`, `changed`, `moved` and `removed` count tracks. `albums` and `artists` are the numbers in the library after the scan. Each error of the scan follows as a line `error: <message>`. An error names the folder, the path or the file that it is about, or counts the tracks that the scan kept although their files are gone. The command exits with status 0, or with status 1 when the scan recorded an error. Warnings about files that the scan leaves for the next scan go to stderr with the log.

A scan that a database error ends prints that error and no summary, and the command exits with status 1.

### Files with large tags

A scan does not read the tags of a file when that takes more than 32 MiB of the file. That is a file with a very large tag, such as several large pictures, and a damaged file whose header declares such a tag.

- The file gets no track. A track that the file already has keeps its tags, its stars, its rating and its play count. When the file changed since the scan that read its tags, the track is not played until the tags are smaller, because the file is no longer the one that the scan read.
- The scan records an error that names the file, at every scan that reads the file. That is every scan for a file without a track and for a file that changed. `wn-server scan` prints the error and exits with status 1.
- Such a file does not keep a scan from removing the tracks of files that are gone.
- `getCoverArt` answers for the picture of such a file as for a missing cover.

Make the tag smaller, for example by removing or shrinking the embedded pictures, and scan again.

The bound limits the bytes that a read takes from the file. It does not limit the memory of every read: a compressed ID3v2 frame is expanded without a limit, and a tag of very many small items takes many times its size in memory.

### Renamed, moved and retagged files

A scan recognises a file by its size and by its first and its last 64 KiB.

- A file that was renamed or moved, within a music folder or into another one, keeps its track: the ID, the stars, the rating, the play count and the entries in playlists and in the play queue.
- A file whose tags were edited in place keeps its track as well.
- A file that was moved and also changed between two scans is a new track. The scan removes the old track with what was attached to it. A scan between the two steps keeps the track.

An album is known by its album artist and its title, and an artist by its name. Letter case, surrounding white space and the Unicode form of a name have no meaning for that. A change of the tags within these limits keeps the album or the artist with its stars and its rating. Any other change of the title or of the name makes a new album or a new artist, and the stars and the rating of the old one are removed with it. The songs keep their stars, ratings and play counts.

## Running

```
wn-server run
```

`run` loads `config.toml`, listens on `server.bind` and `server.port` (`0.0.0.0:4200` by default) and logs one line with the address, the version, the data directory, the number of music folders and whether authentication is on.

`run` opens `library.db` and needs the owner account in it. `run` creates neither and fails at startup when one is missing. `wn-server init` then creates what is missing and keeps `config.toml` (see [Owner account](#owner-account)).

`run` stores the music folders of `config.toml` in the database before it answers the first request. Unless `library.scan_on_start = false`, it then scans them in the background (see [Scanning](#scanning)). The server answers requests while it scans.

`run` starts when a music folder does not exist or cannot be read (see [A folder that is unavailable](#a-folder-that-is-unavailable)), and when ffmpeg is missing (see [ffmpeg](#ffmpeg)).

### Stopping

These signals stop the server. The limit is the time that the server gives its requests to end. Its scan then has the same time again:

| Signal | OS | Limit |
|---|---|---|
| Ctrl-C | Linux, macOS, Windows | 10 seconds |
| SIGTERM, which `kill`, systemd, launchd and `docker stop` send | Linux, macOS | 10 seconds |
| Ctrl-Break | Windows | 10 seconds |
| Closing the console window | Windows | 1.5 seconds |

Logging off and shutting the system down are not in the table. On Windows either one may end the server without the steps below. Microsoft documents that a console program that has loaded `user32.dll` receives no signal for a logoff or a shutdown, and that Windows ends the programs of a user at logoff before it sends one. The server does not listen for the logoff signal. It listens for the shutdown signal and, when that signal arrives, stops with the limit of 1.5 seconds. The database is consistent after a logoff and after a shutdown, and the next start reads the write-ahead log.

The server stops in this order:

1. It withdraws the mDNS advertisement, so clients see the server leave at once (see [Discovery](#discovery)). At the same time it closes its port.
2. The requests that are in progress are answered. A request that is still in progress after the limit is dropped, with one warning in the log.
3. It waits for a running scan to stop, for the limit at most. The scan stops before it reads its next file. It keeps the batches of files that it has stored, and the next scan does the rest. A scan that has not stopped by then is left behind with one warning.
4. It moves the write-ahead log into `library.db` and closes the database. While another program, such as `wn-server scan`, uses the database, the log is not moved. It keeps every change, and that program or the next start moves it.

`run` then exits with status 0. It exits with status 1 when the listener failed or when the database cannot be closed for another reason. A second signal during the stop ends the server at once: it skips the steps that have not run and exits with status 1. The advertisement is withdrawn in that case as well. The database is consistent after that, and also after the system has ended the process. A scan that was cut short is finished by the next scan.

Windows ends a console program 5 seconds after its window is closed. The limit of 1.5 seconds is for that signal: steps 1 to 3 end within 3 seconds, which leaves 2 seconds for step 4. Step 4 takes longer than that while a database read or write of the server still runs, or while another program, such as `wn-server scan`, uses the database. Windows then ends the server before the write-ahead log is moved; the log keeps every change, and the next start reads it.

## Connecting a client

A client needs three values:

- The address of the server: `http://<address>:<port>`, for example `http://192.168.1.5:4200`. The API is below `/rest`, as in `http://192.168.1.5:4200/rest/ping`. The server speaks plain HTTP and has no TLS.
- The user name: `owner`, or the name that `init` got with `--username`.
- The password of that account.

A client on the same network can find the address by itself (see [Discovery](#discovery)).

The server follows the OpenSubsonic API with the protocol version 1.16.1. A client meets these differences from other servers:

- `download` is disabled. A request with a user gets error 50 with the message `download is disabled on this server`, whatever its parameters. No request gets the bytes of a file. `getUser` and `getUsers` report `downloadRole` as `false`.
- `search3` and `search2` accept an empty `query` and then return the whole library in pages (see [Search](#search)).
- `f=jsonp` is not supported (see [Requests and responses](#requests-and-responses)).
- The server has one account, and no API method changes it (see [Users](#users)).
- A transcoded stream has no `Content-Length` (see [Streaming and transcoding](#streaming-and-transcoding)).

### Authentication

Every API method needs a user, except `getOpenSubsonicExtensions`. The server checks the request before the method runs. A client sends the name of the owner account in `u` and one of these credentials:

- `p`: the password, as it is or as `enc:` followed by its hex encoding. A value that starts with `enc:` is always read as hex, so a password that itself starts with `enc:` is sent in the hex form.
- `t` and `s`: a token and its salt. The token is the MD5 digest of the password followed by the salt, in lower-case hex.

`v` and `c` are required. The value of `v` is not compared. The name in `u` is matched without regard to ASCII case.

| Request | Error |
|---|---|
| `v`, `c` or `u` is absent, or the request has no credential, or it has `t` without `s` or `s` without `t` | 10; the message names what is absent |
| the user is unknown, or the password or the token is wrong | 40 |
| `apiKey` alone; the server has no API keys | 42 |
| `apiKey` with `u`, `p`, `t` or `s`, or `p` with `t` or `s` | 43 |

The response does not tell an unknown user from a wrong password. A failed attempt is logged at the `info` level with the values of `u` and `c`, each cut to 64 characters, and the error code. A password, a token or a salt is never logged and is never part of an error message.

`auth.disabled_unsafe = true` turns authentication off. This is unsafe: every request that has `v` and `c` then acts as the owner without a password, for anyone who can reach the port. That includes any web page that is open in a browser on the network: every response carries `Access-Control-Allow-Origin: *` (see [Clients in a browser](#clients-in-a-browser)), so the browser lets the page read the responses. Such a page can then use the server as the owner. It can read and stream the library, and it can call every method, including the methods that change state: stars, ratings, plays, playlists, the play queue and scans. `run` logs a warning at startup while the setting is on.

### Requests and responses

The server answers OpenSubsonic requests at `/rest/<method>` and `/rest/<method>.view`, for GET and POST. A POST may carry parameters in an `application/x-www-form-urlencoded` body of up to 8 MiB, the credentials included; `getOpenSubsonicExtensions` lists this as the `formPost` extension. When a parameter is in the query string and in the body, the values of the query string come first. A larger form body returns error 0, and a POST body of another content type is not read. A HEAD request is answered as a GET request, without the body. Another HTTP method at these paths returns HTTP 405, and any other path returns HTTP 404.

- `f=json` selects JSON. `f=xml` or no `f` selects XML. Any other value, `jsonp` included, returns error 0 in XML.
- An API error is an HTTP 200 response with the status `failed` and an `error` element that holds the Subsonic error code and a message.
- Every response carries the protocol version `1.16.1`, the type `wn-server`, the version of the server as `serverVersion`, and `openSubsonic` as `true`.
- In XML, the elements `searchResult2`, `searchResult3`, `starred` and `starred2` hold their `artist` elements first, then their `album` elements, then their `song` elements. That is the order of the Subsonic 1.16.1 schema.
- A parameter that holds a number, or `true` or `false`, counts as absent when its value is empty. Any other value that is not of the type of the parameter returns error 0 with a message that names the parameter. `true` and `false` are read in any letter case.
- The server reads the parameters, selects the format and looks up the method. It then checks the user, before the method runs. An unsupported `f`, a form body that cannot be read and an unknown method name are therefore answered without a user. Every other request without a user gets the authentication error, from every method except `getOpenSubsonicExtensions`, and also from a method that is not served.
- A method that is not served returns error 0 (see [Methods that are not served](#methods-that-are-not-served)).
- A read of the database that a request starts runs for 5 seconds at most. The server then stops the read, and the request returns error 0. The message is the one the method has for a database that it cannot read, such as `the server cannot read its library`, and the log holds the cause. The time in which a request waits for a database connection does not count, and a write has no such limit.

### Clients in a browser

A client that runs in a browser calls the server from a page of another origin. The server sends the CORS headers that a browser needs for that:

- Every response at `/rest/<method>` and `/rest/<method>.view` carries `Access-Control-Allow-Origin: *`. That includes an API error and an authentication error.
- An `OPTIONS` request to these paths is answered as a preflight request. It needs no user and no parameter. The answer is HTTP 204 with `Access-Control-Allow-Methods: GET, POST, OPTIONS`, `Access-Control-Allow-Headers: Content-Type, Range` and `Access-Control-Max-Age: 86400`.
- Every other response at these paths carries `Access-Control-Expose-Headers: Content-Range, Accept-Ranges, Content-Length`.
- No response carries `Access-Control-Allow-Credentials`. The credentials are request parameters, and the server sets no cookie.
- The server has no list of allowed origins. A response for any other path carries none of these headers.

## API methods

### System

`ping` returns the empty response. `getLicense` returns `license` with `valid` as `true`. `getOpenSubsonicExtensions` needs no user and lists the extensions `formPost` and `transcodeOffset`, each in version 1.

### Browsing

- `getMusicFolders` lists the music folders of `config.toml`, each with an integer `id` and the last component of its path as `name`. The `name` of a root such as `/` or `C:\` is the whole path. The list is there before the first scan. A folder that was removed from `config.toml` is listed until a scan has removed its tracks.
- `getArtists` returns the album artists in indexes by the first letter of the name. A leading `The`, `El`, `La`, `Los`, `Las`, `Le` or `Les` that another word follows is not part of the name for that, a letter with a diacritic counts as its base letter, and a name that starts with no letter from A to Z is in the index `#`. A letter without a base letter, such as `Ø` or `Æ`, is in `#` as well. `ignoredArticles` holds the seven articles. `musicFolderId` limits the answer to the artists that have a file in that folder; an ID that no folder has gives no artist, and a value that is no integer returns error 0. An artist has no `coverArt`.
- `getArtist` returns an artist with its albums by year and then by title; an album without a year follows the others. `getAlbum` returns an album with its songs by disc number, then by track number, then by path. A song without a track number follows the numbered songs of its disc. `getSong` returns one song. The three need `id`: an absent `id` returns error 10, and a value that is the ID of no such item returns error 70.
- A song has `duration` in whole seconds, `suffix`, `contentType`, `bitRate`, `size`, `discNumber` (1 without a disc number tag), and `bitDepth`, `samplingRate` and `channelCount`, which are 0 when the file gives none. `track`, `year`, `genre` and `bitRate` are absent when the file gives none. `path` is the path of the file below its music folder, with `/` between the components. A track that a scan kept after another file took its path has no `path`; the errors of that scan name the track. `artist` is the track artist and `artistId` is the ID of the album artist. `starred`, `userRating`, `playCount` and `played` are the values of the user of the request.
- `getGenres` returns each genre tag of the songs with `songCount` and `albumCount`, in the order of the names without regard to ASCII case. An album counts for the genre that most of its songs have.

### Browsing by folder

- `getIndexes` and `getMusicDirectory` are for clients that browse by folder. The server shows the library as a tree: the album artists, the albums of an artist and the songs of an album. Every ID in the tree is the ID that the other methods use for the same artist, album or song.
- `getIndexes` returns the artists of `getArtists` in the same indexes, each with `id`, `name`, and `starred` and `userRating` of the user, and `musicFolderId` limits them in the same way. `lastModified` is the time at which the artists last changed, in milliseconds since 1970. It is the end of the last scan that changed the library: a scan that added, changed, moved or removed a song, that changed an album or an artist, for example with a new cover, or that followed a scan which did not end. A scan that finds nothing changed keeps it, also when it was cancelled or could not read a folder, and so does a star or a rating. Two scans that change the library never have the same time. It is 0 until a scan that changed the library has ended. A request whose `ifModifiedSince` is at or after `lastModified` gets `indexes` with `ignoredArticles` and `lastModified` and without the `index` list. While a scan runs, and until a scan that changed the library has ended, every request gets the artists. A value of either parameter that is no integer returns error 0.
- `getMusicDirectory` needs `id`. The ID of an artist returns the albums of the artist as directories (`isDir` is `true`), by year and then by title. The ID of an album returns its songs, by disc number, then by track number, then by path. `parent` of a song is the ID of its album, and `parent` of an album is the ID of its artist. An absent `id` returns error 10. The ID of a song and a value that is the ID of no artist and of no album return error 70. A track that a scan kept after another file took its path is in no directory; `getAlbum` lists it.

### Album and song lists

- `getAlbumList2` returns a page of albums as `albumList2`, and `getAlbumList` returns the same albums as directories in `albumList`. `type` is required and is one of `random`, `newest`, `highest`, `frequent`, `recent`, `alphabeticalByName`, `alphabeticalByArtist`, `starred`, `byYear` and `byGenre`; another value returns error 0. `size` is 10 by default, and a value above 500 is lowered to 500. `offset` selects a later page, and `musicFolderId` restricts the list to one music folder. `byYear` needs `fromYear` and `toYear`, and lists the latest year first when `fromYear` is the greater one. An album without a year counts as year 0. `byGenre` needs `genre`. `highest`, `frequent`, `recent` and `starred` list only the albums that the user rated, played or starred, so they are empty until the user has done that. Every list but `random` has a fixed order, so its pages neither repeat nor skip an album. `random` draws its albums anew for every request, and with an `offset` it returns at most the number of albums less the offset.
- `getRandomSongs` returns up to `size` different songs as `randomSongs`: 10 by default and 500 at most. `genre`, `fromYear`, `toYear` and `musicFolderId` restrict the songs. A song without a year counts as year 0.
- `getSongsByGenre` needs `genre` and returns the songs of that genre in the order of their titles as `songsByGenre`. `count` is 10 by default and 500 at most, `offset` selects a later page, and `musicFolderId` restricts the songs to one music folder.
- A `genre` of these methods matches as a whole text and without regard to case. `getGenres` lists spellings that differ by case as separate genres, and a list for one of them holds the albums or songs of all of them.

### Search

`search3` and `search2` return the artists, the albums and the songs whose names hold every word of `query`. `query` is required and may be empty: `query=` and `query=""` match the whole library. A word may be a part of a name, in any letter case: `beat` finds "The Beatles". The words are looked for in the name of an artist, in the title and the artist of an album, and in the title, the album, the artist and the album artist of a song. `%`, `_` and `\` have no special meaning. A double quote is ignored, and so is one `*` at the end of the query. A letter with a diacritic is not its base letter: `edith` does not find "Édith". Unicode has two spellings for such a letter: one character (the composed form, NFC), or the base letter followed by a combining mark (the decomposed form, NFD). The two spellings are one letter in a query and in a tag, so `café` in either spelling finds a title that a tag has in either spelling. The letter case of a letter outside ASCII follows the mapping that Unicode defines for all languages: `I` and `i` are one letter in every text, and `İ` is not `i`. The track artist of a song is compared as the tag spells it. A letter outside ASCII is found there only when the tag has it in lower case: no query finds the `Ó` of "Ólafur Arnalds" there. A letter with a combining mark is found there only when the tag has it as one character. `artistCount`, `albumCount` and `songCount` are 20 by default, and a value above 500 counts as 500. `artistOffset`, `albumOffset` and `songOffset` are 0 by default. Each list is in the order of the names, and the order is the same for every page. A client reads the whole library by asking for pages of 500 items or fewer, one after the other, until a page is shorter than its count. `musicFolderId` limits the lists to one music folder. A query of more than 1024 characters or of more than 32 different words returns error 0. `search` is deprecated and returns error 0 with the message `deprecated; use search3`.

### Stars, ratings and plays

- `star` and `unstar` take `id`, `albumId` and `artistId`, each any number of times. `id` takes the ID of a song, an album or an artist. `setRating` takes `id` and `rating`: 1 to 5 sets the rating and 0 removes it. An absent `rating` returns error 10, and any other rating returns error 0. An ID that names no item returns error 70. A request changes every item it names or none: after an error no star and no rating of the request is stored. `star` for an item that has a star sets the time of the star to the time of the new request. `unstar` for an item without a star succeeds. `getStarred2` and `getStarred` list the starred artists, albums and songs, the one starred last first, and `musicFolderId` limits them to one music folder. Stars and ratings belong to the user of the request. A song keeps them when its file is renamed or moved, and an album or an artist keeps them when the letter case, the surrounding white space or the Unicode form of its name changes (see [Renamed, moved and retagged files](#renamed-moved-and-retagged-files)).
- `scrobble` counts a play of each song that `id` names, for the user of the request. `id` is required and may occur any number of times; a song that is named twice gets two plays. `time` is the time of a play in milliseconds since 1970. A request has no `time` or one for each `id`, and a play without a `time` has the time of the request. Every request counts: the same request sent twice counts its plays twice. `playCount` of a song is the number of its plays, and `played` is the latest of their times, so a play that is reported late does not move `played` back. `playCount` of an album is the sum of the play counts of its songs, and `played` is the latest play of one of them; `frequent` and `recent` of the album lists order the albums by these two values. `submission=false`, the "now playing" notification, is accepted and counts nothing, and `getNowPlaying` returns an empty list. Nothing is sent to another service. An absent `id` returns error 10. A `time` that is no integer from 0 to 253402300799999, a number of `time` values that is not the number of `id` values and a `submission` that is neither `true` nor `false` return error 0. An ID that names no song returns error 70. After an error no play of the request is counted. Plays belong to the user of the request. A song keeps them when its file is renamed or moved, and they are deleted with the song when its file is deleted.

### Playlists

- `getPlaylists` lists the playlists of the user of the request by name, each with `owner`, `public`, `songCount`, `duration`, `created`, `changed` and `coverArt`, the cover of its first song. `coverArt` is absent when the playlist has no song or its first song has no cover. `getPlaylist` adds the songs as `entry`, in the order of the playlist. `createPlaylist` with `name` creates a playlist with the songs of `songId`, in the order of the values; a song that is named twice is in the playlist twice. With `playlistId` it replaces the songs of that playlist and keeps its name, its comment and `public`; `name` is not read then. It returns the playlist with its songs to every client, whatever `v` the request has. `deletePlaylist` deletes a playlist; its songs stay in the library.
- `updatePlaylist` takes `playlistId` and any of `name`, `comment`, `public`, `songIndexToRemove` and `songIdToAdd`. An empty `name` keeps the name, and an empty `comment` removes the comment. Each `songIndexToRemove` is the position of a song in the playlist before the request, counted from 0. The songs at these positions are removed, whatever the order of the values is, and then the songs of `songIdToAdd` are added at the end.
- An ID that names no playlist of the user and an ID that names no song return error 70. A `songIndexToRemove` that is no position of the playlist returns error 0. A playlist holds at most 100 000 songs, and a request that would make it longer returns error 0. After an error nothing of the request is stored. A user sees and changes only the playlists of that user; `public` is stored and has no effect. `getPlaylists` with a `username` other than the name of the user of the request, compared without regard to ASCII case, returns error 50. With authentication off, every name returns the playlists of the owner.
- A playlist keeps a song when its file is renamed or moved. When a file is deleted, the scan that removes the song from the library removes it from every playlist, and the other songs keep their order.

### Play queue

- `savePlayQueue` saves the play queue of the user of the request, so that a client on another device goes on from there. The queue holds the songs of `id` in the order of the values, and a song that is named twice is in the queue twice. `current` is the ID of the current song, and `position` is the position within that song in milliseconds, 0 without a value. Each request replaces the whole queue. `current` must be one of the `id` values (error 0). A request with `id` values and without `current` is accepted, although the OpenSubsonic API requires `current` in it: the songs are saved with no current song, and the `position` of the request is not stored. A request without `id` and without `current` empties the queue. An ID that names no song returns error 70, and a request with more than 100 000 songs returns error 0. After an error the stored queue is the one from before the request.
- `getPlayQueue` returns `playQueue` with the songs as `entry`, `current`, `position`, `username`, `changed` and `changedBy`. `changed` is the time of the last request that saved the queue, in UTC (`2026-10-11T08:15:30Z`), and `changedBy` is the `c` value of that request, cut to 64 characters. The answer holds `playQueue` for an empty queue as well, with `position` 0 and without `current`. A queue that was saved without `current` is returned with its first song as `current` and with `position` 0, because the OpenSubsonic API requires a `current` that is one of the songs when the queue has songs. A user that never saved a queue gets the creation time of the account as `changed` and an empty `changedBy`. A user reads and replaces only the queue of that user.
- The queue keeps a song when its file is renamed or moved. When a file is deleted, the scan that removes the song from the library removes it from the queue, and the other songs keep their order. When the deleted song was the current one, the song that followed it in the queue is the current one, or the last song when none followed, and `position` is 0. In a queue that was saved without `current`, the first song that is left is the current one.

### Lyrics

The server has no lyrics. `getLyrics` returns an empty `lyrics` element, whatever `artist` and `title` are. `getLyricsBySongId` needs `id` and returns an empty `lyricsList` for the ID of a song; an absent `id` returns error 10, and a value that is the ID of no song returns error 70. `getOpenSubsonicExtensions` does not list the `songLyrics` extension.

### Users

- `getUser` needs `username`. The value must be the name of the user of the request, without regard to ASCII case; another name returns error 50. With authentication off, every name returns the owner. `getUsers` lists the owner. The user has `scrobblingEnabled`, `adminRole`, `playlistRole` and `streamRole` as `true`, and every other role is `false`. `folder` holds the IDs of the music folders.
- `createUser`, `updateUser`, `deleteUser` and `changePassword` return error 50. The `wn-server user` commands manage the owner account.

### Scans

`startScan` starts a scan in the background and returns `scanStatus` with `scanning` and `count`, the number of audio files the scan has finished with. `getScanStatus` returns the same for the scan of the server that runs or ran last, and adds `lastScan`, the end time of the latest scan in UTC (`2026-10-11T08:15:30Z`). `lastScan` is not part of OpenSubsonic and is absent until a scan has ended. `scanning` is `true` only for a scan of the server itself, not for one that `wn-server scan` runs. A scan that recorded errors is not an API error: the errors are in the log. See [Scanning](#scanning).

### Methods without data

The server has no artist or album information from other services, no similar or top songs, no list of what is playing, and no shares, podcasts, internet radio stations or bookmarks. `getArtistInfo`, `getArtistInfo2`, `getAlbumInfo`, `getAlbumInfo2`, `getSimilarSongs`, `getSimilarSongs2`, `getTopSongs`, `getNowPlaying`, `getShares`, `getPodcasts`, `getNewestPodcasts`, `getInternetRadioStations` and `getBookmarks` return an empty result. These methods read no parameter. `getAvatar` returns error 70.

### Methods that are not served

These methods of the API are not served. Each returns error 0 with the message `not supported` to a request that has a user: `tokenInfo`, `getVideos`, `getVideoInfo`, `hls`, `getCaptions`, `createShare`, `updateShare`, `deleteShare`, `refreshPodcasts`, `createPodcastChannel`, `deletePodcastChannel`, `deletePodcastEpisode`, `downloadPodcastEpisode`, `jukeboxControl`, `createInternetRadioStation`, `updateInternetRadioStation`, `deleteInternetRadioStation`, `getChatMessages`, `addChatMessage`, `createBookmark`, `deleteBookmark`, `getTranscodeDecision` and `getTranscodeStream`.

Every other name returns error 0 with the message `unknown endpoint`, with or without a user. A method name is matched in its exact letter case.

## Streaming and transcoding

### Original files

- `stream` returns the original file of a track, unchanged, with the content type of its format, unless the request asks for a format or for a lower bitrate (see [Transcoding](#transcoding)). A lossless file stays lossless otherwise. It answers GET, HEAD and POST. For the original file it supports `Range` with one range (`bytes=100-199`, `bytes=100-`, `bytes=-100`), `If-Range`, `If-None-Match` and `If-Modified-Since`. A range that no byte satisfies gets HTTP 416, and a request for several ranges gets the whole file. A `Range` header in a unit other than `bytes`, such as `items=0-1`, is ignored: the response is the whole file with HTTP 200. The response carries `Accept-Ranges: bytes`, `Content-Length`, `ETag`, `Last-Modified` and a `Content-Disposition` with the file name.
- The responses of `stream` hold at most 64 original files open at the same time. A request for one more waits up to 5 seconds and then gets HTTP 503 with `Retry-After: 5` and an empty body; repeat the request after 5 seconds. A slot is free again when a client has read its response or has disconnected. HEAD requests and the answers HTTP 304 and HTTP 416 open no file and never wait. The number is fixed and is no config key.
- `stream` returns error 70 for an ID that names no track and for a track whose file it does not serve. That is a file that is gone, a file that changed on disk since the last scan (another size or another modification time), and a file outside the music folders. A scan gives no track to a file outside the music folders (see [Symbolic links](#symbolic-links)). A track has such a file when a symbolic link changed after the last scan, and when its file is behind a link that leads out of the folders and no scan has removed the track yet. Scan after you change files. For a file whose modification time is before 1970, a request for the original file returns error 0 and a request for a transcode returns error 70.

### Transcoding

- `stream` transcodes with ffmpeg when the request asks for it. `format=opus` returns Opus in Ogg (`audio/ogg`), and `format=mp3` returns MP3 (`audio/mpeg`). The bitrate is `maxBitRate` in kbps, or `transcoding.opus_bitrate_kbps` or `transcoding.mp3_bitrate_kbps` when the request has no `maxBitRate`; a transcode has 32 to 320 kbps. A file that has the requested format already is returned as it is, unless its bitrate is above `maxBitRate`: `format=mp3` on an MP3 file does not encode the file again. The suffix of the file decides whether it has the format, so an Opus stream in a file with the suffix `ogg` is encoded again for `format=opus`. `maxBitRate` without `format` returns Opus at that bitrate when the bitrate of the file is higher, and the original file otherwise. A file whose bitrate the scan could not read is transcoded for every `maxBitRate` above 0 when it is lossless, and for none when it is lossy. `maxBitRate=0` is no limit. `format=raw` returns the original file whatever `maxBitRate` is. The letter case of a `format` has no meaning, and a `format` that names none of `raw`, `opus` and `mp3` is read as no `format`.
- A transcoded stream carries `Accept-Ranges: none` and `Cache-Control: no-store`. `Range` has no effect on it. `timeOffset=N` starts the stream at the second N of the track; `getOpenSubsonicExtensions` lists this as the `transcodeOffset` extension.
- A transcoded stream never has a `Content-Length`, and the server sends every byte of it. `estimateContentLength` is accepted and has no effect. The server does not know the length of a stream before ffmpeg has written it, and it sends no estimate.
- At most `transcoding.max_concurrent` transcodes run at the same time. A request past the limit waits up to 5 seconds and then gets HTTP 503 with `Retry-After: 5`. A client that disconnects ends its transcode. A client that takes nothing of a transcoded stream for 10 minutes, because it is paused or gone, ends it as well: the connection ends without the end of the body, and the transcode slot is free for the next request. Until then such a client holds its slot. Ask for the rest of the track with a new request and `timeOffset`. A HEAD request for a transcode gets the headers and starts no ffmpeg, so it reports neither a server at its limit nor a server without ffmpeg.

### ffmpeg

- Transcoding needs ffmpeg with libopus and libmp3lame. The server does not install or bundle it. It runs the program `ffmpeg` from `PATH`, or the program that `transcoding.ffmpeg` names. `run` logs at startup whether it found ffmpeg, and reads the encoders of the build from `ffmpeg -encoders`. Without ffmpeg the server starts with one warning and serves original files, and a request that needs a transcode returns error 0 with the message `ffmpeg not available`. With an ffmpeg that lacks one of the two encoders the server starts with one warning that names the encoder: a request for that format returns the same error, and the other format is transcoded. Restart the server after you install or replace ffmpeg. When ffmpeg cannot be started for a request, the request returns error 0 with the message `the server cannot start ffmpeg`.
- ffmpeg reads a file of the library only as a local file in one of the audio containers that the server scans (FLAC, MP3, MP4, ADTS AAC, Ogg, WAV, AIFF). A file with an audio file name that is a playlist or a script is refused, so it cannot make ffmpeg read another file or open a network connection. One case rests on a default of ffmpeg and not on the server: an MP4 file can name a second file as the place of its audio data. ffmpeg does not follow such a reference unless its option `enable_drefs` is set, and the server does not set it. With an ffmpeg that follows it, a crafted MP4 file in the library would make ffmpeg read another local file, and the stream would go to the client, which is the owner. When ffmpeg fails for a file, the log has one warning with the exit status of ffmpeg and the ID of the track. The server sends the response headers when ffmpeg has written its first output. A request whose ffmpeg ends without output, or writes none within 10 seconds, returns error 0 with the message `ffmpeg produced no output`, and a request whose ffmpeg fails after it has written output gets an incomplete stream. The messages of ffmpeg itself are logged at the `debug` level.

## Cover art

`getCoverArt` returns the cover whose ID the `coverArt` of an album or of a song holds.

### Cover sources

- The cover of an album is the image file `cover`, `folder`, `front` or `album` with the extension `jpg`, `jpeg`, `png` or `webp` in the directory of its first track. The letter case of the file name has no meaning. Among several such files the name decides first, in that order, and then the extension, in that order.
- An album whose discs have directories of their own, such as `Album/CD1` and `Album/CD2`, has the image file of their parent directory when the directory of its first track holds none. That needs two or more disc directories with one parent directory, and no track of another album at or below that directory.
- An album without such an image file has the embedded picture of its first track that has one.
- An image file that is a symbolic link is not a cover, wherever the link points.
- A cover whose file, or whose audio file with the picture, is reached through a symbolic link to a place outside every folder of `library.folders` returns error 70, as `stream` does for such a track.

### Cover sizes

- Without `size`, and with `size=0`, the body is the image as it is stored, as `image/jpeg`, `image/png` or `image/webp`.
- With `size=N` the body is a JPEG at the step of N: the smallest of 80, 160, 320, 640, 1280 and 2000 pixels that is not smaller than N, and 2000 for a larger N. Its longer side is at most the step, so it can be larger than N, and no image is enlarged. A JPEG whose Exif metadata declares an orientation is turned and mirrored accordingly when it is resized, and the resized cover holds no metadata. The image as it is stored keeps its bytes and its metadata, and the client applies the orientation. The sizes of one step get the same bytes and the same `ETag`: `size=290`, `size=300` and `size=310` all get the cover of 320 pixels.
- The server keeps the resized covers in `cache/covers` of the data directory. A cover has at most six files there for one modification time of its source, one for each step. A scan removes the files of an album whose cover changed, and the files of an album and of a track that it deleted. The files of the cover of a song stay when its audio file changes, until the track is deleted. A file that another version of the server wrote is not read; it stays until a scan removes the files of its album or of its track. The cache has no limit on its size.

### Cover errors and limits

- `getCoverArt` needs `id`: an absent `id` returns error 10. A `size` that is no integer from 0 to 4294967295 returns error 0. An `id` without a cover returns error 70: an ID that the server does not have, the ID of an album without a cover, the ID of a song whose file holds no picture, and the ID of an artist. An item without `coverArt` has no cover, and the server sends no placeholder image. An error is the usual response in XML or JSON.
- Three kinds of pictures are not served, and a request for one returns error 70, also when an item names the cover in `coverArt`: a source image of more than 16 MiB, a picture that is no JPEG, PNG or WebP image, such as an embedded GIF, BMP or TIFF picture, and the picture of an audio file whose tags the server does not read (see [Files with large tags](#files-with-large-tags)).
- Two kinds of images are served as they are stored and are not resized, so a request with `size` returns error 70 for them: an image with a side of more than 4096 pixels, and a WebP image that is lossless or in the extended file form.

### Cover caching

A cover comes with `Content-Length`, `Cache-Control: public, max-age=86400` and an `ETag`. A request that sends that entity tag in `If-None-Match` gets HTTP 304 without a body. The entity tag changes when a scan finds the cover file, or the audio file with the picture, with another modification time. An image that is replaced by one with the same modification time keeps its entity tag. The entity tag of a resized cover also changes with a version of the server that makes resized covers in another way. A HEAD request gets the headers without the body. `Range` is not read: the answer is the whole image.

## Discovery

While `wn-server run` is up, it advertises the server on the local network over mDNS (DNS-SD), so that a client finds the server without its address. `discovery.mdns = false` turns the advertisement off.

- Service type: `_waxnerd._tcp` in the domain `local.`
- Instance name: `server.name`. The mDNS responder appends ` (2)` when another server on the network has the same name.
- Host: `wn-server-<hex>.local.`, where `<hex>` is the first 8 hex digits of the UUID in `instance.id`. Port: `server.port`.

The host is a name of the server, not of the machine. The server publishes its address records under this name only. It never uses the host name of the machine, because a second mDNS responder for that name makes the operating system rename the machine. A client takes the host from the SRV record of the instance and resolves it to the addresses of the server.

The advertised addresses follow `server.bind`:

| `server.bind` | Advertised addresses |
|---|---|
| `0.0.0.0` | every IPv4 address of the network interfaces that are up |
| `::` | every IPv4 and IPv6 address of those interfaces |
| another address | that address only, on the network it belongs to |

The addresses of point-to-point interfaces, such as VPN tunnels, are not advertised. A server on `127.0.0.1` is advertised to clients on the same machine only. On Windows a listener on `::` accepts IPv6 connections only, so the IPv4 addresses that a server on `::` advertises do not accept connections there.

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

The server runs without the advertisement when the advertisement cannot start, and logs one warning that names the cause: `instance.id` cannot be read or does not hold a UUID, or the mDNS responder reports an error. The server also logs one warning when nothing was announced within 10 seconds. The responder keeps running in that case. `RUST_LOG=info,mdns_sd=debug` shows the messages of the mDNS responder.

Stopping the server withdraws the advertisement (see [Running](#running)). On macOS, `dns-sd -B _waxnerd._tcp` lists the servers on the network.

On Windows, Windows Firewall prompts on the first start. The server needs TCP port 4200 (`server.port`) for the API and UDP port 5353 for mDNS.

## Running as a service

`wn-server run` runs in the foreground, logs to stderr and stops at SIGTERM, so a service manager runs it as it is. The server has no service mode of its own.

Each example runs the server as the user who ran `wn-server init`, so that the server finds the data directory of that user. For another account, or for a data directory at another place, pass `--data-dir` or set `WN_SERVER_DATA_DIR`. The examples name the program at the place where `cargo install` puts it. Replace `USER` and the paths with those of the machine.

A service manager starts the server with an environment of its own. When the directory of ffmpeg is not in the `PATH` of that environment, set `transcoding.ffmpeg` to the full path of ffmpeg.

### macOS: launchd

Save this file as `~/Library/LaunchAgents/io.github.waxnerdhq.wn-server.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.github.waxnerdhq.wn-server</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/USER/.cargo/bin/wn-server</string>
    <string>run</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ExitTimeOut</key>
  <integer>30</integer>
  <key>StandardErrorPath</key>
  <string>/Users/USER/Library/Logs/wn-server.log</string>
</dict>
</plist>
```

Load it:

```
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.github.waxnerdhq.wn-server.plist
```

launchd then starts the server now and at each login of the user, and starts it again when it exits. This command stops the server and removes the agent from launchd:

```
launchctl bootout gui/$(id -u)/io.github.waxnerdhq.wn-server
```

- launchd stops the server with SIGTERM and ends it with SIGKILL after the seconds of `ExitTimeOut`. The two limits of a stop are 10 seconds each (see [Stopping](#stopping)), so the file gives the server 30 seconds.
- launchd does not start the agent with the `PATH` of a shell. For an ffmpeg that Homebrew installed, set `transcoding.ffmpeg` to its full path, such as `/opt/homebrew/bin/ffmpeg`.
- The log is in the file of `StandardErrorPath`.

### Linux: systemd

Save this file as `~/.config/systemd/user/wn-server.service`:

```ini
[Unit]
Description=wn-server

[Service]
ExecStart=%h/.cargo/bin/wn-server run
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable and start it:

```
systemctl --user daemon-reload
systemctl --user enable --now wn-server
```

systemd then starts the server now and at each login of the user, and stops it with SIGTERM. `loginctl enable-linger USER` makes systemd start the services of that user at boot and keep them after the logout. `journalctl --user -u wn-server` shows the log.

### Windows: Task Scheduler

Create a task that starts the server when the user logs on:

1. Open Task Scheduler and choose **Create Task**.
2. On **General**, name the task `wn-server` and keep **Run only when user is logged on**.
3. On **Triggers**, add a trigger with **Begin the task: At log on** for the user.
4. On **Actions**, add the action **Start a program** with the program `C:\Users\USER\.cargo\bin\wn-server.exe` and the argument `run`.
5. On **Conditions**, clear **Start the task only if the computer is on AC power**.
6. On **Settings**, clear **Stop the task if it runs longer than**.

The task runs the server in the session of the user, and the server ends when the user logs off. A logoff, a shutdown of the system and the end of the task may end the server without the steps of a stop (see [Stopping](#stopping)). The database is consistent after that.

The server does not run as a Windows service by itself. A service starts before a user logs on. To run the server as a service, use a service wrapper such as NSSM or WinSW with the program `wn-server.exe` and the arguments `--data-dir <directory> run`. The default data directory depends on the account, and a service runs under an account of its own, so the service names the data directory that `init` created.

## Logs

Logs go to stderr. `server.log_level` is the log filter, for example `info` or `warn,wn_server=debug`. The `RUST_LOG` environment variable replaces it when it is set and not empty. The log has one line for each request, at the level `info`: the method, the client (`c`) and the user (`u`), each cut to 64 characters, the HTTP status, and the milliseconds until the response headers. The line is written when the response headers are ready. The lines of `stream` and `getCoverArt` have the level `debug`. A response with an HTTP status from 500 on has a second line at the level `error`. The HTTP 503 of `stream` is the exception: it has the one line of its request, and `stream` logs the reason at the level `info`. The log never holds the query string, the form body or a password, a token, a salt or an API key.

The server writes no log file. `run` and `scan` write the log, and the other commands print their result and their errors only. Colours are written only when stderr is a terminal.

A service manager of [Running as a service](#running-as-a-service) keeps the log in this place:

| Service manager | Place of the log |
|---|---|
| launchd | the file of `StandardErrorPath` |
| systemd | the journal: `journalctl --user -u wn-server` |
| Task Scheduler | none; Task Scheduler does not store the output of a task |

To keep the log of a task, let the task start `cmd.exe` with the arguments `/c C:\Users\USER\.cargo\bin\wn-server.exe run 2>> C:\Users\USER\wn-server.log`.

## Upgrading

To upgrade, stop the server, update the source tree, build and install the newer version as in [Install](#install), and start the server again.

A newer `wn-server` upgrades `library.db` in place. The first command that opens the database (`run`, `scan`, `user` or `init`) applies the schema changes that the database lacks. The tracks, the albums and the artists keep their IDs, and the stars, the ratings, the play counts, the playlists and the play queue stay. A schema change is applied in full or not at all. When one fails, the command prints an error that names the schema change and the schema version that the database was left at, as in `migration 0003_scans_milliseconds_and_drop_has_lyrics.sql failed and the database was left at schema version 2: <cause>`, and exits with status 1. The database is as it was before that schema change.

Stop the server before a newer `wn-server` opens its database, and start the newer one afterwards. A server of the older build that keeps running does not notice the upgrade and keeps writing in the older form. After the upgrade to schema version 3 it stores the times of its scans in seconds, which the newer build reads as milliseconds. The newer build then leaves those scans out of `lastScan` and `lastModified`, so a client is told that the artists did not change after such a scan changed them. The older server reports a wrong `lastModified` of its own until it is restarted.

The upgrade to schema version 3 stores the start and the end of each scan in milliseconds, and counts every scan from before the upgrade as one that changed the library. `lastScan` of `getScanStatus` is the same as before. `lastModified` of `getIndexes` is the end of the latest scan, so a client can read the artists once more after the upgrade.

An older `wn-server` does not open a database that a newer one has upgraded. Every command that opens the database prints `the database has schema version N; this build supports versions 0 to M` and exits with status 1. It changes nothing in the database. There is no downgrade. To be able to return to the older version, stop the server and copy `library.db` before the upgrade. When a file `library.db-wal` is next to it, copy that file as well.

The name of an artist is a spelling from the album artist tags of its files. A file without an album artist tag counts with its artist tag, or as `Various Artists` when it has the compilation flag. A track that was stored at schema version 1 counts with its artist tag until a scan has read its file again. A scan reads the files that changed, and `wn-server scan --full` reads every file, with the one exception that [Scanning](#scanning) names. This changes a name only for an artist whose files spell it in one way in the album artist tags and in another way in the artist tags. Such an artist shows the spelling of the artist tags until then.

A scan stores the title of each track a second time for the search, in the composed form and in lower case. Every scan also sets that text in the rows that do not have it, and it reads no file for that. A row does not have it when it was stored by a build that did not compose the title. Until the first scan of the newer build has ended, `search3` and `search2` do not find such a track by a word of its title that has a letter with a combining mark in the tag; they find the track by its album and its artist. The server scans when it starts unless `library.scan_on_start` is `false`, and `wn-server scan` and `startScan` scan as well. The track keeps its ID, its star, its rating, its play count and its places in playlists.

## Checks and tests

`cargo xtask check` runs the checks of the project, in this order: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and `cargo test --locked --workspace`. It runs on Linux, macOS and Windows. `scripts/check.sh` calls it on Unix.

The tests need no network beyond loopback, no audio hardware and no ffmpeg. They read the five audio files in `tests/fixtures/audio`.

Five tests are marked `#[ignore]` and run on request only:

- Four tests in `src/media/transcode.rs` and `src/media/stream.rs` run ffmpeg. They need ffmpeg with libopus and libmp3lame on `PATH`:

  ```
  cargo test --lib -- --ignored --nocapture real_ffmpeg
  ```

- One test in `src/db/queries/search.rs` builds a library of 100 000 tracks and prints the times of its searches:

  ```
  cargo test --release --lib search::tests::library_of_100_000 -- --ignored --nocapture
  ```

[AGENTS.md](AGENTS.md) holds the rules for a change to this repository.

## Licence

wn-server is licensed under the Apache License, Version 2.0. [LICENSE](LICENSE) holds the text.
