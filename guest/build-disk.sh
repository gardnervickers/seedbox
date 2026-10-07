#!/usr/bin/env bash
# Build the retained-disk probe initramfs (guest/disk.init) and, with a
# second argument, a fresh ext4 image for the fsync ledger.
# Usage: build-disk.sh INITRAMFS [EXT4_IMAGE]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
for p in blkprobe fsledger; do
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Werror -o "$tmp/$p" "$here/$p.c"
done
"$here/build-initramfs.sh" "$1" "$here/disk.init" "$tmp/blkprobe:bin/blkprobe" "$tmp/fsledger:bin/fsledger"
if [ -n "${2:-}" ]; then
  # Fixed UUID, hash seed and time: the image is a retained run input, and
  # its hash is reported as the disk's base_sha256. Defaults differ between
  # e2fsprogs versions and distributions' mke2fs.conf, so use the flake's
  # pinned mke2fs (SEEDBOX_MKE2FS, also the Compose volume formatter) with its
  # built-in profile; the image is then the same on every host for one
  # flake.lock.
  rm -f "$2"
  truncate -s 32M "$2"
  MKE2FS_CONFIG=/nonexistent E2FSPROGS_FAKE_TIME=1 "${SEEDBOX_MKE2FS:?enter nix develop}" -t ext4 -q -F -U 5eedb0c5-0000-4000-8000-000000000001 \
    -E hash_seed=5eedb0c5-0000-4000-8000-000000000002,lazy_itable_init=0,lazy_journal_init=0,root_owner=0:0 "$2"
  sha256sum "$2"
fi
