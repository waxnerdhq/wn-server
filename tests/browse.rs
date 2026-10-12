//! Router tests of the browse methods `getMusicFolders`, `getArtists`,
//! `getArtist`, `getAlbum`, `getSong` and `getGenres` (`docs/design.md`
//! section 4).
//!
//! The tests of the standard library compare the whole response with the
//! document that the library gives: the `subsonic-response` object in JSON
//! and the text of the body in XML. The tables below hold what the builder
//! of the library writes (`LibraryBuilder::standard`) and what the five
//! fixture files are. The IDs, the creation times and the file sizes differ
//! from run to run, so the tests read them from the rows and from the files.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde_json::{Value, json};
use wn_server::api::model;
use wn_server::config::Config;
use wn_server::db::queries::folders;

use common::{
    Catalogue, LibraryBuilder, OWNER, ScannedServer, StandardServer, TestServer, body_string,
    column, content_type, failed, index_names, ok, owner_id, remove_file, subsonic_response,
    two_folder_server, xml_failed, xml_ok,
};

/// The value of `ignoredArticles`.
const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// The message of error 70.
const NOT_FOUND: &str = "not found";

/// What the file of a fixture format gives every track that is a copy of it.
/// Every fixture is mono and lasts about half a second, which is one second
/// after rounding.
struct Format {
    suffix: &'static str,
    content_type: &'static str,
    bit_rate: u32,
    /// 0 for a lossy format, which has no bit depth.
    bit_depth: u32,
    sampling_rate: u32,
}

const FLAC: Format = Format {
    suffix: "flac",
    content_type: "audio/flac",
    bit_rate: 58,
    bit_depth: 16,
    sampling_rate: 22_050,
};

const MP3: Format = Format {
    suffix: "mp3",
    content_type: "audio/mpeg",
    bit_rate: 34,
    bit_depth: 0,
    sampling_rate: 22_050,
};

const M4A: Format = Format {
    suffix: "m4a",
    content_type: "audio/mp4",
    bit_rate: 34,
    bit_depth: 0,
    sampling_rate: 22_050,
};

const OGG: Format = Format {
    suffix: "ogg",
    content_type: "audio/ogg",
    bit_rate: 24,
    bit_depth: 0,
    sampling_rate: 22_050,
};

const OPUS: Format = Format {
    suffix: "opus",
    content_type: "audio/ogg",
    bit_rate: 28,
    bit_depth: 0,
    sampling_rate: 24_000,
};

/// One album of the standard library. Every track of an album has the year
/// and the genre of the album.
struct AlbumFacts {
    name: &'static str,
    /// The album artist.
    artist: &'static str,
    /// Whether the scan finds a cover: the file `cover.jpg`, or a picture
    /// in the first track.
    has_cover: bool,
    song_count: u32,
    /// The sum of the durations of the tracks, rounded to seconds.
    duration: u64,
    year: u32,
    genre: &'static str,
}

const GLASS_HARBOUR: AlbumFacts = AlbumFacts {
    name: "Glass Harbour",
    artist: "Aurora Field",
    has_cover: true,
    song_count: 3,
    duration: 2,
    year: 2019,
    genre: "Electronic",
};

const PAPER_MAPS: AlbumFacts = AlbumFacts {
    name: "Paper Maps",
    artist: "Aurora Field",
    has_cover: false,
    song_count: 3,
    duration: 2,
    year: 2021,
    genre: "Electronic",
};

const SLOW_RIVERS: AlbumFacts = AlbumFacts {
    name: "Slow Rivers",
    artist: "Marlow Quartet",
    has_cover: true,
    song_count: 2,
    duration: 1,
    year: 2015,
    genre: "Jazz",
};

const SUMMER_SAMPLER: AlbumFacts = AlbumFacts {
    name: "Summer Sampler",
    artist: "Various Artists",
    has_cover: false,
    song_count: 2,
    duration: 1,
    year: 2020,
    genre: "Pop",
};

/// One track of the standard library.
struct TrackFacts {
    title: &'static str,
    album: &'static AlbumFacts,
    /// The track artist.
    artist: &'static str,
    track: u32,
    /// The disc number tag, and 1 for a track without one.
    disc_number: u32,
    format: &'static Format,
    /// The path below the music folder.
    path: &'static str,
}

const LOW_TIDE: TrackFacts = TrackFacts {
    title: "Low Tide",
    album: &GLASS_HARBOUR,
    artist: "Aurora Field",
    track: 1,
    disc_number: 1,
    format: &FLAC,
    path: "Aurora Field/Glass Harbour/01 Low Tide.flac",
};

const BREAKWATER: TrackFacts = TrackFacts {
    title: "Breakwater",
    album: &GLASS_HARBOUR,
    artist: "Aurora Field",
    track: 2,
    disc_number: 1,
    format: &FLAC,
    path: "Aurora Field/Glass Harbour/02 Breakwater.flac",
};

const NIGHT_FERRY: TrackFacts = TrackFacts {
    title: "Night Ferry",
    album: &GLASS_HARBOUR,
    artist: "Aurora Field",
    track: 3,
    disc_number: 1,
    format: &FLAC,
    path: "Aurora Field/Glass Harbour/03 Night Ferry.flac",
};

const MERIDIAN: TrackFacts = TrackFacts {
    title: "Meridian",
    album: &PAPER_MAPS,
    artist: "Aurora Field",
    track: 1,
    disc_number: 1,
    format: &MP3,
    path: "Aurora Field/Paper Maps/01 Meridian.mp3",
};

const CONTOUR: TrackFacts = TrackFacts {
    title: "Contour",
    album: &PAPER_MAPS,
    artist: "Aurora Field",
    track: 2,
    disc_number: 1,
    format: &MP3,
    path: "Aurora Field/Paper Maps/02 Contour.mp3",
};

const LEGEND: TrackFacts = TrackFacts {
    title: "Legend",
    album: &PAPER_MAPS,
    artist: "Aurora Field",
    track: 1,
    disc_number: 2,
    format: &MP3,
    path: "Aurora Field/Paper Maps/01 Legend.mp3",
};

const UPSTREAM: TrackFacts = TrackFacts {
    title: "Upstream",
    album: &SLOW_RIVERS,
    artist: "Marlow Quartet",
    track: 1,
    disc_number: 1,
    format: &M4A,
    path: "Marlow Quartet/Slow Rivers/01 Upstream.m4a",
};

const DELTA: TrackFacts = TrackFacts {
    title: "Delta",
    album: &SLOW_RIVERS,
    artist: "Marlow Quartet",
    track: 2,
    disc_number: 1,
    format: &M4A,
    path: "Marlow Quartet/Slow Rivers/02 Delta.m4a",
};

const OPEN_ROAD: TrackFacts = TrackFacts {
    title: "Open Road",
    album: &SUMMER_SAMPLER,
    artist: "Tessa Lund",
    track: 1,
    disc_number: 1,
    format: &OGG,
    path: "Tessa Lund/Summer Sampler/01 Open Road.ogg",
};

const LAST_TRAIN: TrackFacts = TrackFacts {
    title: "Last Train",
    album: &SUMMER_SAMPLER,
    artist: "Aurora Field",
    track: 2,
    disc_number: 1,
    format: &OPUS,
    path: "Aurora Field/Summer Sampler/02 Last Train.opus",
};

/// The albums of the standard library, each with its tracks in the order of
/// `getAlbum`: by disc, then by track number.
const ALBUMS: [(&AlbumFacts, &[&TrackFacts]); 4] = [
    (&GLASS_HARBOUR, &[&LOW_TIDE, &BREAKWATER, &NIGHT_FERRY]),
    (&PAPER_MAPS, &[&MERIDIAN, &CONTOUR, &LEGEND]),
    (&SLOW_RIVERS, &[&UPSTREAM, &DELTA]),
    (&SUMMER_SAMPLER, &[&OPEN_ROAD, &LAST_TRAIN]),
];

/// One artist of the standard library.
struct ArtistFacts {
    name: &'static str,
    /// The name of the index that holds the artist in `getArtists`.
    index: &'static str,
    album_count: u32,
    /// The albums in the order of `getArtist`: by year.
    albums: &'static [&'static AlbumFacts],
}

/// The artists of the standard library in the order of `getArtists`. "Tessa
/// Lund" is a track artist only and is no artist of the library.
const ARTISTS: [ArtistFacts; 3] = [
    ArtistFacts {
        name: "Aurora Field",
        index: "A",
        album_count: 2,
        albums: &[&GLASS_HARBOUR, &PAPER_MAPS],
    },
    ArtistFacts {
        name: "Marlow Quartet",
        index: "M",
        album_count: 1,
        albums: &[&SLOW_RIVERS],
    },
    ArtistFacts {
        name: "Various Artists",
        index: "V",
        album_count: 1,
        albums: &[&SUMMER_SAMPLER],
    },
];

/// What a scanned standard library gives the expected documents: the rows,
/// which hold the IDs and the creation times, and the music folder, which
/// holds the files.
struct Standard {
    catalogue: Catalogue,
    root: PathBuf,
}

impl Standard {
    async fn of(server: &ScannedServer) -> Self {
        Self {
            catalogue: server.catalogue().await,
            root: server.root().to_path_buf(),
        }
    }

    fn artist_id(&self, name: &str) -> &str {
        &self.catalogue.artist(name).id
    }

    fn album_id(&self, album: &AlbumFacts) -> &str {
        &self.catalogue.album(album.name).id
    }

    fn track_id(&self, track: &TrackFacts) -> &str {
        &self.catalogue.song(track.title).track.id
    }

    /// The cover art ID of `album` and of its songs: the ID of the album
    /// when the album has a cover.
    fn cover_art(&self, album: &AlbumFacts) -> Option<&str> {
        album.has_cover.then(|| self.album_id(album))
    }

    /// The size of the file of `track` in bytes.
    fn size(&self, track: &TrackFacts) -> u64 {
        let mut path = self.root.clone();
        path.extend(track.path.split('/'));
        fs::metadata(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .len()
    }

    fn song_created(&self, track: &TrackFacts) -> String {
        model::iso8601(self.catalogue.song(track.title).track.created_at)
    }

    fn album_created(&self, album: &AlbumFacts) -> String {
        model::iso8601(self.catalogue.album(album.name).created_at)
    }

    /// The `song` element of `track` for a user who attached nothing to it.
    fn song_json(&self, track: &TrackFacts) -> Value {
        let album_id = self.album_id(track.album);
        let mut song = json!({
            "id": self.track_id(track),
            "parent": album_id,
            "isDir": false,
            "title": track.title,
            "album": track.album.name,
            "artist": track.artist,
            "track": track.track,
            "year": track.album.year,
            "genre": track.album.genre,
            "size": self.size(track),
            "contentType": track.format.content_type,
            "suffix": track.format.suffix,
            "duration": 1,
            "bitRate": track.format.bit_rate,
            "bitDepth": track.format.bit_depth,
            "samplingRate": track.format.sampling_rate,
            "channelCount": 1,
            "path": track.path,
            "discNumber": track.disc_number,
            "created": self.song_created(track),
            "albumId": album_id,
            "artistId": self.artist_id(track.album.artist),
            "type": "music",
            "mediaType": "song",
            "playCount": 0,
        });
        if let Some(cover_art) = self.cover_art(track.album) {
            song["coverArt"] = json!(cover_art);
        }
        song
    }

    /// The element `<name .../>` of `track` in XML, for a user who attached
    /// nothing to it. The attributes are in the order of their names.
    fn song_xml(&self, name: &str, track: &TrackFacts) -> String {
        let album_id = self.album_id(track.album);
        let cover_art = match self.cover_art(track.album) {
            Some(id) => format!(r#" coverArt="{id}""#),
            None => String::new(),
        };
        format!(
            concat!(
                r#"<{name} album="{album}" albumId="{album_id}" artist="{artist}""#,
                r#" artistId="{artist_id}" bitDepth="{bit_depth}" bitRate="{bit_rate}""#,
                r#" channelCount="1" contentType="{content_type}"{cover_art}"#,
                r#" created="{created}" discNumber="{disc_number}" duration="1""#,
                r#" genre="{genre}" id="{id}" isDir="false" mediaType="song""#,
                r#" parent="{album_id}" path="{path}" playCount="0""#,
                r#" samplingRate="{sampling_rate}" size="{size}" suffix="{suffix}""#,
                r#" title="{title}" track="{track}" type="music" year="{year}"/>"#
            ),
            name = name,
            album = track.album.name,
            album_id = album_id,
            artist = track.artist,
            artist_id = self.artist_id(track.album.artist),
            bit_depth = track.format.bit_depth,
            bit_rate = track.format.bit_rate,
            content_type = track.format.content_type,
            cover_art = cover_art,
            created = self.song_created(track),
            disc_number = track.disc_number,
            genre = track.album.genre,
            id = self.track_id(track),
            path = track.path,
            sampling_rate = track.format.sampling_rate,
            size = self.size(track),
            suffix = track.format.suffix,
            title = track.title,
            track = track.track,
            year = track.album.year,
        )
    }

    /// The `AlbumID3` element of `album` without its songs, for a user who
    /// attached nothing to it.
    fn album_json(&self, album: &AlbumFacts) -> Value {
        let mut element = json!({
            "id": self.album_id(album),
            "name": album.name,
            "artist": album.artist,
            "artistId": self.artist_id(album.artist),
            "songCount": album.song_count,
            "duration": album.duration,
            "created": self.album_created(album),
            "year": album.year,
            "genre": album.genre,
            "playCount": 0,
        });
        if let Some(cover_art) = self.cover_art(album) {
            element["coverArt"] = json!(cover_art);
        }
        element
    }

    /// The attributes of the `album` element of `album` in XML, in the order
    /// of their names.
    fn album_attributes(&self, album: &AlbumFacts) -> String {
        let cover_art = match self.cover_art(album) {
            Some(id) => format!(r#" coverArt="{id}""#),
            None => String::new(),
        };
        format!(
            concat!(
                r#"artist="{artist}" artistId="{artist_id}"{cover_art} created="{created}""#,
                r#" duration="{duration}" genre="{genre}" id="{id}" name="{name}""#,
                r#" playCount="0" songCount="{song_count}" year="{year}""#
            ),
            artist = album.artist,
            artist_id = self.artist_id(album.artist),
            cover_art = cover_art,
            created = self.album_created(album),
            duration = album.duration,
            genre = album.genre,
            id = self.album_id(album),
            name = album.name,
            song_count = album.song_count,
            year = album.year,
        )
    }

    /// The `ArtistID3` element of `artist` without its albums, for a user
    /// who did not star it.
    fn artist_json(&self, artist: &ArtistFacts) -> Value {
        json!({
            "id": self.artist_id(artist.name),
            "name": artist.name,
            "albumCount": artist.album_count,
        })
    }

    /// The attributes of the `artist` element of `artist` in XML, in the
    /// order of their names.
    fn artist_attributes(&self, artist: &ArtistFacts) -> String {
        format!(
            r#"albumCount="{}" id="{}" name="{}""#,
            artist.album_count,
            self.artist_id(artist.name),
            artist.name
        )
    }
}

/// The names of the artists of an `artists` element, in the order of the
/// document.
fn artist_names(artists: &Value) -> Vec<String> {
    index_names(artists)
        .into_iter()
        .flat_map(|(_, names)| names)
        .collect()
}

/// The last component of `path`, which is the name of the music folder
/// `path`.
fn folder_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .expect("the folder has a Unicode name")
}

#[tokio::test]
async fn get_music_folders_returns_the_folder_of_the_library_with_its_integer_id() {
    let server = TestServer::standard().await;
    let name = folder_name(server.root());

    let response = server.get_json("getMusicFolders", &[]).await;
    assert_eq!(
        response,
        ok(
            "musicFolders",
            json!({ "musicFolder": [{ "id": server.folder_id, "name": name }] })
        )
    );
    // The ID is a JSON number, as Subsonic clients read it.
    assert!(
        response["musicFolders"]["musicFolder"][0]["id"].is_i64(),
        "{response}"
    );

    assert_eq!(
        server.get_xml("getMusicFolders", &[]).await,
        xml_ok(&format!(
            r#"<musicFolders><musicFolder id="{}" name="{name}"/></musicFolders>"#,
            server.folder_id
        ))
    );

    // The IDs are the ones that `getUser` reports.
    let user = server.get_json("getUser", &[("username", OWNER)]).await;
    assert_eq!(user["user"]["folder"], json!([server.folder_id]));
}

#[tokio::test]
async fn get_music_folders_lists_the_stored_folders_of_a_server_that_has_not_scanned() {
    let server = TestServer::bare(Config::default()).await;
    assert_eq!(
        server.get_json("getMusicFolders", &[]).await,
        ok("musicFolders", json!({ "musicFolder": [] }))
    );
    assert_eq!(
        server.get_xml("getMusicFolders", &[]).await,
        xml_ok("<musicFolders/>")
    );

    // `run` stores the folders of the config before it serves a request.
    let paths = [PathBuf::from("/music/rock"), PathBuf::from("/music/jazz")];
    server
        .db()
        .write(move |conn| folders::sync_folders(conn, &paths))
        .await
        .expect("store the folders");

    for method in ["getMusicFolders", "getMusicFolders.view"] {
        assert_eq!(
            server.get_json(method, &[]).await,
            ok(
                "musicFolders",
                json!({ "musicFolder": [
                    { "id": 1, "name": "rock" },
                    { "id": 2, "name": "jazz" },
                ]})
            ),
            "{method}"
        );
    }
    assert_eq!(
        server.get_xml("getMusicFolders", &[]).await,
        xml_ok(concat!(
            r#"<musicFolders><musicFolder id="1" name="rock"/>"#,
            r#"<musicFolder id="2" name="jazz"/></musicFolders>"#
        ))
    );
}

#[tokio::test]
async fn get_artists_returns_the_album_artists_grouped_by_letter() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;

    // The first letters of the three names differ, so each artist has an
    // index of its own.
    let indexes: Vec<Value> = ARTISTS
        .iter()
        .map(|artist| json!({ "name": artist.index, "artist": [standard.artist_json(artist)] }))
        .collect();
    let response = server.get_json("getArtists", &[]).await;
    assert_eq!(
        response,
        ok(
            "artists",
            json!({ "ignoredArticles": IGNORED_ARTICLES, "index": indexes })
        )
    );
    assert_eq!(
        index_names(&response["artists"]),
        [
            ("A".to_owned(), vec!["Aurora Field".to_owned()]),
            ("M".to_owned(), vec!["Marlow Quartet".to_owned()]),
            ("V".to_owned(), vec!["Various Artists".to_owned()]),
        ]
    );

    let indexes: String = ARTISTS
        .iter()
        .map(|artist| {
            format!(
                r#"<index name="{}"><artist {}/></index>"#,
                artist.index,
                standard.artist_attributes(artist)
            )
        })
        .collect();
    assert_eq!(
        server.get_xml("getArtists", &[]).await,
        xml_ok(&format!(
            r#"<artists ignoredArticles="{IGNORED_ARTICLES}">{indexes}</artists>"#
        ))
    );
}

#[tokio::test]
async fn get_artists_groups_a_name_by_its_first_letter_after_an_ignored_article() {
    let library = LibraryBuilder::new()
        .track("The Beatles", "First", "One", 1, "mp3")
        .track("beck", "Second", "Two", 1, "mp3")
        .track("Them", "Third", "Three", 1, "mp3")
        .track("Édith Piaf", "Fourth", "Four", 1, "mp3")
        .track("2Pac", "Fifth", "Five", 1, "mp3")
        .track("Los Lobos", "Sixth", "Six", 1, "mp3")
        .track("The", "Seventh", "Seven", 1, "mp3")
        .build();
    let server = TestServer::scanned(library).await;

    let response = server.get_json("getArtists", &[]).await;
    assert_eq!(response["status"], "ok");
    assert_eq!(response["artists"]["ignoredArticles"], IGNORED_ARTICLES);
    let names = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    let expected: [(String, Vec<String>); 5] = [
        // A name that starts with no letter from A to Z.
        ("#".to_owned(), names(&["2Pac"])),
        // "The Beatles" is ordered as "beatles", before "beck".
        ("B".to_owned(), names(&["The Beatles", "beck"])),
        // A letter with a diacritic counts as its base letter.
        ("E".to_owned(), names(&["Édith Piaf"])),
        ("L".to_owned(), names(&["Los Lobos"])),
        // An article is ignored only when another word follows it.
        ("T".to_owned(), names(&["The", "Them"])),
    ];
    assert_eq!(index_names(&response["artists"]), expected);

    let xml = server.get_xml("getArtists", &[]).await;
    assert!(
        xml.contains(&format!(
            r##"<artists ignoredArticles="{IGNORED_ARTICLES}"><index name="#"><artist "##
        )),
        "{xml}"
    );
    assert!(xml.contains(r#"<index name="E"><artist "#), "{xml}");
}

#[tokio::test]
async fn get_artists_of_a_server_without_a_library_has_the_articles_and_no_index() {
    let server = TestServer::bare(Config::default()).await;
    assert_eq!(
        server.get_json("getArtists", &[]).await,
        ok(
            "artists",
            json!({ "ignoredArticles": IGNORED_ARTICLES, "index": [] })
        )
    );
    assert_eq!(
        server.get_xml("getArtists", &[]).await,
        xml_ok(&format!(
            r#"<artists ignoredArticles="{IGNORED_ARTICLES}"/>"#
        ))
    );
}

#[tokio::test]
async fn get_artists_with_music_folder_id_returns_the_artists_of_that_folder() {
    let (server, first, second) = two_folder_server().await;

    // The folders have the IDs 1 and 2, in the order of the config.
    let folders = server.get_json("getMusicFolders", &[]).await;
    assert_eq!(
        folders["musicFolders"],
        json!({ "musicFolder": [
            { "id": 1, "name": folder_name(first.root.path()) },
            { "id": 2, "name": folder_name(second.root.path()) },
        ]})
    );

    let artists = |folder: Option<&'static str>| {
        let server = &server;
        async move {
            let params: Vec<(&str, &str)> =
                folder.map(|id| ("musicFolderId", id)).into_iter().collect();
            let response = server.get_json("getArtists", &params).await;
            assert_eq!(response["status"], "ok", "{folder:?}: {response}");
            assert_eq!(
                response["artists"]["ignoredArticles"], IGNORED_ARTICLES,
                "{folder:?}"
            );
            response["artists"].clone()
        }
    };
    let all = artists(None).await;
    assert_eq!(
        artist_names(&all),
        ["Aurora Field", "Marlow Quartet", "Shared Artist"]
    );
    let in_first = artists(Some("1")).await;
    assert_eq!(artist_names(&in_first), ["Aurora Field", "Shared Artist"]);
    let in_second = artists(Some("2")).await;
    assert_eq!(
        artist_names(&in_second),
        ["Marlow Quartet", "Shared Artist"]
    );

    // The album count is the stored one: it counts the albums of the artist
    // in every folder.
    for document in [&all, &in_first, &in_second] {
        let shared = document["index"]
            .as_array()
            .expect("an index list")
            .iter()
            .find(|index| index["name"] == "S")
            .expect("the index S");
        assert_eq!(shared["artist"][0]["albumCount"], 2, "{document}");
    }
    // An artist of a folder is the element that the whole list holds.
    assert_eq!(in_first["index"][0], all["index"][0]);

    // An empty value counts as an absent parameter.
    assert_eq!(artists(Some("")).await, all);

    // An ID that no folder has: no artist is in that folder.
    for id in ["3", "0", "-1", "9223372036854775807"] {
        assert_eq!(
            server
                .get_json("getArtists", &[("musicFolderId", id)])
                .await,
            ok(
                "artists",
                json!({ "ignoredArticles": IGNORED_ARTICLES, "index": [] })
            ),
            "{id}"
        );
    }
    assert_eq!(
        server
            .get_xml("getArtists", &[("musicFolderId", "3")])
            .await,
        xml_ok(&format!(
            r#"<artists ignoredArticles="{IGNORED_ARTICLES}"/>"#
        ))
    );
}

#[tokio::test]
async fn get_artists_with_a_music_folder_id_that_is_no_integer_is_error_0() {
    let server = TestServer::standard().await;
    let message = "parameter musicFolderId must be an integer";
    for id in ["abc", "1.5", "1 ", "0x1", "9223372036854775808", "ar-1"] {
        for method in ["getArtists", "getArtists.view"] {
            assert_eq!(
                server.get_json(method, &[("musicFolderId", id)]).await,
                failed(0, message),
                "{method} {id:?}"
            );
        }
    }
    assert_eq!(
        server
            .get_xml("getArtists", &[("musicFolderId", "abc")])
            .await,
        xml_failed(0, message)
    );
}

#[tokio::test]
async fn get_artist_returns_the_artist_with_its_albums() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;

    for artist in &ARTISTS {
        let name = artist.name;
        let id = standard.artist_id(name);
        let mut expected = standard.artist_json(artist);
        expected["album"] = artist
            .albums
            .iter()
            .map(|album| standard.album_json(album))
            .collect();
        for method in ["getArtist", "getArtist.view"] {
            assert_eq!(
                server.get_json(method, &[("id", id)]).await,
                ok("artist", expected.clone()),
                "{method} {name}"
            );
        }

        let albums: String = artist
            .albums
            .iter()
            .map(|album| format!("<album {}/>", standard.album_attributes(album)))
            .collect();
        assert_eq!(
            server.get_xml("getArtist", &[("id", id)]).await,
            xml_ok(&format!(
                "<artist {}>{albums}</artist>",
                standard.artist_attributes(artist)
            )),
            "{name}"
        );
    }

    // The albums of "Aurora Field" in the order of their years. The
    // compilation that holds a track of the artist is no album of it.
    let response = server
        .get_json("getArtist", &[("id", &server.artist.aurora_field)])
        .await;
    assert_eq!(
        column(&response["artist"]["album"], "name"),
        ["Glass Harbour", "Paper Maps"]
    );
    assert_eq!(
        column(&response["artist"]["album"], "id"),
        [&server.album.glass_harbour, &server.album.paper_maps]
    );
    assert_eq!(column(&response["artist"]["album"], "year"), [2019, 2021]);
}

#[tokio::test]
async fn get_artist_orders_the_albums_by_year_and_then_by_title() {
    let artist = "Aurora Field";
    let library = LibraryBuilder::new()
        .track(artist, "Gamma", "One", 1, "mp3")
        .track(artist, "Beta", "Two", 1, "mp3")
        .year(2001)
        .track(artist, "Delta", "Three", 1, "mp3")
        .year(1999)
        .track(artist, "Epsilon", "Four", 1, "mp3")
        .track(artist, "alpha", "Five", 1, "mp3")
        .year(2001)
        .build();
    let server = TestServer::scanned(library).await;
    let id = server.catalogue().await.artist(artist).id.clone();

    let response = server.get_json("getArtist", &[("id", &id)]).await;
    assert_eq!(response["artist"]["albumCount"], 5);
    // 1999, then the two albums of 2001 by title without regard to case,
    // then the albums without a year by title.
    assert_eq!(
        column(&response["artist"]["album"], "name"),
        ["Delta", "alpha", "Beta", "Epsilon", "Gamma"]
    );
    let years = column(&response["artist"]["album"], "year");
    assert_eq!(years[..3], [1999, 2001, 2001]);
    // An album without a year has no `year`.
    assert!(years[3].is_null() && years[4].is_null(), "{response}");
}

#[tokio::test]
async fn get_album_returns_the_songs_by_disc_and_then_by_track() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;

    for (album, tracks) in ALBUMS {
        let id = standard.album_id(album);
        let mut expected = standard.album_json(album);
        expected["song"] = tracks
            .iter()
            .map(|track| standard.song_json(track))
            .collect();
        for method in ["getAlbum", "getAlbum.view"] {
            assert_eq!(
                server.get_json(method, &[("id", id)]).await,
                ok("album", expected.clone()),
                "{method} {}",
                album.name
            );
        }
    }

    // "Paper Maps" has two tracks on disc 1 and one on disc 2. The order of
    // the paths is another one: `01 Legend.mp3` is the first path.
    let response = server
        .get_json("getAlbum", &[("id", &server.album.paper_maps)])
        .await;
    let songs = &response["album"]["song"];
    assert_eq!(column(songs, "title"), ["Meridian", "Contour", "Legend"]);
    assert_eq!(column(songs, "discNumber"), [1, 1, 2]);
    assert_eq!(column(songs, "track"), [1, 2, 1]);
    assert_eq!(column(songs, "duration"), [1, 1, 1]);
    assert_eq!(column(songs, "suffix"), ["mp3", "mp3", "mp3"]);
    assert_eq!(
        column(songs, "contentType"),
        ["audio/mpeg", "audio/mpeg", "audio/mpeg"]
    );
    assert_eq!(column(songs, "bitRate"), [34, 34, 34]);
    assert_eq!(
        column(songs, "path"),
        [
            "Aurora Field/Paper Maps/01 Meridian.mp3",
            "Aurora Field/Paper Maps/02 Contour.mp3",
            "Aurora Field/Paper Maps/01 Legend.mp3",
        ]
    );
    // The counts of the album agree with its list of songs.
    assert_eq!(response["album"]["songCount"], 3);
    assert_eq!(response["album"]["duration"], 2);

    // A compilation: the album artist is "Various Artists", and each song
    // names its track artist.
    let response = server
        .get_json("getAlbum", &[("id", &server.album.summer_sampler)])
        .await;
    assert_eq!(response["album"]["artist"], "Various Artists");
    assert_eq!(
        response["album"]["artistId"],
        server.artist.various_artists.as_str()
    );
    let songs = &response["album"]["song"];
    assert_eq!(column(songs, "artist"), ["Tessa Lund", "Aurora Field"]);
    assert_eq!(
        column(songs, "artistId"),
        [
            &server.artist.various_artists,
            &server.artist.various_artists
        ]
    );
}

#[tokio::test]
async fn get_album_xml_has_one_song_element_for_each_track() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;

    for (album, tracks) in ALBUMS {
        let songs: String = tracks
            .iter()
            .map(|track| standard.song_xml("song", track))
            .collect();
        assert_eq!(
            server
                .get_xml("getAlbum", &[("id", standard.album_id(album))])
                .await,
            xml_ok(&format!(
                "<album {}>{songs}</album>",
                standard.album_attributes(album)
            )),
            "{}",
            album.name
        );
    }
}

#[tokio::test]
async fn get_song_on_a_flac_reports_bit_depth_sampling_rate_and_channel_count() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;
    let id = &server.track.low_tide;
    let album_id = &server.album.glass_harbour;

    let response = server.get_json("getSong", &[("id", id)]).await;
    assert_eq!(response["status"], "ok");
    let song = &response["song"];
    assert_eq!(song["bitDepth"], 16);
    assert_eq!(song["samplingRate"], 22_050);
    assert_eq!(song["channelCount"], 1);
    assert_eq!(
        *song,
        json!({
            "id": id,
            "parent": album_id,
            "isDir": false,
            "title": "Low Tide",
            "album": "Glass Harbour",
            "artist": "Aurora Field",
            "track": 1,
            "year": 2019,
            "genre": "Electronic",
            // The album has the file `cover.jpg`.
            "coverArt": album_id,
            "size": standard.size(&LOW_TIDE),
            "contentType": "audio/flac",
            "suffix": "flac",
            "duration": 1,
            "bitRate": 58,
            "bitDepth": 16,
            "samplingRate": 22050,
            "channelCount": 1,
            "path": "Aurora Field/Glass Harbour/01 Low Tide.flac",
            // The track has no disc number tag.
            "discNumber": 1,
            "created": standard.song_created(&LOW_TIDE),
            "albumId": album_id,
            "artistId": server.artist.aurora_field,
            "type": "music",
            "mediaType": "song",
            "playCount": 0,
        })
    );
    assert_eq!(response, ok("song", standard.song_json(&LOW_TIDE)));
}

#[tokio::test]
async fn get_song_returns_each_track_of_the_standard_library() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;

    for (_, tracks) in ALBUMS {
        for track in tracks {
            let id = standard.track_id(track);
            for method in ["getSong", "getSong.view"] {
                assert_eq!(
                    server.get_json(method, &[("id", id)]).await,
                    ok("song", standard.song_json(track)),
                    "{method} {}",
                    track.title
                );
            }
            assert_eq!(
                server.get_xml("getSong", &[("id", id)]).await,
                xml_ok(&standard.song_xml("song", track)),
                "{}",
                track.title
            );
        }
    }

    // A lossy format has no bit depth, and the element reports 0.
    let response = server
        .get_json("getSong", &[("id", &server.track.last_train)])
        .await;
    assert_eq!(response["song"]["bitDepth"], 0);
    assert_eq!(response["song"]["samplingRate"], 24_000);
    assert_eq!(response["song"]["suffix"], "opus");
    assert_eq!(response["song"]["contentType"], "audio/ogg");
    // The album has no cover and the track has no picture.
    assert!(response["song"].get("coverArt").is_none(), "{response}");
}

#[tokio::test]
async fn get_genres_returns_the_genres_with_their_song_and_album_counts() {
    let server = TestServer::standard().await;
    for method in ["getGenres", "getGenres.view"] {
        assert_eq!(
            server.get_json(method, &[]).await,
            ok(
                "genres",
                json!({ "genre": [
                    { "value": "Electronic", "songCount": 6, "albumCount": 2 },
                    { "value": "Jazz", "songCount": 2, "albumCount": 1 },
                    { "value": "Pop", "songCount": 2, "albumCount": 1 },
                ]})
            ),
            "{method}"
        );
    }
}

#[tokio::test]
async fn get_genres_xml_has_the_name_of_a_genre_as_the_text_of_its_element() {
    let server = TestServer::standard().await;
    assert_eq!(
        server.get_xml("getGenres", &[]).await,
        xml_ok(concat!(
            r#"<genres><genre albumCount="2" songCount="6">Electronic</genre>"#,
            r#"<genre albumCount="1" songCount="2">Jazz</genre>"#,
            r#"<genre albumCount="1" songCount="2">Pop</genre></genres>"#
        ))
    );
}

#[tokio::test]
async fn get_genres_orders_the_names_without_regard_to_case_and_escapes_them() {
    let library = LibraryBuilder::new()
        .track("Aurora Field", "Glass Harbour", "Low Tide", 1, "mp3")
        .genre("R&B")
        .track("Aurora Field", "Glass Harbour", "Breakwater", 2, "mp3")
        .genre("R&B")
        .track("Aurora Field", "Glass Harbour", "Night Ferry", 3, "mp3")
        .genre("ambient")
        .track("Marlow Quartet", "Slow Rivers", "Upstream", 1, "mp3")
        .genre("Blues")
        // A track without a genre tag adds no genre.
        .track("Marlow Quartet", "Slow Rivers", "Delta", 2, "mp3")
        .build();
    let server = TestServer::scanned(library).await;

    // The genre of an album is the genre of most of its tracks, so
    // "ambient" is the genre of no album.
    assert_eq!(
        server.get_json("getGenres", &[]).await,
        ok(
            "genres",
            json!({ "genre": [
                { "value": "ambient", "songCount": 1, "albumCount": 0 },
                { "value": "Blues", "songCount": 1, "albumCount": 1 },
                { "value": "R&B", "songCount": 2, "albumCount": 1 },
            ]})
        )
    );
    assert_eq!(
        server.get_xml("getGenres", &[]).await,
        xml_ok(concat!(
            r#"<genres><genre albumCount="0" songCount="1">ambient</genre>"#,
            r#"<genre albumCount="1" songCount="1">Blues</genre>"#,
            r#"<genre albumCount="1" songCount="2">R&amp;B</genre></genres>"#
        ))
    );
}

#[tokio::test]
async fn get_genres_of_a_server_without_a_library_is_an_empty_list() {
    let server = TestServer::bare(Config::default()).await;
    assert_eq!(
        server.get_json("getGenres", &[]).await,
        ok("genres", json!({ "genre": [] }))
    );
    assert_eq!(server.get_xml("getGenres", &[]).await, xml_ok("<genres/>"));
}

#[tokio::test]
async fn unknown_and_malformed_ids_are_error_70_in_json_and_xml() {
    let server = TestServer::standard().await;
    let artist = server.artist.aurora_field.as_str();
    let album = server.album.glass_harbour.as_str();
    let track = server.track.low_tide.as_str();

    // For each method: an ID of its kind that no row has, and the IDs of
    // rows of the two other kinds.
    let cases = [
        (
            "getArtist",
            "ar-ffffffffffffffffffffffffffffffff",
            artist,
            [album, track],
        ),
        (
            "getAlbum",
            "al-ffffffffffffffffffffffffffffffff",
            album,
            [artist, track],
        ),
        (
            "getSong",
            "tr-ffffffffffffffffffffffffffffffff",
            track,
            [artist, album],
        ),
    ];
    for (method, unknown, own, other_kinds) in cases {
        // Values that do not have the shape of an ID.
        let upper = own.to_uppercase();
        let long = format!("{own}0");
        let short = &own[..own.len() - 1];
        let leading_space = format!(" {own}");
        let trailing_space = format!("{own} ");
        let wildcard = format!("{}%", &own[..3]);
        let mut ids = vec![
            unknown,
            "",
            "1",
            "0",
            "-1",
            &own[..3],
            &own[3..],
            "' OR 1=1 --",
            "%",
            "pl-ffffffffffffffffffffffffffffffff",
            upper.as_str(),
            long.as_str(),
            short,
            leading_space.as_str(),
            trailing_space.as_str(),
            wildcard.as_str(),
        ];
        ids.extend(other_kinds);

        for id in ids {
            for name in [method.to_owned(), format!("{method}.view")] {
                assert_eq!(
                    server.get_json(&name, &[("id", id)]).await,
                    failed(70, NOT_FOUND),
                    "{name} {id:?}"
                );
            }
            assert_eq!(
                server.get_xml(method, &[("id", id)]).await,
                xml_failed(70, NOT_FOUND),
                "{method} {id:?}"
            );
        }

        // The ID of the row itself is found.
        let response = server.get_json(method, &[("id", own)]).await;
        assert_eq!(response["status"], "ok", "{method}");
    }
}

#[tokio::test]
async fn methods_that_need_an_id_answer_error_10_without_one() {
    let server = TestServer::standard().await;
    let message = "required parameter is missing: id";
    for method in ["getArtist", "getAlbum", "getSong"] {
        for name in [method.to_owned(), format!("{method}.view")] {
            assert_eq!(
                server.get_json(&name, &[]).await,
                failed(10, message),
                "{name}"
            );
            // The name of a parameter is compared as it is written.
            assert_eq!(
                server
                    .get_json(&name, &[("ID", &server.track.low_tide)])
                    .await,
                failed(10, message),
                "{name}"
            );
        }
        assert_eq!(
            server.get_xml(method, &[]).await,
            xml_failed(10, message),
            "{method}"
        );
    }
}

#[tokio::test]
async fn browse_methods_answer_a_post_with_a_form_body() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;
    let json_of = |response| async move {
        assert_eq!(content_type(&response), "application/json");
        subsonic_response(&body_string(response).await)
    };

    let form = [("id", server.track.low_tide.as_str()), ("f", "json")];
    let response = json_of(server.post_form("getSong.view", &form).await).await;
    assert_eq!(response, ok("song", standard.song_json(&LOW_TIDE)));

    let form = [("id", "tr-1"), ("f", "json")];
    let response = json_of(server.post_form("getSong", &form).await).await;
    assert_eq!(response, failed(70, NOT_FOUND));

    let folder_id = server.folder_id.to_string();
    let form = [("musicFolderId", folder_id.as_str()), ("f", "json")];
    let response = json_of(server.post_form("getArtists", &form).await).await;
    assert_eq!(response, server.get_json("getArtists", &[]).await);
}

/// 2023-03-14T17:51:22Z.
const STARRED_AT: i64 = 1_678_816_282;

/// 2023-03-15T00:00:00Z.
const PLAYED_AT: i64 = 1_678_838_400;

#[tokio::test]
async fn browse_methods_report_what_the_user_attached_to_an_item() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;
    let user_id = owner_id(&server).await;
    let artist = server.artist.aurora_field.clone();
    let album = server.album.glass_harbour.clone();
    let track = server.track.breakwater.clone();
    server
        .db()
        .write(move |conn| {
            let mut insert = conn.prepare(
                "INSERT INTO annotations
                   (user_id, item_id, item_type, starred_at, rating, play_count, last_played_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            insert.execute(params![
                user_id,
                artist,
                "artist",
                STARRED_AT,
                None::<u8>,
                0,
                None::<i64>
            ])?;
            insert.execute(params![
                user_id,
                album,
                "album",
                STARRED_AT,
                4,
                0,
                None::<i64>
            ])?;
            insert.execute(params![
                user_id, track, "track", STARRED_AT, 5, 3, PLAYED_AT
            ])?;
            Ok(())
        })
        .await
        .expect("write the annotations");
    let starred = "2023-03-14T17:51:22Z";
    let played = "2023-03-15T00:00:00Z";

    // The artist: `starred`. The element has no rating and no play count.
    let [aurora_field, marlow_quartet, _] = &ARTISTS;
    let mut artist = standard.artist_json(aurora_field);
    artist["starred"] = json!(starred);
    let response = server.get_json("getArtists", &[]).await;
    assert_eq!(response["artists"]["index"][0]["artist"], json!([artist]));
    // An artist that the user did not star has no `starred`.
    let other = &response["artists"]["index"][1]["artist"][0];
    assert_eq!(*other, standard.artist_json(marlow_quartet));

    // The album, as an entry of its artist and as the answer of `getAlbum`.
    // Its plays are those of its tracks, of which one was played.
    let mut album = standard.album_json(&GLASS_HARBOUR);
    album["starred"] = json!(starred);
    album["userRating"] = json!(4);
    album["playCount"] = json!(3);
    album["played"] = json!(played);
    artist["album"] = json!([album, standard.album_json(&PAPER_MAPS)]);
    assert_eq!(
        server
            .get_json("getArtist", &[("id", &server.artist.aurora_field)])
            .await,
        ok("artist", artist)
    );

    // The song, as an entry of its album and as the answer of `getSong`.
    let mut song = standard.song_json(&BREAKWATER);
    song["starred"] = json!(starred);
    song["userRating"] = json!(5);
    song["playCount"] = json!(3);
    song["played"] = json!(played);
    album["song"] = json!([
        standard.song_json(&LOW_TIDE),
        song,
        standard.song_json(&NIGHT_FERRY),
    ]);
    assert_eq!(
        server
            .get_json("getAlbum", &[("id", &server.album.glass_harbour)])
            .await,
        ok("album", album)
    );
    assert_eq!(
        server
            .get_json("getSong", &[("id", &server.track.breakwater)])
            .await,
        ok("song", song)
    );

    // In XML the four values are attributes of the element.
    let xml = server
        .get_xml("getSong", &[("id", &server.track.breakwater)])
        .await;
    for attribute in [
        format!(r#" playCount="3" played="{played}" "#),
        format!(r#" starred="{starred}" "#),
        r#" userRating="5" "#.to_owned(),
    ] {
        assert!(xml.contains(&attribute), "{attribute}: {xml}");
    }
}

#[tokio::test]
async fn a_track_without_a_file_is_returned_without_a_path() {
    let server = TestServer::standard().await;
    let standard = Standard::of(&server).await;
    let upstream = server.track.upstream.as_str();
    let folder_id = server.folder_id.to_string();
    remove_file(&server, upstream).await;

    // The song keeps every field of the file it had, except `path`. Its
    // cover is the cover of the album.
    let mut song = standard.song_json(&UPSTREAM);
    song.as_object_mut().expect("an object").remove("path");
    let response = server.get_json("getSong", &[("id", upstream)]).await;
    assert_eq!(response, ok("song", song.clone()));
    assert!(response["song"].get("path").is_none(), "{response}");
    assert_eq!(
        response["song"]["coverArt"],
        server.album.slow_rivers.as_str()
    );
    let body = server.get_xml("getSong", &[("id", upstream)]).await;
    assert!(!body.contains(" path="), "{body}");
    // The placeholder path of the row is in no document.
    assert!(!body.contains(&format!("/{upstream}")), "{body}");

    // The album lists the track, and its count agrees with the list.
    let mut album = standard.album_json(&SLOW_RIVERS);
    album["song"] = json!([song, standard.song_json(&DELTA)]);
    assert_eq!(
        server
            .get_json("getAlbum", &[("id", &server.album.slow_rivers)])
            .await,
        ok("album", album)
    );

    // One track of the artist still has a file in the folder.
    let in_folder = [("musicFolderId", folder_id.as_str())];
    let response = server.get_json("getArtists", &in_folder).await;
    assert_eq!(
        artist_names(&response["artists"]),
        ["Aurora Field", "Marlow Quartet", "Various Artists"]
    );

    // No track of the artist has a file: the artist is in no folder. The
    // list without a folder holds it, and `getArtist` returns it.
    remove_file(&server, &server.track.delta).await;
    let response = server.get_json("getArtists", &in_folder).await;
    assert_eq!(
        artist_names(&response["artists"]),
        ["Aurora Field", "Various Artists"]
    );
    let response = server.get_json("getArtists", &[]).await;
    assert_eq!(
        artist_names(&response["artists"]),
        ["Aurora Field", "Marlow Quartet", "Various Artists"]
    );
    let response = server
        .get_json("getArtist", &[("id", &server.artist.marlow_quartet)])
        .await;
    assert_eq!(response["artist"]["albumCount"], 1);
    assert_eq!(column(&response["artist"]["album"], "songCount"), [2]);
}

#[tokio::test]
async fn two_calls_return_the_same_document() {
    let server: StandardServer = TestServer::standard().await;
    let folder_id = server.folder_id.to_string();
    let calls: [(&str, Vec<(&str, &str)>); 7] = [
        ("getMusicFolders", vec![]),
        ("getArtists", vec![]),
        ("getArtists", vec![("musicFolderId", &folder_id)]),
        ("getArtist", vec![("id", &server.artist.aurora_field)]),
        ("getAlbum", vec![("id", &server.album.paper_maps)]),
        ("getSong", vec![("id", &server.track.open_road)]),
        ("getGenres", vec![]),
    ];
    for (method, params) in &calls {
        let first = server.get_xml(method, params).await;
        assert!(first.contains(r#" status="ok" "#), "{method}: {first}");
        for _ in 0..3 {
            assert_eq!(server.get_xml(method, params).await, first, "{method}");
        }
        let first = server.get_json(method, params).await;
        assert_eq!(server.get_json(method, params).await, first, "{method}");
    }
}
