//! The methods that answer without the library: the disabled `download`, the
//! methods of deferred features that return an empty result, and the methods
//! that return an error (`docs/design.md` section 4).
//!
//! No handler of this module takes an argument. A handler therefore reads no
//! parameter, no state and no file, and its answer is the same for every
//! request. The router authenticates the request before it calls the handler
//! (`docs/design.md` section 3.7).

use serde_json::json;

use super::envelope::ApiError;
use super::{ApiResult, Payload};

/// The message of the error that `download` returns.
const DOWNLOAD_DISABLED: &str = "download is disabled on this server";

/// The message of the error that the user management methods return.
const USER_MANAGEMENT_UNAVAILABLE: &str = "user management is not available on this server";

/// A success that carries an object without fields under `key`.
fn empty_result(key: &str) -> ApiResult {
    Ok(Payload::new(key, json!({})))
}

/// A success that carries an object under `key` whose one field is the empty
/// list `list`. JSON holds the list as `[]`, and XML has no element for it.
fn empty_list(key: &str, list: &str) -> ApiResult {
    Ok(Payload::new(key, json!({ list: [] })))
}

/// `download`: error 50. The server does not serve downloads
/// (`REQUIREMENTS.md`), so the handler returns the error for every request
/// and returns no byte of a file. `getUser` reports the same decision as
/// `downloadRole: false`.
pub async fn download() -> ApiResult {
    Err(ApiError::Forbidden(DOWNLOAD_DISABLED.to_owned()))
}

/// `getArtistInfo`: an empty `artistInfo`. The server has no artist
/// biographies, images or similar artists.
pub async fn get_artist_info() -> ApiResult {
    empty_result("artistInfo")
}

/// `getArtistInfo2`: an empty `artistInfo2`.
pub async fn get_artist_info2() -> ApiResult {
    empty_result("artistInfo2")
}

/// `getAlbumInfo`: an empty `albumInfo`. The server has no album notes or
/// images from an external source.
pub async fn get_album_info() -> ApiResult {
    empty_result("albumInfo")
}

/// `getAlbumInfo2`: an empty `albumInfo`. The OpenSubsonic API names the
/// element of this method `albumInfo` as well.
pub async fn get_album_info2() -> ApiResult {
    empty_result("albumInfo")
}

/// `getSimilarSongs`: an empty `similarSongs`. The server has no similar
/// songs.
pub async fn get_similar_songs() -> ApiResult {
    empty_result("similarSongs")
}

/// `getSimilarSongs2`: an empty `similarSongs2`.
pub async fn get_similar_songs2() -> ApiResult {
    empty_result("similarSongs2")
}

/// `getTopSongs`: an empty `topSongs`. The server has no top songs.
pub async fn get_top_songs() -> ApiResult {
    empty_result("topSongs")
}

/// `getNowPlaying`: a `nowPlaying` with no `entry`. The server does not
/// report what is playing. The OpenSubsonic API marks `entry` as required,
/// so the element holds the empty list.
pub async fn get_now_playing() -> ApiResult {
    empty_list("nowPlaying", "entry")
}

/// `getShares`: a `shares` with no `share`. The server has no shares.
pub async fn get_shares() -> ApiResult {
    empty_list("shares", "share")
}

/// `getPodcasts`: a `podcasts` with no `channel`. The server has no podcasts.
pub async fn get_podcasts() -> ApiResult {
    empty_list("podcasts", "channel")
}

/// `getNewestPodcasts`: a `newestPodcasts` with no `episode`.
pub async fn get_newest_podcasts() -> ApiResult {
    empty_list("newestPodcasts", "episode")
}

/// `getInternetRadioStations`: an `internetRadioStations` with no
/// `internetRadioStation`. The server has no radio stations.
pub async fn get_internet_radio_stations() -> ApiResult {
    empty_list("internetRadioStations", "internetRadioStation")
}

/// `getBookmarks`: a `bookmarks` with no `bookmark`. The server has no
/// bookmarks.
pub async fn get_bookmarks() -> ApiResult {
    empty_list("bookmarks", "bookmark")
}

/// `getAvatar`: error 70. No user has an avatar.
pub async fn get_avatar() -> ApiResult {
    Err(ApiError::NotFound)
}

/// `createUser`, `updateUser`, `deleteUser` and `changePassword`: error 50.
/// The server has one account, the owner, and the `wn-server user` commands
/// manage it.
pub async fn user_management() -> ApiResult {
    Err(ApiError::Forbidden(USER_MANAGEMENT_UNAVAILABLE.to_owned()))
}
