#!/bin/bash
# Run the official postgres entrypoint with a guest-memory monitor: every 5 s, print
# MemAvailable, Shmem, the process count and the WAL size to stderr (lines start with PGMON).
# When postgres exits, print the exit code and the end of the kernel log (OOM kills show there).
# With PGMON_RESTART=SECONDS, start postgres again that long after it exits, as a service
# manager would (a startup process that fails, say on an I/O error, ends the postmaster).
df -k / /dev/shm 2>&1 | sed 's/^/PGMON df /' >&2
(
  while sleep 5; do
    mem=$(awk '/^(MemTotal|MemAvailable|Shmem|Cached|Committed_AS):/ {printf "%s%s ", $1, $2}' /proc/meminfo)
    procs=$(ls -d /proc/[0-9]* | wc -l)
    wal=$(du -sk "${PGDATA:-/var/lib/postgresql/data}/pg_wal" 2>/dev/null | cut -f1)
    echo "PGMON t=$(cut -d' ' -f1 /proc/uptime) procs=$procs wal_kb=${wal:-0} $mem" >&2
  done
) &
monitor=$!
while true; do
  docker-entrypoint.sh "$@"
  code=$?
  echo "PGMON exit $code" >&2
  dmesg 2>&1 | tail -60 | sed 's/^/PGMON dmesg /' >&2
  [ -n "$PGMON_RESTART" ] || break
  sleep "$PGMON_RESTART"
  echo "PGMON restart" >&2
done
kill $monitor 2>/dev/null
exit $code
