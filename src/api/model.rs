//! The response models of the API.
//!
//! One set of models serves JSON and XML. Every model derives
//! `serde::Serialize` and carries these serde attributes:
//!
//! - `#[serde(rename_all = "camelCase")]` on the type. The serialised field
//!   names are then the OpenSubsonic names.
//! - `#[serde(skip_serializing_if = "Option::is_none")]` on every `Option`
//!   field. A `None` field is then absent from both formats.
//!
//! # Models and constructors
//!
//! The models of this module are the elements that several API methods
//! share. Their names are the names of the OpenSubsonic API. Each is built
//! from a row of `db::queries::browse` by its constructors:
//!
//! - [`Child`] for a song: [`child`], from a `browse::Song`.
//! - [`Child`] for an album that is listed as a directory:
//!   [`child_dir_for_album`], from a `browse::Album`.
//! - [`AlbumID3`]: [`album`] and [`album_with_songs`], from a
//!   `browse::Album`.
//! - [`ArtistID3`]: [`artist`] and [`artist_with_albums`], from a
//!   `browse::Artist`.
//! - [`Artist`], the artist of the folder methods: [`artist_dir`], from a
//!   `browse::Artist`.
//! - [`Directory`]: [`directory_for_artist`] and [`directory_for_album`].
//! - [`Index`]: [`index`], from a list of `browse::Artist`.
//! - [`Genre`]: [`genre`], from a `browse::Genre`.
//! - [`MusicFolder`]: [`music_folder`], from a `folders::Folder`.
//!
//! A handler builds these elements through the constructors and in no other
//! way (`docs/design.md` section 3.8). A constructor of an item that a user
//! can star, rate or play takes the annotation of the user for the item: the
//! entry of the item in the map that `annotations::get_many` returns, or
//! `None` when the map has none.
//!
//! An element that one method alone returns, such as the `searchResult3`
//! that holds three lists of these models, is a private type in the module
//! of its handler.
//!
//! # Values
//!
//! - A time is ISO 8601 in UTC with whole seconds ([`iso8601`]).
//! - A duration is whole seconds, rounded to the nearest one ([`seconds`]).
//! - A cover art ID is the ID of an album that has a cover, or the ID of a
//!   track ([`album_cover_art`], [`song_cover_art`]).
//!
//! Each model has the fields that the server sends. The OpenSubsonic API
//! defines more optional fields, and the server sends none of them.

use std::collections::BTreeMap;

use serde::Serialize;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::db::queries::annotations::Annotation;
use crate::db::queries::browse::{self, Song};
use crate::db::queries::folders::Folder;

/// The articles that [`index_letter`] and [`index`] ignore at the start of a
/// name, in the order of the `ignoredArticles` value.
const IGNORED_ARTICLES: [&str; 7] = ["The", "El", "La", "Los", "Las", "Le", "Les"];

/// The name of the index of the names that start with no letter from A to Z.
const OTHER_INDEX: &str = "#";

/// The `type` of a song.
const TYPE_MUSIC: &str = "music";

/// The `mediaType` of a song.
const MEDIA_TYPE_SONG: &str = "song";

/// The `mediaType` of an album that is listed as a directory.
const MEDIA_TYPE_ALBUM: &str = "album";

/// The `Child` element: a song, or an album that a folder method lists as a
/// directory.
///
/// `id`, `isDir` and `title` are required by the API. Every other field is
/// optional there. A song has every field below except `starred`,
/// `userRating` and `played`, which follow its annotation, and `track`,
/// `year`, `genre`, `coverArt`, `bitRate` and `path`, which are absent when
/// the track has no such value. A directory has the fields that
/// [`child_dir_for_album`] names.
///
/// `bitDepth`, `samplingRate`, `channelCount`, `mediaType` and `played` are
/// OpenSubsonic additions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Child {
    pub id: String,
    /// The ID of the album of a song. The ID of the artist of an album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub is_dir: bool,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// The track artist of a song. The album artist of an album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// The track number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    /// The size of the file in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    /// The duration in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
    /// The bitrate in kbps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
    /// The sample rate in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_count: Option<u8>,
    /// The path of the file below its music folder, with `/` between the
    /// components.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    /// The ID of the album artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    /// `music` for a song.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<&'static str>,
    /// `song` or `album`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    /// The rating of the user, 1 to 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<u8>,
    pub play_count: u64,
    /// The time of the last play.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played: Option<String>,
}

/// The `AlbumID3` element. With `song` it is the `AlbumID3WithSongs` element
/// that `getAlbum` returns.
///
/// `id`, `name`, `songCount`, `duration` and `created` are required by the
/// API. `userRating` is an OpenSubsonic addition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumID3 {
    pub id: String,
    pub name: String,
    /// The name of the album artist.
    pub artist: String,
    /// The ID of the album artist.
    pub artist_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    pub song_count: u32,
    /// The duration in seconds.
    pub duration: u64,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    /// The rating of the user, 1 to 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<u8>,
    pub play_count: u64,
    /// The songs of the album. `None` where the album is an entry of a list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song: Option<Vec<Child>>,
}

/// The `ArtistID3` element. With `album` it is the `ArtistWithAlbumsID3`
/// element that `getArtist` returns.
///
/// `id` and `name` are required by the API, and `albumCount` is required by
/// the Subsonic schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistID3 {
    pub id: String,
    pub name: String,
    /// Always `None`: an artist has no image (`docs/design.md` section 1.3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    pub album_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    /// The albums of the artist. `None` where the artist is an entry of a
    /// list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<Vec<AlbumID3>>,
}

/// The `Artist` element of the folder methods: an entry of `getIndexes`,
/// `search2` and `getStarred`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    /// The rating of the user, 1 to 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<u8>,
}

/// The `Directory` element that `getMusicDirectory` returns: an artist with
/// its albums, or an album with its songs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    pub id: String,
    /// The ID of the artist of an album. An artist has no parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    /// The rating of the user, 1 to 5.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<u8>,
    /// The play count of an album. An artist has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<u64>,
    pub child: Vec<Child>,
}

/// One index of artists: the `IndexID3` element of `getArtists` when `T` is
/// [`ArtistID3`], and the `Index` element of `getIndexes` when `T` is
/// [`Artist`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Index<T> {
    /// A letter from `A` to `Z`, or `#`.
    pub name: String,
    pub artist: Vec<T>,
}

/// The `Genre` element. In XML the name of the genre is the text of the
/// element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Genre {
    /// The name of the genre.
    pub value: String,
    pub song_count: u64,
    pub album_count: u64,
}

/// The `MusicFolder` element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicFolder {
    /// The integer ID of the folder row.
    pub id: i64,
    pub name: String,
}

/// Returns the `Child` element of `song` with the annotation of the user.
///
/// - `parent` and `albumId` are the ID of the album. `artist` is the track
///   artist, and `artistId` is the ID of the album artist, because a track
///   artist has no row of its own.
/// - `discNumber` is 1 for a track without a disc number.
/// - `bitDepth`, `samplingRate` and `channelCount` are 0 when the file gave
///   none. The OpenSubsonic API asks a server to send a field that it
///   supports with a default value in that case.
/// - `playCount` is 0 for a song that was not played.
/// - `path` is absent for a track without a file, and `coverArt` is then
///   never the ID of the track (`docs/design.md` section 3.4). The other
///   fields of such a track are those of the file it had.
pub fn child(song: &Song, annotation: Option<&Annotation>) -> Child {
    let track = &song.track;
    let annotation = annotation.copied().unwrap_or_default();
    Child {
        id: track.id.clone(),
        parent: Some(track.album_id.clone()),
        is_dir: false,
        title: track.title.clone(),
        album: Some(track.album_name.clone()),
        artist: Some(track.artist_name.clone()),
        track: track.track_no,
        year: track.year,
        genre: track.genre.clone(),
        cover_art: song_cover_art(song),
        size: Some(track.size),
        content_type: Some(track.content_type.clone()),
        suffix: Some(track.suffix.clone()),
        duration: Some(seconds(track.duration_ms)),
        bit_rate: track.bitrate_kbps,
        bit_depth: Some(track.bit_depth.unwrap_or(0)),
        sampling_rate: Some(track.sample_rate.unwrap_or(0)),
        channel_count: Some(track.channels.unwrap_or(0)),
        path: track.has_file().then(|| track.rel_path.clone()),
        disc_number: Some(track.disc_no.unwrap_or(1)),
        created: Some(iso8601(track.created_at)),
        album_id: Some(track.album_id.clone()),
        artist_id: Some(track.artist_id.clone()),
        r#type: Some(TYPE_MUSIC),
        media_type: Some(MEDIA_TYPE_SONG),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
        play_count: annotation.play_count,
        played: annotation.last_played_at.map(iso8601),
    }
}

/// Returns the `Child` element that lists `album` as a directory, with the
/// annotation of the user. `getAlbumList`, `search2`, `getStarred` and
/// `getMusicDirectory` list an album in this form.
///
/// The element has `isDir: true`, the title of the album as `title` and as
/// `album`, the ID of the album artist as `parent` and as `artistId`,
/// `mediaType: album`, and `artist`, `year`, `genre`, `coverArt`,
/// `duration`, `created` and the annotation fields. It has no field of a
/// file.
pub fn child_dir_for_album(album: &browse::Album, annotation: Option<&Annotation>) -> Child {
    let annotation = annotation.copied().unwrap_or_default();
    Child {
        id: album.id.clone(),
        parent: Some(album.artist_id.clone()),
        is_dir: true,
        title: album.name.clone(),
        album: Some(album.name.clone()),
        artist: Some(album.artist_name.clone()),
        track: None,
        year: album.year,
        genre: album.genre.clone(),
        cover_art: album_cover_art(album),
        size: None,
        content_type: None,
        suffix: None,
        duration: Some(seconds(album.duration_ms)),
        bit_rate: None,
        bit_depth: None,
        sampling_rate: None,
        channel_count: None,
        path: None,
        disc_number: None,
        created: Some(iso8601(album.created_at)),
        album_id: None,
        artist_id: Some(album.artist_id.clone()),
        r#type: None,
        media_type: Some(MEDIA_TYPE_ALBUM),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
        play_count: annotation.play_count,
        played: annotation.last_played_at.map(iso8601),
    }
}

/// Returns the `AlbumID3` element of `album` with the annotation of the
/// user, without its songs.
///
/// `songCount` and `duration` are the values that the last scan stored.
/// `playCount` is 0 for an album without a play.
pub fn album(album: &browse::Album, annotation: Option<&Annotation>) -> AlbumID3 {
    let annotation = annotation.copied().unwrap_or_default();
    AlbumID3 {
        id: album.id.clone(),
        name: album.name.clone(),
        artist: album.artist_name.clone(),
        artist_id: album.artist_id.clone(),
        cover_art: album_cover_art(album),
        song_count: album.song_count,
        duration: seconds(album.duration_ms),
        created: iso8601(album.created_at),
        year: album.year,
        genre: album.genre.clone(),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
        play_count: annotation.play_count,
        song: None,
    }
}

/// Returns the `AlbumID3` element of `row` with the annotation of the user
/// and with `songs` as its `song` list, which is the element of `getAlbum`.
pub fn album_with_songs(
    row: &browse::Album,
    annotation: Option<&Annotation>,
    songs: Vec<Child>,
) -> AlbumID3 {
    AlbumID3 {
        song: Some(songs),
        ..album(row, annotation)
    }
}

/// Returns the `ArtistID3` element of `artist` with the annotation of the
/// user, without its albums. `albumCount` is the value that the last scan
/// stored. The element has no `coverArt`.
pub fn artist(artist: &browse::Artist, annotation: Option<&Annotation>) -> ArtistID3 {
    let annotation = annotation.copied().unwrap_or_default();
    ArtistID3 {
        id: artist.id.clone(),
        name: artist.name.clone(),
        cover_art: None,
        album_count: artist.album_count,
        starred: annotation.starred_at.map(iso8601),
        album: None,
    }
}

/// Returns the `ArtistID3` element of `row` with the annotation of the user
/// and with `albums` as its `album` list, which is the element of
/// `getArtist`.
pub fn artist_with_albums(
    row: &browse::Artist,
    annotation: Option<&Annotation>,
    albums: Vec<AlbumID3>,
) -> ArtistID3 {
    ArtistID3 {
        album: Some(albums),
        ..artist(row, annotation)
    }
}

/// Returns the `Artist` element of the folder methods for `artist`, with the
/// annotation of the user.
pub fn artist_dir(artist: &browse::Artist, annotation: Option<&Annotation>) -> Artist {
    let annotation = annotation.copied().unwrap_or_default();
    Artist {
        id: artist.id.clone(),
        name: artist.name.clone(),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
    }
}

/// Returns the `Directory` element of `artist` with the annotation of the
/// user. `children` are its albums, each built with
/// [`child_dir_for_album`]. The element has no `parent` and no `playCount`.
pub fn directory_for_artist(
    artist: &browse::Artist,
    annotation: Option<&Annotation>,
    children: Vec<Child>,
) -> Directory {
    let annotation = annotation.copied().unwrap_or_default();
    Directory {
        id: artist.id.clone(),
        parent: None,
        name: artist.name.clone(),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
        play_count: None,
        child: children,
    }
}

/// Returns the `Directory` element of `album` with the annotation of the
/// user. `children` are its songs, each built with [`child`]. `parent` is
/// the ID of the album artist.
pub fn directory_for_album(
    album: &browse::Album,
    annotation: Option<&Annotation>,
    children: Vec<Child>,
) -> Directory {
    let annotation = annotation.copied().unwrap_or_default();
    Directory {
        id: album.id.clone(),
        parent: Some(album.artist_id.clone()),
        name: album.name.clone(),
        starred: annotation.starred_at.map(iso8601),
        user_rating: annotation.rating,
        play_count: Some(annotation.play_count),
        child: children,
    }
}

/// Groups `artists` into indexes and builds the element of each artist with
/// `model`, which is [`artist`] for `getArtists` and [`artist_dir`] for
/// `getIndexes`.
///
/// The index of an artist is [`index_letter`] of its name. The indexes are
/// in the order of their names, so `#` is the first. The artists of an index
/// are in the order of their names as [`index_letter`] reads them: without a
/// leading article of [`ignored_articles`], without diacritics and in lower
/// case. Two artists with the same such name are in the order of their names
/// and then of their IDs. An index without an artist is not part of the
/// result.
pub fn index<T>(artists: &[browse::Artist], model: impl Fn(&browse::Artist) -> T) -> Vec<Index<T>> {
    let mut groups: BTreeMap<String, Vec<(String, &browse::Artist)>> = BTreeMap::new();
    for artist in artists {
        groups
            .entry(index_letter(&artist.name))
            .or_default()
            .push((sort_text(&artist.name), artist));
    }
    groups
        .into_iter()
        .map(|(name, mut members)| {
            members.sort_by(|(a_sort, a), (b_sort, b)| {
                (a_sort, &a.name, &a.id).cmp(&(b_sort, &b.name, &b.id))
            });
            let artist = members.into_iter().map(|(_, row)| model(row)).collect();
            Index { name, artist }
        })
        .collect()
}

/// Returns the name of the index of the artist `name`: the first letter of
/// the name in upper case when it is a letter from A to Z, and `#` for every
/// other name.
///
/// A leading article of [`ignored_articles`] and the white space after it
/// are not part of the name here, so "The Beatles" is in `B`. An article is
/// matched without regard to ASCII case, and only as a whole word that
/// another word follows: "Them" is in `T`, and so is "The". A letter with a
/// diacritic counts as its base letter, so "Édith Piaf" is in `E`.
pub fn index_letter(name: &str) -> String {
    match sort_text(name).chars().next() {
        Some(letter) if letter.is_ascii_lowercase() => letter.to_ascii_uppercase().to_string(),
        _ => OTHER_INDEX.to_owned(),
    }
}

/// Returns the text that [`index_letter`] and [`index`] group and order an
/// artist name by: the name without a leading article, without the marks of
/// its diacritics and in lower case. "The Beatles" is `beatles`, and "Édith
/// Piaf" is `edith piaf`.
fn sort_text(name: &str) -> String {
    without_article(name)
        .nfd()
        .filter(|character| !is_combining_mark(*character))
        .collect::<String>()
        .to_lowercase()
}

/// Returns the value of `ignoredArticles` in `getArtists` and `getIndexes`:
/// the articles that [`index_letter`] ignores, separated by spaces.
pub fn ignored_articles() -> String {
    IGNORED_ARTICLES.join(" ")
}

/// Returns `name` without leading white space and without a leading article
/// of [`IGNORED_ARTICLES`]. The article is removed only when white space and
/// another word follow it.
fn without_article(name: &str) -> &str {
    let name = name.trim_start();
    let Some((first, rest)) = name.split_once(char::is_whitespace) else {
        return name;
    };
    let rest = rest.trim_start();
    let is_article = IGNORED_ARTICLES
        .iter()
        .any(|article| first.eq_ignore_ascii_case(article));
    if is_article && !rest.is_empty() {
        rest
    } else {
        name
    }
}

/// Returns the `Genre` element of `genre`.
pub fn genre(genre: &browse::Genre) -> Genre {
    Genre {
        value: genre.name.clone(),
        song_count: genre.song_count,
        album_count: genre.album_count,
    }
}

/// Returns the `MusicFolder` element of `folder`. `name` is the last
/// component of the path of the folder. The path itself is not part of the
/// element.
pub fn music_folder(folder: &Folder) -> MusicFolder {
    MusicFolder {
        id: folder.id,
        name: folder.name.clone(),
    }
}

/// Returns the cover art ID of `album`: its own ID when the album has a
/// cover, and `None` otherwise.
pub fn album_cover_art(album: &browse::Album) -> Option<String> {
    album.cover_source.as_ref().map(|_| album.id.clone())
}

/// Returns the cover art ID of `song`: the ID of its album when the album
/// has a cover, else the ID of the track when its file holds a picture, and
/// `None` otherwise.
///
/// A track without a file gives no picture, so its own ID is never its cover
/// art ID (`docs/design.md` section 3.4).
pub fn song_cover_art(song: &Song) -> Option<String> {
    let track = &song.track;
    if song.album_has_cover {
        Some(track.album_id.clone())
    } else if track.has_picture && track.has_file() {
        Some(track.id.clone())
    } else {
        None
    }
}

/// Returns `milliseconds` in whole seconds, rounded to the nearest second.
/// Half a second is rounded up.
pub fn seconds(milliseconds: u64) -> u64 {
    milliseconds / 1000 + u64::from(milliseconds % 1000 >= 500)
}

/// Returns the Unix time `unix_seconds` as ISO 8601 text in UTC with whole
/// seconds: `2023-03-14T17:51:22Z`.
pub fn iso8601(unix_seconds: i64) -> String {
    let (year, month, day) = civil_date(unix_seconds.div_euclid(86_400));
    let second_of_day = unix_seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60
    )
}

/// Returns the year, the month and the day of the day that is `days` days
/// after 1970-01-01, in the proleptic Gregorian calendar.
///
/// The computation counts days from 1 March of year 0. A year that starts in
/// March has its leap day at the end, so the month and the day follow from
/// the day of the year without a table of month lengths.
fn civil_date(days: i64) -> (i64, i64, i64) {
    /// Days from 0000-03-01 to 1970-01-01.
    const EPOCH_DAY: i64 = 719_468;
    /// Days in the 400-year cycle of the calendar.
    const CYCLE_DAYS: i64 = 146_097;

    let days = days + EPOCH_DAY;
    let cycle = days.div_euclid(CYCLE_DAYS);
    let day_of_cycle = days.rem_euclid(CYCLE_DAYS);
    let year_of_cycle = (day_of_cycle - day_of_cycle / 1_460 + day_of_cycle / 36_524
        - day_of_cycle / 146_096)
        / 365;
    let day_of_year =
        day_of_cycle - (365 * year_of_cycle + year_of_cycle / 4 - year_of_cycle / 100);
    // Months count from March: 0 is March and 11 is February.
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_cycle + 400 * cycle + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::envelope::{Format, SubsonicResponse};
    use crate::db::queries::tracks::{self, Track};

    // The models take an ID as it is, so the tests use short ones.
    const ARTIST: &str = "ar-01";
    const ALBUM: &str = "al-01";
    const TRACK: &str = "tr-01";

    /// 2023-03-14T17:51:22Z.
    const CREATED: i64 = 1_678_816_282;

    /// A FLAC track that has a value in every column.
    fn track() -> Track {
        Track {
            id: TRACK.to_owned(),
            folder_id: 1,
            rel_path: "Aurora Field/Glass Harbour/01 Low Tide.flac".to_owned(),
            fingerprint: "19866778:aa".to_owned(),
            title: "Low Tide".to_owned(),
            album_id: ALBUM.to_owned(),
            artist_id: ARTIST.to_owned(),
            artist_name: "Aurora Field".to_owned(),
            album_name: "Glass Harbour".to_owned(),
            track_no: Some(1),
            disc_no: Some(2),
            year: Some(2019),
            genre: Some("Electronic".to_owned()),
            duration_ms: 215_600,
            bitrate_kbps: Some(880),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            size: 19_866_778,
            suffix: "flac".to_owned(),
            content_type: "audio/flac".to_owned(),
            lossless: true,
            has_picture: true,
            has_lyrics: false,
            mtime: 1_600_000_000,
            created_at: CREATED,
            updated_at: CREATED + 60,
        }
    }

    /// [`track`] on an album that has a cover.
    fn song() -> Song {
        Song {
            track: track(),
            album_has_cover: true,
        }
    }

    /// An MP3 track with the values that every track has and no other, on an
    /// album without a cover.
    fn bare_song() -> Song {
        Song {
            track: Track {
                rel_path: "untitled.mp3".to_owned(),
                title: "untitled".to_owned(),
                track_no: None,
                disc_no: None,
                year: None,
                genre: None,
                duration_ms: 499,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                size: 4096,
                suffix: "mp3".to_owned(),
                content_type: "audio/mpeg".to_owned(),
                lossless: false,
                has_picture: false,
                ..track()
            },
            album_has_cover: false,
        }
    }

    /// An album that has a value in every column.
    fn album_row() -> browse::Album {
        browse::Album {
            id: ALBUM.to_owned(),
            artist_id: ARTIST.to_owned(),
            name: "Glass Harbour".to_owned(),
            artist_name: "Aurora Field".to_owned(),
            year: Some(2019),
            genre: Some("Electronic".to_owned()),
            song_count: 3,
            duration_ms: 645_400,
            compilation: false,
            cover_source: Some("/music/Aurora Field/Glass Harbour/cover.jpg".to_owned()),
            cover_mtime: Some(1_600_000_000),
            created_at: CREATED,
        }
    }

    /// [`album_row`] without a year, a genre and a cover.
    fn bare_album_row() -> browse::Album {
        browse::Album {
            year: None,
            genre: None,
            cover_source: None,
            cover_mtime: None,
            ..album_row()
        }
    }

    fn artist_row() -> browse::Artist {
        named("ar-01", "Aurora Field")
    }

    fn named(id: &str, name: &str) -> browse::Artist {
        browse::Artist {
            id: id.to_owned(),
            name: name.to_owned(),
            sort_name: name.to_owned(),
            album_count: 2,
        }
    }

    /// A star at 2023-11-14T22:13:20Z, a rating of 4 and 8 plays, the last
    /// at 2023-11-14T22:21:40Z.
    fn annotation() -> Annotation {
        Annotation {
            starred_at: Some(1_700_000_000),
            rating: Some(4),
            play_count: 8,
            last_played_at: Some(1_700_000_500),
        }
    }

    /// The JSON document and the XML document of a response that carries
    /// `value` under `key`.
    fn documents(key: &str, value: impl Serialize) -> (String, String) {
        let value = serde_json::to_value(value).expect("the model serialises");
        let response = SubsonicResponse::ok(key, value);
        (
            response.render(Format::Json).1,
            response.render(Format::Xml).1,
        )
    }

    /// The JSON document whose payload is `payload`, the text of one entry
    /// of the response object.
    fn json_document(payload: &str) -> String {
        format!(
            concat!(
                r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"wn-server","#,
                r#""serverVersion":"{}","openSubsonic":true,{}}}}}"#
            ),
            crate::VERSION,
            payload
        )
    }

    /// The XML document whose root element holds `payload`.
    fn xml_document(payload: &str) -> String {
        format!(
            concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#,
                r#" version="1.16.1" type="wn-server" serverVersion="{}" openSubsonic="true">"#,
                "{}</subsonic-response>"
            ),
            crate::VERSION,
            payload
        )
    }

    /// Checks both documents of a response that carries `value` under `key`.
    fn assert_documents(key: &str, value: impl Serialize, json: &str, xml: &str) {
        let (json_body, xml_body) = documents(key, value);
        assert_eq!(json_body, json_document(json));
        assert_eq!(xml_body, xml_document(xml));
    }

    /// The `song` entry of [`bare_song`] in JSON, without its key.
    const BARE_SONG_JSON: &str = concat!(
        r#"{"album":"Glass Harbour","albumId":"al-01","artist":"Aurora Field","#,
        r#""artistId":"ar-01","bitDepth":0,"channelCount":0,"contentType":"audio/mpeg","#,
        r#""created":"2023-03-14T17:51:22Z","discNumber":1,"duration":0,"id":"tr-01","#,
        r#""isDir":false,"mediaType":"song","parent":"al-01","path":"untitled.mp3","#,
        r#""playCount":0,"samplingRate":0,"size":4096,"suffix":"mp3","title":"untitled","#,
        r#""type":"music"}"#
    );

    /// The attributes of the element of [`bare_song`] in XML.
    const BARE_SONG_XML: &str = concat!(
        r#"album="Glass Harbour" albumId="al-01" artist="Aurora Field" artistId="ar-01""#,
        r#" bitDepth="0" channelCount="0" contentType="audio/mpeg""#,
        r#" created="2023-03-14T17:51:22Z" discNumber="1" duration="0" id="tr-01""#,
        r#" isDir="false" mediaType="song" parent="al-01" path="untitled.mp3""#,
        r#" playCount="0" samplingRate="0" size="4096" suffix="mp3" title="untitled""#,
        r#" type="music""#
    );

    #[test]
    fn a_song_with_every_value_is_this_document() {
        assert_documents(
            "song",
            child(&song(), Some(&annotation())),
            concat!(
                r#""song":{"album":"Glass Harbour","albumId":"al-01","artist":"Aurora Field","#,
                r#""artistId":"ar-01","bitDepth":16,"bitRate":880,"channelCount":2,"#,
                r#""contentType":"audio/flac","coverArt":"al-01","#,
                r#""created":"2023-03-14T17:51:22Z","discNumber":2,"duration":216,"#,
                r#""genre":"Electronic","id":"tr-01","isDir":false,"mediaType":"song","#,
                r#""parent":"al-01","path":"Aurora Field/Glass Harbour/01 Low Tide.flac","#,
                r#""playCount":8,"played":"2023-11-14T22:21:40Z","samplingRate":44100,"#,
                r#""size":19866778,"starred":"2023-11-14T22:13:20Z","suffix":"flac","#,
                r#""title":"Low Tide","track":1,"type":"music","userRating":4,"year":2019}"#
            ),
            concat!(
                r#"<song album="Glass Harbour" albumId="al-01" artist="Aurora Field""#,
                r#" artistId="ar-01" bitDepth="16" bitRate="880" channelCount="2""#,
                r#" contentType="audio/flac" coverArt="al-01""#,
                r#" created="2023-03-14T17:51:22Z" discNumber="2" duration="216""#,
                r#" genre="Electronic" id="tr-01" isDir="false" mediaType="song""#,
                r#" parent="al-01" path="Aurora Field/Glass Harbour/01 Low Tide.flac""#,
                r#" playCount="8" played="2023-11-14T22:21:40Z" samplingRate="44100""#,
                r#" size="19866778" starred="2023-11-14T22:13:20Z" suffix="flac""#,
                r#" title="Low Tide" track="1" type="music" userRating="4" year="2019"/>"#
            ),
        );
    }

    #[test]
    fn a_song_with_few_values_has_the_defaults_and_no_other_field() {
        // No track number, year, genre, bitrate and cover: the fields are
        // absent. No disc number: disc 1. No bit depth, sample rate and
        // channel count: 0. No annotation: no star, no rating, no play.
        assert_documents(
            "song",
            child(&bare_song(), None),
            &format!(r#""song":{BARE_SONG_JSON}"#),
            &format!("<song {BARE_SONG_XML}/>"),
        );
    }

    #[test]
    fn a_song_without_an_annotation_row_equals_one_with_the_default_annotation() {
        assert_eq!(
            child(&song(), None),
            child(&song(), Some(&Annotation::default()))
        );
        let unplayed = child(&song(), None);
        assert_eq!(unplayed.starred, None);
        assert_eq!(unplayed.user_rating, None);
        assert_eq!(unplayed.play_count, 0);
        assert_eq!(unplayed.played, None);
    }

    #[test]
    fn a_song_takes_each_annotation_value_on_its_own() {
        let starred = Annotation {
            starred_at: Some(0),
            ..Annotation::default()
        };
        let element = child(&song(), Some(&starred));
        assert_eq!(element.starred.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!((element.user_rating, element.play_count), (None, 0));
        assert_eq!(element.played, None);

        let rated = Annotation {
            rating: Some(1),
            ..Annotation::default()
        };
        let element = child(&song(), Some(&rated));
        assert_eq!(element.user_rating, Some(1));
        assert_eq!(element.starred, None);

        let played = Annotation {
            play_count: 3,
            last_played_at: Some(86_399),
            ..Annotation::default()
        };
        let element = child(&song(), Some(&played));
        assert_eq!(element.play_count, 3);
        assert_eq!(element.played.as_deref(), Some("1970-01-01T23:59:59Z"));
        assert_eq!(element.starred, None);
    }

    #[test]
    fn a_song_names_the_track_artist_and_the_id_of_the_album_artist() {
        // A track of a compilation: the album artist is "Various Artists".
        let mut song = song();
        song.track.artist_name = "Tessa Lund".to_owned();
        song.track.artist_id = "ar-various".to_owned();
        let element = child(&song, None);
        assert_eq!(element.artist.as_deref(), Some("Tessa Lund"));
        assert_eq!(element.artist_id.as_deref(), Some("ar-various"));
    }

    /// [`song`] as the scanner leaves a track that it keeps without a file.
    fn song_without_a_file() -> Song {
        let mut song = song();
        song.track.rel_path = tracks::placeholder_path(TRACK);
        song
    }

    #[test]
    fn a_song_without_a_file_has_no_path() {
        let song = song_without_a_file();
        assert!(!song.track.has_file());
        let element = child(&song, Some(&annotation()));
        assert_eq!(element.path, None);

        // Every other field is the field of the song with its file.
        let with_file = child(&self::song(), Some(&annotation()));
        assert_eq!(
            element,
            Child {
                path: None,
                ..with_file
            }
        );
        // The placeholder path is in neither document.
        let (json, xml) = documents("song", &element);
        assert!(!json.contains("\"path\""), "{json}");
        assert!(!xml.contains(" path="), "{xml}");
        assert!(!json.contains("/tr-01"), "{json}");
        assert!(!xml.contains("/tr-01"), "{xml}");
    }

    #[test]
    fn the_cover_of_a_song_is_the_album_then_the_picture_of_its_file() {
        let cover = |album_has_cover, has_picture| {
            let mut song = song();
            song.album_has_cover = album_has_cover;
            song.track.has_picture = has_picture;
            song_cover_art(&song)
        };
        assert_eq!(cover(true, true).as_deref(), Some(ALBUM));
        assert_eq!(cover(true, false).as_deref(), Some(ALBUM));
        assert_eq!(cover(false, true).as_deref(), Some(TRACK));
        assert_eq!(cover(false, false), None);
        // `child` uses the same value.
        assert_eq!(child(&song(), None).cover_art.as_deref(), Some(ALBUM));
        assert_eq!(child(&bare_song(), None).cover_art, None);
    }

    #[test]
    fn the_cover_of_a_song_without_a_file_is_never_its_own_id() {
        // The row says that the file held a picture. The file is gone, so
        // `getCoverArt` has no picture for the ID of the track.
        let mut song = song_without_a_file();
        assert!(song.track.has_picture);
        song.album_has_cover = false;
        assert_eq!(song_cover_art(&song), None);
        assert_eq!(child(&song, None).cover_art, None);

        // The cover of its album does not depend on the file of the track.
        song.album_has_cover = true;
        assert_eq!(song_cover_art(&song).as_deref(), Some(ALBUM));
    }

    #[test]
    fn an_album_with_every_value_is_this_document() {
        assert_documents(
            "album",
            album(&album_row(), Some(&annotation())),
            concat!(
                r#""album":{"artist":"Aurora Field","artistId":"ar-01","coverArt":"al-01","#,
                r#""created":"2023-03-14T17:51:22Z","duration":645,"genre":"Electronic","#,
                r#""id":"al-01","name":"Glass Harbour","playCount":8,"songCount":3,"#,
                r#""starred":"2023-11-14T22:13:20Z","userRating":4,"year":2019}"#
            ),
            concat!(
                r#"<album artist="Aurora Field" artistId="ar-01" coverArt="al-01""#,
                r#" created="2023-03-14T17:51:22Z" duration="645" genre="Electronic""#,
                r#" id="al-01" name="Glass Harbour" playCount="8" songCount="3""#,
                r#" starred="2023-11-14T22:13:20Z" userRating="4" year="2019"/>"#
            ),
        );
    }

    #[test]
    fn an_album_with_few_values_has_the_required_fields() {
        assert_documents(
            "album",
            album(&bare_album_row(), None),
            concat!(
                r#""album":{"artist":"Aurora Field","artistId":"ar-01","#,
                r#""created":"2023-03-14T17:51:22Z","duration":645,"id":"al-01","#,
                r#""name":"Glass Harbour","playCount":0,"songCount":3}"#
            ),
            concat!(
                r#"<album artist="Aurora Field" artistId="ar-01""#,
                r#" created="2023-03-14T17:51:22Z" duration="645" id="al-01""#,
                r#" name="Glass Harbour" playCount="0" songCount="3"/>"#
            ),
        );
    }

    #[test]
    fn an_album_with_songs_holds_them_as_song_entries() {
        let songs = vec![child(&bare_song(), None), child(&bare_song(), None)];
        assert_documents(
            "album",
            album_with_songs(&bare_album_row(), None, songs),
            &format!(
                concat!(
                    r#""album":{{"artist":"Aurora Field","artistId":"ar-01","#,
                    r#""created":"2023-03-14T17:51:22Z","duration":645,"id":"al-01","#,
                    r#""name":"Glass Harbour","playCount":0,"song":[{song},{song}],"#,
                    r#""songCount":3}}"#
                ),
                song = BARE_SONG_JSON
            ),
            &format!(
                concat!(
                    r#"<album artist="Aurora Field" artistId="ar-01""#,
                    r#" created="2023-03-14T17:51:22Z" duration="645" id="al-01""#,
                    r#" name="Glass Harbour" playCount="0" songCount="3">"#,
                    "<song {song}/><song {song}/></album>"
                ),
                song = BARE_SONG_XML
            ),
        );
    }

    #[test]
    fn an_album_with_an_empty_song_list_keeps_the_list_in_json() {
        let element = album_with_songs(&bare_album_row(), None, Vec::new());
        let (json, xml) = documents("album", element);
        assert!(json.contains(r#""song":[],"songCount":3"#), "{json}");
        assert!(
            xml.ends_with(r#"songCount="3"/></subsonic-response>"#),
            "{xml}"
        );
        // An entry of a list has no `song` field at all.
        let (json, _) = documents("album", album(&bare_album_row(), None));
        assert!(!json.contains(r#""song""#), "{json}");
    }

    #[test]
    fn the_cover_of_an_album_is_its_own_id_when_it_has_a_cover() {
        assert_eq!(album_cover_art(&album_row()).as_deref(), Some(ALBUM));
        assert_eq!(album_cover_art(&bare_album_row()), None);
        let embedded = browse::Album {
            cover_source: Some("embedded:tr-01".to_owned()),
            ..album_row()
        };
        assert_eq!(album_cover_art(&embedded).as_deref(), Some(ALBUM));
    }

    #[test]
    fn an_album_as_a_directory_is_this_document() {
        assert_documents(
            "album",
            child_dir_for_album(&album_row(), Some(&annotation())),
            concat!(
                r#""album":{"album":"Glass Harbour","artist":"Aurora Field","#,
                r#""artistId":"ar-01","coverArt":"al-01","created":"2023-03-14T17:51:22Z","#,
                r#""duration":645,"genre":"Electronic","id":"al-01","isDir":true,"#,
                r#""mediaType":"album","parent":"ar-01","playCount":8,"#,
                r#""played":"2023-11-14T22:21:40Z","starred":"2023-11-14T22:13:20Z","#,
                r#""title":"Glass Harbour","userRating":4,"year":2019}"#
            ),
            concat!(
                r#"<album album="Glass Harbour" artist="Aurora Field" artistId="ar-01""#,
                r#" coverArt="al-01" created="2023-03-14T17:51:22Z" duration="645""#,
                r#" genre="Electronic" id="al-01" isDir="true" mediaType="album""#,
                r#" parent="ar-01" playCount="8" played="2023-11-14T22:21:40Z""#,
                r#" starred="2023-11-14T22:13:20Z" title="Glass Harbour" userRating="4""#,
                r#" year="2019"/>"#
            ),
        );
    }

    /// The entry of [`bare_album_row`] as a directory in JSON, without its
    /// key.
    const BARE_ALBUM_DIR_JSON: &str = concat!(
        r#"{"album":"Glass Harbour","artist":"Aurora Field","artistId":"ar-01","#,
        r#""created":"2023-03-14T17:51:22Z","duration":645,"id":"al-01","isDir":true,"#,
        r#""mediaType":"album","parent":"ar-01","playCount":0,"title":"Glass Harbour"}"#
    );

    /// The attributes of the element of [`bare_album_row`] as a directory in
    /// XML.
    const BARE_ALBUM_DIR_XML: &str = concat!(
        r#"album="Glass Harbour" artist="Aurora Field" artistId="ar-01""#,
        r#" created="2023-03-14T17:51:22Z" duration="645" id="al-01" isDir="true""#,
        r#" mediaType="album" parent="ar-01" playCount="0" title="Glass Harbour""#
    );

    #[test]
    fn an_album_as_a_directory_with_few_values_has_no_field_of_a_file() {
        assert_documents(
            "album",
            child_dir_for_album(&bare_album_row(), None),
            &format!(r#""album":{BARE_ALBUM_DIR_JSON}"#),
            &format!("<album {BARE_ALBUM_DIR_XML}/>"),
        );
    }

    #[test]
    fn an_artist_is_this_document() {
        // The rating and the plays of the annotation are not part of the
        // element, and the element has no `coverArt`.
        assert_documents(
            "artist",
            artist(&artist_row(), Some(&annotation())),
            concat!(
                r#""artist":{"albumCount":2,"id":"ar-01","name":"Aurora Field","#,
                r#""starred":"2023-11-14T22:13:20Z"}"#
            ),
            concat!(
                r#"<artist albumCount="2" id="ar-01" name="Aurora Field""#,
                r#" starred="2023-11-14T22:13:20Z"/>"#
            ),
        );
        assert_documents(
            "artist",
            artist(&artist_row(), None),
            r#""artist":{"albumCount":2,"id":"ar-01","name":"Aurora Field"}"#,
            r#"<artist albumCount="2" id="ar-01" name="Aurora Field"/>"#,
        );
        assert_eq!(artist(&artist_row(), Some(&annotation())).cover_art, None);
    }

    #[test]
    fn an_artist_with_albums_holds_them_as_album_entries() {
        let albums = vec![album(&bare_album_row(), None)];
        assert_documents(
            "artist",
            artist_with_albums(&artist_row(), None, albums),
            concat!(
                r#""artist":{"album":[{"artist":"Aurora Field","artistId":"ar-01","#,
                r#""created":"2023-03-14T17:51:22Z","duration":645,"id":"al-01","#,
                r#""name":"Glass Harbour","playCount":0,"songCount":3}],"#,
                r#""albumCount":2,"id":"ar-01","name":"Aurora Field"}"#
            ),
            concat!(
                r#"<artist albumCount="2" id="ar-01" name="Aurora Field">"#,
                r#"<album artist="Aurora Field" artistId="ar-01""#,
                r#" created="2023-03-14T17:51:22Z" duration="645" id="al-01""#,
                r#" name="Glass Harbour" playCount="0" songCount="3"/></artist>"#
            ),
        );
    }

    #[test]
    fn an_artist_of_the_folder_methods_is_this_document() {
        assert_documents(
            "artist",
            artist_dir(&artist_row(), Some(&annotation())),
            concat!(
                r#""artist":{"id":"ar-01","name":"Aurora Field","#,
                r#""starred":"2023-11-14T22:13:20Z","userRating":4}"#
            ),
            concat!(
                r#"<artist id="ar-01" name="Aurora Field""#,
                r#" starred="2023-11-14T22:13:20Z" userRating="4"/>"#
            ),
        );
        assert_documents(
            "artist",
            artist_dir(&artist_row(), None),
            r#""artist":{"id":"ar-01","name":"Aurora Field"}"#,
            r#"<artist id="ar-01" name="Aurora Field"/>"#,
        );
    }

    #[test]
    fn the_directory_of_an_artist_holds_its_albums_as_children() {
        let children = vec![child_dir_for_album(&bare_album_row(), None)];
        assert_documents(
            "directory",
            directory_for_artist(&artist_row(), Some(&annotation()), children),
            &format!(
                concat!(
                    r#""directory":{{"child":[{child}],"id":"ar-01","name":"Aurora Field","#,
                    r#""starred":"2023-11-14T22:13:20Z","userRating":4}}"#
                ),
                child = BARE_ALBUM_DIR_JSON
            ),
            &format!(
                concat!(
                    r#"<directory id="ar-01" name="Aurora Field""#,
                    r#" starred="2023-11-14T22:13:20Z" userRating="4">"#,
                    "<child {child}/></directory>"
                ),
                child = BARE_ALBUM_DIR_XML
            ),
        );
    }

    #[test]
    fn the_directory_of_an_album_holds_its_songs_as_children() {
        let children = vec![child(&bare_song(), None)];
        assert_documents(
            "directory",
            directory_for_album(&album_row(), Some(&annotation()), children),
            &format!(
                concat!(
                    r#""directory":{{"child":[{child}],"id":"al-01","name":"Glass Harbour","#,
                    r#""parent":"ar-01","playCount":8,"starred":"2023-11-14T22:13:20Z","#,
                    r#""userRating":4}}"#
                ),
                child = BARE_SONG_JSON
            ),
            &format!(
                concat!(
                    r#"<directory id="al-01" name="Glass Harbour" parent="ar-01""#,
                    r#" playCount="8" starred="2023-11-14T22:13:20Z" userRating="4">"#,
                    "<child {child}/></directory>"
                ),
                child = BARE_SONG_XML
            ),
        );
    }

    #[test]
    fn a_directory_without_an_annotation_and_without_children_is_this_document() {
        assert_documents(
            "directory",
            directory_for_artist(&artist_row(), None, Vec::new()),
            r#""directory":{"child":[],"id":"ar-01","name":"Aurora Field"}"#,
            r#"<directory id="ar-01" name="Aurora Field"/>"#,
        );
        assert_documents(
            "directory",
            directory_for_album(&album_row(), None, Vec::new()),
            concat!(
                r#""directory":{"child":[],"id":"al-01","name":"Glass Harbour","#,
                r#""parent":"ar-01","playCount":0}"#
            ),
            r#"<directory id="al-01" name="Glass Harbour" parent="ar-01" playCount="0"/>"#,
        );
    }

    #[test]
    fn index_letter_is_the_first_letter_without_a_leading_article() {
        assert_eq!(index_letter("The Beatles"), "B");
        assert_eq!(index_letter("2Pac"), "#");

        let cases = [
            ("Aurora Field", "A"),
            ("zz top", "Z"),
            ("  Marlow Quartet", "M"),
            // Every article of the list, in any letter case.
            ("the beatles", "B"),
            ("THE WHO", "W"),
            ("El Guincho", "G"),
            ("La Roux", "R"),
            ("Los Lobos", "L"),
            ("Las Ketchup", "K"),
            ("Le Tigre", "T"),
            ("Les Rita Mitsouko", "R"),
            ("The  Spaced", "S"),
            ("The\tTabs", "T"),
            // One article is removed, not two.
            ("The The", "T"),
            ("The Los Lobos", "L"),
            // An article is a whole word that another word follows.
            ("Them", "T"),
            ("The", "T"),
            ("The ", "T"),
            ("Lester Young", "L"),
            ("Las", "L"),
            ("The-Dream", "T"),
            // An article of another language is part of the name.
            ("Die Antwoord", "D"),
            ("A Tribe Called Quest", "A"),
            // A letter with a diacritic is its base letter.
            ("Édith Piaf", "E"),
            ("ólafur arnalds", "O"),
            ("The Århus Band", "A"),
            // Everything else.
            ("", "#"),
            ("   ", "#"),
            ("!!!", "#"),
            ("The 1975", "#"),
            ("[Unknown Artist]", "#"),
            ("Øystein", "#"),
            ("Ænima", "#"),
            ("ßeta", "#"),
            ("Игорь", "#"),
            ("坂本龍一", "#"),
        ];
        for (name, letter) in cases {
            assert_eq!(index_letter(name), letter, "{name:?}");
        }
    }

    #[test]
    fn ignored_articles_is_the_list_separated_by_spaces() {
        assert_eq!(ignored_articles(), "The El La Los Las Le Les");
    }

    /// The index names and the artist names of `indexes`.
    fn layout(indexes: &[Index<ArtistID3>]) -> Vec<(&str, Vec<&str>)> {
        indexes
            .iter()
            .map(|index| {
                let names = index.artist.iter().map(|artist| artist.name.as_str());
                (index.name.as_str(), names.collect())
            })
            .collect()
    }

    #[test]
    fn index_groups_the_artists_by_letter_and_orders_them_without_articles() {
        let artists = [
            named("ar-01", "The Beatles"),
            named("ar-02", "2Pac"),
            named("ar-03", "Aurora Field"),
            named("ar-04", "Beach House"),
            named("ar-05", "Los Lobos"),
            named("ar-06", "bob marley"),
            named("ar-07", "Édith Piaf"),
            named("ar-08", "Zebra"),
            named("ar-09", "!!!"),
            named("ar-10", "The Band"),
            named("ar-11", "Lester Young"),
            named("ar-12", "Enya"),
            named("ar-13", "Eagles"),
        ];
        let indexes = index(&artists, |row| artist(row, None));
        assert_eq!(
            layout(&indexes),
            [
                ("#", vec!["!!!", "2Pac"]),
                ("A", vec!["Aurora Field"]),
                (
                    "B",
                    vec!["The Band", "Beach House", "The Beatles", "bob marley"]
                ),
                // A letter with a diacritic is ordered as its base letter.
                ("E", vec!["Eagles", "Édith Piaf", "Enya"]),
                ("L", vec!["Lester Young", "Los Lobos"]),
                ("Z", vec!["Zebra"]),
            ]
        );
    }

    #[test]
    fn index_does_not_depend_on_the_order_of_the_rows() {
        let mut artists = vec![
            named("ar-03", "Beatles"),
            named("ar-02", "The Beatles"),
            named("ar-01", "the beatles"),
            named("ar-05", "Aurora Field"),
            named("ar-04", "Aurora Field"),
        ];
        let expected = [
            ("A", vec!["ar-04", "ar-05"]),
            // The three names are equal without the article and in lower
            // case. The names decide, in byte order, and then the IDs.
            ("B", vec!["ar-03", "ar-02", "ar-01"]),
        ];
        for _ in 0..artists.len() {
            artists.rotate_left(1);
            let indexes = index(&artists, |row| artist(row, None));
            let ids: Vec<(&str, Vec<&str>)> = indexes
                .iter()
                .map(|index| {
                    let ids = index.artist.iter().map(|artist| artist.id.as_str());
                    (index.name.as_str(), ids.collect())
                })
                .collect();
            assert_eq!(ids, expected);
        }
    }

    #[test]
    fn index_of_no_artist_is_empty() {
        assert_eq!(index(&[], |row| artist(row, None)), []);
    }

    #[test]
    fn index_passes_each_artist_to_the_model_once() {
        let artists = [named("ar-01", "The Beatles"), named("ar-02", "2Pac")];
        let annotations = std::collections::HashMap::from([("ar-02".to_owned(), annotation())]);
        let indexes = index(&artists, |row| artist_dir(row, annotations.get(&row.id)));
        assert_eq!(
            indexes,
            [
                Index {
                    name: "#".to_owned(),
                    artist: vec![artist_dir(&artists[1], Some(&annotation()))],
                },
                Index {
                    name: "B".to_owned(),
                    artist: vec![artist_dir(&artists[0], None)],
                },
            ]
        );
    }

    #[test]
    fn the_indexes_of_get_artists_are_this_document() {
        let artists = [named("ar-01", "The Beatles"), named("ar-02", "2Pac")];
        let value = json!({
            "ignoredArticles": ignored_articles(),
            "index": index(&artists, |row| artist(row, None)),
        });
        assert_documents(
            "artists",
            value,
            concat!(
                r#""artists":{"ignoredArticles":"The El La Los Las Le Les","index":["#,
                r##"{"artist":[{"albumCount":2,"id":"ar-02","name":"2Pac"}],"name":"#"},"##,
                r#"{"artist":[{"albumCount":2,"id":"ar-01","name":"The Beatles"}],"name":"B"}"#,
                "]}"
            ),
            concat!(
                r#"<artists ignoredArticles="The El La Los Las Le Les">"#,
                r##"<index name="#"><artist albumCount="2" id="ar-02" name="2Pac"/></index>"##,
                r#"<index name="B"><artist albumCount="2" id="ar-01" name="The Beatles"/>"#,
                "</index></artists>"
            ),
        );
    }

    #[test]
    fn the_indexes_of_get_indexes_are_this_document() {
        let artists = [named("ar-01", "The Beatles")];
        let value = json!({
            "ignoredArticles": ignored_articles(),
            "lastModified": 1_678_816_282_000_i64,
            "index": index(&artists, |row| artist_dir(row, None)),
        });
        assert_documents(
            "indexes",
            value,
            concat!(
                r#""indexes":{"ignoredArticles":"The El La Los Las Le Les","index":["#,
                r#"{"artist":[{"id":"ar-01","name":"The Beatles"}],"name":"B"}],"#,
                r#""lastModified":1678816282000}"#
            ),
            concat!(
                r#"<indexes ignoredArticles="The El La Los Las Le Les""#,
                r#" lastModified="1678816282000">"#,
                r#"<index name="B"><artist id="ar-01" name="The Beatles"/></index></indexes>"#
            ),
        );
    }

    #[test]
    fn a_genre_is_this_document_and_its_name_is_the_text_in_xml() {
        let row = browse::Genre {
            name: "Drum & Bass".to_owned(),
            song_count: 6,
            album_count: 2,
        };
        assert_documents(
            "genres",
            json!({ "genre": [genre(&row)] }),
            r#""genres":{"genre":[{"albumCount":2,"songCount":6,"value":"Drum & Bass"}]}"#,
            r#"<genres><genre albumCount="2" songCount="6">Drum &amp; Bass</genre></genres>"#,
        );
    }

    #[test]
    fn a_music_folder_is_this_document_without_its_path() {
        let folder = Folder {
            id: 7,
            path: std::path::PathBuf::from("/srv/media/Music"),
            name: "Music".to_owned(),
        };
        assert_documents(
            "musicFolders",
            json!({ "musicFolder": [music_folder(&folder)] }),
            r#""musicFolders":{"musicFolder":[{"id":7,"name":"Music"}]}"#,
            r#"<musicFolders><musicFolder id="7" name="Music"/></musicFolders>"#,
        );
    }

    #[test]
    fn text_is_escaped_in_both_documents() {
        let mut song = bare_song();
        song.track.title = "\"polar\" <expedition> & more".to_owned();
        let (json, xml) = documents("song", child(&song, None));
        assert!(
            json.contains(r#""title":"\"polar\" <expedition> & more""#),
            "{json}"
        );
        assert!(
            xml.contains(r#"title="&quot;polar&quot; &lt;expedition&gt; &amp; more""#),
            "{xml}"
        );
    }

    #[test]
    fn seconds_rounds_to_the_nearest_second() {
        let cases = [
            (0, 0),
            (1, 0),
            (499, 0),
            (500, 1),
            (999, 1),
            (1000, 1),
            (1499, 1),
            (1500, 2),
            (215_600, 216),
            (u64::MAX, u64::MAX / 1000 + 1),
        ];
        for (milliseconds, expected) in cases {
            assert_eq!(seconds(milliseconds), expected, "{milliseconds}");
        }
    }

    #[test]
    fn iso8601_formats_unix_seconds_in_utc() {
        let cases = [
            (0, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (-1, "1969-12-31T23:59:59Z"),
            (-86_400, "1969-12-31T00:00:00Z"),
            (946_684_799, "1999-12-31T23:59:59Z"),
            // 2000 is a leap year, and 2100 is not.
            (951_782_400, "2000-02-29T00:00:00Z"),
            (978_307_200, "2001-01-01T00:00:00Z"),
            (1_582_934_400, "2020-02-29T00:00:00Z"),
            (1_614_556_800, "2021-03-01T00:00:00Z"),
            (1_678_816_282, "2023-03-14T17:51:22Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
            (253_402_300_799, "9999-12-31T23:59:59Z"),
        ];
        for (unix_seconds, text) in cases {
            assert_eq!(iso8601(unix_seconds), text, "{unix_seconds}");
        }
    }
}
