#!/bin/sh
# Merges a lane's PR and cleans up after it. This is orchestrator tooling for
# the owner's Mac. A contributor does not need it.
#
# The orchestrator runs it after prep-lane.sh printed the head commit and
# ci-wait.sh reported success for that commit.
#
# The script merges only when the PR is open, its head commit is
# <expected-head-sha>, GitHub reports the merge state CLEAN, the combined
# commit status of the head commit is success, and the PR title starts with
# "<WAX-id>:". It then squash-merges with the subject "<title> (#<pr>)" and
# deletes the remote branch, removes the lane's worktree and local branches,
# fast-forwards main in the main checkout, and moves the Linear issue to Done.
# Linear's GitHub automation is not connected to this repository, so the merge
# does not close the issue.
#
# Exit status: 0 after the merge and the cleanup, 1 when one of the checks
# before the merge fails, 64 on wrong arguments. A failed Linear update is
# printed and does not change the exit status.
set -eu

usage() {
    echo "usage: merge-lane.sh <pr> <WAX-id> <expected-head-sha> <worktree-id>" >&2
    echo "The lane's worktree is .claude/worktrees/agent-<worktree-id> in the main checkout." >&2
    exit 64
}
[ $# -eq 4 ] || usage

PR=$1
ISSUE=$2
SHA=$3
AGENT=$4
REPO=waxnerdhq/wn-server

# The main checkout, also when this script runs from a linked worktree.
COMMON=$(git -C "$(dirname "$0")" rev-parse --path-format=absolute --git-common-dir)
ROOT=$(dirname "$COMMON")
cd "$ROOT"

# GitHub computes the merge state lazily, and the first query after a push
# returns UNKNOWN. The query runs up to six times, five seconds apart, until
# the state is known.
n=0
while :; do
    INFO=$(gh pr view "$PR" --repo "$REPO" --json state,mergeStateStatus,headRefOid,headRefName,title --jq '[.state,.mergeStateStatus,.headRefOid,.headRefName,.title]|@tsv')
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
[ "$STATE" = OPEN ] || { echo "not open"; exit 1; }
[ "$HEAD" = "$SHA" ] || { echo "head mismatch: expected $SHA"; exit 1; }
case "$MSS" in
    CLEAN) ;;
    BEHIND | DIRTY) echo "not clean: needs rebase"; exit 1 ;;
    *) echo "not clean: merge state is $MSS"; exit 1 ;;
esac
CI=$(gh api "repos/$REPO/commits/$HEAD/status" --jq '.state + " " + (.total_count|tostring)')
if [ "${CI% *}" != success ] || [ "${CI#* }" = 0 ]; then
    echo "ci not green: $CI"
    exit 1
fi
case "$TITLE" in
    "$ISSUE:"*) ;;
    *) echo "title does not start with $ISSUE: $TITLE"; exit 1 ;;
esac

gh pr merge "$PR" --repo "$REPO" --squash --delete-branch --subject "$TITLE (#$PR)" >/dev/null
WT=.claude/worktrees/agent-$AGENT
if [ -d "$WT" ]; then
    git worktree remove --force "$WT" || echo "worktree NOT removed: $WT"
else
    echo "worktree already gone"
fi
git branch -D "$BRANCH" "worktree-agent-$AGENT" >/dev/null 2>&1 || true
git pull -q --ff-only origin main
if NO_COLOR=1 linear issue update "$ISSUE" --state Done >/dev/null 2>&1; then
    echo "linear: $ISSUE Done"
else
    echo "linear: FAILED to close $ISSUE"
fi
git log --oneline -1
