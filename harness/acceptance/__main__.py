"""Seedbox acceptance suite (issue #4). Run through harness/acceptance.sh."""
import argparse
import dataclasses
import datetime
import json
import os
from pathlib import Path
import platform
import sys
import time
import traceback

from . import checks
from .engine import EXIT_CODES, Outcome, execute, overall, render
from .launcher import BatchLauncher, cpu_list
from .portable import portable

SIM = Path(__file__).resolve().parents[1]
PINS = Path(__file__).with_name("pins.json")
DEFAULT_CPUS = {"smoke": "2-7", "full": "2-12"}
PIN_KEYS = {"smoke": ["replay", "stop_placement", "race", "checker_control", "fault_effects"]}
PIN_KEYS["full"] = PIN_KEYS["smoke"] + ["known_failing", "fixed_control"]


def log(message):
    print(f"[{time.strftime('%H:%M:%S')}] {message}", flush=True)


def make_launcher(args, pins, out):
    engines = {None: Path(args.vmm).resolve()}
    if args.performance_only:
        engines[checks.BASELINE] = Path(args.performance_only).resolve()
    return BatchLauncher(SIM, args.root, pins["scenarios"], cpu_list(args.cpus or DEFAULT_CPUS.get(args.tier, "2-7")),
                         out, engines, args.artifacts, args.timeout, args.lease_wait, log=log)


def suite(args):
    pins = json.loads(Path(args.pins).read_text())
    started = time.monotonic()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = Path(args.root).resolve() / "runs" / f"acceptance-{args.tier}-{stamp}-{os.getpid()}"
    report = {"tier": args.tier, "started": stamp, "host": platform.node(), "pins": pins["build"],
              "declared": {"performance_only": bool(args.performance_only),
                           "semantics_change": args.semantics_change}}
    outcomes = []
    if args.tier == "portable" or not args.skip_portable:
        outcomes += [Outcome(f"portable: {name}", verdict) for name, verdict in portable(SIM.parent, log)]
    if args.tier != "portable" and not any(o.verdict.status == checks.FAIL for o in outcomes):
        selected = checks.SMOKE if args.tier == "smoke" else checks.FULL
        launcher = make_launcher(args, pins, out)
        scenarios = sorted({pins[key]["scenario"] for key in PIN_KEYS[args.tier]})
        missing = launcher.missing_inputs(scenarios)
        if missing:
            print("missing inputs (build them first; see README.md):\n  " + "\n  ".join(missing), file=sys.stderr)
            return EXIT_CODES["UNRESOLVED"]
        out.mkdir(parents=True, exist_ok=True)
        log(f"hashing inputs; evidence in {out}")
        report["inputs"] = {s: launcher.identity(s) for s in scenarios}
        report["engines"] = launcher.engine_hashes()
        ctx = checks.Context(pins, launcher.identity, args.semantics_change, bool(args.performance_only))
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


def pin(args):
    """Record a (re-)hunted known-failing seed with the identity of the current inputs."""
    pins = json.loads(Path(args.pins).read_text())
    entry = pins["known_failing"]
    out = Path(args.root).resolve() / "runs"
    launcher = BatchLauncher(SIM, args.root, pins["scenarios"], [0], out, {None: Path(args.vmm)},
                             args.artifacts, siblings=lambda cpu: {cpu})
    if args.seed is None or not args.witness:
        sys.exit("pin: --seed and --witness are both required")
    entry["seed"], entry["world_witness"] = args.seed, args.witness
    entry["identity"] = launcher.identity(entry["scenario"])
    if args.build:
        pins["build"] = args.build
    Path(args.pins).write_text(json.dumps(pins, indent=2) + "\n")
    print(json.dumps(entry, indent=2))
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(prog="harness/acceptance.sh", description=__doc__)
    parser.add_argument("tier", choices=["portable", "smoke", "full", "pin"])
    parser.add_argument("--root", default=os.environ.get("SEEDBOX_ROOT", str(Path.home() / "seedbox-sim")),
                        help="working directory with runs/ (default $SEEDBOX_ROOT or ~/seedbox-sim)")
    parser.add_argument("--cpus", help="cores for runs, e.g. 2-7 (default: smoke 2-7, full 2-12)")
    parser.add_argument("--vmm", default=os.environ.get("VMM", str(SIM.parent / "crates/seedbox-vmm/target/release/seedbox-vmm")),
                        help="VMM build under test")
    parser.add_argument("--artifacts", help="take kernel/initrds by name from this directory")
    parser.add_argument("--pins", default=str(PINS))
    parser.add_argument("--timeout", type=int, default=1800, help="per-run host timeout (s)")
    parser.add_argument("--lease-wait", type=int, default=0, help="seconds to wait for busy cores")
    parser.add_argument("--skip-portable", action="store_true", help="smoke/full: skip the portable tier")
    parser.add_argument("--report", help="also write the JSON report here")
    declared = parser.add_mutually_exclusive_group()
    declared.add_argument("--performance-only", metavar="OLD_VMM",
                          help="declare a performance-only change; item 7 compares against this build")
    declared.add_argument("--semantics-change", action="store_true",
                          help="declare a change of run identity (kernel/ABI or semantics)")
    parser.add_argument("--seed", type=int, help="pin: new known-failing seed")
    parser.add_argument("--witness", help="pin: its world witness")
    parser.add_argument("--build", help="pin: description of the build the pins belong to")
    args = parser.parse_args(argv)
    if args.tier == "pin":
        return pin(args)
    try:
        return suite(args)
    except Exception:
        traceback.print_exc()
        return EXIT_CODES["UNRESOLVED"]


if __name__ == "__main__":
    sys.exit(main())
