"""Source finite workload selectors: exact native/plain/client stdout equality.

Progress instrumentation is the original ARM public client. Shared x86 runs
use the public preservation client and are not labeled ARM-profile coverage.
"""
import argparse
import os
from pathlib import Path
import platform
import sys
import tempfile


sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from phase import execute


def run(root, language, case):
    assets = root / "share/translator/languages"
    commands = {
        "js": [str(root / "runtimes/node/bin/node"), "--expose-gc", str(assets / "js/jsfz.js")],
        "lua": [str(root / "runtimes/lua/bin/luajit"), str(assets / "lua/luafz.lua")],
        "go": [str(root / ("bin/translator_gofzcgo" if case == "cgoThreads" else "bin/translator_gofz"))],
        # erl is a script: invoke its actual native emulator through erlexec,
        # with the same runtime-owned boot configuration as the source wrapper.
        "erl": [str(root / "runtimes/erl/bin/erl"), "-noshell", "-noinput", "-pa",
                str(assets / "erl"), "-run", "beamfz", "main"],
    }
    app = commands[language] + [case, "1", "1"]
    with tempfile.TemporaryDirectory(prefix="dr-language-") as tmp:
        base = Path(tmp)
        outputs = []
        for mode in ("native", "plain", "client"):
            work = base / mode
            work.mkdir()
            command = app
            if mode != "native":
                command = [str(root / "runtime/bin64/drrun"), "-use_dll",
                           str(root / "runtime/lib64/release/libdynamorio.so")]
                if mode == "client":
                    client = "libtranslator_progress.so" if platform.machine() == "aarch64" else "libtranslator_observe.so"
                    command += ["-c", str(root / "lib" / client)]
                    if platform.machine() == "aarch64":
                        command += ["--instructions-per-progress-call", "50000"]
                command += ["--"] + app
            env = {"HOME": str(work), "DYNAMORIO_CONFIGDIR": str(work),
                   "FZ_ERL_HOT": str(assets / "erl/hot"),
                   "PATH": str(root / "runtimes/erl/bin") + ":" + str(root / "runtimes/go/bin")}
            print(language, case, mode, flush=True)
            status, out, err = execute(root, command, work, env)
            assert status == 0 and "FAIL" not in out and "OK " in out, (language, case, mode, status, out, err)
            if mode == "client":
                import re
                pattern = r"a64_progress_like: ([1-9][0-9]*) refills" if platform.machine() == "aarch64" else r"path-cost: exit app=([1-9][0-9]*)"
                assert re.search(pattern, err), (language, case, err)
            outputs.append(out)
        assert len(set(outputs)) == 1, (language, case, outputs)
    print("SB-runtime:" + {"js": "Js", "lua": "Lua", "go": "Go", "erl": "Erl"}[language] + "." + case + ": PASS")


if __name__ == "__main__":
    # A direct invocation owns the same whole-tree group as the Rust wrapper.
    if os.getpgrp() != os.getpid():
        os.setpgid(0, 0)
    p = argparse.ArgumentParser()
    p.add_argument("root", type=Path)
    p.add_argument("language", choices=["js", "lua", "go", "erl"])
    p.add_argument("case")
    a = p.parse_args()
    run(a.root.resolve(), a.language, a.case)
