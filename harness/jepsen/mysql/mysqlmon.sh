#!/bin/bash
# Run the official mysql entrypoint with a guest-memory monitor: every 5 s, print
# MemAvailable, Shmem, the process count and the data directory size to stderr
# (lines start with MYMON). When mysqld exits, print its exit code and, where the
# image has dmesg, the end of the kernel log (OOM kills show there).
# Only bash builtins and coreutils: the Oracle Linux slim image has no awk.
while read -r line; do echo "MYMON df $line" >&2; done < <(df -k / /var/lib/mysql 2>&1)
(
  while sleep 5; do
    mem=""
    while read -r key value _; do
      case $key in
        MemTotal:|MemAvailable:|Shmem:|Cached:|Committed_AS:) mem="$mem$key$value " ;;
      esac
    done < /proc/meminfo
    procs=$(ls -d /proc/[0-9]* | wc -l)
    data=$(du -sk /var/lib/mysql 2>/dev/null | cut -f1)
    read -r uptime _ < /proc/uptime
    echo "MYMON t=$uptime procs=$procs data_kb=${data:-0} $mem" >&2
  done
) &
monitor=$!
docker-entrypoint.sh "$@"
code=$?
kill $monitor 2>/dev/null
echo "MYMON exit $code" >&2
if command -v dmesg >/dev/null; then
  dmesg 2>&1 | tail -60 | while read -r line; do echo "MYMON dmesg $line" >&2; done
fi
exit $code
