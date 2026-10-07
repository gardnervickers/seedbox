#!/usr/bin/env bash
# Focused Linux KVM race qualification; four seeds and one retained replay.
# Usage (project Nix shell): bash crates/seedbox/tests/accept-race.sh KERNEL CPU OUTPUT
set -euo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
cd "$(dirname "$0")/../../.."
kernel=$(realpath "$1")
cpu=$2
output=$(realpath -m "$3")
case "$cpu" in ''|*[!0-9]*) echo 'CPU must be an integer' >&2; exit 2 ;; esac
test -f "$kernel"
test ! -e "$output"
mkdir "$output"
cargo build --release -p seedbox --locked
cargo build-vmm
bash guest/build-compose.sh "$output/compose-init"
cli="$PWD/target/release/seedbox"
if test -n "${CARGO_TARGET_DIR:-}"; then cli="$(realpath "$CARGO_TARGET_DIR")/release/seedbox"; fi
sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" \
  python3 crates/seedbox/tests/race-cases.py "$cli" \
  "$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" "$kernel" "$cpu" "$output"
