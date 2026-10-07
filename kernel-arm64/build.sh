#!/usr/bin/env bash
# Run only inside the Ubuntu arm64 VM. All heavy work stays on its Linux disk.
set -euo pipefail
if [ "$#" -ne 1 ]; then echo "usage: $0 OUTPUT_DIRECTORY" >&2; exit 2; fi
if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != aarch64 ]; then
  echo "build inside the Ubuntu arm64 VM; native macOS builds are unsupported" >&2; exit 2
fi
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$1"
artifacts="$(cd "$1" && pwd)"
build="${SEEDBOX_ARM64_BUILD:-/var/tmp/seedbox-arm64-build-$(id -u)}"
mkdir -p "$build"
build="$(cd "$build" && pwd)"
case "$(findmnt -n -o FSTYPE -T "$build")" in
  virtiofs|9p|nfs*|fuse*) echo "build dir must be on the VM's Linux disk: $build" >&2; exit 2 ;;
esac
for tool in curl sha256sum tar make gcc musl-gcc python3 objdump flex bison bc; do
  command -v "$tool" >/dev/null || { echo "missing $tool; see docs/arm64-backends.md for VM dependencies" >&2; exit 2; }
done
version=6.12.112
want=164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03
tarball="$build/linux-$version.tar.xz"
if [ ! -f "$tarball" ]; then
  curl --fail --location "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$version.tar.xz" -o "$tarball.part"
  mv "$tarball.part" "$tarball"
fi
printf '%s  %s\n' "$want" "$tarball" | sha256sum --check --status
# A fresh unique tree: no unscoped rm -rf or stale .config/object reuse.
run="$(mktemp -d "$build/run.XXXXXX")"
printf '%s\n' "$run" > "$artifacts/build-directory.txt"
tar -xf "$tarball" -C "$run"
src="$run/linux-$version"
out="$run/out"
mkdir -p "$out"
python3 "$here/apply.py" "$src"
export ARCH=arm64
export KBUILD_BUILD_USER=seedbox KBUILD_BUILD_HOST=seedbox KBUILD_BUILD_VERSION=1
export KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970' SOURCE_DATE_EPOCH=0
make -s -C "$src" O="$out" allnoconfig
(cd "$run" && "$src/scripts/kconfig/merge_config.sh" -m -O "$out" "$out/.config" "$here/config.fragment")
make -s -C "$src" O="$out" olddefconfig
python3 - "$here/config.fragment" "$out/.config" <<'PY'
import pathlib, sys
requested, actual = [pathlib.Path(p).read_text() for p in sys.argv[1:]]
for line in requested.splitlines():
    if not line.startswith('CONFIG_'): continue
    key, value = line.split('=', 1)
    if value == 'n':
        assert key + '=y' not in actual and key + '=m' not in actual, f'{key} unexpectedly enabled'
    else:
        assert line in actual.splitlines(), f'configuration dependency dropped {line}'
PY
make -s -C "$src" O="$out" -j"${BUILD_JOBS:-4}" Image
# Audit executable code, including the vDSO, before publishing runnable inputs.
python3 "$here/audit.py" "$out/vmlinux" "$out/arch/arm64/kernel/vdso/vdso.so.dbg" | tee "$artifacts/counter-audit.txt"
musl-gcc -static -Os -fno-ident -ffile-prefix-map="$here"=/seedbox/kernel-arm64 -Wl,--build-id=none -Wall -Wextra -Werror "$here/init.c" -o "$run/init"
python3 "$here/build-initramfs.py" "$run/init" "$run/init.cpio"
cp "$out/arch/arm64/boot/Image" "$artifacts/Image"
cp "$out/.config" "$artifacts/kernel.config"
cp "$run/init.cpio" "$artifacts/init.cpio"
(cd "$artifacts" && sha256sum Image init.cpio kernel.config > SHA256SUMS)
cat "$artifacts/SHA256SUMS"
printf 'build tree: %s\n' "$run"
printf 'retained inputs: %s\n' "$artifacts"
