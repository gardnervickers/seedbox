#!/usr/bin/env bash
# Usage: build-initramfs.sh OUTPUT INIT_SCRIPT [SRC:DEST ...]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$1"; init="$2"; shift 2
root="$(mktemp -d)"
trap 'chmod -R u+w "$root"; rm -rf "$root"' EXIT
mkdir -p "$root"/{bin,sbin,usr/bin,usr/sbin,dev,proc,sys,tmp,etc,data}
cp "$SEEDBOX_BUSYBOX" "$root/bin/busybox"
"$SEEDBOX_STATIC_CC" -O2 -static -o "$root/bin/sbx" "$here/sbx.c"
for extra in "$@"; do
  src="${extra%%:*}"; dest="${extra#*:}"
  mkdir -p "$root/$(dirname "$dest")"
  cp -a "$src" "$root/$dest"
done
cp "$init" "$root/init"
chmod 755 "$root/init" "$root/bin/sbx"
# Fixed ordering and timestamps make the archive reproducible.
(cd "$root" && find . -print0 | LC_ALL=C sort -z | xargs -0 touch -h -d @0 && find . -print0 | LC_ALL=C sort -z | cpio --null -o --format=newc --reproducible --quiet) > "$out"
sha256sum "$out"
