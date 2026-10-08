#!/usr/bin/env bash
# Maintainer build; run in nix develop on an x86_64 Linux build host.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
if test "$(uname -s)/$(uname -m)" != Linux/x86_64; then
  printf '%s\n' 'build-cli.sh requires an x86_64 Linux build host' >&2
  exit 2
fi
: "${SEEDBOX_STATIC_CC:?enter nix develop on Linux}"
: "${SEEDBOX_BUSYBOX:?enter nix develop on Linux}"
: "${SEEDBOX_MKE2FS:?enter nix develop on Linux}"
: "${SEEDBOX_BUSYBOX_SOURCE:?missing pinned BusyBox source; enter nix develop}"
: "${SEEDBOX_E2FSPROGS_SOURCE:?missing pinned e2fsprogs source; enter nix develop}"
: "${SEEDBOX_MUSL_SOURCE:?missing pinned musl source; enter nix develop}"
for source in "$SEEDBOX_BUSYBOX_SOURCE" "$SEEDBOX_E2FSPROGS_SOURCE" "$SEEDBOX_MUSL_SOURCE"; do
  test -f "$source" || { printf 'missing provisioned source: %s\n' "$source" >&2; exit 2; }
done
stage=$(mktemp -d /tmp/seedbox-linux-build.XXXXXX)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/assets" "$stage/sources"
SEEDBOX_WORK="$stage/kernel-work" SEEDBOX_KBUILD="$stage/kernel-build" SEEDBOX_ARTIFACTS="$stage/assets" bash kernel/build.sh
bash guest/build-compose.sh "$stage/assets/compose-init"
install -m 600 "$SEEDBOX_BUSYBOX" "$stage/assets/busybox"
install -m 600 "$SEEDBOX_MKE2FS" "$stage/assets/mke2fs"
# The existing kernel builder verifies its pinned source tarball before use.
cp "$stage"/kernel-work/src/linux-*.tar.xz "$stage/sources/linux.source"
cp "$SEEDBOX_BUSYBOX_SOURCE" "$stage/sources/busybox.source"
cp "$SEEDBOX_E2FSPROGS_SOURCE" "$stage/sources/e2fsprogs.source"
cp "$SEEDBOX_MUSL_SOURCE" "$stage/sources/musl.source"
source_revision=${SEEDBOX_SOURCE_REVISION:-$(git rev-parse --verify HEAD)}
source_dirty=${SEEDBOX_SOURCE_DIRTY:-0}
if test -z "${SEEDBOX_SOURCE_DIRTY+x}" && test -n "$(git --no-optional-locks status --porcelain --untracked-files=no)"; then source_dirty=1; fi
mkdir -p artifacts/linux-runtime
python3 tools/linux-runtime/package.py --assets "$stage/assets" --sources "$stage/sources" \
  --output artifacts/linux-runtime/runtime.tar.gz --revision "$source_revision" --dirty "$source_dirty"
SEEDBOX_LINUX_RUNTIME_BUNDLE="$root/artifacts/linux-runtime/runtime.tar.gz" cargo build -p seedbox --release --locked
printf 'built: %s\n' "$root/target/release/seedbox"
