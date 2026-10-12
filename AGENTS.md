# wn-server

wn-server is a headless music server. It scans a local music collection into SQLite and serves it to OpenSubsonic clients over HTTP on the local network. It is one Rust binary with CLI subcommands and a TOML config file. This file holds the rules for working in this repository. `docs/design.md` is the design reference. `README.md` is the user documentation.

## Layout

- `src/main.rs`: the binary entry. `src/lib.rs`: the module tree.
- `src/cli.rs`, `src/config.rs`: commands and configuration.
- `src/db/`: SQLite access and queries. `migrations/`: numbered SQL files.
- `src/library/`: tag reading, scanner, cover art.
- `src/auth/`: credential verification and request authentication.
- `src/api/`: the OpenSubsonic endpoints, envelope and models.
- `src/media/`: streaming and transcoding.
- `src/discovery.rs`, `src/server.rs`: mDNS and composition.
- `docs/design.md`: the design. An issue cites it by section heading.
- `tests/`: router-level integration tests. `tests/fixtures/audio/`: five tiny audio files. `tests/common/mod.rs`: `LibraryBuilder` and `TestServer`.
- `examples/make-library.rs`: writes the library of the end-to-end session. `examples/e2e.rs`: the end-to-end client session.
- `xtask/`: the `cargo xtask check` command. `scripts/check.sh`: its Unix wrapper. `scripts/make-fixtures.sh`: regenerates the fixtures (maintainer tool, Unix).
- `scripts/orchestrator/`: the scripts of the merge sequence (see Orchestration).
- `.woodpecker/`: CI pipelines.

## Tooling

- Rust 1.98.1, pinned in `rust-toolchain.toml`. Edition 2024. `unsafe_code = "forbid"`.
- `cargo xtask check` runs four checks in this order: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace` and `cargo doc --locked --workspace --no-deps` with `RUSTDOCFLAGS="-D warnings"`. The fourth check fails on a warning of rustdoc, such as a link in a doc comment that names no item, or a link from the documentation of a public item to a private item. The `xtask` alias passes `--locked`, so a `Cargo.lock` that does not match the manifests fails the check. It is the one definition of the checks and runs on Linux, Windows and macOS. `scripts/check.sh` calls it.
- The project builds and runs on Linux, Windows and macOS. Code that needs one OS is behind `#[cfg(unix)]` or `#[cfg(windows)]` with a comment saying why. Relative paths are stored with `/` separators. Scripts a contributor needs are Rust (`xtask`, `examples/`), not shell or Python.
- CI is Woodpecker (`.woodpecker/check.yaml`), one Linux container, no cache. The pipeline runs on pull requests only. It runs `cargo xtask check` natively and `cargo clippy --locked --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` as a Windows cross build. The pipeline sets `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only` for its builds, which shortens them. A local `cargo xtask check` runs with cargo's defaults. macOS is verified by the local `cargo xtask check` run on the developer's Mac before every push. Windows tests run natively by hand until a Windows agent exists.
- The project is licensed Apache-2.0. A new dependency needs a licence compatible with that (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 as a dependency). GPL, LGPL and AGPL crates are not added. All dependencies are declared in the root `Cargo.toml`; a feature PR does not add one without saying so in the PR.
- ffmpeg is an external program found on PATH. The project does not link or bundle it.
- Tests run without network beyond loopback, audio hardware or ffmpeg. The default suite finishes in under 30 seconds after compiling in a debug build. A test that needs more is `#[ignore]`. No test sleeps or polls for more than one second.
- Linear: team `WAX`, workspace `waxnerd` (`.linear.toml`). The `linear` CLI default workspace is correct for this repo.

## Process

- Every PR references one WAX Linear issue. One issue is one branch and one PR.
- Branch name: `adnan/wax-nnn-short-slug`. PR title: `WAX-nnn: Sentence case summary`. The PR body contains `Fixes WAX-nnn`.
- The issue moves to In Progress before the first code change and to In Review when the PR is pushed.
- The loop is plan, implement, review, apply fixes. The planner writes the issue, the implementer writes the code and tests, the reviewer reads the diff and ranks findings, the implementer applies the fixes.
- The implementer's report lists every deviation from the issue and everything in the issue or the design that the implementer found wrong. The report is part of the PR description. The reviewer rules on each item.
- When a review has no blocking findings, the implementer applies the should-fix findings and the PR merges without a second review pass. A second pass is required when the first pass had blocking findings, or when the issue's scope changed after the review.
- Follow-up work found in a review becomes a new WAX issue in the project, with its blocking relations set. It is not added to the PR under review unless the orchestrator amends the issue text first.
- All checks run locally with `cargo xtask check` (or `scripts/check.sh`) before a push. Nobody states that checks pass without having run them in that session.
- The orchestrator squash-merges when the review has no blocking findings and the checks are green (owner decision for this repo, 2026-10-11).
- A PR merges only when the Woodpecker status on its head commit is green. That status is required in addition to the local check.
- The Woodpecker pipeline runs on pull requests and not on `main`. The merge sequence rebases a PR on `origin/main`, waits for the pipeline on that head commit, and merges that commit only when it contains the head of `main` (see Orchestration), so the merge commit on `main` has the tree that the pipeline checked. The merge commit gets no Woodpecker status. A commit that reaches `main` without a pull request is not checked by the pipeline.
- Linear's GitHub automation is not connected to this repository, so a merged PR does not close its issue. `scripts/orchestrator/merge-lane.sh` moves the issue to Done after the merge. The merger verifies that the issue is closed, and closes it when the script did not.
- This repository is public. Details of the owner's CI infrastructure stay out of PRs, commit messages and anything public that an issue links to.
- A commit message ends with the attribution line the environment provides.

## Migrations

- A schema change is a new numbered file in `migrations/` and one line in `MIGRATIONS` (`src/db/mod.rs`). `docs/design.md` section 3.4 describes how `Db::open` applies it.
- A migration file that is on `main` is never edited. Databases exist that applied it, and they do not apply it again.
- A new column is appended last, with `ALTER TABLE ... ADD COLUMN`, and is added to `ADDED_SINCE_V1` in the tests of `src/db/mod.rs` with the value that a row from before holds. A column that a migration removes goes into `REMOVED_SINCE_V1`, and a value that it multiplies into `MULTIPLIED_SINCE_V1`.
- No query of the server uses `SELECT *`. A statement names its columns, so a migration that adds or removes a column changes no result. The row dumps of the migration tests read every column on purpose.
- The issue and `docs/design.md` section 3.4 state what the migration does to the rows that exist.
- A migration test starts from a database of the previous schema version with rows in every table, and checks that every row, every rowid and every ID is kept. The test from schema version 1 covers the new migration as well.
- A migration that fails leaves the database at its version with its rows, and its error names the migration and that version. A test covers a failure after the first statements of the migration ran.

## Orchestration

- The main session orchestrates and does not implement.
- For each unblocked issue the orchestrator starts an implementer in its own git worktree, then a reviewer on the PR, then has the implementer apply the blocking and should-fix findings, then runs the merge sequence, and starts the issues the merge unblocked.
- The reviewer posts its findings as one comment on the PR. The implementer answers them in the PR. The orchestrator receives counts and verdicts, not the findings.
- The reviewer runs in the implementer's worktree so that it can run the checks on the PR's code.
- A lane brief that omits the reviewer pass or the Linear state moves is defective and is corrected before the lane starts.
- The merge sequence is three scripts in `scripts/orchestrator/`. The orchestrator runs them in this order after the implementer reports its fixes. `<worktree-id>` names the lane's worktree, `.claude/worktrees/agent-<worktree-id>`.
  1. `prep-lane.sh <worktree-id>` rebases the lane's branch on `origin/main`, formats the code, reruns `cargo xtask check`, pushes, and prints the head commit. `main` moves after every merge, and parallel PRs touch shared tables such as `METHODS`. When the branch already contains `origin/main`, the script checks that the head commit is pushed and prints it. The format commit ends with the attribution line in `PREP_LANE_ATTRIBUTION`.
  2. `ci-wait.sh <pr> <sha>` waits for the Woodpecker status on that commit.
  3. `merge-lane.sh <pr> <WAX-id> <sha> <worktree-id>` checks that the PR's head commit is `<sha>`, that GitHub reports the merge state as clean, that the head commit contains the head of `main` on GitHub, and that the CI status is green. GitHub reports a clean merge state for a branch that is behind `main` and has no conflict with it, so the script reads from GitHub how many commits the head commit is behind `main`. When the number is not 0 or the read fails, it ends with `not up to date with main: run prep-lane.sh` and merges nothing. It then squash-merges, removes the worktree and the lane's branches, updates `main`, and closes the Linear issue (see Process).
- The three scripts are POSIX `sh` and run on the owner's Mac only. They are orchestrator tooling, not contributor tooling, so the rule that contributor scripts are Rust holds.
- `.gitattributes` sets `merge=union` on `src/lib.rs` only, so a rebase keeps the `pub mod` lines of both sides in that file and the format step orders them. A rebase conflict in any other file stops `prep-lane.sh` and goes back to the lane's implementer.
- Work state lives in Linear and git. Anything the orchestrator knows that those do not show goes into a handoff note for the next orchestrator session.

## Documentation

- A PR that changes documented behaviour updates the document in the same PR. A PR that changes designed behaviour updates `docs/design.md` in the same PR.
- Updating means replacing the old text, not appending to it. A document describes the present state only; no changelog paragraphs, no "previously".
- `README.md` describes installation, configuration, running and the API behaviour a client sees. `docs/design.md` describes the design. `AGENTS.md` describes how to work in the repository. Doc comments describe what a function does now.
- Writing style for docs, comments, commit messages and PR text: declarative, plain sentences, one idea per sentence. No metaphors, no slogans, no rhetorical build-up.

## Verification

- Before pushing: `cargo xtask check` passes on the developer's machine.
- Behaviour visible over HTTP has a router-level test in `tests/`. Pure logic has unit tests next to the code.
- Transcoding tests use `FakeTranscoder`; the four tests that run ffmpeg are `#[ignore]`. A test that runs `run_server` sets `transcoding.ffmpeg` to a path that names no program, such as `common::transcoder::no_ffmpeg()`, so that the default suite starts no ffmpeg.
- A PR description states which tests were added and, for manual checks (mDNS, real clients, native Windows runs), pastes the command and its output.
- The reviewer checks that each acceptance criterion in the issue is covered by a test or a recorded manual check, and that nothing from the issue's out-of-scope list was built.
