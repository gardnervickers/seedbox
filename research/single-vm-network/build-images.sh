#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"; results=${RESULTS_DIR:-results}; mkdir -p artifacts "$results"
shared=${SEEDBOX_SHARED:-$HOME/seedbox-sim}
export CARGO_BUILD_JOBS=1 GOFLAGS=-p=1 GOMAXPROCS=1
(cd sim/vmm && cargo build --release)
iproute=$(dirname "$(dirname "$SEEDBOX_IP")")
printf '%s\n' "$iproute" > "$results/iproute-path.txt"
nix-store -q --requisites "$iproute" > "$results/iproute-closure.txt"
for variant in single multi; do
 sim/guest/build-netns.sh "$root/artifacts/$variant.cpio" "$shared/src/etcd/v3.5.2/etcd" "$variant"
done
python3 research/single-vm-network/prepare.py --root "$root"
sha256sum artifacts/bzImage artifacts/kernel.config artifacts/single.cpio artifacts/multi.cpio sim/vmm/target/release/seedbox-vmm "$shared/src/etcd/v3.5.2/etcd" "$iproute/bin/ip" "$iproute/bin/tc" > "$results/input-sha256.txt"
