#!/bin/bash
# One Galera node under Seedbox. Arguments: extra mariadbd options (the node's
# address and name, the flush setting). The data directory is on a volume.
#
# Every boot, the node reports its state to the oracle and waits for the oracle to
# say whether it bootstraps a new cluster or joins one, as galera_new_cluster and
# galera_recovery would under systemd:
#   1. On first boot, initialize the data directory (mariadb-install-db, then a
#      temporary server without wsrep or networking adds root@'%' with an empty
#      password). Otherwise recover the last committed position from InnoDB with
#      `mariadbd --wsrep-recover`, which must run here, inside the node's guest.
#   2. Listen on port 4599 for one connection, send one JSON line
#      {"node", "fresh", "uuid", "seqno", "grastate_uuid", "grastate_seqno",
#      "safe_to_bootstrap"}, and read one command line: `bootstrap` or `join`.
#   3. Start mariadbd: `bootstrap` sets safe_to_bootstrap and adds
#      --wsrep-new-cluster; both pass the recovered --wsrep-start-position.
# Lines starting with GALMON go to stderr: the boot steps, and every 10 s guest
# memory, the process count and the data size.
set -u
datadir=/var/lib/mysql/data
node=${GALERA_NODE:-$(hostname)}
log() { echo "GALMON $*" >&2; }

mkdir -p "$datadir" /run/mysqld
chown mysql:mysql "$datadir" /run/mysqld
(
  while sleep 10; do
    mem=""
    while read -r key value _; do
      case $key in
        MemTotal:|MemAvailable:|Shmem:|Committed_AS:) mem="$mem$key$value " ;;
      esac
    done < /proc/meminfo
    procs=$(ls -d /proc/[0-9]* | wc -l)
    data=$(du -sk "$datadir" 2>/dev/null | cut -f1)
    read -r uptime _ < /proc/uptime
    log "t=$uptime procs=$procs data_kb=${data:-0} $mem"
  done
) &

fresh=false
uuid=""
seqno=-1
if [ ! -d "$datadir/mysql" ]; then
  fresh=true
  log "initializing $datadir"
  mariadb-install-db --user=mysql --datadir="$datadir" --skip-test-db \
    --auth-root-authentication-method=normal >/tmp/install.log 2>&1 \
    || { cat /tmp/install.log >&2; exit 1; }
  mariadbd --user=mysql --datadir="$datadir" --wsrep-on=OFF --skip-networking \
    --socket=/run/mysqld/init.sock --log-error=/tmp/init.err &
  init=$!
  for _ in $(seq 120); do
    mariadb-admin --socket=/run/mysqld/init.sock ping >/dev/null 2>&1 && break
    sleep 1
  done
  mariadb --socket=/run/mysqld/init.sock -uroot -e \
    "CREATE USER 'root'@'%' IDENTIFIED BY ''; GRANT ALL ON *.* TO 'root'@'%' WITH GRANT OPTION; FLUSH PRIVILEGES;" \
    || { cat /tmp/init.err >&2; exit 1; }
  mariadb-admin --socket=/run/mysqld/init.sock -uroot shutdown
  wait $init
  sync
  log "initialized"
else
  log "recovering"
  mariadbd --user=mysql "$@" --wsrep-recover --log-error=/tmp/recover.err >/dev/null 2>&1
  while read -r line; do log "recover: $line"; done < <(grep -E "WSREP: (Recovered|.*position)" /tmp/recover.err)
  position=$(grep -o "Recovered position: .*" /tmp/recover.err | tail -1 | cut -d' ' -f3)
  if [ -n "$position" ]; then
    uuid=${position%%:*}
    seqno=${position##*:}
  fi
fi

grastate_uuid=""
grastate_seqno=-1
safe=0
if [ -f "$datadir/grastate.dat" ]; then
  while read -r key value _; do
    case $key in
      uuid:) grastate_uuid=$value ;;
      seqno:) grastate_seqno=$value ;;
      safe_to_bootstrap:) safe=$value ;;
    esac
  done < "$datadir/grastate.dat"
fi
case $seqno in ''|*[!0-9-]*) seqno=-1 ;; esac
case $grastate_seqno in ''|*[!0-9-]*) grastate_seqno=-1 ;; esac
case $safe in 1) ;; *) safe=0 ;; esac
report="{\"node\": \"$node\", \"fresh\": $fresh, \"uuid\": \"$uuid\", \"seqno\": $seqno, \"grastate_uuid\": \"$grastate_uuid\", \"grastate_seqno\": $grastate_seqno, \"safe_to_bootstrap\": $safe}"
log "report $report"

command=""
while [ "$command" != bootstrap ] && [ "$command" != join ]; do
  # One connection: send the report, then wait (up to an hour) for the reply.
  command=$(printf '%s\n' "$report" | socat -t 3600 TCP-LISTEN:4599,reuseaddr STDIO 2>/dev/null | head -1 | tr -d '\r')
done
log "command $command"

start=()
if [ -n "$uuid" ] && [ "$seqno" -ge 0 ]; then
  start=("--wsrep-start-position=$uuid:$seqno")
fi
if [ "$command" = bootstrap ]; then
  if [ -f "$datadir/grastate.dat" ]; then
    sed -i 's/^safe_to_bootstrap: 0/safe_to_bootstrap: 1/' "$datadir/grastate.dat"
  fi
  exec mariadbd --user=mysql "$@" "${start[@]}" --wsrep-new-cluster
fi
exec mariadbd --user=mysql "$@" "${start[@]}"
