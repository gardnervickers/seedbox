#!/usr/bin/env bash
# Explicit hardware acceptance; run inside nix develop ./sim.
# Usage: accept-runtime.sh KERNEL CPU NEW_OUTPUT_DIRECTORY
set -euo pipefail
test "$(uname -s)" = Linux
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
kernel=$(realpath "$1"); cpu=$2; output=$(realpath -m "$3")
mkdir "$output"
cargo build --release --manifest-path sim/vmm/Cargo.toml
cargo test --release --locked --manifest-path sim/seedbox/Cargo.toml --test runtime --no-run --message-format=json > "$output/build.jsonl"
bash sim/guest/build-compose.sh "$output/compose-init"
test_binary=$(jq -r 'select(.reason=="compiler-artifact" and .target.name=="runtime") | .executable // empty' "$output/build.jsonl")
test -n "$test_binary"
exec sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" \
  SEEDBOX_TEST_KERNEL="$kernel" SEEDBOX_TEST_CPU="$cpu" SEEDBOX_TEST_OUTPUT="$output" \
  SEEDBOX_TEST_VMM="$root/sim/vmm/target/release/seedbox-vmm" SEEDBOX_TEST_INIT="$output/compose-init" \
  "$test_binary" --ignored --nocapture --test-threads=1
