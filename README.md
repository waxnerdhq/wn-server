# wn-server

wn-server is a headless music server for OpenSubsonic clients. The design is described in [docs/design.md](docs/design.md).

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
| `library.folders` | `[]` | music folders; at least one, and each is the absolute path of a directory |
| `library.scan_on_start` | `true` | scan the folders when the server starts |
| `library.rescan_interval_minutes` | `0` | minutes between periodic scans; 0 disables them |
| `transcoding.ffmpeg` | `"ffmpeg"` | ffmpeg program name or path |
| `transcoding.max_concurrent` | `2` | transcodes that run at the same time; at least 1 |
| `transcoding.opus_bitrate_kbps` | `192` | Opus bitrate when the client names none; 32 to 320 |
| `transcoding.mp3_bitrate_kbps` | `320` | MP3 bitrate when the client names none; 32 to 320 |
| `discovery.mdns` | `true` | advertise the server over mDNS |

The file is checked when a command loads it. An error names the key.

The default `server.name` is `wn-server` on every machine. `init` writes it to `config.toml`. Edit `name` in the `[server]` table to give the server another name, for example when two servers run on one network. A name that the file holds is kept as written. The server takes no name from the host name of the machine.

`server.name` holds 1 to 63 bytes of UTF-8. A character outside ASCII takes 2 to 4 bytes. A command that loads the file fails when the name is empty or longer, and the error names `server.name` and the length of the name. Set `name` in the `[server]` table to a name within the limit, or remove the key to get the default. `wn-server config path` prints the path of the file.

A folder path is written as a TOML literal string, in single quotes, so that a backslash in a Windows path is kept as written:

```toml
[library]
folders = ['C:\Music', 'D:\More Music']
```

A folder path is absolute. A command that loads the file fails when `library.folders` is empty, when a path is relative, or when a path names a file, and the error names `library.folders`.

A folder that does not exist or cannot be read is not an error of the file. The drive of the folder may be unplugged, or its network share may be down. `run` and `scan` start without it (see [Running](#running) and [Scanning](#scanning)). `init` is stricter: it refuses a `--folder` that does not exist.

## Running

```
wn-server run
```

`run` loads `config.toml`, listens on `server.bind` and `server.port` (`0.0.0.0:4200` by default) and logs one line with the address, the version, the data directory, the number of music folders and whether authentication is on.

`run` opens `library.db` and needs the owner account in it. `run` creates neither and fails at startup when one is missing. `wn-server init` then creates what is missing and keeps `config.toml` (see [Owner account](#owner-account)).

`run` stores the music folders of `config.toml` in the database before it answers the first request, and then scans them in the background (see [Scanning](#scanning)). The server answers requests while it scans.

`run` starts when a music folder does not exist or cannot be read, for example while a drive is unplugged. It logs one warning at startup that names the folder, and serves the music of the other folders. The folder stays in the list of `getMusicFolders`. Its tracks stay in the library with their stars, ratings and playlist entries; their files cannot be read until the folder is back. The server does not watch for the folder. The next scan after the folder is back reads it (see [Scanning](#scanning) for what starts a scan).

These signals stop the server. The limit is the time that the server gives its requests, and then its scan, to end:

| Signal | OS | Limit |
|---|---|---|
| Ctrl-C | Linux, macOS, Windows | 10 seconds |
| SIGTERM, which `kill`, systemd, launchd and `docker stop` send | Linux, macOS | 10 seconds |
| Ctrl-Break | Windows | 10 seconds |
| Closing the console window | Windows | 1.5 seconds |

Logging off and shutting the system down are not in the table. On Windows either one may end the server without the steps below. Microsoft documents that a console program that has loaded `user32.dll` receives no signal for a logoff or a shutdown, and that Windows ends the programs of a user at logoff before it sends one. The database is consistent after that, and the next start reads the write-ahead log.

The server stops in this order:

1. It withdraws the mDNS advertisement, so clients see the server leave at once (see [Discovery](#discovery)). At the same time it closes its port.
2. The requests that are in progress are answered. A request that is still in progress after the limit is dropped, with one warning in the log.
3. It waits for a running scan to stop, for the limit at most. The scan stops before it reads its next file. It keeps the batches of files that it has stored, and the next scan does the rest. A scan that has not stopped by then is left behind with one warning.
4. It moves the write-ahead log into `library.db` and closes the database.

`run` then exits with status 0. A second signal during the stop ends the server at once: it skips the steps that have not run and exits with status 1. The advertisement is withdrawn in that case as well. The database is consistent after that, and also after the system has ended the process. A scan that was cut short is finished by the next scan.

Windows ends a console program 5 seconds after its window is closed. The limit of 1.5 seconds is for that signal: steps 1 to 3 end within 3 seconds, which leaves 2 seconds for step 4. Step 4 takes longer than that only while a database read or write of the server still runs, or while another program, such as `wn-server scan`, uses the database. Windows then ends the server before the write-ahead log is moved; the log keeps every change, and the next start reads it.

Logs go to stderr. `server.log_level` is the log filter, for example `info` or `warn,wn_server=debug`. The `RUST_LOG` environment variable replaces it when it is set and not empty. The log has one line for each request, at the level `info`: the method, the client (`c`) and the user (`u`), each cut to 64 characters, the HTTP status, and the milliseconds until the response headers. The lines of `stream` and `getCoverArt` have the level `debug`. The log never holds the query string, the form body or a password, a token, a salt or an API key.

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

`seen` is the number of supported audio files. `added`, `changed`, `moved` and `removed` count tracks. `albums` and `artists` are the numbers in the library after the scan. Each error of the scan follows as a line `error: <message>` that names the folder, the path or the file. The command exits with status 0, or with status 1 when the scan recorded an error. Warnings about files that the scan leaves for the next scan go to stderr with the log.

A scan with a music folder that does not exist or cannot be read reads the other folders and records an error that names the folder. `wn-server scan` logs the folder when it starts, prints that error after the summary and exits with status 1. Such a scan deletes no track, in any folder: it keeps the tracks of the unavailable folder, and it also keeps a track of another folder whose file is gone, because that file may have moved to the folder that it could not read. It stores new, changed and moved files of the folders it read. When the folder is back, the next scan finds its tracks under the same IDs, and removes the tracks whose files are gone. To remove the tracks of a folder for good, take the folder out of `library.folders`: a scan that reads every remaining folder in full, and finds music in each, then removes them.

### Symbolic links

The server indexes and serves only the files inside the folders of `library.folders`. A scan resolves each symbolic link that it finds in a music folder and compares the target of the link with the music folders.

- A link to a file or to a directory outside every music folder is not followed. The files behind it are not in the library. Such a link is no error of the scan. The scan logs each one at the `debug` level, and the number of them in each folder at the `info` level.
- A link to a file or to a directory inside a music folder adds no second track. A file that the scan also finds at its own path has its track there. A file that the scan finds through a link only has its track at the path of the link. That is a file in a directory that the scan skips, such as a directory whose name starts with `.`.
- A music folder can itself be a symbolic link.
- A link that cannot be resolved is an error of the scan: its target is gone, or the server may not read the path to it. The scan cannot tell what is behind the link, so it deletes no track while the link is there. Repair the link or remove it.

To add the music of another disk, name its directory in `library.folders`. Do not link it into a music folder.

A library can hold tracks whose files are behind a link to a place outside the music folders. The next scan that reads every folder in full removes these tracks, with their stars, ratings, play counts and playlist entries. To keep them, add the directory that the link points to to `library.folders` before that scan. The tracks then move to the new folder and keep what is attached to them. When the tracks are already removed, adding the folder creates new tracks without those values. The same holds for a track at a path through a link when the scan finds the file at its own path too: the track at the own path of the file stays, and the track at the path through the link is removed.

A hard link is no symbolic link. The server cannot tell a hard link from the file, so a hard link in a music folder to a file outside the folders is indexed and served.

## Upgrading

A newer `wn-server` upgrades `library.db` in place. The first command that opens the database (`run`, `scan`, `user` or `init`) applies the schema changes that the database lacks. The tracks, the albums and the artists keep their IDs, and the stars, the ratings, the play counts, the playlists and the play queue stay. A schema change is applied in full or not at all. When one fails, the command prints an error that names the schema change and the schema version that the database was left at, as in `migration 0003_scans_milliseconds_and_drop_has_lyrics.sql failed and the database was left at schema version 2: <cause>`, and exits with status 1. The database is as it was before that schema change.

Stop the server before a newer `wn-server` opens its database, and start the newer one afterwards. A server of the older build that keeps running does not notice the upgrade and keeps writing in the older form. After the upgrade to schema version 3 it stores the times of its scans in seconds, which the newer build reads as milliseconds. The newer build then leaves those scans out of `lastScan` and `lastModified`, so a client is told that the artists did not change after such a scan changed them. The older server reports a wrong `lastModified` of its own until it is restarted.

The upgrade to schema version 3 stores the start and the end of each scan in milliseconds, and counts every scan from before the upgrade as one that changed the library. `lastScan` of `getScanStatus` is the same as before. `lastModified` of `getIndexes` is the end of the latest scan, so a client can read the artists once more after the upgrade.

An older `wn-server` does not open a database that a newer one has upgraded. Every command that opens the database prints `the database has schema version N; this build supports versions 0 to M` and exits with status 1. It changes nothing in the database. There is no downgrade. To be able to return to the older version, stop the server and copy `library.db` before the upgrade.

The name of an artist is a spelling from the album artist tags of its files. A file without an album artist tag counts with its artist tag. A track that was stored at schema version 1 counts with its artist tag until a scan has read its file again. A scan reads the files that changed, and `wn-server scan --full` reads every file. This changes a name only for an artist whose files spell it in one way in the album artist tags and in another way in the artist tags. Such an artist shows the spelling of the artist tags until then.

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

`auth.disabled_unsafe = true` turns authentication off. This is unsafe: every request that has `v` and `c` then acts as the owner without a password, for anyone who can reach the port. That includes any web page that is open in a browser on the network: every response carries `Access-Control-Allow-Origin: *` (see [API](#api)), so the browser lets the page read the responses. Such a page can then use the server as the owner. It can read and stream the library, and it can call every method, including the methods that change state: stars, ratings, plays, playlists, the play queue and scans. `run` logs a warning at startup while the setting is on.

## API

The server answers OpenSubsonic requests at `/rest/<method>` and `/rest/<method>.view`, for GET and POST. A POST may carry parameters in an `application/x-www-form-urlencoded` body of up to 8 MiB, the credentials included; `getOpenSubsonicExtensions` lists this as the `formPost` extension. When a parameter is in the query string and in the body, the values of the query string come first. Any other path returns HTTP 404.

- `f=json` selects JSON. `f=xml` or no `f` selects XML. Any other value, `jsonp` included, returns error 0 in XML.
- An API error is an HTTP 200 response with the status `failed` and an `error` element that holds the Subsonic error code and a message.
- The check of the user comes before the method. A request without a user gets the authentication error from every method except `getOpenSubsonicExtensions`, also from a method that the server does not serve.
- A method of the OpenSubsonic API that the server does not serve returns error 0 with the message `not supported`. A name that is not an OpenSubsonic method returns error 0 with the message `unknown endpoint`.
- A request that makes the server read its database for more than 5 seconds returns error 0. The server stops the read after that time. The message is the one the method has for a database that it cannot read, such as `the server cannot read its library`, and the log holds the cause.

<!-- The comment lines below hold the places of the methods that have no text in this section yet. A change that adds a handler module replaces the line of the module with its text and keeps the blank lines (docs/design.md section 3.8). -->

- `getMusicFolders` lists the music folders of `config.toml`, each with an integer `id` and the last component of its path as `name`. The list is there before the first scan. A folder that was removed from `config.toml` is listed until a scan has removed its tracks.
- `getArtists` returns the album artists in indexes by the first letter of the name. A leading `The`, `El`, `La`, `Los`, `Las`, `Le` or `Les` is not part of the name for that, a letter with a diacritic counts as its base letter, and a name that starts with no letter from A to Z is in the index `#`. `ignoredArticles` holds the seven articles. `musicFolderId` limits the answer to the artists that have a file in that folder; an ID that no folder has gives no artist, and a value that is no integer returns error 0. An artist has no `coverArt`.
- `getArtist` returns an artist with its albums by year and then by title; an album without a year follows the others. `getAlbum` returns an album with its songs by disc number, then by track number, then by path. `getSong` returns one song. The three need `id`: an absent `id` returns error 10, and a value that is the ID of no such item returns error 70.
- A song has `duration` in whole seconds, `suffix`, `contentType`, `bitRate`, `size`, `discNumber` (1 without a disc number tag), and `bitDepth`, `samplingRate` and `channelCount`, which are 0 when the file gives none. `path` is the path of the file below its music folder, with `/` between the components. A track that a scan kept after another file took its path has no `path`; the errors of that scan name the track. `artist` is the track artist and `artistId` is the ID of the album artist. `starred`, `userRating`, `playCount` and `played` are the values of the user of the request.
- `getGenres` returns each genre tag of the songs with `songCount` and `albumCount`, in the order of the names without regard to case. An album counts for the genre that most of its songs have.

- `getIndexes` and `getMusicDirectory` are for clients that browse by folder. The server shows the library as a tree: the album artists, the albums of an artist and the songs of an album. Every ID in the tree is the ID that the other methods use for the same artist, album or song.
- `getIndexes` returns the artists of `getArtists` in the same indexes, each with `id`, `name`, and `starred` and `userRating` of the user, and `musicFolderId` limits them in the same way. `lastModified` is the time at which the artists last changed, in milliseconds since 1970. It is the end of the last scan that changed the library: a scan that added, changed, moved or removed a song, that changed an album or an artist, for example with a new cover, or that followed a scan which did not end. A scan that finds nothing changed keeps it, also when it was cancelled or could not read a folder, and so does a star or a rating. Two scans that change the library never have the same time. It is 0 until a scan that changed the library has ended. A request whose `ifModifiedSince` is at or after `lastModified` gets `indexes` with `ignoredArticles` and `lastModified` and without the `index` list. While a scan runs, every request gets the artists. A value of either parameter that is no integer returns error 0.
- `getMusicDirectory` needs `id`. The ID of an artist returns the albums of the artist as directories (`isDir` is `true`), by year and then by title. The ID of an album returns its songs, by disc number, then by track number, then by path. `parent` of a song is the ID of its album, and `parent` of an album is the ID of its artist. An absent `id` returns error 10. The ID of a song and a value that is the ID of no artist and of no album return error 70. A track that a scan kept after another file took its path is in no directory; `getAlbum` lists it.

- `getAlbumList2` returns a page of albums as `albumList2`, and `getAlbumList` returns the same albums as directories in `albumList`. `type` is required and is one of `random`, `newest`, `highest`, `frequent`, `recent`, `alphabeticalByName`, `alphabeticalByArtist`, `starred`, `byYear` and `byGenre`; another value returns error 0. `size` is 10 by default, and a value above 500 is lowered to 500. `offset` selects a later page, and `musicFolderId` restricts the list to one music folder. `byYear` needs `fromYear` and `toYear`, and lists the latest year first when `fromYear` is the greater one. An album without a year counts as year 0. `byGenre` needs `genre`. `highest`, `frequent`, `recent` and `starred` list only the albums that the user rated, played or starred, so they are empty until the user has done that. Every list but `random` has a fixed order, so its pages neither repeat nor skip an album. `random` draws its albums anew for every request, and with an `offset` it returns at most the number of albums less the offset.
- `getRandomSongs` returns up to `size` different songs as `randomSongs`: 10 by default and 500 at most. `genre`, `fromYear`, `toYear` and `musicFolderId` restrict the songs. A song without a year counts as year 0.
- `getSongsByGenre` needs `genre` and returns the songs of that genre in the order of their titles as `songsByGenre`. `count` is 10 by default and 500 at most, `offset` selects a later page, and `musicFolderId` restricts the songs to one music folder.
- A `genre` of these methods matches as a whole text and without regard to case. `getGenres` lists spellings that differ by case as separate genres, and a list for one of them holds the albums or songs of all of them.

- `search3` and `search2` return the artists, the albums and the songs whose names hold every word of `query`. `query` is required and may be empty: `query=` and `query=""` match the whole library. A word may be a part of a name, in any letter case: `beat` finds "The Beatles". The words are looked for in the name of an artist, in the title and the artist of an album, and in the title, the album, the artist and the album artist of a song. `%`, `_` and `\` have no special meaning. A double quote is ignored, and so is one `*` at the end of the query. A letter with a diacritic is not its base letter: `edith` does not find "Édith". In the track artist of a song, a letter outside ASCII is found only in the letter case of the tag. `artistCount`, `albumCount` and `songCount` are 20 by default, and a value above 500 counts as 500. `artistOffset`, `albumOffset` and `songOffset` are 0 by default. Each list is in the order of the names, and the order is the same for every page. A client reads the whole library by asking for pages of 500 items or fewer, one after the other, until a page is shorter than its count. `musicFolderId` limits the lists to one music folder. A query of more than 1024 characters or of more than 32 different words returns error 0. `search` is deprecated and returns error 0 with the message `deprecated; use search3`.

- `star` and `unstar` take `id`, `albumId` and `artistId`, each any number of times. `id` takes the ID of a song, an album or an artist. `setRating` takes `id` and `rating`: 1 to 5 sets the rating and 0 removes it. Any other rating returns error 0. An ID that names no item returns error 70. A request changes every item it names or none: after an error no star and no rating of the request is stored. `star` for an item that has a star sets the time of the star to the time of the new request. `unstar` for an item without a star succeeds. `getStarred2` and `getStarred` list the starred artists, albums and songs, the one starred last first, and `musicFolderId` limits them to one music folder. Stars and ratings belong to the user of the request. A song keeps them when its file is renamed or moved, and an album or an artist keeps them when the spelling of its name changes.
- `scrobble` counts a play of each song that `id` names, for the user of the request. `id` is required and may occur any number of times; a song that is named twice gets two plays. `time` is the time of a play in milliseconds since 1970. A request has no `time` or one for each `id`, and a play without a `time` has the time of the request. Every request counts: the same request sent twice counts its plays twice. `playCount` of a song is the number of its plays, and `played` is the latest of their times, so a play that is reported late does not move `played` back. `playCount` of an album is the sum of the play counts of its songs, and `played` is the latest play of one of them; `frequent` and `recent` of the album lists order the albums by these two values. `submission=false`, the "now playing" notification, is accepted and counts nothing, and `getNowPlaying` returns an empty list. Nothing is sent to another service. An absent `id` returns error 10. A `time` that is no integer from 0 to 253402300799999, a number of `time` values that is not the number of `id` values and a `submission` that is neither `true` nor `false` return error 0. An ID that names no song returns error 70. After an error no play of the request is counted. Plays belong to the user of the request. A song keeps them when its file is renamed or moved, and they are deleted with the song when its file is deleted.

- `getPlaylists` lists the playlists of the user of the request by name, each with `owner`, `public`, `songCount`, `duration`, `created`, `changed` and `coverArt`, the cover of its first song. `getPlaylist` adds the songs as `entry`, in the order of the playlist. `createPlaylist` with `name` creates a playlist with the songs of `songId`, in the order of the values; a song that is named twice is in the playlist twice. With `playlistId` it replaces the songs of that playlist and keeps its name and its comment; `name` is not read then. It returns the playlist with its songs to every client, whatever `v` the request has. `deletePlaylist` deletes a playlist; its songs stay in the library.
- `updatePlaylist` takes `playlistId` and any of `name`, `comment`, `public`, `songIndexToRemove` and `songIdToAdd`. An empty `name` keeps the name, and an empty `comment` removes the comment. Each `songIndexToRemove` is the position of a song in the playlist before the request, counted from 0. The songs at these positions are removed, whatever the order of the values is, and then the songs of `songIdToAdd` are added at the end.
- An ID that names no playlist of the user and an ID that names no song return error 70. A `songIndexToRemove` that is no position of the playlist returns error 0. A playlist holds at most 100 000 songs, and a request that would make it longer returns error 0. After an error nothing of the request is stored. A user sees and changes only the playlists of that user; `public` is stored and has no effect. `getPlaylists` with a `username` other than the name of the user of the request returns error 50.
- A playlist keeps a song when its file is renamed or moved. When a file is deleted, the scan that removes the song from the library removes it from every playlist, and the other songs keep their order.

- `savePlayQueue` saves the play queue of the user of the request, so that a client on another device goes on from there. The queue holds the songs of `id` in the order of the values, and a song that is named twice is in the queue twice. `current` is the ID of the current song, and `position` is the position within that song in milliseconds, 0 without a value. Each request replaces the whole queue. `current` must be one of the `id` values (error 0). A request with `id` values and without `current` is accepted, although the OpenSubsonic API requires `current` in it: the songs are saved with no current song, and the `position` of the request is not stored. A request without `id` and without `current` empties the queue. An ID that names no song returns error 70, and a request with more than 100 000 songs returns error 0. After an error the stored queue is the one from before the request.
- `getPlayQueue` returns `playQueue` with the songs as `entry`, `current`, `position`, `username`, `changed` and `changedBy`. `changed` is the time of the last request that saved the queue, in UTC (`2026-10-11T08:15:30Z`), and `changedBy` is the `c` value of that request, cut to 64 characters. The answer holds `playQueue` for an empty queue as well, with `position` 0 and without `current`. A queue that was saved without `current` is returned with its first song as `current` and with `position` 0, because the OpenSubsonic API requires a `current` that is one of the songs when the queue has songs. Navidrome answers in the same way. A user that never saved a queue gets the creation time of the account as `changed` and an empty `changedBy`. A user reads and replaces only the queue of that user.
- The queue keeps a song when its file is renamed or moved. When a file is deleted, the scan that removes the song from the library removes it from the queue, and the other songs keep their order. When the deleted song was the current one, the song that followed it in the queue is the current one, or the last song when none followed, and `position` is 0. In a queue that was saved without `current`, the first song that is left is the current one.

- The server has no lyrics. `getLyrics` returns an empty `lyrics` element, whatever `artist` and `title` are. `getLyricsBySongId` needs `id` and returns an empty `lyricsList` for the ID of a song; an absent `id` returns error 10, and a value that is the ID of no song returns error 70. `getOpenSubsonicExtensions` does not list the `songLyrics` extension.

- `stream` returns the original file of a track, unchanged, with the content type of its format, unless the request asks for a format or for a lower bitrate (see the next items). A lossless file stays lossless otherwise. It answers GET, HEAD and POST. For the original file it supports `Range` with one range (`bytes=100-199`, `bytes=100-`, `bytes=-100`), `If-Range`, `If-None-Match` and `If-Modified-Since`. A range that no byte satisfies gets HTTP 416, and a request for several ranges gets the whole file. The response carries `Accept-Ranges: bytes`, `Content-Length`, `ETag`, `Last-Modified` and a `Content-Disposition` with the file name.
- `stream` returns error 70 for an ID that names no track and for a track whose file it does not serve. That is a file that is gone, a file that changed on disk since the last scan (another size or another modification time), and a file outside the music folders. A scan gives no track to a file outside the music folders (see [Symbolic links](#symbolic-links)). A track has such a file when a symbolic link changed after the last scan, and when its file is behind a link that leads out of the folders and no scan has removed the track yet. Scan after you change files. A file whose modification time is before 1970 returns error 0.
- `stream` transcodes with ffmpeg when the request asks for it. `format=opus` returns Opus in Ogg (`audio/ogg`), and `format=mp3` returns MP3 (`audio/mpeg`). The bitrate is `maxBitRate` in kbps, or `transcoding.opus_bitrate_kbps` or `transcoding.mp3_bitrate_kbps` when the request has no `maxBitRate`; a transcode has 32 to 320 kbps. A file that has the requested format already is returned as it is, unless its bitrate is above `maxBitRate`: `format=mp3` on an MP3 file does not encode the file again. `maxBitRate` without `format` returns Opus at that bitrate when the bitrate of the file is higher, and the original file otherwise. `maxBitRate=0` is no limit. `format=raw` returns the original file whatever `maxBitRate` is. The letter case of a `format` has no meaning, and a `format` that names none of `raw`, `opus` and `mp3` is read as no `format`.
- A transcoded stream carries `Accept-Ranges: none` and `Cache-Control: no-store`. `Range` has no effect on it. `timeOffset=N` starts the stream at the second N of the track; `getOpenSubsonicExtensions` lists this as the `transcodeOffset` extension.
- A transcoded stream never has a `Content-Length`, and the server sends every byte of it. `estimateContentLength` is accepted and has no effect. The server does not know the length of a stream before ffmpeg has written it. The parameter asks for an estimate, the bitrate times the duration. Measured Opus streams had between two thirds of that length and a tenth more, and a stream that is cut at such a length loses the end of the track.
- At most `transcoding.max_concurrent` transcodes run at the same time. A request past the limit waits up to 5 seconds and then gets HTTP 503 with `Retry-After: 5`. A client that disconnects ends its transcode.
- Transcoding needs ffmpeg with libopus and libmp3lame. The server does not install or bundle it. It runs the program `ffmpeg` from `PATH`, or the program that `transcoding.ffmpeg` names. `run` logs at startup whether it found ffmpeg. Without ffmpeg the server starts with one warning and serves original files, and a request that needs a transcode returns error 0 with the message `ffmpeg not available`. Restart the server after you install ffmpeg.
- ffmpeg reads a file of the library only as a local file in one of the audio containers that the server scans (FLAC, MP3, MP4, ADTS AAC, Ogg, WAV, AIFF). A file with an audio file name that is a playlist or a script is refused, so it cannot make ffmpeg read another file or open a network connection. One case rests on a default of ffmpeg and not on the server: an MP4 file can name a second file as the place of its audio data. ffmpeg does not follow such a reference unless its option `enable_drefs` is set, and the server does not set it. With an ffmpeg that follows it, a crafted MP4 file in the library would make ffmpeg read another local file, and the stream would go to the client, which is the owner. When ffmpeg fails for a file, the log has one warning with the exit status of ffmpeg and the ID of the track, and the stream of that request is empty or incomplete. The messages of ffmpeg itself are logged at the `debug` level.

- `getCoverArt` returns the cover whose ID the `coverArt` of an album or of a song holds. The cover of an album is the image file `cover`, `folder`, `front` or `album` with the extension `jpg`, `jpeg`, `png` or `webp` in the directory of its first track. An album whose discs have directories of their own, such as `Album/CD1` and `Album/CD2`, has the image file of their parent directory when the directory of its first track holds none. That needs two or more disc directories with one parent directory, and no track of another album at or below that directory. An album without such an image file has the embedded picture of its first track that has one. Without `size`, and with `size=0`, the body is the image as it is stored, as `image/jpeg`, `image/png` or `image/webp`. With `size=N` the body is a JPEG at the step of N: the smallest of 80, 160, 320, 640, 1280 and 2000 pixels that is not smaller than N, and 2000 for a larger N. Its longer side is at most the step, so it can be larger than N, and no image is enlarged. A JPEG whose Exif metadata declares an orientation is turned and mirrored accordingly when it is resized, and the resized cover holds no metadata. The image as it is stored keeps its bytes and its metadata, and the client applies the orientation. The sizes of one step get the same bytes and the same `ETag`: `size=290`, `size=300` and `size=310` all get the cover of 320 pixels. The server keeps at most six resized files for a cover, in `cache/covers` of the data directory. A cover whose file, or whose audio file with the picture, is reached through a symbolic link to a place outside every folder of `library.folders` returns error 70, as `stream` does for such a track.
- `getCoverArt` needs `id`: an absent `id` returns error 10. A `size` that is no integer from 0 to 4294967295 returns error 0. An `id` without a cover returns error 70: an ID that the server does not have, the ID of an album without a cover, the ID of a song whose file holds no picture, and the ID of an artist. An item without `coverArt` has no cover, and the server sends no placeholder image. An error is the usual response in XML or JSON.
- A cover comes with `Content-Length`, `Cache-Control: public, max-age=86400` and an `ETag`. A request that sends that entity tag in `If-None-Match` gets HTTP 304 without a body. The entity tag changes when a scan finds the cover file, or the audio file with the picture, with another modification time. The entity tag of a resized cover also changes with a version of the server that makes resized covers in another way. A HEAD request gets the headers without the body. `Range` is not read: the answer is the whole image.

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

Stopping the server withdraws the advertisement (see [Running](#running)). On macOS, `dns-sd -B _waxnerd._tcp` lists the servers on the network.

On Windows, Windows Firewall prompts on the first start. The server needs TCP port 4200 (`server.port`) for the API and UDP port 5353 for mDNS.
