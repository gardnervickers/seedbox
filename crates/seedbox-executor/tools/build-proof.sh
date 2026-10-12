#!/usr/bin/env bash
# Run from the checkout's nix develop shell on the target Linux ISA.
# This opt-in ELF has its own _start and raw Linux ABI; no libc or ELF interpreter.
set -euo pipefail
cd "$(dirname "$0")/../../.."
export RUSTFLAGS="${RUSTFLAGS:-} -C panic=abort"
cargo rustc -p seedbox-executor --example linux-proof --features linux-proof \
  --release -j 2 -- -C link-arg=-nostartfiles -C link-arg=-static -C link-arg=-no-pie
