//! The music library on disk.
//!
//! # The module list
//!
//! The list below has one line for each module that `docs/design.md`
//! section 3.1 names for this directory, in the order of that section, and a
//! blank line follows every entry. A module that the server does not have
//! yet is a comment line with its name.
//!
//! A change that adds a module replaces the comment line of that module and
//! keeps the blank lines. Two changes then never touch neighbouring lines,
//! and git merges them without a conflict. A change that needs a module the
//! list does not name adds the name to the design, and an entry with its
//! blank line above the last line of this file. That line marks the end of
//! the list and does not change, so the new entry is no neighbour of the
//! entry before it.
//!
//! rustfmt orders `mod` lines that follow each other without a blank line.
//! Every entry is followed by a blank line, so rustfmt keeps this order.

pub mod tags;

pub mod covers;
pub mod scanner;

// The controller runs the scanner for the server.
pub mod scan_controller;

// covers

// lyrics

// The end of the list. A new entry goes above this line.
