#!/usr/bin/env python3
"""Bounded CI failure evidence, never a test verdict or a source of SDK builds."""
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import zlib

MIB = 1024 * 1024
LOG_HALF = 8 * MIB
BINARY_FILE = 64 * MIB
BINARY_TOTAL = 160 * MIB
TEXT_FILE = MIB
TEXT_TOTAL = 32 * MIB
FILES = 128


def capture_log(path):
    """Drain/forward every byte, retain at most 8 MiB head + 8 MiB tail."""
    path.parent.mkdir(parents=True, exist_ok=True)
    head, tail, total = bytearray(), bytearray(), 0
    try:
        while chunk := os.read(sys.stdin.fileno(), 65536):
            sys.stdout.buffer.write(chunk)
            sys.stdout.buffer.flush()
            total += len(chunk)
            prefix = min(len(chunk), LOG_HALF - len(head))
            head.extend(chunk[:prefix])
            tail.extend(chunk[prefix:])
            if len(tail) > LOG_HALF:
                del tail[:-LOG_HALF]
    finally:
        with path.open('wb') as output:
            output.write(head)
            if total > len(head) + len(tail):
                output.write(b'\n[CI log truncated: retained 8 MiB head and tail]\n')
            output.write(tail)


def debuglink(data):
    # Our hosted x86/ARM runtimes are ELF64 little-endian. Record GNU's companion
    # identity separately from the co-installed SDK path and file SHA256.
    if data[:6] != b'\x7fELF\x02\x01':
        raise ValueError('expected little-endian ELF64')
    offset = struct.unpack_from('<Q', data, 40)[0]
    size, count, strings = struct.unpack_from('<HHH', data, 58)
    headers = [struct.unpack_from('<IIQQQQIIQQ', data, offset + i * size) for i in range(count)]
    names = data[headers[strings][4]:headers[strings][4] + headers[strings][5]]
    for header in headers:
        if names[header[0]:].split(b'\0', 1)[0] == b'.gnu_debuglink':
            section = data[header[4]:header[4] + header[5]]
            name = section.split(b'\0', 1)[0]
            crc = struct.unpack_from('<I', section, (len(name) + 4) & ~3)[0]
            return name.decode(), crc
    return None


def probe_elf_paths(source):
    """Record the two probe ELFs' loader contract without changing their launch."""
    result = subprocess.run(['readelf', '-W', '-l', '-d', str(source)],
                            capture_output=True, text=True, timeout=10, check=True)
    output = result.stdout
    interpreter = re.search(r'Requesting program interpreter: ([^\]]+)', output)
    needed = re.findall(r'\(NEEDED\).*?Shared library: \[([^\]]+)\]', output)
    paths = {}
    for tag, label in [('RPATH', 'rpath'), ('RUNPATH', 'runpath')]:
        match = re.search(r'\(' + tag + r'\).*?Library (?:rpath|runpath): \[([^\]]*)\]', output)
        paths[label] = match.group(1) if match else None
    directories = []
    for value in (paths['rpath'], paths['runpath']):
        if value:
            directories.extend(value.replace('${ORIGIN}', str(source.parent))
                               .replace('$ORIGIN', str(source.parent)).split(':'))
    return {'interpreter': interpreter.group(1) if interpreter else None,
            'interpreter_exists': Path(interpreter.group(1)).is_file() if interpreter else None,
            'needed': needed, **paths,
            'needed_in_embedded_paths': {
                name: next((str(candidate) for directory in directories
                            if (candidate := Path(directory) / name).is_file()), None)
                for name in needed}}


def collect(root, temporary, environment):
    output = temporary / 'dr-artifacts'
    output.mkdir(parents=True, exist_ok=False)
    manifest = {'limits': {'binary_file': BINARY_FILE, 'binary_total': BINARY_TOTAL,
                           'text_file': TEXT_FILE, 'text_total': TEXT_TOTAL, 'report_files': FILES},
                'fixtures': {}, 'files': [], 'warnings': []}

    def warning(message):
        if len(manifest['warnings']) < 64:
            print(f'::warning::{message}', file=sys.stderr)
            manifest['warnings'].append(message[:1000])

    for name, directory in [('seedbox_head', root), ('dynamorio_head', root / 'vendor/dynamorio')]:
        manifest[name] = subprocess.check_output(['git', '-C', str(directory), 'rev-parse', 'HEAD'], text=True).strip()
    manifest['github'] = {key: environment.get(key) for key in
                          ('GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_SHA',
                           'GITHUB_REF', 'GITHUB_HEAD_REF', 'RUNNER_ARCH')}
    remaining = BINARY_TOTAL
    for flavor, variable in [('release', 'SEEDBOX_DYNAMORIO_FIXTURES'),
                             ('debug', 'SEEDBOX_DYNAMORIO_DEBUG_FIXTURES')]:
        fixture = environment.get(variable)
        manifest['fixtures'][flavor] = fixture
        if not fixture:
            warning(f'{variable} missing; no {flavor} runtime to retain')
            continue
        library = Path(fixture) / f'runtime/lib64/{flavor}/libdynamorio.so'
        link = None
        for source in (library, library.with_name('libdynamorio.so.debug')):
            try:
                size = source.stat().st_size
                if size > min(BINARY_FILE, remaining):
                    warning(f'omitted oversized binary {source}: {size} bytes')
                    continue
                with source.open('rb') as stream:
                    data = stream.read(min(BINARY_FILE, remaining) + 1)
                if len(data) != size:
                    raise ValueError('binary changed during collection')
                if source == library:
                    link = debuglink(data)
                verified = source != library and link == (source.name, zlib.crc32(data))
                if source != library and not verified:
                    warning(f'co-installed debug companion {source}: GNU debuglink CRC mismatch or absent; retained for offline verification')
                destination = output / 'runtime' / flavor / source.name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                remaining -= size
                manifest['files'].append({'source': str(source), 'resolved_source': str(source.resolve()),
                                          'retained': str(destination.relative_to(output)), 'bytes': size,
                                          'sha256': hashlib.sha256(data).hexdigest(),
                                          'gnu_debuglink': link, 'companion_crc_verified': verified,
                                          'crc32': zlib.crc32(data)})
            except (OSError, ValueError, IndexError, struct.error) as error:
                warning(f'cannot retain {source}: {error}')
        if environment.get('RUNNER_ARCH') == 'ARM64':
            # The context case is AArch64-only. Keep its exact application and
            # public client from each fixture alongside the selected runtime.
            probe_elf = None
            for relative in ('bin/translator_progress_context',
                             'lib/libtranslator_progress.so'):
                source = Path(fixture) / relative
                try:
                    size = source.stat().st_size
                    if size > min(BINARY_FILE, remaining):
                        warning(f'omitted oversized binary {source}: {size} bytes')
                        continue
                    with source.open('rb') as stream:
                        data = stream.read(min(BINARY_FILE, remaining) + 1)
                    if len(data) != size:
                        raise ValueError('binary changed during collection')
                    destination = output / 'fixtures' / flavor / Path(relative).name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(data)
                    remaining -= size
                    record = {'source': str(source), 'resolved_source': str(source.resolve()),
                              'retained': str(destination.relative_to(output)), 'bytes': size,
                              'sha256': hashlib.sha256(data).hexdigest()}
                    try:
                        record['elf'] = probe_elf_paths(source)
                        if relative == 'bin/translator_progress_context':
                            probe_elf = record['elf']
                    except (OSError, subprocess.CalledProcessError,
                            subprocess.TimeoutExpired) as error:
                        warning(f'cannot inspect ELF loader paths {source}: {error}')
                    manifest['files'].append(record)
                except (OSError, ValueError) as error:
                    warning(f'cannot retain {source}: {error}')
            if flavor == 'release' and probe_elf and probe_elf['interpreter']:
                # The first redirect precedes main in the hosted failure. Keep
                # exactly the probe's embedded AArch64 interpreter for PC offsets.
                source = Path(probe_elf['interpreter'])
                if not (source.is_absolute() and source.name == 'ld-linux-aarch64.so.1'
                        and probe_elf['needed_in_embedded_paths'].get(source.name) == str(source)):
                    warning(f'probe interpreter is not an embedded AArch64 dependency: {source}')
                else:
                    try:
                        size = source.stat().st_size
                        if size > min(BINARY_FILE, remaining):
                            warning(f'omitted oversized binary {source}: {size} bytes')
                        else:
                            with source.open('rb') as stream:
                                data = stream.read(min(BINARY_FILE, remaining) + 1)
                            if len(data) != size:
                                raise ValueError('binary changed during collection')
                            destination = output / 'fixtures' / 'interpreter' / source.name
                            destination.parent.mkdir(parents=True, exist_ok=True)
                            destination.write_bytes(data)
                            remaining -= size
                            manifest['files'].append({
                                'source': str(source), 'resolved_source': str(source.resolve()),
                                'retained': str(destination.relative_to(output)),
                                'bytes': size, 'sha256': hashlib.sha256(data).hexdigest()})
                    except (OSError, ValueError) as error:
                        warning(f'cannot retain probe interpreter {source}: {error}')
    # Walk only the checker's retained report directory, never Cargo/Nix trees,
    # home directories, symlinked report paths, cores or arbitrary binary files.
    remaining, count, visited = TEXT_TOTAL, 0, 0
    sources = [(temporary / 'dr-logs', 'logs'), (root / 'target/check-reports', 'check-reports')]
    for source, label in sources:
        if source.is_symlink() or not source.is_dir():
            warning(f'no diagnostic source directory: {source}')
            continue
        for directory, dirs, names in os.walk(source, followlinks=False):
            visited += 1
            dirs[:] = sorted(d for d in dirs if not (Path(directory) / d).is_symlink())
            if len(Path(directory).relative_to(source).parts) >= 8:
                dirs.clear()
            if visited > 4096:
                warning('report traversal limited to 4096 directories')
                break
            for name in sorted(names):
                path = Path(directory) / name
                if path.is_symlink() or not path.is_file() or path.suffix not in (
                        '.json', '.txt', '.log', '.out', '.stderr', '.stdout', '.yaml', '.yml', '.tsv'):
                    continue
                if count >= FILES or remaining <= 0:
                    warning('text retention budget exhausted; remaining reports omitted')
                    break
                try:
                    size = path.stat().st_size
                    # The bounded ordinary log retains its head and tail;
                    # other reports are prefix-limited to 1 MiB.
                    limit = 2 * LOG_HALF + 128 if label == 'logs' else TEXT_FILE
                    amount = min(size, limit, remaining)
                    destination = output / label / path.relative_to(source)
                    if amount < size:
                        destination = destination.with_name(destination.name + '.truncated')
                        warning(f'truncated report {path}: {amount}/{size} bytes')
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    with path.open('rb') as stream:
                        data = stream.read(amount)
                    destination.write_bytes(data)
                except OSError as error:
                    warning(f'cannot retain report {path}: {error}')
                    continue
                remaining -= len(data)
                count += 1
                manifest['files'].append({'source': str(path), 'retained': str(destination.relative_to(output)),
                                          'source_bytes': size, 'bytes': len(data), 'truncated': amount < size})
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    if not manifest['files']:
        warning('no diagnostic sources retained; manifest records missing inputs')


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--log':
        capture_log(Path(sys.argv[2]))
    elif len(sys.argv) == 1:
        collect(Path.cwd(), Path(os.environ['RUNNER_TEMP']), os.environ)
    else:
        sys.exit('usage: artifacts.py [--log PATH]')
