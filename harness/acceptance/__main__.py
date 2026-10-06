"""Seedbox acceptance suite (issue #4). Run through harness/acceptance.sh."""
import argparse
import dataclasses
import datetime
import json
import os
from pathlib import Path
import platform
import subprocess
import signal
import sys
import time
import traceback

from . import checks
from .engine import EXIT_CODES, Outcome, execute, overall, render
from .launcher import CliLauncher, cpu_list, sha256
from .portable import portable
from .prepare import prepare_projects

SIM = Path(__file__).resolve().parents[1]
CASES = Path(__file__).with_name("cases.json")
DEFAULT_CPUS = {"smoke": "2-5", "full": "2-5"}
CASE_KEYS = {"smoke": ["replay", "stop_placement", "race", "checker_control", "fault_effects"]}
CASE_KEYS["full"] = CASE_KEYS["smoke"] + ["fixed_control"]


def log(message):
    print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)


def make_launcher(args, scenarios, out):
    binary = Path(args.seedbox).resolve() if args.seedbox else None
    if binary is None:
        with (out / "build.log").open("w") as build_log:
            built = subprocess.run(["cargo", "build", "--locked", "-p", "seedbox", "--message-format=json"],
                                   cwd=SIM.parent, text=True, stdout=subprocess.PIPE, stderr=build_log, check=True)
        artifacts = [json.loads(line) for line in built.stdout.splitlines()]
        binary, = [Path(row["executable"]) for row in artifacts
                   if row.get("reason") == "compiler-artifact" and row.get("executable")
                   and row["target"]["name"] == "seedbox" and "bin" in row["target"]["kind"]]
    kernel = Path(args.kernel).resolve() if args.kernel else (
        Path(args.artifacts).resolve() if args.artifacts else Path(args.root).resolve() / "artifacts"
    ) / "bzImage"
    busybox = Path(os.environ["SEEDBOX_BUSYBOX"])
    engines = {None: Path(args.vmm).resolve()}
    if args.performance_only:
        engines[checks.BASELINE] = Path(args.performance_only).resolve()
    for path in (binary, kernel, busybox, *engines.values()):
        if not path.is_file():
            raise FileNotFoundError(path)
    return CliLauncher(binary, scenarios, cpu_list(args.cpus or DEFAULT_CPUS[args.tier]),
                       out, engines, kernel, busybox, args.lease_wait, log=log)


def suite(args):
    cases = json.loads(Path(args.cases).read_text())
    started = time.monotonic()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = Path(args.root).resolve() / "runs" / f"acceptance-{args.tier}-{stamp}-{os.getpid()}"
    report = {"tier": args.tier, "started": stamp, "host": platform.node(), "cases": cases,
              "declared": {"performance_only": bool(args.performance_only)}}
    outcomes = []
    if args.tier == "portable" or not args.skip_portable:
        outcomes += [Outcome(f"portable: {name}", verdict) for name, verdict in portable(SIM.parent, log)]
    if args.tier != "portable" and not any(o.verdict.status == checks.FAIL for o in outcomes):
        selected = checks.SMOKE if args.tier == "smoke" else checks.FULL
        scenarios = sorted({cases[key]["scenario"] for key in CASE_KEYS[args.tier]})
        prepared = prepare_projects(SIM, cases, scenarios, out)
        launcher = make_launcher(args, prepared, out)
        report["projects"] = prepared
        report["engines"] = {name or "candidate": sha256(path) for name, path in launcher.engines.items()}
        report["seedbox"] = sha256(launcher.seedbox)
        ctx = checks.Context(cases, performance_only=bool(args.performance_only))
        outcomes += execute(selected, ctx, launcher)
    verdict = overall(outcomes)
    report["checks"] = [{"name": o.name, **dataclasses.asdict(o.verdict), "host_seconds": o.host_seconds,
                         "runs": [r.summary() for r in o.runs]} for o in outcomes]
    report["wall_seconds"] = round(time.monotonic() - started, 1)
    report["verdict"] = verdict
    print(render(outcomes))
    print(f"\n{verdict} ({args.tier}, {report['wall_seconds']} s wall)")
    if args.tier != "portable":
        out.mkdir(parents=True, exist_ok=True)
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"report: {out / 'report.json'}")
    if args.report:
        Path(args.report).write_text(json.dumps(report, indent=2) + "\n")
    return EXIT_CODES[verdict]


def main(argv=None):
    parser = argparse.ArgumentParser(prog="harness/acceptance.sh", description=__doc__)
    parser.add_argument("tier", choices=["portable", "smoke", "full"])
    parser.add_argument("--root", default=os.environ.get("SEEDBOX_ROOT", str(Path.home() / "seedbox-sim")),
                        help="working directory with runs/ (default $SEEDBOX_ROOT or ~/seedbox-sim)")
    parser.add_argument("--cpus", help="cores for runs, e.g. 2-7 (default: 2-5)")
    parser.add_argument("--vmm", default=os.environ.get("VMM", str(SIM.parent / "crates/seedbox-vmm/target/release/seedbox-vmm")),
                        help="VMM build under test")
    parser.add_argument("--artifacts", help="directory containing the controlled bzImage; guest images come from YAML")
    parser.add_argument("--kernel", help="controlled kernel (default: ARTIFACTS/bzImage or ROOT/artifacts/bzImage)")
    parser.add_argument("--cases", default=str(CASES))
    parser.add_argument("--seedbox", help="seedbox CLI to test (default: build this checkout)")
    parser.add_argument("--lease-wait", type=int, default=0, help="seconds to wait for busy cores")
    parser.add_argument("--skip-portable", action="store_true", help="smoke/full: skip the portable tier")
    parser.add_argument("--report", help="also write the JSON report here")
    parser.add_argument("--performance-only", metavar="OLD_VMM",
                        help="declare a performance-only change; compare against this build")
    args = parser.parse_args(argv)
    interrupted = False
    def cancel(_signum, _frame):
        nonlocal interrupted
        if not interrupted:
            interrupted = True
            raise KeyboardInterrupt
    previous = {sig: signal.signal(sig, cancel) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        return suite(args)
    except KeyboardInterrupt:
        print("acceptance cancelled after CLI cleanup", file=sys.stderr)
        return 130
    except Exception:
        traceback.print_exc()
        return EXIT_CODES["UNRESOLVED"]
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    sys.exit(main())
