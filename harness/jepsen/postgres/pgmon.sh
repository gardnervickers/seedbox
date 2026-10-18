#!/bin/bash
# Run the official postgres entrypoint with a guest-memory monitor: every 5 s, print
# MemAvailable, Shmem, the process count and the WAL size to stderr (lines start with PGMON).
# When postgres exits, print the exit code and the end of the kernel log (OOM kills show there).
# With PGMON_PS set, also print every postgres process's state every 5 s (PGPS lines).
# With PGMON_WAL=N, after postgres first fails in a boot, print pg_controldata and N WAL records
# from the latest checkpoint's redo point (PGWAL lines).
# With PGMON_RESTART=SECONDS, start postgres again that long after it exits, as a service
# manager would (a startup process that fails, say on an I/O error, ends the postmaster).
df -k / /dev/shm 2>&1 | sed 's/^/PGMON df /' >&2
echo "PGMON core_pattern=$(cat /proc/sys/kernel/core_pattern 2>/dev/null) ulimit_c=$(ulimit -c)" >&2
(
  while sleep 5; do
    mem=$(awk '/^(MemTotal|MemAvailable|Shmem|Cached|Committed_AS):/ {printf "%s%s ", $1, $2}' /proc/meminfo)
    procs=$(ls -d /proc/[0-9]* | wc -l)
    wal=$(du -sk "${PGDATA:-/var/lib/postgresql/data}/pg_wal" 2>/dev/null | cut -f1)
    echo "PGMON t=$(cut -d' ' -f1 /proc/uptime) procs=$procs wal_kb=${wal:-0} $mem" >&2
    if [ -n "$PGMON_PS" ]; then
      # Every postgres process: pid, parent, state, kernel wait channel, blocked and pending
      # signals, and its title (the postmaster's children name their role in it).
      line=""
      for d in /proc/[0-9]*; do
        [ "$(cat $d/comm 2>/dev/null)" = postgres ] || continue
        read -r _ _ state ppid _ < <(sed 's/^.*) //; s/^/x x /' $d/stat 2>/dev/null)
        blk=$(awk '/^SigBlk/ {print $2}' $d/status 2>/dev/null)
        pnd=$(awk '/^(SigPnd|ShdPnd)/ {printf "%s/", $2}' $d/status 2>/dev/null)
        title=$(tr '\0' ' ' < $d/cmdline 2>/dev/null | cut -c1-48)
        line="$line | ${d#/proc/}<${ppid} $state $(cat $d/wchan 2>/dev/null) blk=$blk pnd=$pnd $title"
      done
      echo "PGPS t=$(cut -d' ' -f1 /proc/uptime)$line" >&2
    fi
  done
) &
monitor=$!
while true; do
  docker-entrypoint.sh "$@"
  code=$?
  echo "PGMON exit $code" >&2
  dmesg 2>&1 | tail -60 | sed 's/^/PGMON dmesg /' >&2
  if [ -n "$PGMON_WAL" ] && [ "$code" != 0 ] && [ ! -e /tmp/pgmon-wal ]; then
    # Once per boot, after postgres fails: the control file and the WAL from the latest
    # checkpoint's redo point (PGWAL lines), to tell which records recovery replays.
    touch /tmp/pgmon-wal
    pgdata=${PGDATA:-/var/lib/postgresql/data}
    gosu postgres pg_controldata "$pgdata" 2>&1 | sed 's/^/PGWAL control /' >&2
    redo=$(gosu postgres pg_controldata "$pgdata" 2>/dev/null | awk -F': *' '/REDO location/ {print $2}')
    gosu postgres pg_waldump -p "$pgdata/pg_wal" -s "$redo" -n "${PGMON_WAL}" 2>&1 | cut -c1-240 | sed 's/^/PGWAL /' >&2
  fi
  [ -n "$PGMON_RESTART" ] || break
  sleep "$PGMON_RESTART"
  echo "PGMON restart" >&2
done
kill $monitor 2>/dev/null
exit $code
