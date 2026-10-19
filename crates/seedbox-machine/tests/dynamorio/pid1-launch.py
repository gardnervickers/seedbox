#!/usr/bin/env python3
"""Prefer unprivileged PID1; use only the scoped namespace helper if denied."""
from pathlib import Path
import os
import shutil
import subprocess
import sys

launcher = str(Path(__file__).resolve().parent / "seedbox-dr-isolate")
arguments = sys.argv[1:]
if arguments[:2] == ["preflight", "pid1"]:
    prefix, suffix = arguments[:2], arguments[2:]
elif arguments[:1] == ["pid1"]:
    prefix, suffix = arguments[:1], arguments[1:]
else:
    sys.exit("isolate: scoped PID1 launcher accepts only pid1/preflight pid1")
probe = subprocess.run([launcher, "preflight", "pid1"], capture_output=True)
if probe.returncode == 0:
    if prefix[0] == "preflight":
        sys.stderr.buffer.write(probe.stderr)
        sys.exit(0)
    os.execv(launcher, [launcher, *arguments])
sudo = shutil.which("sudo", path="/run/wrappers/bin:/usr/bin:/bin")
if not sudo:
    sys.stderr.buffer.write(probe.stderr)
    sys.exit("isolate: setup PID namespace denied; scoped sudo launcher unavailable")
# The immutable Nix-store helper creates only the PID namespace; before exec,
# its child drops every supplementary group and the requested normal uid/gid.
os.execv(sudo, [sudo, "-n", "--preserve-env=DYNAMORIO_CONFIGDIR", launcher, *prefix, "--as-user", str(os.getuid()),
               str(os.getgid()), *suffix])
