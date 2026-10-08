#!/usr/bin/env python3
"""Cargo's cached guest-asset build. Never compiles a second Seedbox executable."""
import fcntl
import hashlib
import gzip
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[2]
BUILD_LOG = None
INPUTS = ['flake.nix', 'flake.lock', 'guest', 'kernel', 'kernel-arm64', 'tools/runtime/Dockerfile', 'tools/runtime/build-assets.sh', 'tools/runtime/filesystem.py',
          'tools/linux-runtime/package.py']


def run(args, **kwargs):
    print('cargo::warning=assets: ' + ' '.join(map(str, args)), flush=True)
    if BUILD_LOG is not None:
        kwargs.setdefault('stdout', BUILD_LOG)
        kwargs.setdefault('stderr', subprocess.STDOUT)
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def build(output, architecture):
    global BUILD_LOG
    files = []
    for name in INPUTS:
        path = ROOT / name
        files.extend(sorted(p for p in path.rglob('*') if p.is_file() and '__pycache__' not in p.parts) if path.is_dir() else [path])
    digest = hashlib.sha256(architecture.encode())
    digest.update(Path(__file__).read_bytes())
    for path in files:
        digest.update(str(path.relative_to(ROOT)).encode() + b'\0')
        digest.update(path.read_bytes())
    key = digest.hexdigest()
    cache_home = Path(os.environ.get('XDG_CACHE_HOME', str(Path.home() / ('Library/Caches' if sys.platform == 'darwin' else '.cache'))))
    cache = cache_home / 'seedbox' / 'assets' / key
    cache.mkdir(parents=True, exist_ok=True)
    with (cache / 'lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        package = cache / 'runtime.tar.gz'
        checksum = cache / 'sha256'
        if package.is_file() and checksum.is_file() and hashlib.file_digest(package.open('rb'), 'sha256').hexdigest() == checksum.read_text().strip():
            print('cargo::warning=assets: reusing guest assets ' + key, flush=True)
        else:
            BUILD_LOG = (cache / 'build.log').open('w')
            print('cargo::warning=assets: build log ' + str(cache / 'build.log'), flush=True)
            requested = os.environ.get('SEEDBOX_BUILDER')
            candidates = [requested] if requested else ['docker', 'podman']
            engine = None
            found = []
            for candidate in candidates:
                client = shutil.which(candidate)
                if not client:
                    continue
                found.append(client)
                try:
                    ready = subprocess.run([client, 'info'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15).returncode == 0
                except subprocess.TimeoutExpired:
                    ready = False
                if ready:
                    engine = client
                    break
            if not engine:
                if found:
                    raise RuntimeError('container engine unavailable; start Docker Desktop or your Podman machine and check DOCKER_HOST / CONTAINER_HOST; tried ' + ', '.join(found))
                raise RuntimeError('no Docker/Podman client found; install Docker or Podman and start its Linux engine (or enter nix develop)')
            info = json.loads(subprocess.check_output([engine, 'info', '--format', '{{json .}}'], text=True, timeout=15))
            memory = info.get('MemTotal', info.get('host', {}).get('memTotal', 0))
            if memory and memory < 3 * 1024**3:
                raise RuntimeError('guest asset builds need at least 4 GiB assigned to the container engine; increase Docker Desktop memory in Settings, or stop your Podman machine and run podman machine set --memory 4096, then start it again')
            with tempfile.TemporaryDirectory(prefix='seedbox-assets-') as directory:
                stage = Path(directory)
                context = stage / 'context'
                context.mkdir()
                for path in files:
                    dest = context / path.relative_to(ROOT)
                    dest.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, dest)
                tag = 'seedbox-tools:' + key
                version = subprocess.check_output([engine, '--version'], text=True, timeout=15)
                build_options = ([] if 'podman' in version.lower() else ['--load']) + [
                    '--platform', 'linux/' + architecture, '--build-arg', 'SOURCE_DATE_EPOCH=0']
                run([engine, 'build', *build_options, '--target', 'assets', '-t', tag + '-assets',
                     '-f', context / 'tools/runtime/Dockerfile', context])
                container_image = tag + '-assets'
                container = subprocess.check_output([engine, 'create', '--entrypoint', '/bin/sh', container_image], text=True).strip()
                assets = stage / 'assets'
                try:
                    run([engine, 'cp', container + ':/assets', assets])
                finally:
                    run([engine, 'rm', '-f', container])
                # Store only ordinary files, never the image's /nix/store symlink.
                for child in assets.iterdir():
                    if child.is_symlink():
                        child.unlink()
                for name in ['tools.nar', 'tools.path', 'init.cpio', 'build-directory.txt', 'SHA256SUMS', 'bzImage']:
                    (assets / name).unlink(missing_ok=True)
                run([engine, 'build', *build_options, '-t', tag,
                     '-f', context / 'tools/runtime/Dockerfile', context])
                image = subprocess.check_output([engine, 'image', 'inspect', '--format', '{{.Id}}', tag], text=True).strip()
                if not image.startswith('sha256:'):
                    image = 'sha256:' + image
                run([engine, 'save', '-o', assets / 'tools-image.tar', tag])
                records = {p.name: hashlib.file_digest(p.open('rb'), 'sha256').hexdigest()
                           for p in sorted(assets.iterdir()) if p.is_file()}
                (assets / 'runtime.json').write_text(json.dumps({'version': 1, 'image': image, 'files': records}, indent=2) + '\n')
                temporary = cache / 'runtime.partial'
                with temporary.open('wb') as raw:
                    with gzip.GzipFile(filename='', fileobj=raw, mode='wb', compresslevel=6, mtime=0) as compressed:
                        with tarfile.open(fileobj=compressed, mode='w') as archive:
                            for path in sorted(assets.iterdir()):
                                if path.is_file():
                                    header = archive.gettarinfo(path, arcname=path.name)
                                    header.uid = header.gid = header.mtime = 0
                                    header.uname = header.gname = ''
                                    with path.open('rb') as source:
                                        archive.addfile(header, source)
                temporary.replace(package)
                checksum.write_text(hashlib.file_digest(package.open('rb'), 'sha256').hexdigest() + '\n')
        shutil.copyfile(package, output)


if __name__ == '__main__':
    try:
        build(Path(sys.argv[1]), sys.argv[2])
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        if BUILD_LOG is not None:
            BUILD_LOG.flush()
            print('build log: ' + BUILD_LOG.name, file=sys.stderr)
            lines = Path(BUILD_LOG.name).read_text(errors='replace').splitlines()
            print('\n'.join(lines[-20:]), file=sys.stderr)
        sys.exit('guest asset build failed: ' + str(error))
