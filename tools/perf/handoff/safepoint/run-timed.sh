#!/bin/sh
# usage: run-timed.sh SECONDS NAME -- <run.sh args...>
# Runs run.sh under timeout(1) with output to a file; on timeout, SIGKILLs the
# podman client and removes any leftover containers. Appends to logs/NAME.log.
set -u
here=$(cd "$(dirname "$0")" && pwd)
secs=$1; name=$2; shift 2
[ "$1" = "--" ] && shift
log=$here/logs/$name.log
echo "\$ build/jvm-crash/repro/run.sh $*" > "$log"
start=$(date +%s)
timeout -s KILL "$secs" "$here/run.sh" "$@" >> "$log" 2>&1
rc=$?
end=$(date +%s)
if [ $rc -ne 0 ]; then
    for c in $(podman ps -q --filter ancestor=docker.io/library/alpine:3.22); do
        podman rm -f "$c" >/dev/null 2>&1
    done
fi
echo "exit=$rc wall_seconds=$((end - start))" >> "$log"
echo "$name: exit=$rc wall=$((end - start))s"
tail -n 3 "$log"
