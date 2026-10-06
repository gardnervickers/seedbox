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
  # Deliberately restore the former chroot implementation in an isolated
  # negative-control binary. The fixture must detect escape to its ancestors.
  python3 - "$output/compose-init-chroot.c" "$output/compose-init-bootstrap-stdin.c" <<'PY'
from pathlib import Path
import sys
source = Path("guest/compose-init.c").read_text()
pivot = 'syscall(SYS_pivot_root, ".", ".") || umount2(".", MNT_DETACH)'
assert source.count(pivot) == 1
Path(sys.argv[1]).write_text(source.replace(pivot, 'chroot(".")'))
private_stdin = '''    int input = open("/dev/null", O_RDONLY|O_CLOEXEC);
    if (input < 0 || dup2(input, STDIN_FILENO) < 0 || close(input)) die("private service stdin");'''
assert source.count(private_stdin) == 1
Path(sys.argv[2]).write_text(source.replace(private_stdin, '    /* Negative control: retain bootstrap stdin. */'))
PY
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -Iguest \
    "$output/compose-init-chroot.c" -o "$output/compose-init-chroot"
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -Iguest \
    "$output/compose-init-bootstrap-stdin.c" -o "$output/compose-init-bootstrap-stdin"
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
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror guest/io-check.c -o "$output/io-check"
  python3 - "$output/compose-init-inherited-io.c" <<'PY'
from pathlib import Path
import sys
source = Path("guest/compose-init.c").read_text()
clear = '    if (ioperm(0, 65536, 0)) die("clear service I/O permissions");'
assert source.count(clear) == 1
Path(sys.argv[1]).write_text(source.replace(clear, '    /* Negative control: inherit supervisor I/O permissions. */'))
PY
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -Iguest \
    "$output/compose-init-inherited-io.c" -o "$output/compose-init-inherited-io"
  rm "$output/compose-init-inherited-io.c"
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
    --io-check "$output/io-check" --inherited-io-guest-init "$output/compose-init-inherited-io" \
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
