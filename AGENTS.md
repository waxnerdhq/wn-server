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
- `examples/e2e.rs`: the end-to-end client session.
- `xtask/`: the `cargo xtask check` command. `scripts/check.sh`: its Unix wrapper. `scripts/make-fixtures.sh`: regenerates the fixtures (maintainer tool, Unix).
- `.woodpecker/`: CI pipelines.

## Tooling

- Rust 1.98.1, pinned in `rust-toolchain.toml`. Edition 2024. `unsafe_code = "forbid"`.
- `cargo xtask check` runs `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings` and `cargo test --locked --workspace`. The `xtask` alias passes `--locked`, so a `Cargo.lock` that does not match the manifests fails the check. It is the one definition of the checks and runs on Linux, Windows and macOS. `scripts/check.sh` calls it.
- The project builds and runs on Linux, Windows and macOS. Code that needs one OS is behind `#[cfg(unix)]` or `#[cfg(windows)]` with a comment saying why. Relative paths are stored with `/` separators. Scripts a contributor needs are Rust (`xtask`, `examples/`), not shell or Python.
- CI is Woodpecker (`.woodpecker/check.yaml`), one Linux container, no cache. It runs `cargo xtask check` natively and `cargo clippy --locked --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings` as a Windows cross build. macOS is verified by the local `cargo xtask check` run on the developer's Mac before every push. Windows tests run natively by hand until a Windows agent exists.
- The project is licensed Apache-2.0. A new dependency needs a licence compatible with that (MIT, Apache-2.0, BSD, ISC, Zlib, MPL-2.0 as a dependency). GPL, LGPL and AGPL crates are not added. All dependencies are declared in the root `Cargo.toml`; a feature PR does not add one without saying so in the PR.
- ffmpeg is an external program found on PATH. The project does not link or bundle it.
- Tests run without network, audio hardware or ffmpeg. The default suite finishes in under 30 seconds after compiling in a debug build. A test that needs more is `#[ignore]`. No test sleeps or polls for more than one second.
- Linear: team `WAX`, workspace `waxnerd` (`.linear.toml`). The `linear` CLI default workspace is correct for this repo.

## Process

- Every PR references one WAX Linear issue. One issue is one branch and one PR.
- Branch name: `adnan/wax-nnn-short-slug`. PR title: `WAX-nnn: Sentence case summary`. The PR body contains `Fixes WAX-nnn`.
- The issue moves to In Progress before the first code change and to In Review when the PR is pushed.
- The loop is plan, implement, review, apply fixes. The planner writes the issue, the implementer writes the code and tests, the reviewer reads the diff and ranks findings, the implementer applies the fixes.
- All checks run locally with `cargo xtask check` (or `scripts/check.sh`) before a push. Nobody states that checks pass without having run them in that session.
- The orchestrator squash-merges when the review has no blocking findings and the checks are green (owner decision for this repo, 2026-10-11).
- After the merge, the merger verifies that the Linear issue is closed, and closes it when the automation did not.
- A commit message ends with the attribution line the environment provides.

## Orchestration

- The main session orchestrates and does not implement.
- For each unblocked issue the orchestrator starts an implementer in its own git worktree, then a reviewer on the PR, then has the implementer apply the blocking and should-fix findings, then squash-merges, deletes the branch, verifies the Linear issue closed, and starts the issues the merge unblocked.
- The reviewer posts its findings as one comment on the PR. The implementer answers them in the PR. The orchestrator receives counts and verdicts, not the findings.
- The reviewer runs in the implementer's worktree so that it can run the checks on the PR's code. The orchestrator removes the worktree after the merge.
- A lane brief that omits the reviewer pass or the Linear state moves is defective and is corrected before the lane starts.
- Before the squash-merge, the lane rebases its branch on `main` and reruns `cargo xtask check`, because `main` moves after every merge and parallel PRs touch shared tables such as `METHODS`.
- Work state lives in Linear and git. Anything the orchestrator knows that those do not show goes into a handoff note for the next orchestrator session.

## Documentation

- A PR that changes documented behaviour updates the document in the same PR. A PR that changes designed behaviour updates `docs/design.md` in the same PR.
- Updating means replacing the old text, not appending to it. A document describes the present state only; no changelog paragraphs, no "previously".
- `README.md` describes installation, configuration, running and the API behaviour a client sees. `docs/design.md` describes the design. `AGENTS.md` describes how to work in the repository. Doc comments describe what a function does now.
- Writing style for docs, comments, commit messages and PR text: declarative, plain sentences, one idea per sentence. No metaphors, no slogans, no rhetorical build-up.

## Verification

- Before pushing: `cargo xtask check` passes on the developer's machine.
- Behaviour visible over HTTP has a router-level test in `tests/`. Pure logic has unit tests next to the code.
- Transcoding tests use `FakeTranscoder`; the one test that runs ffmpeg is `#[ignore]`.
- A PR description states which tests were added and, for manual checks (mDNS, real clients, native Windows runs), pastes the command and its output.
- The reviewer checks that each acceptance criterion in the issue is covered by a test or a recorded manual check, and that nothing from the issue's out-of-scope list was built.
