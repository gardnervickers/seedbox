#!/usr/bin/env bash
# Explicit ARM Linux experiment. No default/runtime selection or asset build.
set -euo pipefail
cd "$(dirname "$0")/../../.."
export RUSTFLAGS="${RUSTFLAGS:-} -C panic=abort"
cargo rustc --locked -p seedbox-executor --example linked-proof --features linux-proof \
  --release -j 2 -- -C link-arg=-nostartfiles -C link-arg=-static -C link-arg=-no-pie
