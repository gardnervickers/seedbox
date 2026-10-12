#!/usr/bin/env python3
"""Build the reproducer and DynamoRIO variants on the local arm64 Nix builder.

usage: build.py repro
       build.py dr NAME BUILD_TYPE [PATCH...]
PATCH paths are relative to the checkout root (e.g. guest/progress/dynamorio-exclusives.patch
or build/jvm-crash/repro/patches/fix.patch). Outputs go to build/jvm-crash/repro/out/.
"""
import io, pathlib, subprocess, sys, tarfile

root = pathlib.Path(__file__).resolve().parents[3]
here = root / 'build/jvm-crash/repro'
image = 'docker.io/nixos/nix:latest'


def run(files, script, out, extra=()):
    out.mkdir(parents=True, exist_ok=True)
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode='w') as t:
        for name in files:
            t.add(root / name, arcname=name, recursive=False)
        for name, data in extra:
            info = tarfile.TarInfo(name); info.size = len(data)
            t.addfile(info, io.BytesIO(data))
    ident = subprocess.check_output(['podman', 'image', 'inspect', image, '--format', '{{.Id}}'],
                                    text=True).strip().removeprefix('sha256:')
    body = ('mkdir -p /source /output\ntar -xf - -C /source\ncd /source\n'
            'export NIX_CONFIG="experimental-features = nix-command flakes\nsandbox = false"\n' + script)
    c = subprocess.check_output(['podman', 'create', '-i', '--platform', 'linux/arm64', '--mount',
                                 f'type=volume,source=seedbox-nix-{ident[:24]},destination=/nix',
                                 image, 'sh', '-eu', '-c', body], text=True).strip()
    try:
        subprocess.run(['podman', 'start', '-ai', c], input=buf.getvalue(), check=True)
        subprocess.run(['podman', 'cp', c + ':/output/.', str(out)], check=True)
    finally:
        subprocess.run(['podman', 'rm', '-f', c], check=True, capture_output=True)


def main():
    if sys.argv[1] == 'repro':
        run(['flake.nix', 'flake.lock', 'nix/runtime.nix', 'build/jvm-crash/repro/repro.c'],
            "nix develop .#assets --command sh -eu -c '$SEEDBOX_STATIC_CC -O2 -g -std=gnu11 -static "
            "-pthread build/jvm-crash/repro/repro.c -o /output/repro'\n",
            here / 'out')
    elif sys.argv[1] == 'dr':
        name, build_type, patches = sys.argv[2], sys.argv[3], sys.argv[4:]
        expr = ('import ./build/jvm-crash/repro/dr-variant.nix { name = "%s"; buildType = "%s"; '
                'patches = [ %s ]; }\n' % (name, build_type, ' '.join('"%s"' % p for p in patches)))
        # dr-variant.nix resolves patch paths relative to itself, so put it at the root.
        files = ['flake.lock', 'nix/dynamorio.nix', 'build/jvm-crash/repro/dr-variant.nix',
                 'guest/progress/dynamorio-exclusives.patch', 'guest/progress/dynamorio-rebuild-vmareas.patch',
                 *[p for p in patches if not p.startswith('guest/progress/')]]
        # Debug cc1 is OOM-killed at --cores 3; keep Debug at --cores 1.
        cores = '1' if build_type.lower() == 'debug' else '3'
        run(files,
            'cp build/jvm-crash/repro/dr-variant.nix ./dr-variant.nix\n'
            'cat > variant.nix <<EOF\n' + expr.replace('./build/jvm-crash/repro/dr-variant.nix', './dr-variant.nix') + 'EOF\n'
            f'nix build --cores {cores} --max-jobs 1 --impure --file variant.nix --no-link --print-out-paths > /output/store-path\n'
            'cp -r "$(cat /output/store-path)/." /output/\n',
            here / 'out' / f'dr-{name}')
    elif sys.argv[1] == 'client309':
        run(['flake.lock', 'build/jvm-crash/repro/progress-309-client.nix',
             'build/jvm-crash/repro/progress-309/progress.c',
             'build/jvm-crash/repro/progress-309/budget.h',
             'build/jvm-crash/repro/progress-309/CMakeLists.txt'],
            'cp build/jvm-crash/repro/progress-309-client.nix ./progress-309-client.nix\n'
            'nix build --cores 1 --max-jobs 1 --impure --file progress-309-client.nix --no-link '
            '--print-out-paths > /output/store-path\n'
            'cp "$(cat /output/store-path)/libseedbox_progress_309.so" /output/\n',
            here / 'out')
    elif sys.argv[1] == 'client':
        run(['flake.lock', 'build/jvm-crash/repro/park-client.nix',
             'build/jvm-crash/repro/park.c', 'build/jvm-crash/repro/park-CMakeLists.txt'],
            'cp build/jvm-crash/repro/park-client.nix ./park-client.nix\n'
            'nix build --cores 1 --max-jobs 1 --impure --file park-client.nix --no-link '
            '--print-out-paths > /output/store-path\n'
            'cp "$(cat /output/store-path)/librepro_park.so" /output/\n',
            here / 'out')
    else:
        raise SystemExit(__doc__)


main()
