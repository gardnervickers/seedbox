#!/usr/bin/env bash
# Linux KVM acceptance. This DOES start guests and temporarily set the scoped
# MSR workaround through the Rust executor. Run only after OCI acceptance.
# Usage (Nix shell): bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU OUTPUT [lifecycle|faults]
# Set SEEDBOX_ACCEPT_DEV=1 to run an unpinned engine or kernel with --allow-dev-engine.
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
cargo build --release -p seedbox --locked
cargo build-vmm
bash guest/build-compose.sh "$output/compose-init"
cli="$root/target/release/seedbox"
vmm="$root/crates/seedbox-vmm/target/release/seedbox-vmm"
if test -n "${CARGO_TARGET_DIR:-}"; then
  target=$(realpath "$CARGO_TARGET_DIR")
  cli="$target/release/seedbox"
fi
if test "${4:-}" = faults; then
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -DSEEDBOX_DROP_EFFECTS \
    guest/compose-init.c -o "$output/compose-init-missing"
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -DSEEDBOX_REJECT_EFFECTS \
    guest/compose-init.c -o "$output/compose-init-rejecting"
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -DSEEDBOX_TEST_READY_DELAY_NS=4000000000ULL \
    guest/compose-init.c -o "$output/compose-init-delayed"
  exec sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" \
    python3 examples/faults/run.py --seedbox "$cli" --vmm "$vmm" \
    --kernel "$kernel" --guest-init "$output/compose-init" --cpu "$cpu" \
    --state "$output/state" --output "$output" \
    --negative-guest-init "$output/compose-init-missing" \
    --rejecting-guest-init "$output/compose-init-rejecting" \
    --delayed-guest-init "$output/compose-init-delayed" \
    ${SEEDBOX_ACCEPT_DEV:+--allow-dev-engine}
fi
"$cli" init "$output/redis" --template redis
"$cli" init "$output/etcd" --template etcd
# Root owns the inputs it will execute and the writable artifact store. Build
# outputs above remain user-owned and are copied by the content store before use.
sudo -n env PATH="$PATH" SEEDBOX_BUSYBOX="$SEEDBOX_BUSYBOX" \
  bash crates/seedbox/tests/runtime-cases.sh "$cli" "$vmm" "$kernel" "$cpu" "$output" "${4:-}"
