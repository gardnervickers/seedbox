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


def run(root, language, case, debug=False):
    assets = root / "share/translator/languages"
    commands = {
        "cs": [str(root / "runtimes/dotnet/bin/dotnet"), str(assets / "cs/csfz.dll")],
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
                command = [str(root / "runtime/bin64/drrun")]
                if debug:
                    command += ["-debug"]
                profile = "debug" if debug else "release"
                command += ["-use_dll",
                            str(root / f"runtime/lib64/{profile}/libdynamorio.so")]
                if mode == "client":
                    client = "libtranslator_progress.so" if platform.machine() == "aarch64" else ("libtranslator_clr_observe.so" if language == "cs" else "libtranslator_observe.so")
                    command += ["-c", str(root / "lib" / client)]
                    if platform.machine() == "aarch64":
                        command += ["--instructions-per-progress-call", "50000"]
                    elif language == "cs":
                        # Exact public module path from the same packaged SDK/runtime.
                        modules = list((root / "runtimes/dotnet/share/dotnet/shared/Microsoft.NETCore.App").glob("*/libcoreclr.so"))
                        assert len(modules) == 1, modules
                        command += [str(modules[0].resolve())]
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
                if language == "cs" and platform.machine() != "aarch64":
                    assert re.search(r"path-cost: exit app=[1-9][0-9]* vm=[1-9][0-9]*", err), err
                    assert "file_name=libcoreclr.so" in err, err
            if language == "cs":
                expected = (assets / "cs" / (case + ".stdout")).read_text()
                assert out == expected, (language, case, mode, out, expected)
            outputs.append(out)
        assert len(set(outputs)) == 1, (language, case, outputs)
    print("SB-runtime:" + {"js": "Js", "lua": "Lua", "go": "Go", "erl": "Erl", "cs": "Cs"}[language] + "." + case + ": PASS")


if __name__ == "__main__":
    # A direct invocation owns the same whole-tree group as the Rust wrapper.
    if os.getpgrp() != os.getpid():
        os.setpgid(0, 0)
    p = argparse.ArgumentParser()
    p.add_argument("root", type=Path)
    p.add_argument("language", choices=["js", "lua", "go", "erl", "cs"])
    p.add_argument("case")
    p.add_argument("--debug", action="store_true",
                   help="Use matching genuine Debug fixtures and runtime.")
    a = p.parse_args()
    run(a.root.resolve(), a.language, a.case, a.debug)
