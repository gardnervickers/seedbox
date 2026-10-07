#!/usr/bin/env bash
# Run in this directory's pinned Nix shell on Mac or Linux. No guest execution.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build
zig cc -target x86_64-linux-musl -O2 -static -Wall -Wextra -Werror -o build/probe probe.c
python3 - <<'PY'
import hashlib, json, pathlib, shutil, subprocess
p = pathlib.Path
cc = str(p(shutil.which('zig')).resolve())
files = ['probe.c', 'build-guest.sh', 'flake.nix', 'flake.lock', 'build/probe']
record = {
    'schema': 'seedbox.mac-tcg.guest-build.v1',
    'compiler': cc,
    'compiler_version': subprocess.check_output([cc, 'version'], text=True),
    'sha256': {name: hashlib.sha256(p(name).read_bytes()).hexdigest() for name in files},
}
p('build/guest-build.json').write_text(json.dumps(record, indent=2) + '\n')
PY
