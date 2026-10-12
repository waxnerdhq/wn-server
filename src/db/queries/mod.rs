//! Query functions, one module per group of tables or per group of API
//! methods (`docs/design.md` section 3.1).
//!
//! # The module list
//!
//! This file holds the list of the query modules and nothing else. The list
//! has one line for each module that the design names, in the order of the
//! design, and a blank line follows every entry. A module that the server
//! does not have yet is a comment line with its name.
//!
//! A change that adds a module replaces the comment line of that module and
//! keeps the blank lines. Two changes then never touch neighbouring lines,
//! and git merges them without a conflict. A change that needs a module the
//! list does not name adds the name to the design, and an entry with its
//! blank line above `mod int;`. That line is the last of the list and does
//! not change, so the new entry is no neighbour of a comment line.
//!
//! rustfmt orders `mod` lines that follow each other without a blank line.
//! Every entry is followed by a blank line, so rustfmt keeps this order.
//!
//! # Which module a query belongs to
//!
//! The modules of the tables (`tracks`, `albums`, `artists`, `annotations`,
//! `users`, `scans`, `folders`) hold what the scanner, the CLI and the
//! authentication use. `browse` reads the library for the API: it holds the
//! rows that the response models are built from and the functions that read
//! them by ID. The module of a group of API methods (`lists`, `search`,
//! `playlists`, `play_queue`, `lyrics`) selects IDs with its own SQL and
//! reads the rows through `browse`, so it adds no function to another
//! module. `indexes` is the module of the folder methods. They read their
//! rows through `browse`, and the module holds what `browse` does not have:
//! the time of the last change of the artists and whether a scan has no end
//! time. It reads both from the `scans` table.

pub mod tracks;

pub mod albums;

pub mod artists;

pub mod annotations;

pub mod browse;

pub mod lists;

pub mod search;

pub mod playlists;

pub mod play_queue;

// lyrics

pub mod users;

pub mod scans;

pub mod folders;

pub mod indexes;

mod int;
