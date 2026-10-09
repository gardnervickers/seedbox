#!/usr/bin/env bash
# Internal guest-helper build; guest kernels are released independently.
set -euo pipefail
output=${1:?asset output directory}
mkdir -p "$output/sources"
bash guest/build-compose.sh "$output/compose-init"
cp "$SEEDBOX_MKE2FS" "$output/mke2fs"
cp "$SEEDBOX_E2FSPROGS_SOURCE" "$output/sources/e2fsprogs.source"
cp "$SEEDBOX_MUSL_SOURCE" "$output/sources/musl.source"
python3 - "$output" <<'PY'
import importlib.util, json, pathlib, sys
output = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location('package', 'tools/linux-runtime/package.py')
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)
notice, sources = package.notices(output / 'sources', ('e2fsprogs', 'musl'))
(output / 'NOTICE.txt').write_bytes(notice)
(output / 'provenance.json').write_text(json.dumps({'sources': sources}, indent=2) + '\n')
PY
rm -rf "$output/sources"
