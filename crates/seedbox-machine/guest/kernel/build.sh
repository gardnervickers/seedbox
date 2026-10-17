#!/usr/bin/env bash
# Build the crate-owned standard x86-64 TCG kernel, independently of PV patching.
set -euo pipefail
source_archive=$1
fragment=$2
output=$3
mkdir source build "$output"
tar -xf "$source_archive" -C source --strip-components=1
make() { command make -s -C source O="$PWD/build" ARCH=x86_64 LLVM=1 HOSTCC="${HOST_CC:-cc}" HOSTCXX="${HOST_CXX:-c++}" HOSTLD="${HOST_LD:-ld}" "$@"; }
make defconfig
source/scripts/kconfig/merge_config.sh -m -O "$PWD/build" build/.config "$fragment"
make olddefconfig
# Verify requested built-in facilities survived dependency resolution. These
# checks validate this build recipe; they do not gate runtime host admission.
while IFS= read -r option; do
  case "$option" in
    CONFIG_*=y|CONFIG_*=\"*\") grep -Fxq "$option" build/.config || { echo "kernel option not retained: $option" >&2; exit 1; } ;;
    CONFIG_*=n) name=${option%=n}; if grep -Eq "^$name=[ym]$" build/.config; then echo "kernel option not disabled: $name" >&2; exit 1; fi ;;
  esac
done < "$fragment"
make -j"${NIX_BUILD_CORES:-4}" bzImage
cp build/arch/x86/boot/bzImage "$output/kernel"
cp build/.config "$output/kernel.config"
cp build/System.map "$output/System.map"
