//! The response models of the API.
//!
//! One set of models serves JSON and XML. Every model derives
//! `serde::Serialize` and carries these serde attributes:
//!
//! - `#[serde(rename_all = "camelCase")]` on the type. The serialised field
//!   names are then the OpenSubsonic names.
//! - `#[serde(skip_serializing_if = "Option::is_none")]` on every `Option`
//!   field. A `None` field is then absent from both formats.
