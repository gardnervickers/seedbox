#!/usr/bin/env bash
set -eu
task_root="$(cd "$(dirname "$0")" && pwd)"
task_cc="${SEEDBOX_STATIC_CC%/cc}/x86_64-unknown-linux-musl-cc"
mkdir -p "$task_root/busy-root"
"$task_cc" -O2 -static -fno-pie -no-pie "$task_root/busy-guest.c" -o "$task_root/busy-root/init"
cd "$task_root/busy-root"
find . -print0 | sort -z | cpio --null -o --format=newc > "$task_root/inputs/busy-initramfs"
nm -n init | rg 'seedbox_busy_(start|end)'
sha256sum init "$task_root/inputs/busy-initramfs"
