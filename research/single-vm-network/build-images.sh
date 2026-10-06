#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"; mkdir -p artifacts results
shared=${SEEDBOX_SHARED:-$HOME/seedbox-sim}
export CARGO_BUILD_JOBS=1 GOFLAGS=-p=1 GOMAXPROCS=1
(cd sim/vmm && cargo build --release)
(cd research/single-vm-network/client && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -ldflags='-s -w -buildid=' -o "$root/artifacts/client" .)
"$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -o artifacts/netprobe research/single-vm-network/netprobe.c
# No daemon compilation: require substitutes or an already present binary output.
nix build ./research/single-vm-network#networkTools --max-jobs 0 --no-link --print-out-paths > results/iproute-path.txt
for candidate in $(cat results/iproute-path.txt); do
 if [ -x "$candidate/bin/ip" ]; then iproute="$candidate"; fi
done
[ -n "${iproute:-}" ] || { echo 'iproute2 binary output absent' >&2; exit 1; }
nix-store -q --requisites "$iproute" > results/iproute-closure.txt
stage=
cleanup() { if [ -n "$stage" ]; then chmod -R u+w "$stage"; rm -rf "$stage"; fi; }
trap cleanup EXIT
for variant in single multi; do
 stage=$(mktemp -d)
 mkdir -p "$stage"/{bin,sbin,usr/bin,usr/sbin,dev,proc,sys,tmp,etc,data,run,var/run,nix/store}
 cp "$SEEDBOX_BUSYBOX" "$stage/bin/busybox"
 "$SEEDBOX_STATIC_CC" -O2 -static -o "$stage/bin/sbx" sim/guest/sbx.c
 cp "$shared/src/etcd/v3.5.2/etcd" "$stage/bin/etcd"
 cp artifacts/client artifacts/netprobe "$stage/bin/"
 cp research/single-vm-network/network.sh "$stage/bin/"
 cp research/single-vm-network/"$variant.init" "$stage/init"
 # Preserve Nix's absolute ELF interpreter/library paths and tool plugins.
 while read -r closure_path; do cp -a "$closure_path" "$stage/nix/store/"; done < results/iproute-closure.txt
 cp "$iproute/bin/ip" "$stage/bin/ip"; cp "$iproute/bin/tc" "$stage/bin/tc"
 chmod 755 "$stage/init" "$stage/bin/network.sh"
 (cd "$stage" && find . -print0 | LC_ALL=C sort -z | xargs -0 touch -h -d @0 && find . -print0 | LC_ALL=C sort -z | cpio --null -o --format=newc --reproducible --quiet) > artifacts/"$variant.cpio"
 cleanup
 stage=
done
python3 research/single-vm-network/prepare.py --root "$root"
sha256sum artifacts/* sim/vmm/target/release/seedbox-vmm "$shared/src/etcd/v3.5.2/etcd" "$iproute/bin/ip" "$iproute/bin/tc" > results/input-sha256.txt
