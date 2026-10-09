#!/usr/bin/env bash
set -euo pipefail
docker-entrypoint.sh postgres &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
ready=false
for ((attempt=0; attempt<200; attempt++)); do
  if pg_isready -h 127.0.0.1 -U postgres; then ready=true; break; fi
  sleep 0.1
done
"$ready"
test "$(psql -h 127.0.0.1 -U postgres -Atc 'select 6 * 7')" = 42
test "$(psql -h 127.0.0.1 -U postgres -Atc 'show dynamic_shared_memory_type')" = posix
kill "$server"
wait "$server"
trap - EXIT
echo 'Postgres initdb and query passed'
