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
# Discover new Python tests without maintaining per-module lists.
(cd harness/tests && python3 -m unittest discover -s . -q)
(cd python && python3 -m unittest discover -s tests -q)
# Compile hardware-feature tests without opening KVM or HVF. Default execution
# keeps hardware tests ignored, while their bodies remain type-checked.
if [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]]; then
  env -u CARGO_TARGET_DIR cargo test-vmm --all-features --no-run
  env -u CARGO_TARGET_DIR cargo test-vmm
fi
env -u CARGO_TARGET_DIR cargo test-arm64 --all-features --no-run
env -u CARGO_TARGET_DIR cargo test-arm64
python3 kernel-arm64/test_inputs.py -q
python3 crates/seedbox-arm64/test_compare.py -q
cargo test --workspace --locked --all-features --no-run
cargo test --workspace --locked
for scenario in harness/acceptance/workloads/*.yaml; do
  cargo run --quiet --locked -p seedbox -- validate "$scenario"
done
