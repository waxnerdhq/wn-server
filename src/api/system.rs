//! The system methods: `ping`, `getLicense` and `getOpenSubsonicExtensions`.

use serde::Serialize;

use super::{ApiResult, EXTENSIONS, Payload};
use crate::auth::extract::Authed;

/// The `license` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct License {
    valid: bool,
}

/// One `openSubsonicExtensions` element.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Extension {
    name: &'static str,
    versions: &'static [u32],
}

/// `ping`: succeeds for a request that has a user, and carries nothing.
pub async fn ping(_user: Authed) -> ApiResult {
    Ok(Payload::empty())
}

/// `getLicense`: the server needs no license, so the license is valid.
pub async fn get_license(_user: Authed) -> ApiResult {
    Ok(Payload::new("license", License { valid: true }))
}

/// `getOpenSubsonicExtensions`: the entries of [`EXTENSIONS`]. The method is
/// public, so the handler takes no user.
pub async fn get_open_subsonic_extensions() -> ApiResult {
    let extensions: Vec<Extension> = EXTENSIONS
        .iter()
        .map(|&(name, versions)| Extension { name, versions })
        .collect();
    Ok(Payload::new("openSubsonicExtensions", extensions))
}
