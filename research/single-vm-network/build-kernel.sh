#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
src="$root/build/linux-6.12.112"; out="$root/build/kernel-out"
# Cache reuse is private; validate the Seedbox ABI source against merged main.
cmp sim/kernel/seedbox.c "$src/arch/x86/kernel/seedbox.c"
cmp sim/kernel/seedbox.h "$src/arch/x86/include/asm/seedbox.h"
mkdir -p artifacts
# Relocate cached Kbuild paths; retain timestamp-preserved object/source copies.
python3 - "$out" "${SEEDBOX_SHARED:-$HOME/seedbox-sim}/build" "$root/build" <<'PY'
import pathlib,sys
root,old,new=map(str,sys.argv[1:])
for f in pathlib.Path(root).rglob('*.cmd'):
 s=f.read_text(); t=s.replace(old,new)
 if s!=t:f.write_text(t)
p=pathlib.Path(root)/'Makefile'; s=p.read_text(); p.write_text(s.replace(old,new))
PY
for option in NET_NS VETH NET_SCHED NET_SCH_NETEM NET_SCH_INGRESS NET_CLS NET_CLS_MATCHALL NET_CLS_ACT NET_ACT_GACT; do
 "$src/scripts/config" --file "$out/.config" --enable "$option"
done
make -s -C "$src" O="$out" olddefconfig
for option in NET_NS VETH NET_SCHED NET_SCH_NETEM NET_SCH_INGRESS NET_CLS_MATCHALL NET_CLS_ACT NET_ACT_GACT; do
 rg -q "^CONFIG_$option=y" "$out/.config" || { echo "$option not enabled" >&2; exit 1; }
done
export KBUILD_BUILD_USER=seedbox KBUILD_BUILD_HOST=seedbox KBUILD_BUILD_TIMESTAMP='Thu Jan 1 00:00:00 UTC 1970' SOURCE_DATE_EPOCH=0
make -s -C "$src" O="$out" -j1 bzImage
cp "$out/arch/x86/boot/bzImage" artifacts/bzImage
cp "$out/.config" artifacts/kernel.config
sha256sum artifacts/bzImage artifacts/kernel.config
