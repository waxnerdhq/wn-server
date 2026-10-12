#!/bin/sh
# Waits for the CI result of one commit. This is orchestrator tooling for the
# owner's Mac. A contributor does not need it.
#
# The orchestrator runs it after prep-lane.sh printed the head commit of a
# lane, and before merge-lane.sh. <pr> appears in the output only.
#
# Woodpecker reports the pipeline result as a commit status on GitHub. The
# script reads the combined status of <head-sha> every 20 seconds. A read that
# fails prints gh's message and one line on stderr, and the script reads again
# at the next poll.
#
# Exit status:
#   0   success
#   1   failure; the script prints the context, the state and the URL of the
#       newest status
#   2   no status within about 4 minutes; the result line says
#       "status read failed" when no read succeeded in that time
#   3   the status is still pending after about 20 minutes
#   64  wrong arguments
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
read_ok=0
while :; do
    # gh prints the error body of a failed request on stdout, so the output is
    # used only when gh succeeds.
    if ST=$(gh api "repos/$REPO/commits/$SHA/status" --jq '.state + " " + (.total_count|tostring)'); then
        read_ok=1
    else
        echo "status read failed pr=$PR sha=$SHA" >&2
        ST="error 0"
    fi
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
        if [ "$read_ok" = 0 ]; then
            echo "CI status read failed for 4 min pr=$PR sha=$SHA"
        else
            echo "CI no status after 4 min pr=$PR sha=$SHA"
        fi
        exit 2
    fi
    if [ "$i" -ge 60 ]; then
        echo "CI still pending after 20 min pr=$PR sha=$SHA"
        exit 3
    fi
    sleep 20
done
