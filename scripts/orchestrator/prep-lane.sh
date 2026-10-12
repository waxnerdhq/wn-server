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
# changes nothing and checks that the head commit is the pushed one. It prints
# HEAD=<sha>, the commit that ci-wait.sh and merge-lane.sh take.
#
# The format commit ends with the attribution line in PREP_LANE_ATTRIBUTION.
# When the variable is unset, the line is the default below.
#
# The output of the rebase, the format step and the check goes to
# ${TMPDIR:-/tmp}/prep-lane-<worktree-id>.log.
#
# Exit status:
#   0   the branch is ready and pushed
#   1   the worktree has uncommitted changes or is not on a branch
#   2   the rebase failed; the script aborted it, and it listed the conflicting
#       files when there were any
#   3   the format step or the check failed after the rebase; the worktree
#       holds the rebased branch, unpushed, and after a failed format step it
#       can hold uncommitted changes
#   4   the branch contains origin/main and its head is not the pushed head
#   64  wrong arguments
# Any other status comes from a failed git or cargo command. The worktree may
# then hold the rebased branch, unpushed.
set -eu

usage() {
    echo "usage: prep-lane.sh <worktree-id>" >&2
    echo "The lane's worktree is .claude/worktrees/agent-<worktree-id> in the main checkout." >&2
    exit 64
}
[ $# -eq 1 ] || usage

ATTRIBUTION="${PREP_LANE_ATTRIBUTION:-Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>}"

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
BRANCH=$(git symbolic-ref -q --short HEAD) || {
    echo "worktree is not on a branch"
    exit 1
}
git fetch -q origin
if git merge-base --is-ancestor origin/main HEAD; then
    # The PR's head is origin/<branch>. A commit that exists only in the
    # worktree is not the merge candidate.
    PUSHED=$(git rev-parse -q --verify "refs/remotes/origin/$BRANCH") || PUSHED=none
    if [ "$(git rev-parse HEAD)" != "$PUSHED" ]; then
        echo "NOT PUSHED: HEAD is $(git rev-parse HEAD), origin/$BRANCH is $PUSHED"
        exit 4
    fi
    echo "up to date with main"
else
    if ! git rebase -q origin/main >"$LOG" 2>&1; then
        CONFLICTS=$(git diff --name-only --diff-filter=U) || true
        git rebase --abort >/dev/null 2>&1 || true
        if [ -n "$CONFLICTS" ]; then
            echo "REBASE CONFLICT:"
            echo "$CONFLICTS"
        else
            echo "REBASE FAILED:"
            tail -25 "$LOG"
        fi
        exit 2
    fi
    if ! cargo fmt --all -- --check >/dev/null 2>&1; then
        if ! cargo fmt --all >"$LOG" 2>&1; then
            echo "FORMAT FAILED after rebase:"
            tail -25 "$LOG"
            exit 3
        fi
        git commit -q -am "Format module declarations after rebase on main

$ATTRIBUTION"
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
