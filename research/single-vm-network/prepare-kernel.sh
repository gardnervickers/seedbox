#!/usr/bin/env bash
# Prepare private inputs; shared tree is never edited or hard-linked.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"; mkdir -p build artifacts results
shared=${SEEDBOX_SHARED:-$HOME/seedbox-sim}
version=6.12.112
if [ ! -d build/linux-$version ]; then
 if [ -d "$shared/build/linux-$version" ] && cmp -s sim/kernel/seedbox.c "$shared/build/linux-$version/arch/x86/kernel/seedbox.c" && cmp -s sim/kernel/seedbox.h "$shared/build/linux-$version/arch/x86/include/asm/seedbox.h"; then
  cp -a --reflink=auto "$shared/build/linux-$version" build/
  cp -a --reflink=auto "$shared/build/kernel-out" build/
 else
  tarball="$shared/src/linux-$version.tar.xz"
  printf '%s  %s\n' 164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03 "$tarball" | sha256sum -c --quiet
  tar -xf "$tarball" -C build
  python3 sim/kernel/apply.py "build/linux-$version"
  mkdir -p build/kernel-out
  make -s -C "build/linux-$version" O="$root/build/kernel-out" allnoconfig
  while read -r state option; do
   case "$state" in
    y) "build/linux-$version/scripts/config" --file build/kernel-out/.config --enable "$option" ;;
    n) "build/linux-$version/scripts/config" --file build/kernel-out/.config --disable "$option" ;;
   esac
  done < sim/kernel/config.options
  "build/linux-$version/scripts/config" --file build/kernel-out/.config --set-val SERIAL_8250_NR_UARTS 2 --set-val SERIAL_8250_RUNTIME_UARTS 2
 fi
fi
# For a running warm build, call this before build-kernel.sh to capture initial state.
[ -e results/kernel-initial.config ] || cp build/kernel-out/.config results/kernel-initial.config
sha256sum sim/kernel/* build/linux-$version/arch/x86/kernel/seedbox.c build/linux-$version/arch/x86/include/asm/seedbox.h > results/kernel-source-inputs.txt
