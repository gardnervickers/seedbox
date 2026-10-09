#!/bin/sh
set -eu
/docker-entrypoint.sh nginx -g 'daemon off;' &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
ready=false
attempt=0
while [ "$attempt" -lt 100 ]; do
  if wget -q -O /tmp/response http://127.0.0.1/; then ready=true; break; fi
  attempt=$((attempt + 1))
  sleep 0.1
done
"$ready"
grep -q 'Welcome to nginx' /tmp/response
kill -QUIT "$server"
wait "$server"
trap - EXIT
echo 'nginx request passed; access log goes through /dev/stdout'
