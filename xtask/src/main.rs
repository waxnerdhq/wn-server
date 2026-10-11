//! Repository tasks. `cargo xtask check` runs the checks that a change must
//! pass before it is pushed. This crate is the one definition of those checks.

use std::env;
use std::process::{self, Command};

/// The cargo binary that compiled this crate. Running the checks with it keeps
/// them on the toolchain that runs xtask.
const CARGO: &str = env!("CARGO");

/// The arguments of each check, in the order the checks run.
const CHECKS: [&[&str]; 3] = [
    &["fmt", "--all", "--", "--check"],
    &[
        "clippy",
        "--locked",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ],
    &["test", "--locked"],
];

const USAGE: &str = "usage: cargo xtask check";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [task] if task == "check" => check(),
        _ => {
            eprintln!("{USAGE}");
            process::exit(2);
        }
    }
}

/// Runs every check in order. Exits with the exit code of the first check that
/// fails.
fn check() {
    for cargo_args in CHECKS {
        eprintln!("xtask: cargo {}", cargo_args.join(" "));
        let status = match Command::new(CARGO).args(cargo_args).status() {
            Ok(status) => status,
            Err(error) => {
                eprintln!("xtask: cannot run {CARGO}: {error}");
                process::exit(1);
            }
        };
        if !status.success() {
            // A check that a signal ended has no exit code.
            process::exit(status.code().unwrap_or(1));
        }
    }
}
