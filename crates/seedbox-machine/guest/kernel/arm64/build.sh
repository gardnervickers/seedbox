#!/usr/bin/env bash
# Build only the crate-owned ABI3 kernel; helper/application archives are separate.
set -euo pipefail
archive=$1
inputs=$2
output=$3
mkdir source build "$output"
tar -xf "$archive" -C source --strip-components=1
python3 "$inputs/apply.py" source
export ARCH=arm64
make -s -C source O="$PWD/build" HOSTCC="$HOST_CC" HOSTCXX="$HOST_CXX" RUSTC=false allnoconfig
source/scripts/kconfig/merge_config.sh -m -O "$PWD/build" build/.config "$inputs/config.fragment"
make -s -C source O="$PWD/build" HOSTCC="$HOST_CC" HOSTCXX="$HOST_CXX" RUSTC=false olddefconfig
python3 "$inputs/check-config.py" source "$inputs/config.fragment" build/.config
make -s -C source O="$PWD/build" HOSTCC="$HOST_CC" HOSTCXX="$HOST_CXX" RUSTC=false -j"${NIX_BUILD_CORES:-4}" Image
python3 "$inputs/audit.py" --objdump "$TARGET_OBJDUMP" build/vmlinux build/arch/arm64/kernel/vdso/vdso.so.dbg | tee "$output/counter-audit.txt"
cp build/arch/arm64/boot/Image "$output/kernel"
cp build/.config "$output/kernel.config"
cp build/System.map "$output/System.map"
mkdir -p "$output/licenses"
cp source/COPYING "$output/licenses/COPYING"
cp source/LICENSES/preferred/GPL-2.0 "$output/licenses/GPL-2.0"
