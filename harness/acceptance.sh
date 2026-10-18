#!/usr/bin/env bash
# Exit contract: run 0 PASS / 1 FAIL / 2 ERROR / 3 INCOMPLETE / 124 TIMEOUT;
# replay 0 MATCH / 1 MISMATCH / 2 NOT_COMPARABLE; diff 0 MATCH / 1 DIFFERENT / 2 setup.
# Portable source checks only. Hardware scenarios are ordinary seedbox run commands.
set -euo pipefail
if [[ ${1:-portable} != portable || $# -gt 1 ]]; then
  echo 'Usage: harness/acceptance.sh portable; see README.md for seedbox run commands' >&2
  exit 2
fi
cd "$(dirname "$0")/.."
timed() {
  local label=$1
  shift
  local TIMEFORMAT="timing: portable/$label: %3R seconds (user %3U, sys %3S)"
  time "$@"
}

check_shells() {
  # Check discovery itself: a failing process substitution would otherwise leave
  # the loop empty while the portable suite silently skipped every shell file.
  local scripts
  scripts=$(git ls-files --cached --others --exclude-standard '*.sh' ':!:research/**')
  while IFS= read -r script; do
    [[ -f $script ]] || continue
    bash -n "$script"
  done <<< "$scripts"
}
timed shell-syntax check_shells

# Discover new Python tests without maintaining per-module lists.
timed harness-python bash -c 'cd harness/tests && python3 -m unittest discover -s . -q'
timed client-python bash -c 'cd python && python3 -m unittest discover -s tests -q'
timed jvm-fuzz-python bash -c 'cd tools/jvm-fuzz && python3 -m unittest discover -s . -q'
# Compile hardware-feature tests without opening KVM or HVF. Default execution
# keeps hardware tests ignored, while their bodies remain type-checked.
if [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]]; then
  timed vmm-feature-build env -u CARGO_TARGET_DIR cargo test-vmm --all-features --no-run
  timed vmm-tests env -u CARGO_TARGET_DIR cargo test-vmm
fi
timed arm64-feature-build env -u CARGO_TARGET_DIR cargo test-arm64 --all-features --no-run
timed arm64-tests env -u CARGO_TARGET_DIR cargo test-arm64
timed kernel-inputs python3 kernel-arm64/test_inputs.py -q
timed arm64-compare python3 crates/seedbox-arm64/test_compare.py -q
timed workspace-feature-build cargo test --workspace --locked --all-features --no-run
timed workspace-tests cargo test --workspace --locked
validate_workloads() {
  for scenario in harness/acceptance/workloads/*.yaml harness/acceptance/long/*.yaml; do
    cargo run --quiet --locked -p seedbox -- validate "$scenario"
  done
}
timed workload-validation validate_workloads
