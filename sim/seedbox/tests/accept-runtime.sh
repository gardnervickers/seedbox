#!/usr/bin/env bash
# Linux KVM acceptance. This DOES start guests and temporarily set the scoped
# MSR workaround through the Rust executor. Run only after OCI acceptance.
# Usage (Nix shell): bash sim/seedbox/tests/accept-runtime.sh KERNEL CPU OUTPUT [lifecycle]
set -euo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
kernel=$(realpath "$1")
cpu=$2
output=$(realpath -m "$3")
case "$cpu" in ''|*[!0-9]*) echo 'CPU must be an integer' >&2; exit 2 ;; esac
test -f "$kernel"
test ! -e "$output"
mkdir "$output"
cargo build --release --manifest-path sim/seedbox/Cargo.toml --locked
cargo build --release --manifest-path sim/vmm/Cargo.toml
bash sim/guest/build-compose.sh "$output/compose-init"
cli="$root/sim/seedbox/target/release/seedbox"
vmm="$root/sim/vmm/target/release/seedbox-vmm"
"$cli" init "$output/redis" --template redis
"$cli" init "$output/etcd" --template etcd
# Root owns the inputs it will execute and the writable artifact store. Build
# outputs above remain user-owned and are copied by the content store before use.
sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" \
  bash sim/seedbox/tests/runtime-cases.sh "$cli" "$vmm" "$kernel" "$cpu" "$output" "${4:-}"
