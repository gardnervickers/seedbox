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


run(['flake.nix','flake.lock','nix/runtime.nix','build/perf/hvf-probes/jit.c','build/perf/hvf-probes/ic-client.c','build/perf/hvf-probes/CMakeLists.txt','build/perf/hvf-probes/client.nix'],
    "nix develop .#assets --command sh -eu -c '$SEEDBOX_STATIC_CC -O2 -static build/perf/hvf-probes/jit.c -o /output/jit'\n"
    "cp build/perf/hvf-probes/client.nix ./client.nix\n"
    "nix build --cores 1 --max-jobs 1 --impure --file client.nix --no-link --print-out-paths > /output/client-store-path\n"
    "cp \"$(cat /output/client-store-path)/libic_counter.so\" /output/\n",
    root / 'build/perf/hvf-probes/out')
