//! The end-to-end session: an OpenSubsonic client that calls a running server
//! over TCP and checks each answer.
//!
//! ```text
//! cargo run --example e2e -- <base url> <user> <password> <library directory>
//! ```
//!
//! `<base url>` is the address of the server, such as `http://127.0.0.1:4200`.
//! `<user>` and `<password>` are those of the owner account. `<library
//! directory>` is the directory that `examples/make-library.rs` wrote and that
//! the server has as its one music folder: the session compares the bytes
//! that the server sends with the files in it. The session therefore runs on
//! the machine of the server.
//!
//! Each step prints one line, `PASS <step>` or `FAIL <step>: <detail>`, and
//! the last line holds the number of steps, the number of failed steps and
//! the time. The exit status is 0 when every step passed, 1 when one failed
//! and 2 for wrong arguments. A session ends within 60 seconds: a request
//! that is sent later fails, a response is read no longer than that, and the
//! step of either fails.
//!
//! The session leaves the server as it found it, apart from three values:
//! one more play of one track, the time of a star that it found and the time
//! of the play queue. It removes the star and the playlist that it creates,
//! it sets a star again that it found, and it ends with the play queue that
//! it found. It deletes no playlist but the one that it created. A session
//! that ended early can leave a star, a playlist with the name of
//! [`PLAYLIST_NAME`] and its play queue behind. The next session passes on
//! such a server: it keeps the star and the playlist, and it does not put
//! such a queue back.
//!
//! The client speaks HTTP/1.1 over `std::net::TcpStream` with one connection
//! for each request. It reads a body with `Content-Length`, a chunked body
//! and a body that ends with the connection, which is all that the server
//! sends.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde_json::Value;

const USAGE: &str =
    "usage: cargo run --example e2e -- <base url> <user> <password> <library directory>";

/// The value of `c` in every request. `getPlayQueue` returns it as
/// `changedBy` of a queue that the session saved.
const CLIENT: &str = "wn-server-e2e";

/// The value of `v` in every request.
const API_VERSION: &str = "1.16.1";

/// The time after which the session sends no request.
const SESSION_LIMIT: Duration = Duration::from_secs(60);

/// The longest time a request waits for its connection, for a write and for
/// the next bytes of its response. A wait also ends when the time of the
/// session is over. A transcode request waits up to 15 s for its first byte
/// (`docs/design.md` section 3.9).
const REQUEST_LIMIT: Duration = Duration::from_secs(20);

/// The most bytes the client reads of one response.
const RESPONSE_LIMIT: usize = 64 * 1024 * 1024;

/// The longest time the session waits for a scan of the library to end.
const SCAN_LIMIT: Duration = Duration::from_secs(20);

/// The time between two `getScanStatus` requests of that wait.
const SCAN_POLL: Duration = Duration::from_millis(100);

/// The name of the playlist of the session. The session reads and deletes
/// its playlist by the ID that `createPlaylist` returns, so another playlist
/// with this name has no effect on it and stays.
const PLAYLIST_NAME: &str = "wn-server e2e session";

/// The extensions that `getOpenSubsonicExtensions` lists, with their
/// versions (`docs/design.md` section 4).
const EXTENSIONS: [(&str, &[u64]); 2] = [("formPost", &[1]), ("transcodeOffset", &[1])];

/// The artists of the library with the number of their albums.
const ARTISTS: [(&str, u64); 3] = [
    ("Aurora Field", 2),
    ("Marlow Quartet", 1),
    ("Various Artists", 1),
];

/// The albums of the library.
const ALBUMS: [&str; 4] = [
    "Glass Harbour",
    "Paper Maps",
    "Slow Rivers",
    "Summer Sampler",
];

/// The tracks of the library.
const SONGS: [&str; 10] = [
    "Breakwater",
    "Contour",
    "Delta",
    "Last Train",
    "Legend",
    "Low Tide",
    "Meridian",
    "Night Ferry",
    "Open Road",
    "Upstream",
];

/// The tracks of "Glass Harbour" in the order of the album. They are FLAC
/// files.
const GLASS_HARBOUR: [&str; 3] = ["Low Tide", "Breakwater", "Night Ferry"];

/// The tracks of "Paper Maps" in the order of the album: two of disc 1 and
/// one of disc 2. They are MP3 files.
const PAPER_MAPS: [&str; 3] = ["Meridian", "Contour", "Legend"];

/// The steps of the session, in the order in which they run. A later step
/// uses the IDs that an earlier one read.
const STEPS: &[(&str, Step)] = &[
    ("ping with a token", Session::ping),
    (
        "ping with a wrong token is error 40",
        Session::ping_wrong_token,
    ),
    ("ping with a form body (formPost)", Session::ping_form_post),
    ("ping at /rest/ping.view", Session::ping_view),
    (
        "getOpenSubsonicExtensions without a user lists formPost and transcodeOffset",
        Session::extensions,
    ),
    ("CORS headers and the preflight request", Session::cors),
    (
        "startScan, and getScanStatus until the scan has ended",
        Session::scan,
    ),
    ("getMusicFolders", Session::music_folders),
    ("getArtists", Session::artists),
    ("getArtist", Session::artist),
    ("getAlbum", Session::album),
    ("getAlbum as XML (f=xml)", Session::album_xml),
    ("getSong", Session::song),
    ("search3 with an empty query", Session::search_all),
    ("search3 with a word", Session::search_word),
    ("getAlbumList2 type=newest", Session::album_list),
    ("getCoverArt size=64 is a JPEG", Session::cover_small),
    (
        "getCoverArt with a size between two steps answers the larger step",
        Session::cover_step,
    ),
    (
        "getCoverArt without size is the image file",
        Session::cover_stored,
    ),
    ("stream is the file", Session::stream_raw),
    ("stream with Range: bytes=100-199", Session::stream_range),
    (
        "stream with a Range of another unit is the whole file",
        Session::stream_other_unit,
    ),
    (
        "stream format=opus maxBitRate=96 is Ogg without Content-Length",
        Session::stream_opus,
    ),
    (
        "stream format=mp3 is MP3 without Content-Length",
        Session::stream_mp3,
    ),
    (
        "stream format=opus timeOffset=1 (transcodeOffset)",
        Session::stream_offset,
    ),
    (
        "stream format=mp3 of an MP3 file is the file",
        Session::stream_mp3_of_mp3,
    ),
    ("star, getStarred2, unstar", Session::star),
    ("scrobble, getSong", Session::scrobble),
    (
        "createPlaylist, getPlaylist, deletePlaylist",
        Session::playlist,
    ),
    (
        "savePlayQueue with current, getPlayQueue",
        Session::queue_with_current,
    ),
    (
        "savePlayQueue without current, getPlayQueue names the first song",
        Session::queue_without_current,
    ),
    (
        "savePlayQueue without a song, getPlayQueue",
        Session::queue_without_song,
    ),
    (
        "getLyricsBySongId is an empty lyricsList",
        Session::lyrics_by_song_id,
    ),
    ("getLyrics is an empty text", Session::lyrics),
    ("download is error 50", Session::download),
];

/// One step of the session. An error is the detail of its `FAIL` line.
type Step = fn(&mut Session) -> Result<(), String>;

fn main() -> ExitCode {
    // The library directory is a path, which need not be Unicode.
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let [base_url, user, password, library] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let (Some(base_url), Some(user), Some(password)) =
        (base_url.to_str(), user.to_str(), password.to_str())
    else {
        eprintln!("e2e: the base URL, the user and the password must be Unicode text");
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let started = Instant::now();
    let client = match Client::new(base_url, user, password, started + SESSION_LIMIT) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("e2e: {error}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut session = Session::new(client, PathBuf::from(library));

    let mut failed = 0;
    for (name, step) in STEPS {
        match step(&mut session) {
            Ok(()) => println!("PASS {name}"),
            Err(detail) => {
                failed += 1;
                println!("FAIL {name}: {}", one_line(&detail));
            }
        }
    }
    let elapsed = started.elapsed();
    let mut steps = STEPS.len();
    if elapsed >= SESSION_LIMIT {
        // A request that was sent within the limit can end after it.
        steps += 1;
        failed += 1;
        println!(
            "FAIL the session ends within {} seconds",
            SESSION_LIMIT.as_secs()
        );
    }
    println!(
        "{steps} steps, {failed} failed, {:.1} s",
        elapsed.as_secs_f64()
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `detail` as one line of at most 400 characters.
fn one_line(detail: &str) -> String {
    let line: String = detail
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(400)
        .collect();
    if detail.chars().count() > 400 {
        format!("{line}…")
    } else {
        line
    }
}

/// `Ok` when `condition` holds, and the text of `detail` as the error
/// otherwise.
fn ensure(condition: bool, detail: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(detail()) }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// One HTTP response.
struct Reply {
    status: u16,
    /// The headers, with their names in lower case.
    headers: Vec<(String, String)>,
    /// The body, without the framing of a chunked transfer.
    body: Vec<u8>,
}

impl Reply {
    /// The value of the header `name`, which is in lower case.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The status, the content type and the length of the body, for the
    /// detail of a failed step. The start of a JSON or XML body follows.
    fn summary(&self) -> String {
        let content_type = self.header("content-type").unwrap_or_default();
        let summary = format!(
            "HTTP {} with Content-Type {content_type:?} and {} bytes",
            self.status,
            self.body.len()
        );
        if content_type.ends_with("json") || content_type.ends_with("xml") {
            let start = &self.body[..self.body.len().min(300)];
            format!("{summary}: {}", String::from_utf8_lossy(start))
        } else {
            summary
        }
    }

    /// Checks the status.
    fn expect_status(&self, status: u16) -> Result<(), String> {
        ensure(self.status == status, || {
            format!("expected HTTP {status}, got {}", self.summary())
        })
    }

    /// Checks that the header `name` has the value `value`.
    fn expect_header(&self, name: &str, value: &str) -> Result<(), String> {
        ensure(self.header(name) == Some(value), || {
            let found = match self.header(name) {
                Some(found) => format!("{name}: {found}"),
                None => format!("no {name}"),
            };
            format!(
                "expected {name}: {value}, got {found} in {}",
                self.summary()
            )
        })
    }

    /// Checks that the response is HTTP 200 with the content type
    /// `content_type`. An API error is HTTP 200 with the envelope, so the
    /// content type tells it from the bytes of a file or of an image.
    fn expect_media(&self, content_type: &str) -> Result<(), String> {
        self.expect_status(200)?;
        self.expect_header("content-type", content_type)
    }

    /// The value of `subsonic-response` of a JSON answer, with either
    /// status.
    fn envelope(&self) -> Result<Value, String> {
        self.expect_media("application/json")?;
        let document: Value = serde_json::from_slice(&self.body)
            .map_err(|error| format!("{error} in {}", self.summary()))?;
        document
            .get("subsonic-response")
            .cloned()
            .ok_or_else(|| format!("no subsonic-response in {}", self.summary()))
    }

    /// The value of `subsonic-response` of an answer with the status `ok`.
    fn ok(&self) -> Result<Value, String> {
        let envelope = self.envelope()?;
        ensure(envelope["status"] == "ok", || {
            format!("the answer is {envelope}")
        })?;
        Ok(envelope)
    }

    /// Checks that the answer is the error with the code `code`.
    fn expect_error(&self, code: u64) -> Result<(), String> {
        let envelope = self.envelope()?;
        ensure(
            envelope["status"] == "failed" && envelope["error"]["code"] == code,
            || format!("expected error {code}, got {envelope}"),
        )
    }
}

/// The HTTP client of the session and the account that it signs in with.
struct Client {
    /// The host of the base URL, without the brackets of an IPv6 address.
    host: String,
    port: u16,
    /// The host and the port as the base URL holds them, for the `Host`
    /// header.
    authority: String,
    /// The path of the base URL without a `/` at its end. The path of a
    /// request follows it.
    prefix: String,
    user: String,
    password: String,
    /// The time after which no request is sent.
    deadline: Instant,
}

impl Client {
    /// A client for the server at `base_url`, which has the form
    /// `http://<host>[:<port>][/<path>]`.
    fn new(base_url: &str, user: &str, password: &str, deadline: Instant) -> Result<Self, String> {
        let rest = base_url
            .strip_prefix("http://")
            .ok_or_else(|| format!("the base URL {base_url:?} does not start with http://"))?;
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        let (host, port) = match authority.strip_prefix('[') {
            // An IPv6 address is in brackets, and its colons are no port.
            Some(bracketed) => {
                let (host, after) = bracketed
                    .split_once(']')
                    .ok_or_else(|| format!("the base URL {base_url:?} has no ]"))?;
                (host, after.strip_prefix(':'))
            }
            None => match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            },
        };
        let port = match port {
            Some(port) => port
                .parse()
                .map_err(|_| format!("the base URL {base_url:?} has no valid port"))?,
            None => 80,
        };
        if host.is_empty() {
            return Err(format!("the base URL {base_url:?} has no host"));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            authority: authority.to_owned(),
            prefix: path.trim_end_matches('/').to_owned(),
            user: user.to_owned(),
            password: password.to_owned(),
            deadline,
        })
    }

    /// `params`, then the credentials of the account with a token for
    /// `password` and a new salt, then `f=json` unless `params` name a
    /// format. The text is a query string or a form body.
    fn fields(&self, params: &[(&str, &str)], password: &str) -> String {
        let salt = salt();
        let token = token(password, &salt);
        let mut fields = params.to_vec();
        fields.extend([
            ("u", self.user.as_str()),
            ("t", token.as_str()),
            ("s", salt.as_str()),
            ("v", API_VERSION),
            ("c", CLIENT),
        ]);
        if !params.iter().any(|(name, _)| *name == "f") {
            fields.push(("f", "json"));
        }
        urlencode(&fields)
    }

    /// Sends `GET /rest/<method>` with `params` and the credentials in the
    /// query string and with `headers`.
    fn get_with_headers(
        &self,
        method: &str,
        params: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<Reply, String> {
        let query = self.fields(params, &self.password);
        self.send("GET", &format!("/rest/{method}?{query}"), headers, &[])
    }

    /// Sends `GET /rest/<method>` with `params` and the credentials.
    fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<Reply, String> {
        self.get_with_headers(method, params, &[])
    }

    /// Sends `GET /rest/<method>` and returns the value of
    /// `subsonic-response` of an answer with the status `ok`.
    fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        self.get(method, params)?
            .ok()
            .map_err(|error| format!("{method}: {error}"))
    }

    /// The time that the session has left. An error when that time is over.
    fn time_left(&self) -> Result<Duration, String> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "the {} seconds of the session are over",
                SESSION_LIMIT.as_secs()
            ));
        }
        Ok(left)
    }

    /// Sends one request on a new connection and reads the response to the
    /// end of the connection. `path` follows the path of the base URL.
    fn send(
        &self,
        verb: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<Reply, String> {
        let named = |error: String| format!("{verb} {}: {error}", request_name(path));
        let limit = self.time_left()?.min(REQUEST_LIMIT);

        let mut request = format!(
            "{verb} {}{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.prefix, self.authority
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");

        let mut stream = self.connect(limit)?;
        stream
            .set_write_timeout(Some(limit))
            .and_then(|()| stream.write_all(request.as_bytes()))
            .and_then(|()| stream.write_all(body))
            .map_err(|error| named(waited(&error, limit)))?;
        let received = self.receive(&mut stream).map_err(named)?;
        parse_reply(verb, &received).map_err(named)
    }

    /// Reads a response to the end of the connection. Each read waits for
    /// [`REQUEST_LIMIT`] at most, and not longer than the session has left:
    /// a response that arrives in small parts does not keep a step running
    /// past the end of the session.
    fn receive(&self, stream: &mut TcpStream) -> Result<Vec<u8>, String> {
        let mut received = Vec::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let wait = self.time_left()?.min(REQUEST_LIMIT);
            stream
                .set_read_timeout(Some(wait))
                .map_err(|error| error.to_string())?;
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(received),
                Ok(count) => received.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(waited(&error, wait)),
            }
            if received.len() > RESPONSE_LIMIT {
                return Err(format!("the response has more than {RESPONSE_LIMIT} bytes"));
            }
        }
    }

    /// Connects to the first address of the host that accepts a connection.
    fn connect(&self, limit: Duration) -> Result<TcpStream, String> {
        let addresses = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .map_err(|error| format!("{}: {error}", self.authority))?;
        let mut last = format!("{} has no address", self.authority);
        for address in addresses {
            match TcpStream::connect_timeout(&address, limit) {
                Ok(stream) => return Ok(stream),
                Err(error) => last = format!("connect to {address}: {error}"),
            }
        }
        Err(last)
    }
}

/// The text of the error of a read or of a write that waited for `limit` at
/// most. A wait that reached its limit is the error `WouldBlock` on Unix and
/// `TimedOut` on Windows, and the text of the system for it does not name the
/// limit.
fn waited(error: &io::Error, limit: Duration) -> String {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut if limit < REQUEST_LIMIT => format!(
            "no byte before the {} seconds of the session were over",
            SESSION_LIMIT.as_secs()
        ),
        ErrorKind::WouldBlock | ErrorKind::TimedOut => {
            format!("no byte within {} s", limit.as_secs())
        }
        _ => error.to_string(),
    }
}

/// The path of a request without its query string, which holds a token.
fn request_name(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

/// Parses the bytes of one response to a request with the method `verb`.
fn parse_reply(verb: &str, received: &[u8]) -> Result<Reply, String> {
    let head_end = find(received, b"\r\n\r\n").ok_or_else(|| {
        format!(
            "the response of {} bytes has no end of its headers",
            received.len()
        )
    })?;
    let head = std::str::from_utf8(&received[..head_end])
        .map_err(|error| format!("the response headers are no UTF-8: {error}"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("the status line is {status_line:?}"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let rest = &received[head_end + 4..];
    let chunked = header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    let body = if verb == "HEAD" || matches!(status, 204 | 304) {
        // These responses have no body, whatever their headers say.
        ensure(rest.is_empty(), || {
            format!("HTTP {status} has a body of {} bytes", rest.len())
        })?;
        Vec::new()
    } else if chunked {
        dechunk(rest)?
    } else if let Some(length) = header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| format!("Content-Length is {length:?}"))?;
        ensure(rest.len() == length, || {
            format!(
                "the body has {} bytes and Content-Length is {length}",
                rest.len()
            )
        })?;
        rest.to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Reply {
        status,
        headers,
        body,
    })
}

/// The body of a chunked transfer. A transfer without its last chunk is an
/// error: the server ended the connection before the end of the body.
fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n")
            .ok_or_else(|| "the chunked body ends without its last chunk".to_owned())?;
        let line = String::from_utf8_lossy(&rest[..line_end]);
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("the chunked body has the chunk size {line:?}"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest
            .get(..size)
            .filter(|_| rest.get(size..size + 2) == Some(b"\r\n"))
            .ok_or_else(|| "the chunked body ends within a chunk".to_owned())?;
        body.extend_from_slice(chunk);
        rest = &rest[size + 2..];
    }
}

/// The position of the first `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A new salt: 16 hex characters.
fn salt() -> String {
    let mut bytes = [0_u8; 8];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// The Subsonic token of `password` for `salt`: the MD5 digest of the
/// password followed by the salt, in lower-case hex.
fn token(password: &str, salt: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(password.as_bytes());
    hasher.update(salt.as_bytes());
    hex::encode(hasher.finalize())
}

/// Encodes `fields` as `application/x-www-form-urlencoded` data, which is
/// also the form of a query string. Every byte outside the unreserved set of
/// RFC 3986 is a percent escape.
fn urlencode(fields: &[(&str, &str)]) -> String {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                _ => format!("%{byte:02X}"),
            })
            .collect()
    };
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

// ---------------------------------------------------------------------------
// JSON and XML
// ---------------------------------------------------------------------------

/// The text at the JSON pointer `pointer` of `value`.
fn text<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{pointer} is no text in {value}"))
}

/// The list at the JSON pointer `pointer` of `value`.
fn list<'a>(value: &'a Value, pointer: &str) -> Result<&'a [Value], String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{pointer} is no list in {value}"))
}

/// The text that each item of `items` has under `key`. An item without such
/// a text gives an empty one.
fn texts<'a>(items: &'a [Value], key: &str) -> Vec<&'a str> {
    items
        .iter()
        .map(|item| item[key].as_str().unwrap_or_default())
        .collect()
}

/// `names` in the order of their bytes.
fn sorted<'a>(names: &[&'a str]) -> Vec<&'a str> {
    let mut names = names.to_vec();
    names.sort_unstable();
    names
}

/// One element of an XML document: its name and its attributes.
struct Element {
    name: String,
    attributes: BTreeMap<String, String>,
}

/// The elements of the document `xml` in the order of their start tags.
fn elements(xml: &str) -> Result<Vec<Element>, String> {
    let element = |start: &BytesStart<'_>| -> Result<Element, String> {
        let mut attributes = BTreeMap::new();
        for attribute in start.attributes() {
            let attribute = attribute.map_err(|error| format!("{error} in {xml}"))?;
            let value = attribute
                .normalized_value(XmlVersion::Explicit1_0)
                .map_err(|error| format!("{error} in {xml}"))?;
            attributes.insert(attribute.key.0.to_owned(), value.into_owned());
        }
        Ok(Element {
            name: start.name().0.to_owned(),
            attributes,
        })
    };
    let mut reader = Reader::from_str(xml);
    let mut found = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| format!("{error} in {xml}"))?
        {
            Event::Start(start) | Event::Empty(start) => found.push(element(&start)?),
            Event::Eof => return Ok(found),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

/// An album that a step read.
struct Album {
    id: String,
    /// The cover art ID. An album without a cover has none.
    cover_art: Option<String>,
}

/// A track that a step read.
struct Song {
    id: String,
    /// The path of the file below the music folder, with `/` between its
    /// components.
    path: String,
}

/// The play queue that the session found.
struct Queue {
    ids: Vec<String>,
    current: Option<String>,
    position: u64,
    changed_by: String,
}

/// The state of a session: the client, the library on disk and what the
/// steps read so far.
struct Session {
    client: Client,
    /// The directory of the library, which is the music folder of the server.
    library: PathBuf,
    /// The artist IDs by name.
    artists: BTreeMap<String, String>,
    /// The albums by title.
    albums: BTreeMap<String, Album>,
    /// The tracks by title.
    songs: BTreeMap<String, Song>,
    /// The play queue from before the first step that saves one.
    queue_before: Option<Queue>,
}

impl Session {
    fn new(client: Client, library: PathBuf) -> Self {
        Self {
            client,
            library,
            artists: BTreeMap::new(),
            albums: BTreeMap::new(),
            songs: BTreeMap::new(),
            queue_before: None,
        }
    }

    /// The ID of the artist `name`, which an earlier step read.
    fn artist_id(&self, name: &str) -> Result<&str, String> {
        self.artists
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("no earlier step read the artist {name:?}"))
    }

    /// The album `title`, which an earlier step read.
    fn album_of(&self, title: &str) -> Result<&Album, String> {
        self.albums
            .get(title)
            .ok_or_else(|| format!("no earlier step read the album {title:?}"))
    }

    /// The cover art ID of the album `title`, which an earlier step read.
    fn cover_of(&self, title: &str) -> Result<&str, String> {
        self.album_of(title)?
            .cover_art
            .as_deref()
            .ok_or_else(|| format!("the album {title:?} has no coverArt"))
    }

    /// The track `title`, which an earlier step read.
    fn song_of(&self, title: &str) -> Result<&Song, String> {
        self.songs
            .get(title)
            .ok_or_else(|| format!("no earlier step read the track {title:?}"))
    }

    /// Keeps the ID and the cover art ID of the album element `album` for
    /// the steps that follow. A step calls this before it checks the
    /// element, so that a failed check does not fail the later steps too.
    fn keep_album(&mut self, album: &Value) {
        if let (Some(name), Some(id)) = (album["name"].as_str(), album["id"].as_str()) {
            let cover_art = album["coverArt"].as_str().map(str::to_owned);
            let id = id.to_owned();
            self.albums
                .entry(name.to_owned())
                .or_insert(Album { id, cover_art });
        }
    }

    /// Keeps the ID and the path of the song element `song` for the steps
    /// that follow, as [`Session::keep_album`] does for an album.
    fn keep_song(&mut self, song: &Value) {
        let fields = (
            song["title"].as_str(),
            song["id"].as_str(),
            song["path"].as_str(),
        );
        if let (Some(title), Some(id), Some(path)) = fields {
            let (id, path) = (id.to_owned(), path.to_owned());
            self.songs
                .entry(title.to_owned())
                .or_insert(Song { id, path });
        }
    }

    /// The bytes of the file at `rel_path` below the library directory.
    /// `rel_path` has `/` between its components.
    fn file(&self, rel_path: &str) -> Result<Vec<u8>, String> {
        let mut path = self.library.clone();
        path.extend(rel_path.split('/'));
        fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
    }

    // -- the system methods --------------------------------------------------

    fn ping(&mut self) -> Result<(), String> {
        let answer = self.client.call("ping", &[])?;
        ensure(
            answer["version"] == API_VERSION
                && answer["type"] == "wn-server"
                && answer["openSubsonic"] == true
                && answer["serverVersion"].is_string(),
            || format!("the answer is {answer}"),
        )
    }

    fn ping_wrong_token(&mut self) -> Result<(), String> {
        let wrong = format!("{}x", self.client.password);
        let query = self.client.fields(&[], &wrong);
        self.client
            .send("GET", &format!("/rest/ping?{query}"), &[], &[])?
            .expect_error(40)
    }

    fn ping_form_post(&mut self) -> Result<(), String> {
        // The credentials are in the body, and the query string is absent.
        let body = self.client.fields(&[], &self.client.password);
        let headers = [("Content-Type", "application/x-www-form-urlencoded")];
        self.client
            .send("POST", "/rest/ping", &headers, body.as_bytes())?
            .ok()
            .map(drop)
    }

    fn ping_view(&mut self) -> Result<(), String> {
        self.client.call("ping.view", &[]).map(drop)
    }

    fn extensions(&mut self) -> Result<(), String> {
        // The method is public: the request has no parameter but the format.
        let answer = self
            .client
            .send("GET", "/rest/getOpenSubsonicExtensions?f=json", &[], &[])?
            .ok()?;
        let mut listed: Vec<(&str, Vec<u64>)> = Vec::new();
        for extension in list(&answer, "/openSubsonicExtensions")? {
            let versions = list(extension, "/versions")?
                .iter()
                .filter_map(Value::as_u64)
                .collect();
            listed.push((text(extension, "/name")?, versions));
        }
        listed.sort();
        let expected: Vec<(&str, Vec<u64>)> = EXTENSIONS
            .iter()
            .map(|(name, versions)| (*name, versions.to_vec()))
            .collect();
        ensure(listed == expected, || {
            format!("expected {expected:?}, got {listed:?}")
        })
    }

    fn cors(&mut self) -> Result<(), String> {
        // A preflight request has no parameter and no user.
        let preflight = self.client.send("OPTIONS", "/rest/ping", &[], &[])?;
        preflight.expect_status(204)?;
        preflight.expect_header("access-control-allow-origin", "*")?;
        preflight.expect_header("access-control-allow-methods", "GET, POST, OPTIONS")?;
        preflight.expect_header("access-control-allow-headers", "Content-Type, Range")?;

        let answer = self.client.get("ping", &[])?;
        answer.expect_header("access-control-allow-origin", "*")?;
        answer.expect_header(
            "access-control-expose-headers",
            "Content-Range, Accept-Ranges, Content-Length",
        )
    }

    // -- the scan ------------------------------------------------------------

    /// Starts a scan and waits for its end. The step runs before the steps
    /// that read the library, so they read a library that a scan has stored,
    /// also when the session starts while the scan of the server start runs.
    fn scan(&mut self) -> Result<(), String> {
        let started = self.client.call("startScan", &[])?;
        ensure(started["scanStatus"]["scanning"] == true, || {
            format!("startScan answers {started}")
        })?;

        let waiting = Instant::now();
        let status = loop {
            let status = self.client.call("getScanStatus", &[])?;
            if status["scanStatus"]["scanning"] == false {
                break status;
            }
            if waiting.elapsed() > SCAN_LIMIT {
                return Err(format!(
                    "the scan has not ended after {} s: {status}",
                    SCAN_LIMIT.as_secs()
                ));
            }
            thread::sleep(SCAN_POLL);
        };
        let tracks = SONGS.len() as u64;
        ensure(status["scanStatus"]["count"] == tracks, || {
            format!("expected count {tracks}, got {status}")
        })?;
        // The end time has the form 2026-10-11T08:15:30Z.
        let last_scan = text(&status, "/scanStatus/lastScan")?;
        ensure(last_scan.len() == 20 && last_scan.ends_with('Z'), || {
            format!("lastScan is {last_scan:?}")
        })
    }

    // -- the methods that read the library ------------------------------------

    fn music_folders(&mut self) -> Result<(), String> {
        let answer = self.client.call("getMusicFolders", &[])?;
        let folders = list(&answer, "/musicFolders/musicFolder")?;
        let library = std::path::absolute(&self.library)
            .map_err(|error| format!("{}: {error}", self.library.display()))?;
        let name = library
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        ensure(
            folders.len() == 1 && folders[0]["id"].is_i64() && folders[0]["name"] == name.as_str(),
            || format!("expected one folder with the name {name:?}, got {answer}"),
        )
    }

    fn artists(&mut self) -> Result<(), String> {
        let answer = self.client.call("getArtists", &[])?;
        let mut found = Vec::new();
        let mut misplaced = Vec::new();
        for index in list(&answer, "/artists/index")? {
            for artist in list(index, "/artist")? {
                let name = text(artist, "/name")?;
                self.artists
                    .insert(name.to_owned(), text(artist, "/id")?.to_owned());
                found.push((name, artist["albumCount"].as_u64().unwrap_or_default()));
                // An artist is in the index of the first letter of its name.
                let letter = index["name"].as_str().unwrap_or_default();
                if letter.is_empty() || !name.starts_with(letter) {
                    misplaced.push(format!("{name:?} in the index {letter:?}"));
                }
            }
        }
        ensure(misplaced.is_empty(), || misplaced.join(", "))?;
        found.sort_unstable();
        ensure(found == ARTISTS, || {
            format!("expected {ARTISTS:?}, got {found:?}")
        })
    }

    fn artist(&mut self) -> Result<(), String> {
        let id = self.artist_id("Aurora Field")?.to_owned();
        let answer = self.client.call("getArtist", &[("id", &id)])?;
        let albums = list(&answer, "/artist/album")?;
        for album in albums {
            self.keep_album(album);
        }
        // The albums of an artist are in the order of their years.
        let expected = [("Glass Harbour", 2019, 3), ("Paper Maps", 2021, 3)];
        let found: Vec<(&str, u64, u64)> = albums
            .iter()
            .map(|album| {
                (
                    album["name"].as_str().unwrap_or_default(),
                    album["year"].as_u64().unwrap_or_default(),
                    album["songCount"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        ensure(found == expected, || {
            format!("expected {expected:?}, got {found:?}")
        })?;
        for album in albums {
            ensure(
                album["artistId"] == id.as_str() && album["coverArt"].is_string(),
                || format!("the album has another artistId or no coverArt: {album}"),
            )?;
        }
        Ok(())
    }

    fn album(&mut self) -> Result<(), String> {
        let albums = [
            ("Glass Harbour", GLASS_HARBOUR, "flac", "audio/flac"),
            ("Paper Maps", PAPER_MAPS, "mp3", "audio/mpeg"),
        ];
        for (title, tracks, suffix, content_type) in albums {
            let id = self.album_of(title)?.id.clone();
            let answer = self.client.call("getAlbum", &[("id", &id)])?;
            ensure(answer["album"]["name"] == title, || {
                format!("expected the album {title:?}, got {answer}")
            })?;
            let songs = list(&answer, "/album/song")?;
            for song in songs {
                self.keep_song(song);
            }
            let titles = texts(songs, "title");
            ensure(titles == tracks, || {
                format!("expected the tracks {tracks:?}, got {titles:?}")
            })?;
            for song in songs {
                ensure(
                    song["suffix"] == suffix
                        && song["contentType"] == content_type
                        && song["albumId"] == id.as_str(),
                    || format!("expected a {suffix} track of the album, got {song}"),
                )?;
            }
        }
        Ok(())
    }

    fn album_xml(&mut self) -> Result<(), String> {
        let id = self.album_of("Glass Harbour")?.id.clone();
        let reply = self.client.get("getAlbum", &[("id", &id), ("f", "xml")])?;
        reply.expect_media("application/xml")?;
        let xml = String::from_utf8_lossy(&reply.body);
        let elements = elements(&xml)?;
        let attribute = |element: &Element, name: &str| {
            element.attributes.get(name).cloned().unwrap_or_default()
        };

        let [root, album, below @ ..] = elements.as_slice() else {
            return Err(format!("the document has no album element: {xml}"));
        };
        ensure(
            root.name == "subsonic-response"
                && attribute(root, "xmlns") == "http://subsonic.org/restapi"
                && attribute(root, "status") == "ok"
                && attribute(root, "version") == API_VERSION
                && attribute(root, "openSubsonic") == "true",
            || format!("the root element is not the envelope: {xml}"),
        )?;
        ensure(
            album.name == "album"
                && attribute(album, "id") == id
                && attribute(album, "name") == "Glass Harbour",
            || format!("the first element is not the album: {xml}"),
        )?;
        let titles: Vec<String> = below
            .iter()
            .filter(|element| element.name == "song")
            .map(|song| attribute(song, "title"))
            .collect();
        ensure(titles == GLASS_HARBOUR, || {
            format!("expected the song elements {GLASS_HARBOUR:?}: {xml}")
        })
    }

    fn song(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let album = self.album_of("Glass Harbour")?;
        let size = self.file(&low_tide.path)?.len() as u64;
        let answer = self.client.call("getSong", &[("id", &low_tide.id)])?;
        let song = &answer["song"];
        ensure(
            song["id"] == low_tide.id.as_str()
                && song["title"] == "Low Tide"
                && song["album"] == "Glass Harbour"
                && song["albumId"] == album.id.as_str()
                && song["artist"] == "Aurora Field"
                && song["path"] == "Aurora Field/Glass Harbour/01 Low Tide.flac"
                && song["track"] == 1
                && song["year"] == 2019
                && song["genre"] == "Electronic"
                && song["size"] == size
                && song["isDir"] == false,
            || format!("expected Low Tide with the size {size}, got {song}"),
        )
    }

    fn search_all(&mut self) -> Result<(), String> {
        // Symfonium reads the whole library with an empty query.
        let answer = self.client.call("search3", &[("query", "")])?;
        let artists = texts(list(&answer, "/searchResult3/artist")?, "name");
        let albums = list(&answer, "/searchResult3/album")?;
        let songs = list(&answer, "/searchResult3/song")?;

        // A search names an item by the ID that the browse methods gave it.
        let mut renamed = Vec::new();
        for song in songs {
            if let (Some(title), Some(id)) = (song["title"].as_str(), song["id"].as_str())
                && let Some(known) = self.songs.get(title)
                && known.id != id
            {
                renamed.push(format!(
                    "{title:?} is {} in getAlbum and {id} in search3",
                    known.id
                ));
            }
            self.keep_song(song);
        }
        for album in albums {
            self.keep_album(album);
        }

        let albums = texts(albums, "name");
        let titles = texts(songs, "title");
        let expected_artists = ARTISTS.map(|(name, _)| name);
        ensure(
            sorted(&artists) == expected_artists
                && sorted(&albums) == ALBUMS
                && sorted(&titles) == SONGS,
            || format!("expected the whole library, got {artists:?}, {albums:?} and {titles:?}"),
        )?;
        ensure(renamed.is_empty(), || renamed.join(", "))
    }

    fn search_word(&mut self) -> Result<(), String> {
        // The letter case of a word has no meaning.
        let answer = self.client.call("search3", &[("query", "FERRY")])?;
        let artists = list(&answer, "/searchResult3/artist")?;
        let albums = list(&answer, "/searchResult3/album")?;
        let titles = texts(list(&answer, "/searchResult3/song")?, "title");
        ensure(
            artists.is_empty() && albums.is_empty() && titles == ["Night Ferry"],
            || format!("expected the one song Night Ferry, got {answer}"),
        )
    }

    fn album_list(&mut self) -> Result<(), String> {
        let answer = self.client.call("getAlbumList2", &[("type", "newest")])?;
        let albums = list(&answer, "/albumList2/album")?;
        let titles = texts(albums, "name");
        ensure(sorted(&titles) == ALBUMS, || {
            format!("expected {ALBUMS:?}, got {titles:?}")
        })?;
        ensure(
            albums.iter().all(|album| album["created"].is_string()),
            || format!("an album has no created: {answer}"),
        )
    }

    // -- the covers ----------------------------------------------------------

    fn cover_small(&mut self) -> Result<(), String> {
        let id = self.cover_of("Glass Harbour")?;
        let reply = self
            .client
            .get("getCoverArt", &[("id", id), ("size", "64")])?;
        reply.expect_media("image/jpeg")?;
        ensure(reply.body.starts_with(&[0xff, 0xd8, 0xff]), || {
            format!("the body does not start as a JPEG: {}", reply.summary())
        })?;
        reply.expect_header("cache-control", "public, max-age=86400")?;
        // 64 is below the first step, which is 80 pixels.
        expect_step(&reply, id, 80)
    }

    fn cover_step(&mut self) -> Result<(), String> {
        // The cover of this album is 600 × 400 pixels. The steps around the
        // size 100 are 80 and 160.
        let id = self.cover_of("Paper Maps")?;
        let between = self
            .client
            .get("getCoverArt", &[("id", id), ("size", "100")])?;
        between.expect_media("image/jpeg")?;
        expect_step(&between, id, 160)?;
        let image = image::load_from_memory(&between.body)
            .map_err(|error| format!("the cover cannot be decoded: {error}"))?;
        ensure(
            image.width() == 160 && (106..=107).contains(&image.height()),
            || {
                format!(
                    "expected a cover of 160 × 107 pixels, got {} × {}",
                    image.width(),
                    image.height()
                )
            },
        )?;

        // The sizes of one step are answered with the same bytes and the
        // same entity tag.
        let at_step = self
            .client
            .get("getCoverArt", &[("id", id), ("size", "160")])?;
        at_step.expect_media("image/jpeg")?;
        ensure(
            at_step.body == between.body && at_step.header("etag") == between.header("etag"),
            || "size=100 and size=160 have different bytes or entity tags".to_owned(),
        )?;

        // A client that holds the cover of one size has the cover of every
        // size of that step.
        let etag = between.header("etag").unwrap_or_default();
        let held = self.client.get_with_headers(
            "getCoverArt",
            &[("id", id), ("size", "150")],
            &[("If-None-Match", etag)],
        )?;
        held.expect_status(304)
    }

    fn cover_stored(&mut self) -> Result<(), String> {
        let id = self.cover_of("Glass Harbour")?;
        let file = self.file("Aurora Field/Glass Harbour/cover.jpg")?;
        let reply = self.client.get("getCoverArt", &[("id", id)])?;
        reply.expect_media("image/jpeg")?;
        ensure(reply.body == file, || {
            format!(
                "the body has {} bytes and differs from the file of {} bytes",
                reply.body.len(),
                file.len()
            )
        })?;
        expect_step(&reply, id, 0)
    }

    // -- stream --------------------------------------------------------------

    fn stream_raw(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let file = self.file(&low_tide.path)?;
        let reply = self.client.get("stream", &[("id", &low_tide.id)])?;
        reply.expect_media("audio/flac")?;
        reply.expect_header("content-length", &file.len().to_string())?;
        reply.expect_header("accept-ranges", "bytes")?;
        expect_bytes(&reply, &file)
    }

    fn stream_range(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let file = self.file(&low_tide.path)?;
        let reply = self.client.get_with_headers(
            "stream",
            &[("id", &low_tide.id)],
            &[("Range", "bytes=100-199")],
        )?;
        reply.expect_status(206)?;
        reply.expect_header("content-type", "audio/flac")?;
        reply.expect_header("content-range", &format!("bytes 100-199/{}", file.len()))?;
        expect_bytes(&reply, &file[100..200])
    }

    fn stream_other_unit(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let file = self.file(&low_tide.path)?;
        let reply = self.client.get_with_headers(
            "stream",
            &[("id", &low_tide.id)],
            &[("Range", "items=0-1")],
        )?;
        reply.expect_media("audio/flac")?;
        reply.expect_header("accept-ranges", "bytes")?;
        expect_bytes(&reply, &file)
    }

    fn stream_opus(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let reply = self.client.get(
            "stream",
            &[
                ("id", &low_tide.id),
                ("format", "opus"),
                ("maxBitRate", "96"),
            ],
        )?;
        expect_transcode(&reply, "audio/ogg")?;
        // An Ogg stream starts with a page, and the first page of an Opus
        // stream holds the identification header.
        ensure(
            reply.body.starts_with(b"OggS") && find(&reply.body, b"OpusHead").is_some(),
            || format!("the body is no Ogg Opus stream: {}", reply.summary()),
        )
    }

    fn stream_mp3(&mut self) -> Result<(), String> {
        let low_tide = self.song_of("Low Tide")?;
        let reply = self
            .client
            .get("stream", &[("id", &low_tide.id), ("format", "mp3")])?;
        expect_transcode(&reply, "audio/mpeg")?;
        // An MP3 stream starts with an ID3v2 tag or with the sync bits of a
        // frame.
        let starts_as_mp3 = reply.body.starts_with(b"ID3")
            || matches!(reply.body.as_slice(), [0xff, second, ..] if second & 0xe0 == 0xe0);
        ensure(starts_as_mp3, || {
            format!("the body is no MP3 stream: {}", reply.summary())
        })
    }

    fn stream_offset(&mut self) -> Result<(), String> {
        // The track is half a second long, so a stream from the second 1 on
        // holds the headers of the Ogg stream and no audio.
        let low_tide = self.song_of("Low Tide")?;
        let whole = self
            .client
            .get("stream", &[("id", &low_tide.id), ("format", "opus")])?;
        expect_transcode(&whole, "audio/ogg")?;
        let rest = self.client.get(
            "stream",
            &[
                ("id", &low_tide.id),
                ("format", "opus"),
                ("timeOffset", "1"),
            ],
        )?;
        expect_transcode(&rest, "audio/ogg")?;
        ensure(
            rest.body.starts_with(b"OggS") && rest.body.len() < whole.body.len(),
            || {
                format!(
                    "the stream from the second 1 on has {} bytes and the whole stream has {}",
                    rest.body.len(),
                    whole.body.len()
                )
            },
        )
    }

    fn stream_mp3_of_mp3(&mut self) -> Result<(), String> {
        // A file that has the format of the request is not encoded again.
        let meridian = self.song_of("Meridian")?;
        let file = self.file(&meridian.path)?;
        let reply = self
            .client
            .get("stream", &[("id", &meridian.id), ("format", "mp3")])?;
        reply.expect_media("audio/mpeg")?;
        reply.expect_header("content-length", &file.len().to_string())?;
        expect_bytes(&reply, &file)
    }

    // -- annotations ---------------------------------------------------------

    /// The IDs of the songs that `getStarred2` lists.
    fn starred_songs(&self) -> Result<Vec<String>, String> {
        let answer = self.client.call("getStarred2", &[])?;
        let songs = list(&answer, "/starred2/song")?;
        ensure(songs.iter().all(|song| song["starred"].is_string()), || {
            format!("a starred song has no starred time: {answer}")
        })?;
        Ok(texts(songs, "id").into_iter().map(str::to_owned).collect())
    }

    fn star(&mut self) -> Result<(), String> {
        let id = self.song_of("Breakwater")?.id.clone();
        let found = self.starred_songs()?.contains(&id);
        self.client.call("star", &[("id", &id)])?;
        let starred = self.starred_songs();
        // The star is removed whatever the list held.
        self.client.call("unstar", &[("id", &id)])?;
        let left = self.starred_songs();
        // A star that the session found is set again.
        if found {
            self.client.call("star", &[("id", &id)])?;
        }
        let (starred, left) = (starred?, left?);
        ensure(starred.contains(&id), || {
            format!("getStarred2 lists {starred:?} after star for {id}")
        })?;
        ensure(!left.contains(&id), || {
            format!("getStarred2 lists {left:?} after unstar for {id}")
        })
    }

    fn scrobble(&mut self) -> Result<(), String> {
        // The API has no method that removes a play, so the step compares
        // the count with the one from before it.
        let id = self.song_of("Night Ferry")?.id.clone();
        let before = self.client.call("getSong", &[("id", &id)])?;
        let before = before["song"]["playCount"].as_u64().unwrap_or_default();
        self.client
            .call("scrobble", &[("id", &id), ("submission", "true")])?;
        let after = self.client.call("getSong", &[("id", &id)])?;
        let song = &after["song"];
        ensure(
            song["playCount"] == before + 1 && song["played"].is_string(),
            || {
                format!(
                    "expected playCount {} and a played time, got {song}",
                    before + 1
                )
            },
        )
    }

    // -- playlists -----------------------------------------------------------

    fn playlist(&mut self) -> Result<(), String> {
        let first = self.song_of("Night Ferry")?.id.clone();
        let second = self.song_of("Low Tide")?.id.clone();
        let created = self.client.call(
            "createPlaylist",
            &[
                ("name", PLAYLIST_NAME),
                ("songId", &first),
                ("songId", &second),
            ],
        )?;
        let id = text(&created, "/playlist/id")?.to_owned();
        let checked = self.check_playlist(&created, &id, &[&first, &second]);
        // The playlist is deleted whatever the checks found.
        self.client.call("deletePlaylist", &[("id", &id)])?;
        checked?;
        self.client
            .get("getPlaylist", &[("id", &id)])?
            .expect_error(70)
    }

    /// Checks the answer of `createPlaylist` and the answer of `getPlaylist`
    /// for the playlist `id` with the songs `songs`.
    fn check_playlist(&self, created: &Value, id: &str, songs: &[&str]) -> Result<(), String> {
        let read = self.client.call("getPlaylist", &[("id", id)])?;
        for answer in [created, &read] {
            let playlist = &answer["playlist"];
            let entries = texts(list(playlist, "/entry")?, "id");
            ensure(
                playlist["id"] == id
                    && playlist["name"] == PLAYLIST_NAME
                    && playlist["songCount"] == songs.len() as u64
                    && text(playlist, "/owner")?.eq_ignore_ascii_case(&self.client.user)
                    && entries == songs,
                || format!("expected the playlist with the songs {songs:?}, got {playlist}"),
            )?;
        }
        Ok(())
    }

    // -- the play queue ------------------------------------------------------

    /// Reads the play queue of the user.
    fn read_queue(&self) -> Result<Queue, String> {
        let answer = self.client.call("getPlayQueue", &[])?;
        let queue = &answer["playQueue"];
        ensure(
            text(queue, "/username")?.eq_ignore_ascii_case(&self.client.user)
                && queue["changed"].is_string(),
            || format!("the queue has no username or no changed: {queue}"),
        )?;
        Ok(Queue {
            ids: texts(list(queue, "/entry")?, "id")
                .into_iter()
                .map(str::to_owned)
                .collect(),
            current: queue["current"].as_str().map(str::to_owned),
            position: queue["position"]
                .as_u64()
                .ok_or_else(|| format!("the queue has no position: {queue}"))?,
            changed_by: text(queue, "/changedBy")?.to_owned(),
        })
    }

    /// The IDs of the three songs that the session saves as its queue.
    fn queue_songs(&self) -> Result<[String; 3], String> {
        Ok([
            self.song_of("Low Tide")?.id.clone(),
            self.song_of("Breakwater")?.id.clone(),
            self.song_of("Night Ferry")?.id.clone(),
        ])
    }

    fn queue_with_current(&mut self) -> Result<(), String> {
        if self.queue_before.is_none() {
            self.queue_before = Some(self.read_queue()?);
        }
        let [first, second, third] = self.queue_songs()?;
        self.client.call(
            "savePlayQueue",
            &[
                ("id", &first),
                ("id", &second),
                ("id", &third),
                ("current", &second),
                ("position", "1500"),
            ],
        )?;
        let queue = self.read_queue()?;
        ensure(
            queue.ids == [first, second.clone(), third]
                && queue.current.as_deref() == Some(second.as_str())
                && queue.position == 1500
                && queue.changed_by == CLIENT,
            || {
                format!(
                    "expected three songs, the second as current at 1500 ms, got {:?}, {:?} at {} ms by {:?}",
                    queue.ids, queue.current, queue.position, queue.changed_by
                )
            },
        )
    }

    fn queue_without_current(&mut self) -> Result<(), String> {
        if self.queue_before.is_none() {
            self.queue_before = Some(self.read_queue()?);
        }
        // Supersonic sends this request for a queue that has not played. The
        // position of a request without a current song is not stored.
        let [first, second, third] = self.queue_songs()?;
        self.client.call(
            "savePlayQueue",
            &[
                ("id", &first),
                ("id", &second),
                ("id", &third),
                ("position", "7000"),
            ],
        )?;
        let queue = self.read_queue()?;
        ensure(
            queue.ids == [first.clone(), second, third]
                && queue.current.as_deref() == Some(first.as_str())
                && queue.position == 0,
            || {
                format!(
                    "expected three songs and the first as current at 0 ms, got {:?}, {:?} at {} ms",
                    queue.ids, queue.current, queue.position
                )
            },
        )
    }

    fn queue_without_song(&mut self) -> Result<(), String> {
        if self.queue_before.is_none() {
            self.queue_before = Some(self.read_queue()?);
        }
        self.client.call("savePlayQueue", &[])?;
        let queue = self.read_queue()?;
        ensure(
            queue.ids.is_empty() && queue.current.is_none() && queue.position == 0,
            || {
                format!(
                    "expected a queue without a song, got {:?}, {:?} at {} ms",
                    queue.ids, queue.current, queue.position
                )
            },
        )?;

        // The queue that the session found is saved again, unless it had no
        // song or an earlier session left it behind.
        let Some(before) = &self.queue_before else {
            return Ok(());
        };
        if before.ids.is_empty() || before.changed_by == CLIENT {
            return Ok(());
        }
        let position = before.position.to_string();
        let mut params: Vec<(&str, &str)> =
            before.ids.iter().map(|id| ("id", id.as_str())).collect();
        if let Some(current) = &before.current {
            params.push(("current", current));
            params.push(("position", &position));
        }
        self.client.call("savePlayQueue", &params).map(drop)
    }

    // -- lyrics and download -------------------------------------------------

    fn lyrics_by_song_id(&mut self) -> Result<(), String> {
        let id = &self.song_of("Low Tide")?.id;
        let answer = self.client.call("getLyricsBySongId", &[("id", id)])?;
        ensure(
            answer["lyricsList"]
                .as_object()
                .is_some_and(serde_json::Map::is_empty),
            || format!("expected a lyricsList without a field, got {answer}"),
        )
    }

    fn lyrics(&mut self) -> Result<(), String> {
        let answer = self.client.call(
            "getLyrics",
            &[("artist", "Aurora Field"), ("title", "Low Tide")],
        )?;
        ensure(answer["lyrics"]["value"] == "", || {
            format!("expected lyrics with an empty value, got {answer}")
        })
    }

    fn download(&mut self) -> Result<(), String> {
        let id = &self.song_of("Low Tide")?.id;
        self.client.get("download", &[("id", id)])?.expect_error(50)
    }
}

/// Checks that the entity tag of the cover `reply` names the cover `id` at
/// the step `step`. The tag of the image as it is stored has the step 0.
fn expect_step(reply: &Reply, id: &str, step: u32) -> Result<(), String> {
    let etag = reply.header("etag").unwrap_or_default();
    ensure(etag.starts_with(&format!("\"{id}-{step}-")), || {
        format!("expected an ETag of the step {step}, got {etag:?}")
    })
}

/// Checks that the body of `reply` is `expected`.
fn expect_bytes(reply: &Reply, expected: &[u8]) -> Result<(), String> {
    ensure(reply.body == expected, || {
        format!(
            "the body has {} bytes and differs from the {} bytes of the file",
            reply.body.len(),
            expected.len()
        )
    })
}

/// Checks the headers of a transcoded stream with the content type
/// `content_type`. The length of such a stream is not known when its headers
/// are sent, so it has no `Content-Length` and its transfer is chunked.
fn expect_transcode(reply: &Reply, content_type: &str) -> Result<(), String> {
    reply.expect_media(content_type)?;
    ensure(reply.header("content-length").is_none(), || {
        format!(
            "the stream has Content-Length: {}",
            reply.header("content-length").unwrap_or_default()
        )
    })?;
    reply.expect_header("transfer-encoding", "chunked")?;
    reply.expect_header("accept-ranges", "none")?;
    reply.expect_header("cache-control", "no-store")?;
    ensure(!reply.body.is_empty(), || "the stream is empty".to_owned())
}
