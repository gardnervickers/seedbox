#!/usr/bin/env bash
# Run inside the project's aarch64-linux Nix development shell.
set -euo pipefail
output=${1:?usage: build-linux.sh OUTPUT_DIRECTORY [BUILT_KERNEL_DIRECTORY]}
kernel_runtime=${2:-}
mkdir -p "$output"
output=$(realpath "$output")
test "$(uname -m)" = aarch64
cargo build -p seedbox --release --locked
if test -z "$kernel_runtime"; then
  kernel_runtime="$output/kernel-build"
  bash kernel-arm64/build.sh --application "$kernel_runtime"
fi
for file in Image compose-init kernel.config counter-audit.txt; do
  cp "$kernel_runtime/$file" "$output/$file"
done
cp target/release/seedbox "$output/linux-helper"
install -m 600 "$SEEDBOX_BUSYBOX" "$output/busybox"
install -m 600 "$SEEDBOX_MKE2FS" "$output/mke2fs"

# Only the executable's dynamic libraries and the OCI tools belong in the
# runtime closure. The Rust compiler and kernel build shell are build inputs.
python3 - "$output" <<'PY'
import hashlib, json, pathlib, re, shutil, subprocess, sys
output = pathlib.Path(sys.argv[1])
tools = {name: shutil.which(name) for name in ('skopeo', 'umoci', 'podman')}
if any(path is None for path in tools.values()):
    raise SystemExit('missing project OCI tools')
roots = set()
for path in tools.values():
    roots.add('/'.join(path.split('/')[:4]))
libraries = subprocess.check_output(['ldd', str(output / 'linux-helper')], text=True)
roots.update(re.findall(r'/nix/store/[a-z0-9]{32}-[^/\s]+', libraries))
if not roots:
    raise SystemExit('helper runtime closure was not found')
nix_store = shutil.which('nix-store') or str(pathlib.Path.home() / '.nix-profile/bin/nix-store')
closure = subprocess.check_output([nix_store, '--query', '--requisites', *sorted(roots)], text=True).splitlines()
with (output / 'store.nar').open('wb') as target:
    subprocess.run([nix_store, '--export', *sorted(set(closure))], stdout=target, check=True)
files = ('Image', 'compose-init', 'busybox', 'mke2fs', 'linux-helper', 'store.nar', 'kernel.config', 'counter-audit.txt')
manifest = {'version': 1, 'files': {name: hashlib.file_digest((output / name).open('rb'), 'sha256').hexdigest() for name in files},
            'tools': {'skopeo': tools['skopeo'], 'umoci': tools['umoci'], 'docker': tools['podman'], 'build_epoch': None}}
(output / 'runtime.json').write_text(json.dumps(manifest, indent=2) + '\n')
PY
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -czf "$output/runtime.tar.gz" -C "$output" \
  runtime.json Image compose-init busybox mke2fs linux-helper store.nar kernel.config counter-audit.txt
