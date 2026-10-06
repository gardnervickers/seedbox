#!/usr/bin/env bash
# Linux KVM acceptance. This DOES start guests and temporarily set the scoped
# MSR workaround through the Rust executor. Run only after OCI acceptance.
# Usage (Nix shell): bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU OUTPUT [lifecycle|faults|isolation] [shared-kernel|machine-per-service]
set -euo pipefail
test "$(uname -s)" = Linux
test "$(uname -m)" = x86_64
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
kernel=$(realpath "$1")
cpu=$2
layout=${5:-shared-kernel}
output=$(realpath -m "$3")
case "$cpu" in ''|*[!0-9]*) echo 'CPU must be an integer' >&2; exit 2 ;; esac
test -f "$kernel"
test ! -e "$output"
mkdir "$output"
cargo build --release -p seedbox --locked
cargo build-vmm
bash guest/build-compose.sh "$output/compose-init"
if test "${4:-}" = isolation; then
  mkdir "$output/isolation"
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror guest/isolation-check.c -o "$output/isolation/check"
  cat > "$output/isolation/Dockerfile" <<'DOCKERFILE'
FROM scratch
COPY check /check
ENTRYPOINT ["/check"]
DOCKERFILE
  cat > "$output/isolation/compose.yaml" <<'COMPOSE'
services:
  check:
    build: .
    command: [check]
  fill:
    build: .
    command: [fill]
x-seedbox:
  version: 1
  seed: 1009
  check: {service: check, exit_code: 0}
  limits: {simulated_time: 30s, wall_time: 5m}
COMPOSE
fi
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
  exec sudo -n env PATH="$PATH" \
    python3 examples/faults/run.py --seedbox "$cli" --vmm "$vmm" \
    --kernel "$kernel" --guest-init "$output/compose-init" --cpu "$cpu" \
    --state "$output/state" --output "$output" --layout "$layout" \
    --negative-guest-init "$output/compose-init-missing" \
    --rejecting-guest-init "$output/compose-init-rejecting" \
    --delayed-guest-init "$output/compose-init-delayed"
fi
"$cli" init "$output/redis" --template redis
"$cli" init "$output/etcd" --template etcd
# Root owns the inputs it will execute and the writable artifact store. Build
# outputs above remain user-owned and are copied by the content store before use.
sudo -n env PATH="$PATH" \
  bash crates/seedbox/tests/runtime-cases.sh "$cli" "$vmm" "$kernel" "$cpu" "$output" "${4:-}" "$layout"
