#!/bin/sh
# Waits for the CI result of one commit. This is orchestrator tooling for the
# owner's Mac. A contributor does not need it.
#
# The orchestrator runs it after prep-lane.sh printed the head commit of a
# lane, and before merge-lane.sh. <pr> appears in the output only.
#
# Woodpecker reports the pipeline result as a commit status on GitHub. The
# script reads the combined status of <head-sha> every 20 seconds.
#
# Exit status: 0 on success, 1 on failure (the script prints the context, the
# state and the URL of the newest status), 2 when no status appears within
# about 4 minutes, 3 when the status is still pending after about 20 minutes,
# 64 on wrong arguments.
set -u

usage() {
    echo "usage: ci-wait.sh <pr> <head-sha>" >&2
    exit 64
}
[ $# -eq 2 ] || usage

PR=$1
SHA=$2
REPO=waxnerdhq/wn-server
i=0
seen=0
while :; do
    # A failed read counts as no status. gh prints the error body of a failed
    # request on stdout, so the output is used only when gh succeeds.
    ST=$(gh api "repos/$REPO/commits/$SHA/status" --jq '.state + " " + (.total_count|tostring)' 2>/dev/null) || ST="error 0"
    STATE=${ST% *}
    COUNT=${ST#* }
    [ "$COUNT" = 0 ] || seen=1
    if [ "$COUNT" != 0 ] && [ "$STATE" = success ]; then
        echo "CI success pr=$PR sha=$SHA"
        exit 0
    fi
    if [ "$COUNT" != 0 ] && { [ "$STATE" = failure ] || [ "$STATE" = error ]; }; then
        echo "CI $STATE pr=$PR sha=$SHA"
        gh api "repos/$REPO/commits/$SHA/statuses" --jq '.[0] | .context + " " + .state + " " + .target_url'
        exit 1
    fi
    i=$((i + 1))
    if [ "$seen" = 0 ] && [ "$i" -ge 12 ]; then
        echo "CI no status after 4 min pr=$PR sha=$SHA"
        exit 2
    fi
    if [ "$i" -ge 60 ]; then
        echo "CI still pending after 20 min pr=$PR sha=$SHA"
        exit 3
    fi
    sleep 20
done
