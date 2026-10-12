#!/bin/sh
# Merges a lane's PR and cleans up after it. This is orchestrator tooling for
# the owner's Mac. A contributor does not need it.
#
# The orchestrator runs it after prep-lane.sh printed the head commit and
# ci-wait.sh reported success for that commit.
#
# The script merges only when the PR is open, its head commit is
# <expected-head-sha>, GitHub reports the merge state CLEAN, the head commit
# contains the head of main on GitHub, the combined commit status of the head
# commit is success, and the PR title starts with "<WAX-id>:". It then
# squash-merges with the subject "<title> (#<pr>)" and deletes the remote
# branch.
#
# After the merge it removes the lane's worktree and local branches,
# fast-forwards main in the main checkout, and moves the Linear issue to Done.
# Each of these steps runs even when an earlier one fails. Linear's GitHub
# automation is not connected to this repository, so the merge does not close
# the issue.
#
# Exit status:
#   0   the PR is merged and every later step succeeded
#   1   a check or a read before the merge failed; nothing is merged
#   2   the merge command failed, and the PR is not merged or its state could
#       not be read
#   3   the PR is merged and a later step failed; the output names the step
#   64  wrong arguments
set -eu

usage() {
    echo "usage: merge-lane.sh <pr> <WAX-id> <expected-head-sha> <worktree-id>" >&2
    echo "The lane's worktree is .claude/worktrees/agent-<worktree-id> in the main checkout." >&2
    exit 64
}
[ $# -eq 4 ] || usage

# Ends the script before the merge.
die() {
    echo "$1"
    exit 1
}

PR=$1
ISSUE=$2
SHA=$3
AGENT=$4
REPO=waxnerdhq/wn-server

# The main checkout, also when this script runs from a linked worktree.
COMMON=$(git -C "$(dirname "$0")" rev-parse --path-format=absolute --git-common-dir) || die "cannot find the main checkout"
ROOT=$(dirname "$COMMON")
cd "$ROOT" || die "cannot enter $ROOT"

# GitHub computes the merge state lazily, and the first query after a push
# returns UNKNOWN. The query runs up to six times, five seconds apart, until
# the state is known.
n=0
while :; do
    INFO=$(gh pr view "$PR" --repo "$REPO" --json state,mergeStateStatus,headRefOid,headRefName,title --jq '[.state,.mergeStateStatus,.headRefOid,.headRefName,.title]|@tsv') || die "cannot read the PR"
    MSS=$(printf '%s' "$INFO" | cut -f2)
    [ "$MSS" = UNKNOWN ] || break
    n=$((n + 1))
    [ "$n" -lt 6 ] || break
    sleep 5
done
STATE=$(printf '%s' "$INFO" | cut -f1)
HEAD=$(printf '%s' "$INFO" | cut -f3)
BRANCH=$(printf '%s' "$INFO" | cut -f4)
TITLE=$(printf '%s' "$INFO" | cut -f5)
echo "pr=$PR state=$STATE merge=$MSS head=$HEAD"
[ "$STATE" = OPEN ] || die "not open"
[ "$HEAD" = "$SHA" ] || die "head mismatch: expected $SHA"
case "$MSS" in
    CLEAN) ;;
    BEHIND | DIRTY) die "not clean: needs rebase" ;;
    *) die "not clean: merge state is $MSS" ;;
esac
# GitHub reports CLEAN for a branch that lacks commits of main and does not
# conflict with them. The check pipeline runs on the head commit of the PR and
# not on main, so the merge commit has a checked tree only when the head commit
# contains the head of main. behind_by is the number of commits of main that
# the head commit lacks. A read that fails leaves BEHIND empty and ends the
# script like a value other than 0.
BEHIND=$(gh api "repos/$REPO/compare/main...$HEAD" --jq .behind_by) || BEHIND=
[ "$BEHIND" = 0 ] || die "not up to date with main: run prep-lane.sh"
CI=$(gh api "repos/$REPO/commits/$HEAD/status" --jq '.state + " " + (.total_count|tostring)') || die "cannot read the commit status"
if [ "${CI% *}" != success ] || [ "${CI#* }" = 0 ]; then
    die "ci not green: $CI"
fi
case "$TITLE" in
    "$ISSUE:"*) ;;
    *) die "title does not start with $ISSUE: $TITLE" ;;
esac

# A failed step from here on does not end the script. FAILED records it.
FAILED=0
if ! gh pr merge "$PR" --repo "$REPO" --squash --delete-branch --subject "$TITLE (#$PR)" >/dev/null; then
    # A failed merge command does not show whether the PR is merged.
    AFTER=$(gh pr view "$PR" --repo "$REPO" --json state --jq .state 2>/dev/null) || AFTER=unknown
    if [ "$AFTER" != MERGED ]; then
        echo "merge command failed: PR state is $AFTER"
        exit 2
    fi
    echo "merge command FAILED after it merged the PR"
    FAILED=1
fi

WT=.claude/worktrees/agent-$AGENT
if [ ! -d "$WT" ]; then
    echo "worktree already gone"
elif ! git worktree remove --force "$WT"; then
    echo "worktree NOT removed: $WT"
    FAILED=1
fi
git branch -D "$BRANCH" "worktree-agent-$AGENT" >/dev/null 2>&1 || true
ON=$(git symbolic-ref -q --short HEAD) || ON="no branch"
if [ "$ON" != main ]; then
    echo "main NOT updated: the main checkout is on $ON"
    FAILED=1
elif ! git pull -q --ff-only origin main; then
    echo "main NOT updated: the pull failed"
    FAILED=1
fi
if NO_COLOR=1 linear issue update "$ISSUE" --state Done >/dev/null 2>&1; then
    echo "linear: $ISSUE Done"
else
    echo "linear: FAILED to close $ISSUE"
    FAILED=1
fi
git log --oneline -1 || FAILED=1
[ "$FAILED" = 0 ] || exit 3
