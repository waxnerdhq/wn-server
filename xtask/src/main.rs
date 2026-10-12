//! Repository tasks. `cargo xtask check` runs the checks that a change must
//! pass before it is pushed. This crate is the one definition of those checks.

use std::env;
use std::process::{self, Command};

/// The cargo binary that compiled this crate. Running the checks with it keeps
/// them on the toolchain that runs xtask.
const CARGO: &str = env!("CARGO");

/// One check: a cargo command.
struct Check {
    /// The arguments of cargo.
    args: &'static [&'static str],
    /// The variables that the check sets in the environment of cargo. A
    /// variable replaces the value that the environment of xtask holds.
    env: &'static [(&'static str, &'static str)],
}

/// The checks, in the order in which they run.
const CHECKS: [Check; 4] = [
    Check {
        args: &["fmt", "--all", "--", "--check"],
        env: &[],
    },
    Check {
        args: &[
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        env: &[],
    },
    Check {
        args: &["test", "--locked", "--workspace"],
        env: &[],
    },
    // rustdoc builds the documentation of the two crates of the workspace
    // and of no dependency. `cargo doc` has no option that turns the
    // warnings of rustdoc into errors, so the variable passes `-D warnings`
    // to rustdoc. A link in a doc comment that names no item is such a
    // warning.
    Check {
        args: &["doc", "--locked", "--workspace", "--no-deps"],
        env: &[("RUSTDOCFLAGS", "-D warnings")],
    },
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
    for check in CHECKS {
        let variables: Vec<String> = check
            .env
            .iter()
            .map(|(name, value)| format!("{name}=\"{value}\" "))
            .collect();
        eprintln!(
            "xtask: {}cargo {}",
            variables.concat(),
            check.args.join(" ")
        );
        let status = Command::new(CARGO)
            .args(check.args)
            .envs(check.env.iter().copied())
            .status();
        let status = match status {
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
