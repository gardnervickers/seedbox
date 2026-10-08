#!/usr/bin/env bash
# Build in a Linux arm64 environment with a native Linux filesystem.
set -euo pipefail
application=false
if [ "${1:-}" = --application ]; then application=true; shift; fi
if [ "$#" -ne 1 ]; then echo "usage: $0 [--application] OUTPUT_DIRECTORY" >&2; exit 2; fi
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != aarch64 ]; then
  echo "build inside Linux arm64; Cargo release builds provide this container environment" >&2; exit 2
fi
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$1"
artifacts="$(cd "$1" && pwd)"
build="${SEEDBOX_ARM64_BUILD:-/var/tmp/seedbox-arm64-build-$(id -u)}"
mkdir -p "$build"
build="$(cd "$build" && pwd)"
static_cc="${SEEDBOX_STATIC_CC:-musl-gcc}"
for tool in curl sha256sum tar xz make gcc "$static_cc" python3 objdump flex bison bc perl findmnt ld ar nm objcopy readelf; do
  command -v "$tool" >/dev/null || { echo "missing $tool; use the project assets development shell" >&2; exit 2; }
done
case "$(findmnt -n -o FSTYPE -T "$build")" in
  virtiofs|9p|nfs*|fuse*) echo "build dir must use a native Linux filesystem: $build" >&2; exit 2 ;;
esac
read -r version want < <(python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); print(s["version"],s["sha256"])' "$here/../kernel/source.json")
tarball="$build/linux-$version.tar.xz"
if [ ! -f "$tarball" ] && [ -n "${SEEDBOX_KERNEL_SOURCE:-}" ]; then
  cp "$SEEDBOX_KERNEL_SOURCE" "$tarball"
fi
if [ ! -f "$tarball" ]; then
  curl --fail --location "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz" -o "$tarball.part"
  mv "$tarball.part" "$tarball"
fi
printf '%s  %s\n' "$want" "$tarball" | sha256sum --check --status
# A fresh unique tree: no unscoped rm -rf or stale .config/object reuse.
run="$(mktemp -d "$build/run.XXXXXX")"
printf '%s\n' "$run" > "$artifacts/build-directory.txt"
# Catch static libc/compiler problems before the expensive kernel build.
"$static_cc" -static -Os -fno-ident -ffile-prefix-map="$here"=/seedbox/kernel-arm64 -Wl,--build-id=none -Wall -Wextra -Werror "$here/init.c" -o "$run/init"
if "$application"; then
  "$static_cc" -static -O2 -Wall -Wextra -Werror -ffile-prefix-map="$here/../guest"=/seedbox/guest \
    -Wl,--build-id=none "$here/../guest/compose-init.c" -o "$run/compose-init"
fi
python3 "$here/build-initramfs.py" "$run/init" "$run/init.cpio"
tar -xf "$tarball" -C "$run"
src="$run/linux-$version"
out="$run/out"
mkdir -p "$out"
apply_arguments=()
if "$application"; then apply_arguments+=(--application); fi
python3 "$here/apply.py" "$src" "${apply_arguments[@]}"
export ARCH=arm64
export KBUILD_BUILD_USER=seedbox KBUILD_BUILD_HOST=seedbox KBUILD_BUILD_VERSION=1
export KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970' SOURCE_DATE_EPOCH=0
# This C-only guest does not use Rust. Kconfig probes rustc even with RUST=n;
# pin it absent so installing host Cargo cannot change the resolved config.
# Use a make command-line override: the kernel Makefile overrides the environment.
make -s -C "$src" O="$out" RUSTC=false allnoconfig
fragment="$run/config.fragment"
cp "$here/config.fragment" "$fragment"
if "$application"; then cat "$here/application.fragment" >> "$fragment"; fi
(cd "$run" && "$src/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" "$fragment")
make -s -C "$src" O="$out" RUSTC=false olddefconfig
python3 "$here/check-config.py" "$src" "$fragment" "$out/.config"
make -s -C "$src" O="$out" RUSTC=false -j"${BUILD_JOBS:-4}" Image
# Audit executable code, including the vDSO, before publishing runnable inputs.
python3 "$here/audit.py" "$out/vmlinux" "$out/arch/arm64/kernel/vdso/vdso.so.dbg" | tee "$artifacts/counter-audit.txt"
cp "$out/arch/arm64/boot/Image" "$artifacts/Image"
cp "$out/.config" "$artifacts/kernel.config"
cp "$run/init.cpio" "$artifacts/init.cpio"
if "$application"; then cp "$run/compose-init" "$artifacts/compose-init"; fi
(cd "$artifacts" && sha256sum Image init.cpio kernel.config > SHA256SUMS)
cat "$artifacts/SHA256SUMS"
printf 'build tree: %s\n' "$run"
printf 'retained inputs: %s\n' "$artifacts"
