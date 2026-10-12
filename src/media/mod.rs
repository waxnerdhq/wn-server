//! Streaming and transcoding (`docs/design.md` section 3.9).
//!
//! # The module list
//!
//! The list below has one line for each module that `docs/design.md`
//! section 3.1 names for this directory, in the order of that section, and a
//! blank line follows every entry. A module that the server does not have
//! yet is a comment line with its name. A change that adds a module replaces
//! the comment line of that module and keeps the blank lines.

pub mod stream;

pub mod transcode;
