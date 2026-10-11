//! The system methods: `ping`, `getLicense` and `getOpenSubsonicExtensions`.

use serde::Serialize;

use super::{ApiResult, EXTENSIONS, Payload};

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

/// `ping`: succeeds and carries nothing.
pub async fn ping() -> ApiResult {
    Ok(Payload::empty())
}

/// `getLicense`: the server needs no license, so the license is valid.
pub async fn get_license() -> ApiResult {
    Ok(Payload::new("license", License { valid: true }))
}

/// `getOpenSubsonicExtensions`: the entries of [`EXTENSIONS`].
pub async fn get_open_subsonic_extensions() -> ApiResult {
    let extensions: Vec<Extension> = EXTENSIONS
        .iter()
        .map(|&(name, versions)| Extension { name, versions })
        .collect();
    Ok(Payload::new("openSubsonicExtensions", extensions))
}
