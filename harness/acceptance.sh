#!/usr/bin/env bash
# Portable source checks only. Hardware scenarios are ordinary seedbox run commands.
set -euo pipefail
if [[ ${1:-portable} != portable || $# -gt 1 ]]; then
  echo 'Usage: harness/acceptance.sh portable; see README.md for seedbox run commands' >&2
  exit 2
fi
cd "$(dirname "$0")/.."
while IFS= read -r script; do
  bash -n "$script"
done < <(git ls-files '*.sh' ':!:research/**')
tests=(test_msr54 test_scenarios)
if [[ $(uname -s) == Linux ]]; then tests+=(test_batch); fi
(cd harness/tests && python3 -m unittest -q "${tests[@]}")
(cd python && python3 -m unittest discover -s tests -q)
if [[ $(uname -s) == Linux ]]; then cargo test-vmm; fi
cargo test --workspace --locked
for scenario in harness/acceptance/workloads/*.yaml; do
  cargo run --quiet --locked -p seedbox -- validate "$scenario"
done
