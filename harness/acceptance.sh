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
while IFS= read -r script; do
  bash -n "$script"
done < <(git ls-files '*.sh' ':!:research/**')
tests=(test_msr54 test_scenarios test_race_acceptance test_smoke test_mac_check test_host_plan test_host_pairs test_host_check)
if [[ $(uname -s) == Linux ]]; then tests+=(test_batch); fi
(cd harness/tests && python3 -m unittest -q "${tests[@]}")
(cd python && python3 -m unittest discover -s tests -q)
# The portable pre-check may share a workspace cache. Keep the standalone VMM
# in its prescribed crates/seedbox-vmm/target directory.
if [[ $(uname -s) == Linux ]]; then env -u CARGO_TARGET_DIR cargo test-vmm; fi
cargo test --workspace --locked
for scenario in harness/acceptance/workloads/*.yaml; do
  cargo run --quiet --locked -p seedbox -- validate "$scenario"
done
