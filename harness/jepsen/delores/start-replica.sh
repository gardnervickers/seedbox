#!/bin/sh
# Seed the virtual-log metadata, then serve: compose's one-shot `init` service,
# folded into every replica. `docker-kv init` creates the view register with a
# conditional PUT (If-None-Match: *), so exactly one replica's attempt seeds it
# and every other attempt, and every later restart, finds it present ("already
# exists"). Until the bucket exists and S3 answers, init fails and is retried.
while :; do
  out=$(docker-kv init 2>&1)
  status=$?
  printf '%s\n' "$out" >&2
  [ "$status" -eq 0 ] && break
  case "$out" in
    *"already exists"*) break ;;
  esac
  sleep 1
done
exec docker-kv replica
