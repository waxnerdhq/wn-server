//! The user methods: `getUser` and `getUsers` (`docs/design.md` section 4).
//!
//! The server has one account, the owner, so both methods return the user of
//! the request.

use axum::extract::State;
use serde::Serialize;

use super::envelope::ApiError;
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::Db;
use crate::db::queries::folders;

/// The message of the error that `getUser` returns for the name of another
/// user.
const OTHER_USER: &str = "username must be the name of the user of the request";

/// The `user` element. It has no `email`, because an account has none.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct User {
    username: String,
    scrobbling_enabled: bool,
    admin_role: bool,
    settings_role: bool,
    download_role: bool,
    upload_role: bool,
    playlist_role: bool,
    cover_art_role: bool,
    comment_role: bool,
    podcast_role: bool,
    stream_role: bool,
    jukebox_role: bool,
    share_role: bool,
    video_conversion_role: bool,
    /// The IDs of the music folders that the user can read.
    folder: Vec<i64>,
}

impl User {
    /// The element of the owner account `username`, which reads the music
    /// folders `folder`.
    ///
    /// The owner administers the server, streams, keeps playlists and has its
    /// plays counted. Every other role is `false`: the server has no such
    /// feature, or the feature is not offered through the API.
    fn owner(username: String, folder: Vec<i64>) -> Self {
        Self {
            username,
            scrobbling_enabled: true,
            admin_role: true,
            settings_role: false,
            // `download` is disabled (`REQUIREMENTS.md`), so no user has the
            // role. The `download` method returns error 50.
            download_role: false,
            upload_role: false,
            playlist_role: true,
            cover_art_role: false,
            comment_role: false,
            podcast_role: false,
            stream_role: true,
            jukebox_role: false,
            share_role: false,
            video_conversion_role: false,
            folder,
        }
    }
}

/// The `users` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Users {
    user: Vec<User>,
}

/// `getUser`: the user of the request.
///
/// `username` is required; the error has code 10. It must be the name of the
/// user of the request, compared without regard to ASCII case as the lookup
/// of an account is. Another name is error 50, whether an account has that
/// name or not. With `auth.disabled_unsafe` every request acts as the owner,
/// so every name returns the owner.
pub async fn get_user(
    State(state): State<AppState>,
    Authed(caller): Authed,
    params: Params,
) -> ApiResult {
    let username = params.req_str("username")?;
    let own = username.eq_ignore_ascii_case(&caller.name);
    if !own && !state.config.auth.disabled_unsafe {
        return Err(ApiError::Forbidden(OTHER_USER.to_owned()));
    }
    let user = owner_element(&state.db, caller.name).await?;
    Ok(Payload::new("user", user))
}

/// `getUsers`: the list of the accounts, which holds the user of the request
/// and no other.
pub async fn get_users(State(state): State<AppState>, Authed(caller): Authed) -> ApiResult {
    let user = owner_element(&state.db, caller.name).await?;
    Ok(Payload::new("users", Users { user: vec![user] }))
}

/// The `user` element of the owner account `username`, with the IDs of the
/// rows of the `folders` table in ID order. A database that cannot be read
/// is error 0.
async fn owner_element(db: &Db, username: String) -> Result<User, ApiError> {
    let folders = db.read(folders::list).await.map_err(|error| {
        tracing::error!(%error, "cannot read the music folders");
        ApiError::Other("the server cannot read its music folders".to_owned())
    })?;
    let ids = folders.into_iter().map(|folder| folder.id).collect();
    Ok(User::owner(username, ids))
}
