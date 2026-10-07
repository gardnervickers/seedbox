#!/usr/bin/env bash
# Sourceable so the timing/summary contract can be tested without builds or Nix.
set -euo pipefail
precheck_results=()
precheck_failed=0

precheck_step() {
  local name=$1 started=$SECONDS result=PASS
  shift
  echo "running: $name" >&2
  if "$@" >&2; then
    result=PASS
  else
    result=FAIL
    precheck_failed=1
  fi
  precheck_results+=("$result $name $((SECONDS - started))s")
}

precheck_summary() {
  printf '%s\n' "${precheck_results[@]}"
  return "$precheck_failed"
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  precheck_step fmt cargo fmt -p seedbox --check
  precheck_step clippy cargo clippy -p seedbox --all-targets --locked -- -D warnings
  precheck_step workspace cargo test --workspace --locked
  precheck_step portable bash harness/acceptance.sh portable
  precheck_step harness-python python3 -m unittest discover harness/tests
  precheck_step python python3 -m unittest discover python/tests
  precheck_summary
fi
