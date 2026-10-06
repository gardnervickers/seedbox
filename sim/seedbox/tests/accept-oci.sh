#!/usr/bin/env bash
# Run inside `nix develop ./sim` on the supported Linux amd64 host.
# Only the final OCI test binary runs as root. No guests, KVM, affinity, or MSRs.
set -euo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
bash sim/guest/build-compose.sh "$scratch/compose-init"
cargo check --manifest-path sim/vmm/Cargo.toml
cargo test --manifest-path sim/seedbox/Cargo.toml --locked
cargo clippy --manifest-path sim/seedbox/Cargo.toml --locked --all-targets -- -D warnings
test_binary=$(cargo test --manifest-path sim/seedbox/Cargo.toml --locked \
  --test oci --no-run --message-format=json | \
  jq -r 'select(.reason == "compiler-artifact" and .target.name == "oci" and .executable != null) | .executable')
test -n "$test_binary"
sudo -n env PATH="$PATH" "$test_binary" --ignored --nocapture --test-threads=1
