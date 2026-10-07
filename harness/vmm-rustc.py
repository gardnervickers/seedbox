#!/usr/bin/env python3
"""Compiler wrapper selected only by cargo build-vmm / test-vmm."""
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys


def remaps(root, env, sysroot):
    # Last matching prefix wins. Map the broad home first and checkout last,
    # including logical spellings when the shell entered through a symlink.
    mappings = []
    for path, replacement in (
        (env.get('HOME'), '/seedbox-build/home'),
        (env.get('CARGO_HOME') or str(Path.home() / '.cargo'), '/seedbox-build/cargo'),
        (sysroot, '/seedbox-build/rust'),
        (str(root), '.'),
    ):
        if path:
            mappings.append(f'--remap-path-prefix={path}={replacement}')
            resolved = str(Path(path).resolve())
            if resolved != path:
                mappings.append(f'--remap-path-prefix={resolved}={replacement}')
    logical = env.get('PWD')
    if logical and Path(logical).resolve() == root:
        mappings.append(f'--remap-path-prefix={logical}=.')
    return mappings


def linker_env(env):
    env = dict(env)
    # nix develop's synthetic $out is <checkout>/outputs/out. stdenv's
    # _addRpathPrefix adds -rpath $out/lib, consumed by the linker wrapper.
    # It is not a runtime library location. Keep all real library/store flags,
    # including target-suffixed NIX_LDFLAGS used by newer wrappers.
    for name, value in env.items():
        if name == 'NIX_LDFLAGS' or name.startswith('NIX_LDFLAGS_'):
            filtered = value
            output = env.get('out', '')
            if output.endswith('/outputs/out') and not output.startswith('/nix/store/'):
                # Remove the exact synthetic output first, even if its checkout
                # spelling contains spaces. Nix otherwise word-splits flags.
                filtered = re.sub(r'(?<!\S)-rpath\s+' + re.escape(output + '/lib')
                                  + r'(?=\s|$)', '', filtered)
            words = filtered.split()
            kept = []
            i = 0
            while i < len(words):
                if (words[i] == '-rpath' and i + 1 < len(words)
                        and not words[i + 1].startswith('/nix/store/')
                        and (words[i + 1] == 'outputs/out/lib'
                             or words[i + 1].endswith('/outputs/out/lib'))):
                    i += 2
                else:
                    kept.append(words[i])
                    i += 1
            if kept != words or filtered != value:
                env[name] = ' '.join(kept)
    return env


def main():
    root = Path(__file__).resolve().parent.parent
    command = sys.argv[1:]
    # Cargo can nest a rustc-workspace-wrapper before the actual compiler.
    prefix = command[:2] if len(command) > 1 and not command[1].startswith('-') else command[:1]
    env = linker_env(os.environ)
    if command[len(prefix):] == ['-vV']:
        # Cargo fingerprints verbose compiler identity, but does not fingerprint
        # rustc-wrapper itself. Include our contents so old unremapped artifacts
        # and later wrapper revisions cannot be silently reused. No local paths
        # enter this fingerprint, and the real compiler version stays visible.
        version = subprocess.check_output([*prefix, '-vV'], env=env, text=True)
        digest = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        sys.stdout.write(version + f'seedbox-vmm-wrapper: {digest}\n')
        return
    sysroot = subprocess.check_output([*prefix, '--print', 'sysroot'], env=env, text=True).strip()
    command.extend(remaps(root, env, sysroot))
    os.execvpe(command[0], command, env)


if __name__ == '__main__':
    main()
