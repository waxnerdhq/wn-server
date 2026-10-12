//! The annotation methods: `star`, `unstar`, `setRating`, `getStarred` and
//! `getStarred2` (`docs/design.md` section 4).
//!
//! A star and a rating belong to the user of the request. The three methods
//! that write check every ID of the request and write in one transaction
//! (`annotations::apply`), so a request changes every item it names or none.
//!
//! An ID is the ID of a track, an album or an artist, and its prefix names
//! the kind (`docs/design.md` section 3.3). The folder methods list albums
//! and artists as directories under the same IDs, so the star of an album is
//! one star, whether a client sets it through `id` or through `albumId`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use serde::Serialize;

use super::envelope::ApiError;
use super::model::{self, AlbumID3, Artist, ArtistID3, Child};
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::Db;
use crate::db::queries::annotations::{self, Annotation, Change, Item, ItemKind, Rating};
use crate::db::queries::browse;

/// The message of the error that `setRating` returns for a rating outside 0
/// to 5.
const RATING_RANGE: &str = "parameter rating must be an integer from 0 to 5";

/// The `starred2` element of `getStarred2`: the starred items in the forms
/// of the ID3 methods.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Starred2 {
    artist: Vec<ArtistID3>,
    album: Vec<AlbumID3>,
    song: Vec<Child>,
}

/// The `starred` element of `getStarred`: the starred items in the forms of
/// the folder methods. An album is a directory.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Starred {
    artist: Vec<Artist>,
    album: Vec<Child>,
    song: Vec<Child>,
}

/// `star`: stars the items of the request for the user.
///
/// - `id` takes the ID of a track, an album or an artist. `albumId` takes
///   the ID of an album and `artistId` the ID of an artist. Each parameter
///   may occur several times, and none is required: a request without an ID
///   stars nothing and succeeds.
/// - A value that is not the ID of an existing item of the kind its
///   parameter takes is error 70, and the request then stars nothing.
/// - An item that has a star takes the time of this request as the time of
///   its star.
pub async fn star(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let items = named_items(&params)?;
    write(&state.db, user.id, items, |at| Change::Star { at }).await
}

/// `unstar`: removes the star of the items of the request for the user.
///
/// The parameters and the errors are those of [`star`]. An item that exists
/// and has no star is no error. The rating and the play count of an item
/// stay.
pub async fn unstar(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let items = named_items(&params)?;
    write(&state.db, user.id, items, |_| Change::Unstar).await
}

/// `setRating`: sets the rating of one item for the user.
///
/// - `id` is required and takes the ID of a track, an album or an artist.
///   A value that is not the ID of an existing item is error 70.
/// - `rating` is required: 1 to 5 sets the rating, and 0 removes it. An
///   absent or empty value is error 10. A value that is no integer, and an
///   integer outside 0 to 5, is error 0, and nothing is written.
/// - The star and the play count of the item stay.
pub async fn set_rating(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let id = params.req_str("id")?;
    let rating = params
        .opt_i64("rating")?
        .ok_or_else(|| ApiError::MissingParam("rating".to_owned()))?;
    let rating = Rating::new(rating).ok_or_else(|| ApiError::Other(RATING_RANGE.to_owned()))?;
    let item = Item::parse(id).ok_or(ApiError::NotFound)?;
    write(&state.db, user.id, vec![item], move |_| {
        Change::Rate(rating)
    })
    .await
}

/// `getStarred2`: the artists, the albums and the songs that the user
/// starred, as `starred2` with `ArtistID3`, `AlbumID3` and `Child` elements.
///
/// Each list has the item that was starred last as its first.
/// `musicFolderId` limits the lists to the items of one music folder
/// (`annotations::starred`).
pub async fn get_starred2(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let rows = starred_rows(&state.db, user.id, &params).await?;
    let of = |id: &str| rows.annotations.get(id);
    let starred = Starred2 {
        artist: rows
            .artists
            .iter()
            .map(|artist| model::artist(artist, of(&artist.id)))
            .collect(),
        album: rows
            .albums
            .iter()
            .map(|album| model::album(album, of(&album.id)))
            .collect(),
        song: rows.song_elements(),
    };
    Ok(Payload::new("starred2", starred))
}

/// `getStarred`: the artists, the albums and the songs that the user
/// starred, as `starred` in the forms of the folder methods: `Artist`
/// elements, albums as directories, and `Child` elements.
///
/// The items, their order and `musicFolderId` are those of
/// [`get_starred2`].
pub async fn get_starred(
    State(state): State<AppState>,
    Authed(user): Authed,
    params: Params,
) -> ApiResult {
    let rows = starred_rows(&state.db, user.id, &params).await?;
    let of = |id: &str| rows.annotations.get(id);
    let starred = Starred {
        artist: rows
            .artists
            .iter()
            .map(|artist| model::artist_dir(artist, of(&artist.id)))
            .collect(),
        album: rows
            .albums
            .iter()
            .map(|album| model::child_dir_for_album(album, of(&album.id)))
            .collect(),
        song: rows.song_elements(),
    };
    Ok(Payload::new("starred", starred))
}

/// The items that a `star` or an `unstar` request names: the values of
/// `id`, then of `albumId`, then of `artistId`.
///
/// A value of `id` is the ID of a track, an album or an artist. A value of
/// `albumId` is the ID of an album, and a value of `artistId` the ID of an
/// artist. Any other value, an empty one included, names no item and is
/// error 70.
fn named_items(params: &Params) -> Result<Vec<Item>, ApiError> {
    let parameters = [
        ("id", None),
        ("albumId", Some(ItemKind::Album)),
        ("artistId", Some(ItemKind::Artist)),
    ];
    let mut items = Vec::new();
    for (name, kind) in parameters {
        for id in params.all(name) {
            let item = Item::parse(id)
                .filter(|item| kind.is_none_or(|kind| item.kind() == kind))
                .ok_or(ApiError::NotFound)?;
            items.push(item);
        }
    }
    Ok(items)
}

/// Applies the change that `change` returns to `items` for the user
/// `user_id`, and answers with the empty response. An item without a row is
/// error 70, and nothing is written then.
///
/// `change` gets the time of the write in Unix seconds. The time is read
/// once the write connection is held. While the clock runs forward, the
/// request that writes later therefore does not store the earlier time.
async fn write(
    db: &Db,
    user_id: i64,
    items: Vec<Item>,
    change: impl FnOnce(i64) -> Change + Send + 'static,
) -> ApiResult {
    let written = db
        .write(move |conn| annotations::apply(conn, user_id, &items, change(unix_now())))
        .await
        .map_err(|error| {
            tracing::error!(%error, "cannot write the annotations");
            ApiError::Other("the server cannot store the change".to_owned())
        })?;
    if written {
        Ok(Payload::empty())
    } else {
        Err(ApiError::NotFound)
    }
}

/// The time now in whole Unix seconds.
fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        // The clock is set to a time before 1970.
        Err(error) => i64::try_from(error.duration().as_secs()).map_or(i64::MIN, |before| -before),
    }
}

/// The rows of the starred items of one user, with the annotations of the
/// user for them.
struct StarredRows {
    artists: Vec<browse::Artist>,
    albums: Vec<browse::Album>,
    songs: Vec<browse::Song>,
    annotations: HashMap<String, Annotation>,
}

impl StarredRows {
    /// The `Child` element of each starred song.
    fn song_elements(&self) -> Vec<Child> {
        self.songs
            .iter()
            .map(|song| model::child(song, self.annotations.get(&song.track.id)))
            .collect()
    }
}

/// Reads the starred items of the user `user_id`, each list in the order of
/// `annotations::starred`. `musicFolderId` is optional; a value that is no
/// integer is error 0.
///
/// The statements run in one read transaction, so the lists and the
/// annotations are of one state of the database.
async fn starred_rows(db: &Db, user_id: i64, params: &Params) -> Result<StarredRows, ApiError> {
    let folder_id = params.opt_i64("musicFolderId")?;
    db.read(move |conn| {
        let snapshot = conn.unchecked_transaction()?;
        let artist_ids = annotations::starred(&snapshot, user_id, ItemKind::Artist, folder_id)?;
        let album_ids = annotations::starred(&snapshot, user_id, ItemKind::Album, folder_id)?;
        let track_ids = annotations::starred(&snapshot, user_id, ItemKind::Track, folder_id)?;
        let artists = browse::artists_by_ids(&snapshot, &artist_ids)?;
        let albums = browse::albums_by_ids(&snapshot, &album_ids)?;
        let songs = browse::tracks_by_ids(&snapshot, &track_ids)?;
        let ids = [artist_ids, album_ids, track_ids].concat();
        let annotations = annotations::get_many(&snapshot, user_id, &ids)?;
        Ok(StarredRows {
            artists,
            albums,
            songs,
            annotations,
        })
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "cannot read the starred items");
        ApiError::Other("the server cannot read the starred items".to_owned())
    })
}
