#!/usr/bin/env python3
"""Build Seedbox with current guest inputs. Never invoked by the runtime."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(args, *, cwd, capture=False, check=True, **kwargs):
    return subprocess.run(args, cwd=cwd, check=check, text=True,
                          stdout=subprocess.PIPE if capture else sys.stderr, **kwargs)


def builder():
    explicit = os.environ.get('SEEDBOX_ASSET_BUILDER')
    for candidate in ([explicit] if explicit else ['podman', 'docker']):
        if shutil.which(candidate) and subprocess.run([candidate, 'info'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0:
            return candidate
    raise RuntimeError('start Podman or Docker Desktop to build Mac guest inputs')


def build_assets(root, output):
    if platform.system() == 'Linux':
        kernel = run(['nix', 'build', '--no-link', '--print-out-paths', '.#guestKernel'], cwd=root, capture=True).stdout.strip()
        runtime = run(['nix', 'build', '--no-link', '--print-out-paths', '.#runtimeArchive'], cwd=root, capture=True).stdout.strip()
        shutil.copyfile(Path(kernel) / ('bzImage' if platform.machine() == 'x86_64' else 'Image'), output / 'kernel')
        shutil.copyfile(runtime, output / 'runtime.tar.gz')
        return
    client = builder()
    image = 'docker.io/nixos/nix:latest'
    inspected = subprocess.run([client, 'image', 'inspect', image, '--format', '{{.Id}}'], capture_output=True, text=True)
    if inspected.returncode:
        run([client, 'pull', '--platform', 'linux/arm64', image], cwd=root)
        inspected = run([client, 'image', 'inspect', image, '--format', '{{.Id}}'], cwd=root, capture=True)
    image_id = inspected.stdout.strip().removeprefix('sha256:')
    if not image_id or any(c not in '0123456789abcdef' for c in image_id):
        raise RuntimeError('container engine returned an invalid builder image ID')
    # Persist Nix's cache. Scope the volume to the image so it cannot hide a
    # newer image's Nix executable with an older /nix mount.
    volume = 'seedbox-nix-' + image_id[:24]
    with tempfile.TemporaryDirectory(prefix='source-', dir=output) as temporary:
        temporary = Path(temporary)
        files = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=root)
        names = [name for name in files.split(b'\0') if name and os.path.lexists(root / os.fsdecode(name))]
        (temporary / 'files').write_bytes(b'\0'.join(names) + b'\0')
        run(['tar', '--null', '-T', str(temporary / 'files'), '-cf', str(temporary / 'source.tar')], cwd=root, env=dict(os.environ, COPYFILE_DISABLE='1'))
        script = '''mkdir /source /output
 tar -xf - -C /source
 cd /source
 export NIX_CONFIG="experimental-features = nix-command flakes
sandbox = false"
 kernel=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#guestKernel)
 runtime=$(nix build --cores 2 --max-jobs 1 --no-link --print-out-paths .#runtimeArchive)
 cp "$kernel/Image" /output/kernel
 cp "$runtime" /output/runtime.tar.gz
'''
        container = run([client, 'create', '-i', '--platform', 'linux/arm64', '--mount', f'type=volume,source={volume},destination=/nix', image, 'sh', '-eu', '-c', script], cwd=root, capture=True).stdout.strip()
        try:
            with (temporary / 'source.tar').open('rb') as source:
                run([client, 'start', '--attach', '--interactive', container], cwd=root, stdin=source)
            run([client, 'cp', container + ':/output/.', str(output)], cwd=root)
        finally:
            run([client, 'rm', '--force', container], cwd=root)


def build_cli(root, output, cache):
    environment = dict(os.environ, SEEDBOX_BUILD_HELPERS=str(output / 'runtime.tar.gz'))
    # Only complete builds use this checkout-local Cargo cache, under our lock.
    result = run(['cargo', 'build', '--release', '--locked', '-p', 'seedbox', '--target-dir', str(cache), '--message-format=json'], cwd=root, env=environment, capture=True, check=False)
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-message":
            print(message["message"].get("rendered", ""), file=sys.stderr, end="")
    if result.returncode:
        raise RuntimeError(f"native CLI build failed (exit {result.returncode})")
    executables = [item['executable'] for line in result.stdout.splitlines()
                   for item in [json.loads(line)] if item.get('reason') == 'compiler-artifact'
                   and item.get('executable') and item['target']['name'] == 'seedbox']
    if len(executables) != 1:
        raise RuntimeError('Cargo did not return one Seedbox executable')
    shutil.copy2(executables[0], output / 'seedbox')


def build(root=ROOT, *, assets_only=False):
    root = Path(root).resolve()
    directory = root / 'build'
    directory.mkdir(exist_ok=True)
    with (directory / '.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        pending = Path(tempfile.mkdtemp(prefix='.pending-', dir=directory))
        try:
            build_assets(root, pending)
            if not assets_only:
                build_cli(root, pending, directory / 'cargo')
            complete = directory / ('run-' + pending.name.removeprefix('.pending-'))
            pending.rename(complete)
            if not assets_only:
                pointer = directory / '.current-next'
                pointer.unlink(missing_ok=True)
                pointer.symlink_to(complete.name, target_is_directory=True)
                pointer.replace(directory / 'current')
            return complete
        finally:
            if pending.exists():
                shutil.rmtree(pending)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--assets-only', action='store_true', help='Build guest inputs only, leaving the current complete build unchanged.')
    args = parser.parse_args()
    if (platform.system(), platform.machine()) not in [('Linux', 'x86_64'), ('Linux', 'aarch64'), ('Darwin', 'arm64')]:
        parser.error('requires Linux x86-64/ARM64 or Apple Silicon macOS')
    try:
        print(build(assets_only=args.assets_only))
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f'build: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
