//! The wn-server binary. The commands are in `wn_server::cli`.

use std::process::ExitCode;

use clap::Parser;
use wn_server::cli::{self, Cli};

fn main() -> ExitCode {
    cli::run(&Cli::parse())
}
