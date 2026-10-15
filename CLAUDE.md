# wn-server

Go program that wraps Navidrome for the waxnerd ecosystem. `README.md` holds the architecture and is the design record. `docs/` holds diagrams.

## Working rules

- Use subagents for implementing and reviewing. Use less expensive models such as sonnet for straightforward tasks. Use higher-cost models such as fable or opus for complicated tasks.
- Keep unit testing lean. It is fine to leave tests unwritten when the critical paths are already covered.
- Run tests and lint locally. For checks that the operator can do faster by hand, such as browser testing or runtime testing, hand them to the operator unless told otherwise.
- Pause for approval on a task before starting it, unless told not to.
- Keep all documentation up to date as things progress and change. `README.md` describes the current state, not only the plan.
- Keep Linear up to date as things progress and change. This project is in the `waxnerd` workspace, team `WAX`.
- Issues that need changes in wn-cloud are filed in the same Linear project and labelled for wn-cloud. Do not implement them here. The operator hands them to a separate wn-cloud agent.
- When the context window is low, suggest a handoff and prepare for clearing context and resuming with a new agent.

## Conventions

- Module path `github.com/waxnerdhq/wn-server`. One package per component under `internal/`.
- `make check` runs fmt, lint, test, and build. Run it before opening a pull request.
- The release target is Linux x86_64 only for now. Do not spend effort on other platforms.
- Navidrome is a separate process. Do not import Navidrome packages. The checkout at `../navidrome` is for reading its behaviour.
- The Rust predecessor at `../wn-server-vibecoded` is reference only.
