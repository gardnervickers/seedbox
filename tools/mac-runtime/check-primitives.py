#!/usr/bin/env python3
"""Build, sign and run bounded HVF primitive tests on Apple Silicon."""

import hashlib
import json
import platform
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise SystemExit("requires an Apple Silicon Mac; run inside nix develop")
    root = Path(__file__).resolve().parents[2]
    build = subprocess.run(
        [
            "cargo", "test", "--locked", "--manifest-path",
            str(root / "crates/seedbox-arm64/Cargo.toml"),
            "--lib", "--no-run", "--message-format=json",
        ],
        cwd=root, text=True, stdout=subprocess.PIPE, check=True,
    )
    artifacts = [json.loads(line) for line in build.stdout.splitlines()]
    executables = [
        item["executable"] for item in artifacts
        if item.get("reason") == "compiler-artifact"
        and item.get("executable")
        and item["target"]["kind"] == ["lib"]
        and item["target"]["name"] == "seedbox_arm64"
        and item["profile"]["test"]
    ]
    if len(executables) != 1:
        raise SystemExit("expected one ARM library test executable")
    test = "backend::hvf::tests::canceled_entry_holds_no_instruction_and_preserves_pc"
    with tempfile.TemporaryDirectory(prefix="seedbox-hvf-primitives-") as work:
        binary = Path(work) / "native-tests"
        shutil.copy2(executables[0], binary)
        subprocess.run(
            ["codesign", "--force", "--sign", "-", "--entitlements",
             str(root / "crates/seedbox-arm64/entitlements.plist"), str(binary)],
            check=True,
        )
        subprocess.run(
            [str(binary), test, "--exact", "--ignored", "--nocapture"],
            check=True, timeout=20,
        )
        print(json.dumps({
            "test": test,
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "timeout_seconds": 20,
            "result": "PASS",
        }, sort_keys=True))


if __name__ == "__main__":
    main()
