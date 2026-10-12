//! The lyrics methods: `getLyrics` and `getLyricsBySongId`
//! (`docs/design.md` section 4).
//!
//! The server has no lyrics in version 1. The scanner reads no lyrics tag
//! and no lyrics file, and the database stores none. Each method therefore
//! answers every song as a server answers a song without lyrics: with a
//! success that holds no lyrics. A client that asks for the lyrics of a song
//! shows none and reports no error.
//!
//! No handler of this module opens a file. `getLyrics` reads no parameter
//! and no state. `getLyricsBySongId` reads `id` and looks the track up in
//! the database.
//!
//! The server does not advertise the `songLyrics` extension, which
//! `getLyricsBySongId` belongs to. The method answers the clients that call
//! it without the extension.

use axum::extract::State;
use serde::Serialize;

use super::envelope::ApiError;
use super::params::Params;
use super::{ApiResult, AppState, Payload};
use crate::auth::extract::Authed;
use crate::db::queries::browse;
use crate::ids::{self, Kind};

/// The `lyrics` element of a song without lyrics.
///
/// The Subsonic API returns an empty `lyrics` element when it finds no
/// lyrics. The OpenSubsonic API requires `value`, the text of the lyrics, so
/// the element holds the empty text. The optional `artist` and `title` are
/// absent. In XML, `value` is the text content of the element.
#[derive(Serialize)]
struct Lyrics {
    value: &'static str,
}

/// The `lyricsList` element of a song without lyrics. `structuredLyrics`,
/// its one field in the OpenSubsonic API, is optional, and the element has
/// no field.
#[derive(Serialize)]
struct LyricsList {}

/// `getLyrics`: a `lyrics` element without lyrics. The handler reads neither
/// `artist` nor `title`, so the answer is the same for every request that
/// has a user.
pub async fn get_lyrics(_user: Authed) -> ApiResult {
    Ok(Payload::new("lyrics", Lyrics { value: "" }))
}

/// `getLyricsBySongId`: a `lyricsList` without lyrics for the track `id`.
/// A track without a file has a row and gets the same answer.
///
/// # Errors
///
/// An absent `id` is error 10. Error 70 answers a value that is the ID of no
/// track: an empty value, a value that `ids::parse` rejects, the ID of an
/// item of another kind, and a track ID that no row has. A value of another
/// shape does not reach SQL. A database that cannot be read is error 0.
pub async fn get_lyrics_by_song_id(
    State(state): State<AppState>,
    _user: Authed,
    params: Params,
) -> ApiResult {
    let id = params.req_str("id")?;
    if !matches!(ids::parse(id), Some((Kind::Track, _))) {
        return Err(ApiError::NotFound);
    }
    let id = id.to_owned();
    let known = state
        .db
        .read(move |conn| Ok(browse::track(conn, &id)?.is_some()))
        .await
        .map_err(|error| {
            tracing::error!(%error, "cannot read the library");
            ApiError::Other("the server cannot read its library".to_owned())
        })?;
    if known {
        Ok(Payload::new("lyricsList", LyricsList {}))
    } else {
        Err(ApiError::NotFound)
    }
}
