#!/usr/bin/env bash
# Emit a URL-safe local flake reference without fetching or copying submodules.
set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

root=$(git -C "${1:-.}" rev-parse --show-toplevel) ||
  fail 'cannot find Seedbox checkout; run this helper from its Git worktree'
# Shell quoting is not URL encoding: #, ? and % have flake-reference syntax.
# Work bytewise for UTF-8 paths too, without a pre-Nix Python dependency.
LC_ALL=C
encoded=
for ((i = 0; i < ${#root}; i++)); do
  byte=${root:i:1}
  case "$byte" in
    [a-zA-Z0-9/._~-]) encoded+=$byte ;;
    *) printf -v escaped '%%%02X' "'$byte"; encoded+=$escaped ;;
  esac
done
printf 'git+file://%s\n' "$encoded"
