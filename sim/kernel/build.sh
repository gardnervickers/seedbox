#!/usr/bin/env bash
# Build the Seedbox guest kernel from a clean pinned Linux tree.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="${SEEDBOX_WORK:-$HOME/seedbox-sim}"
mkdir -p "$work/src"
version=6.12.112
tarball="$work/src/linux-$version.tar.xz"
# Separate build and output directories let a new kernel be built while
# runs use the current one.
build="${SEEDBOX_KBUILD:-$work/build}"
artifacts="${SEEDBOX_ARTIFACTS:-$work/artifacts}"
want=164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03
[ -f "$tarball" ] || curl -sSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz" -o "$tarball"
echo "$want  $tarball" | sha256sum -c --quiet
src="$build/linux-$version"
out="$build/kernel-out"
rm -rf "$src"
mkdir -p "$build" "$out"
tar -xf "$tarball" -C "$build"
python3 "$here/apply.py" "$src"
make -s -C "$src" O="$out" allnoconfig
while read -r state option; do
  case "$state" in
    y) "$src/scripts/config" --file "$out/.config" --enable "$option" ;;
    n) "$src/scripts/config" --file "$out/.config" --disable "$option" ;;
  esac
done < "$here/config.options"
"$src/scripts/config" --file "$out/.config" --set-val SERIAL_8250_NR_UARTS 2 --set-val SERIAL_8250_RUNTIME_UARTS 2
make -s -C "$src" O="$out" olddefconfig
# Fail if a requested option did not survive dependency resolution.
while read -r state option; do
  if [ "$state" = y ] && ! grep -q "^CONFIG_$option=y" "$out/.config"; then
    echo "option $option not enabled" >&2; exit 1
  fi
done < "$here/config.options"
export KBUILD_BUILD_USER=seedbox KBUILD_BUILD_HOST=seedbox KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970' SOURCE_DATE_EPOCH=0
make -s -C "$src" O="$out" -j"${BUILD_JOBS:-16}" bzImage
mkdir -p "$artifacts"
cp "$out/arch/x86/boot/bzImage" "$artifacts/bzImage"
cp "$out/.config" "$artifacts/kernel.config"
sha256sum "$artifacts/bzImage" "$artifacts/kernel.config"
