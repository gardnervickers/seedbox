#!/usr/bin/env bash
# Usage: build-netns.sh OUTPUT ETCD_BINARY [single|multi]
# Run in the project's Linux shell. Both layouts embed the same iproute2 closure.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$(realpath -m "$1"); etcd=$(realpath "$2"); variant=${3:-single}
case "$variant" in single) init="$here/single.init" ;; multi) init="$here/cluster.init" ;; *) exit 2 ;; esac
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
"$here/build-client.sh" "$stage/client"
"$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -o "$stage/netprobe" "$here/netprobe.c"
extras=("$etcd:bin/etcd" "$stage/client:bin/client" "$stage/netprobe:bin/netprobe" "$here/network.sh:bin/network.sh" "$here/members.sh:bin/members.sh" "$SEEDBOX_IP:bin/ip" "$SEEDBOX_TC:bin/tc")
# Absolute interpreter/library/plugin references require the complete runtime closure.
while read -r path; do extras+=("$path:${path#/}"); done < <(nix-store -q --requisites "$(dirname "$(dirname "$SEEDBOX_IP")")")
"$here/build-initramfs.sh" "$out" "$init" "${extras[@]}"
