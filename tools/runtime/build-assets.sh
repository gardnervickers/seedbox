#!/usr/bin/env bash
# Internal Cargo asset build, executed on a Linux container filesystem.
set -euo pipefail
output=${1:?asset output directory}
mkdir -p "$output/sources"
if test "$(uname -m)" = aarch64; then
  SEEDBOX_ARM64_BUILD=/tmp/kernel bash kernel-arm64/build.sh --application "$output"
  cp /tmp/kernel/linux-*.tar.xz "$output/sources/linux.source"
else
  SEEDBOX_WORK=/tmp/kernel SEEDBOX_KBUILD=/tmp/kernel/build SEEDBOX_ARTIFACTS="$output" bash kernel/build.sh
  bash guest/build-compose.sh "$output/compose-init"
  cp "$output/bzImage" "$output/Image"
  cp /tmp/kernel/src/linux-*.tar.xz "$output/sources/linux.source"
fi
cp "$SEEDBOX_BUSYBOX" "$output/busybox"
cp "$SEEDBOX_MKE2FS" "$output/mke2fs"
cp "$SEEDBOX_BUSYBOX_SOURCE" "$output/sources/busybox.source"
cp "$SEEDBOX_E2FSPROGS_SOURCE" "$output/sources/e2fsprogs.source"
cp "$SEEDBOX_MUSL_SOURCE" "$output/sources/musl.source"
mkdir -p /tools-rootfs/nix/store /tools-rootfs/tmp
chmod 1777 /tools-rootfs/tmp
while IFS= read -r path; do
  cp -a "$path" /tools-rootfs/nix/store/
done < <(nix-store --query --requisites "$(readlink /preparation-tools)")
ln -s "$(readlink /preparation-tools)" /tools-rootfs/tools
python3 - "$output" <<'PY'
import hashlib, importlib.util, json, pathlib, sys
output = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location('package', 'tools/linux-runtime/package.py')
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)
notice, sources = package.notices(output / 'sources')
(output / 'NOTICE.txt').write_bytes(notice)
(output / 'provenance.json').write_text(json.dumps({'sources': sources, 'flake_lock_sha256': package.digest(pathlib.Path('flake.lock'))}, indent=2) + '\n')
PY
rm -rf "$output/sources"
