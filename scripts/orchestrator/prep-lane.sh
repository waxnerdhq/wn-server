#!/bin/sh
# Prepares a lane's branch for the merge. This is orchestrator tooling for the
# owner's Mac. A contributor does not need it.
#
# The orchestrator runs it after the implementer reports that its review fixes
# are pushed, and before ci-wait.sh and merge-lane.sh.
#
# When origin/main is not an ancestor of the branch, the script rebases the
# branch on origin/main, runs `cargo fmt --all` and commits the result when the
# rebase left the code unformatted, runs `cargo xtask check`, and force-pushes
# with a lease. When the branch already contains origin/main, the script
# changes nothing. In both cases it prints HEAD=<sha>, the commit that
# ci-wait.sh and merge-lane.sh take.
#
# The check output goes to ${TMPDIR:-/tmp}/prep-lane-<worktree-id>.log.
#
# Exit status: 0 when the branch is ready, 1 when the worktree has uncommitted
# changes, 2 on a rebase conflict (the script lists the files and aborts the
# rebase), 3 when the check fails, 64 on wrong arguments.
set -eu

usage() {
    echo "usage: prep-lane.sh <worktree-id>" >&2
    echo "The lane's worktree is .claude/worktrees/agent-<worktree-id> in the main checkout." >&2
    exit 64
}
[ $# -eq 1 ] || usage

# The main checkout, also when this script runs from a linked worktree.
COMMON=$(git -C "$(dirname "$0")" rev-parse --path-format=absolute --git-common-dir)
ROOT=$(dirname "$COMMON")
TMP=${TMPDIR:-/tmp}
LOG=${TMP%/}/prep-lane-$1.log

cd "$ROOT/.claude/worktrees/agent-$1"
if [ -n "$(git status --porcelain)" ]; then
    echo "worktree dirty"
    git status --short | head
    exit 1
fi
git fetch -q origin
if git merge-base --is-ancestor origin/main HEAD; then
    echo "up to date with main"
else
    if ! git rebase -q origin/main >/dev/null 2>&1; then
        echo "REBASE CONFLICT:"
        git diff --name-only --diff-filter=U
        git rebase --abort
        exit 2
    fi
    if ! cargo fmt --all -- --check >/dev/null 2>&1; then
        cargo fmt --all
        git commit -q -am "Format module declarations after rebase on main

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
        echo "formatted after rebase: $(git show --stat --format= HEAD | tail -1)"
    fi
    if ! cargo xtask check >"$LOG" 2>&1; then
        echo "CHECK FAILED after rebase:"
        tail -25 "$LOG"
        exit 3
    fi
    grep -E '^test result' "$LOG" | awk '{p+=$4; f+=$6} END {print "check ok: " p " passed, " f " failed"}'
    git push -q --force-with-lease
fi
echo "HEAD=$(git rev-parse HEAD)"
