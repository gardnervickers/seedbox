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
  [[ -f $script ]] || continue
  bash -n "$script"
done < <(git ls-files --cached --others --exclude-standard '*.sh' ':!:research/**' | sort -u)
# New unittest modules are discovered without editing this runner.
(cd harness/tests && python3 -m unittest discover -s . -q)
(cd python && python3 -m unittest discover -s tests -q)
# No device probing or hardware features in the portable suite.
if [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]]; then
  env -u CARGO_TARGET_DIR cargo test-vmm
fi
# ARM policy/mock tests are portable, including on Linux x86-64.
env -u CARGO_TARGET_DIR cargo test-arm64
python3 kernel-arm64/test_inputs.py -q
python3 crates/seedbox-arm64/test_compare.py -q
cargo test --workspace --locked
for scenario in harness/acceptance/workloads/*.yaml; do
  cargo run --quiet --locked -p seedbox -- validate "$scenario"
done
