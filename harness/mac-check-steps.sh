#!/usr/bin/env bash
# Sourceable so the timing/summary contract can be tested without builds or Nix.
set -euo pipefail
check_results=()
check_failed=0

check_step() {
  local name=$1 started result=pass log line
  shift
  started=$(date +%s)
  log=$(mktemp "${TMPDIR:-/tmp}/seedbox-mac-check-step.XXXXXX")
  echo "running: $name" >&2
  if "$@" > "$log" 2>&1; then
    result=pass
  else
    result=FAIL
    check_failed=1
  fi
  cat "$log" >&2
  printf -v line '%-8s %s %4ss' "$name" "$result" "$(($(date +%s) - started))"
  check_results+=("$line")
  while IFS= read -r line; do
    check_results+=("$line")
  done < <(grep '^phase ' "$log" || true)
  rm -f "$log"
}

check_summary() {
  printf '%s\n' "${check_results[@]}"
  return "$check_failed"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  check_step fmt cargo fmt -p seedbox --check
  check_step clippy cargo clippy -p seedbox --all-targets --locked -- -D warnings
  check_step portable bash harness/acceptance.sh portable
  check_summary
fi
