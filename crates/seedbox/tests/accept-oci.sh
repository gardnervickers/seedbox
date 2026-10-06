#!/usr/bin/env bash
# Run inside `nix develop` on the supported Linux amd64 host.
# Only the final OCI test binary runs as root. No guests, KVM, affinity, or MSRs.
set -euo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
bash guest/build-compose.sh "$scratch/compose-init"
cargo check --locked --manifest-path crates/seedbox-vmm/Cargo.toml
cargo test -p seedbox --locked
cargo clippy -p seedbox --locked --all-targets -- -D warnings
test_binary=$(cargo test -p seedbox --locked \
  --test oci --no-run --message-format=json | \
  jq -r 'select(.reason == "compiler-artifact" and .target.name == "oci" and .executable != null) | .executable')
test -n "$test_binary"
sudo -n env PATH="$PATH" "$test_binary" --ignored --nocapture --test-threads=1
