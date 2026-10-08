#!/usr/bin/env python3
"""Sign Darwin executable linker outputs before rustc publishes them."""
import os
from pathlib import Path
import subprocess
import sys

args = sys.argv[1:]
# Nix's compiler wrapper supplies SDK and library paths. Use the same compiler
# rustc would normally select, rather than bypassing it with /usr/bin/clang.
subprocess.run([os.environ.get('CC', 'cc'), *args], check=True)
if '-o' in args and '-dynamiclib' not in args and '-bundle' not in args:
    output = args[args.index('-o') + 1]
    entitlement = Path(__file__).resolve().parents[2] / 'crates/seedbox-arm64/entitlements.plist'
    signed = subprocess.run(['/usr/bin/codesign', '--force', '--sign', '-', '--entitlements', str(entitlement), output], stderr=subprocess.PIPE)
    if signed.returncode:
        sys.stderr.buffer.write(signed.stderr)
        sys.exit(signed.returncode)
    subprocess.run(['/usr/bin/codesign', '--verify', '--strict', output], check=True)
