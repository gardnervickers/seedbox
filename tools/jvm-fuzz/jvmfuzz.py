#!/usr/bin/env python3
"""Generate small JVM programs, run them natively and under Seedbox, and compare.

Workflow (see README.md):

    jvmfuzz.py gen    --seed 1 --out CORPUS     # sources, cases and batches
    jvmfuzz.py build  CORPUS                    # one image: javac stage + glibc JRE
    jvmfuzz.py native CORPUS --runs 2           # docker/podman, one pinned CPU
    jvmfuzz.py sim    CORPUS --replay           # seedbox run (+ seedbox replay)
    jvmfuzz.py report CORPUS                    # verdicts and diffs

Every case is one JVM process: (program, arguments, JVM flag profile). Cases are
grouped into batches; one batch is one container run or one simulation, whose
oracle runs the cases in order and frames each case's output.
"""

import argparse
import concurrent.futures
import difflib
import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

import randprog

HERE = Path(__file__).resolve().parent

BASE_FLAGS = ["-Xmx384m", "-Xss1m", "-XX:-UsePerfData"]

# JVM flag profiles. Each exercises a different code generator, collector,
# atomic-instruction choice or code-cache behavior. All are product flags (or
# diagnostic flags unlocked explicitly) of the Temurin 21 JRE.
PROFILES = {
    "default": [],
    "int": ["-Xint"],
    "c1": ["-XX:TieredStopAtLevel=1"],
    "c2": ["-XX:-TieredCompilation"],
    "batch": ["-Xbatch", "-XX:CICompilerCount=2"],
    "xcomp": ["-Xcomp", "-XX:TieredStopAtLevel=1"],
    "parallel": ["-XX:+UseParallelGC"],
    "g1": ["-XX:+UseG1GC"],
    "zgc": ["-XX:+UseZGC", "-XX:+ZGenerational"],
    "zgc-single": ["-XX:+UseZGC"],
    "shenandoah": ["-XX:+UseShenandoahGC"],
    "nolse": ["-XX:-UseLSE"],
    "nocoops": ["-XX:-UseCompressedOops", "-XX:-UseCompressedClassPointers"],
    "smallcode": ["-XX:ReservedCodeCacheSize=16m", "-XX:-SegmentedCodeCache"],
    "noshare": ["-Xshare:off"],
    "safepoint": ["-XX:+UnlockDiagnosticVMOptions", "-XX:GuaranteedSafepointInterval=1"],
    "simd": ["-XX:+UseSIMDForMemoryOps", "-XX:+UseBlockZeroing"],
    "nointrinsics": ["-XX:+UnlockDiagnosticVMOptions", "-XX:-UseAES", "-XX:-UseSHA", "-XX:-UseCRC32", "-XX:-UseCRC32CIntrinsics", "-XX:-UseAdler32Intrinsics",
                     "-XX:-UseMultiplyToLenIntrinsic", "-XX:-UseSquareToLenIntrinsic", "-XX:-UseMulAddIntrinsic",
                     "-XX:-UseMontgomeryMultiplyIntrinsic", "-XX:-UseMontgomerySquareIntrinsic", "-XX:-UseVectorizedMismatchIntrinsic"],
    # The guest kernel has no transparent huge pages, so THP flags only add a warning there.
    "pretouch": ["-XX:+AlwaysPreTouch", "-Xms384m"],
    "c2stress": ["-XX:+UnlockDiagnosticVMOptions", "-XX:+StressLCM", "-XX:+StressGCM", "-XX:+StressIGVN", "-XX:+StressCCP",
                 "-XX:StressSeed=@SEED@"],
    "smallstack": ["-Xss256k", "-XX:MaxJavaStackTraceDepth=64"],
    "noinline": ["-XX:-Inline", "-XX:-UseOnStackReplacement"],
    # Monitors always inflated (no mark-word CAS), or JDK 21's lightweight locking.
    "lm0": ["-XX:+UnlockExperimentalVMOptions", "-XX:LockingMode=0"],
    "lm2": ["-XX:+UnlockExperimentalVMOptions", "-XX:LockingMode=2"],
    # Thread-register traffic: a safepoint poll every loop iteration and tiny TLABs.
    "x28storm": ["-XX:+UseCountedLoopSafepoints", "-XX:LoopStripMiningIter=1", "-XX:TLABSize=2k",
                 "-XX:-ResizeTLAB", "-XX:MinTLABSize=1k"],
    # Every implicit null check stays a signal, an uncommon trap and a recompilation.
    "deoptstorm": ["-XX:PerBytecodeTrapLimit=1000000", "-XX:PerMethodTrapLimit=1000000",
                   "-XX:PerMethodRecompilationCutoff=-1", "-XX:-OmitStackTraceInFastThrow"],
    # Scalar paths of the hand-written copy, compare and zeroing stubs.
    "stubflags": ["-XX:-UseSIMDForMemoryOps", "-XX:-UseSIMDForArrayEquals", "-XX:-UseSimpleArrayEquals",
                  "-XX:+AvoidUnalignedAccesses", "-XX:BlockZeroingLowLimit=64"],
    # Heap verification around every collection localizes corruption to the first bad GC.
    "verifygc": ["-XX:+UnlockDiagnosticVMOptions", "-XX:+VerifyBeforeGC", "-XX:+VerifyAfterGC"],
    # A stuck safepoint aborts with a report instead of hanging.
    "sptimeout": ["-XX:+UnlockDiagnosticVMOptions", "-XX:+SafepointTimeout", "-XX:SafepointTimeoutDelay=20000",
                  "-XX:+AbortVMOnSafepointTimeout"],
}

# Probes and what they need. "det": the output is expected to be identical to a
# native run. Non-deterministic probes are only compared with their replay.
PROBES = {
    "Arith": {}, "JitDeopt": {}, "Exceptions": {}, "GcStress": {}, "References": {}, "StackWalk": {},
    "Monitors": {}, "JucLocks": {}, "Atomics": {}, "Queues": {}, "Executors": {}, "VirtualThreads": {},
    "Interrupts": {}, "Reflection": {}, "Indy": {}, "ClassLoading": {}, "Strings": {}, "TextFormat": {},
    "ArraysSimd": {}, "VectorApi": {"extra": ["--add-modules", "jdk.incubator.vector"]},
    "Crypto": {}, "BigNumbers": {}, "Collections": {}, "Serialization": {}, "Compression": {},
    "FileIo": {}, "OffHeap": {}, "Sockets": {}, "Http": {},
    # Each launch mechanism with background compilation, without a JIT, and with synchronous C1
    # only (no code is written while a child is spawned).
    "Processes": {"isolate": True, "variants": [[f"-Djdk.lang.Process.launchMechanism={m}"] + jit
                                                for m in ("POSIX_SPAWN", "FORK", "VFORK")
                                                for jit in ([], ["-Xint"], ["-Xbatch", "-XX:CICompilerCount=1",
                                                                             "-XX:TieredStopAtLevel=1"])]},
    "Signals": {"isolate": True}, "Safepoints": {}, "Time": {}, "Preemption": {},
    # Under lock-mode and heap-verification profiles its JIT patches code often enough that
    # translator synch-alls make it take about 230 s of simulated time.
    "SignalBomb": {"preview": True, "isolate": True, "timeout": 600}, "InterruptedSyscalls": {"preview": True, "isolate": True},
    "AsyncClose": {"isolate": True}, "DeoptStorm": {}, "X28Storm": {}, "FpRegPressure": {}, "ThreadChurn": {},
    "AddressSpace": {"det": False},
    "IcStorm": {}, "MappedSigbus": {}, "ArrayCopyStubs": {}, "SleepSort": {"det": False}, "Clocks": {}, "MmapChurn": {},
    "VthreadStorm": {"extra": ["-Djdk.virtualThreadScheduler.parallelism=4"]},
    # "exit_codes": the expected exit status for each argument variant.
    "SigquitAttach": {"preview": True}, "Ffm": {"preview": True}, "SafepointsNative": {}, "Durability": {}, "ClassStorm": {},
    "CodeReuse": {"extra": ["-XX:ReservedCodeCacheSize=6m", "-XX:-SegmentedCodeCache"]},
    # Deliberate deaths: the exit status must match; the report is compared with the replay only.
    "Crash": {"det": False, "isolate": True, "extra": ["-Xmx96m", "-XX:+CrashOnOutOfMemoryError"],
              "arg_variants": ["segv-int", "segv-c2", "oom", "halthook"],
              "exit_codes": {"segv-int": 134, "segv-c2": 134, "oom": 134, "halthook": 3}},
    "Redefine": {"extra": ["-javaagent:/fuzz/classes/agent.jar"]}, "CodeChurn": {"whitebox": True},
    "ExitMatrix": {"isolate": True, "arg_variants": ["daemons", "exit7", "halt5", "uncaught"],
                   "exit_codes": {"daemons": 0, "exit7": 7, "halt5": 5, "uncaught": 1}},
    # Native AArch64 probes (probes/c/cfz.c), one test each, run without a JVM.
    **{f"C.{name}": {"native": name} for name in (
        "casp", "lse2", "ldapr", "dczva", "carry", "fpcr", "tbi", "rwrx", "mremapcode", "membarrier",
        "dlopen", "segvbarrier", "siglongjmp", "ucontext", "pthreadcancel", "seccomp", "autodisarm",
        "signalfd", "pselect", "cputimer", "futexwaitv", "epolloneshot", "pidfd", "clone3", "memfdexec",
        "vmops", "timerfd", "rseq", "iouring", "ptrace", "wipeonfork", "scmrights", "ifunc")},
    # What the host offers; differences from native describe the guest ("info").
    "C.capabilities": {"native": "capabilities", "info": True},
    "Jfr": {"arg_variants": ["all 1", "exec 1", "exec 10", "alloc,monitor,park 1", "none 1",
                             "exec 1 soe", "exec 1 npe", "exec 1 alloc", "exec 5 lock", "exec 1 native",
                             "exec,threaddump 1 vthread", "native 1 native", "threaddump 1 lock",
                             "exceptions 1 npe", "threaddump,native 2 soe"]},
    "Race": {"det": False},
}

# Open translator bugs, as case-id patterns (see repros/README.md).
# `gen --skip-known` leaves these out so a run reports only new problems.
KNOWN_ISSUES = {
}

CASE_TIMEOUT = 240
SIM_MEMORY_MIB = 2048


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, **kw)


def engine_env():
    """The caller's environment; DOCKER_HOST selects the container engine."""
    return dict(os.environ)


def load(corpus):
    return json.loads((Path(corpus) / "corpus.json").read_text())


# gen -----------------------------------------------------------------------

def cmd_gen(a):
    out = Path(a.out)
    if out.exists():
        sys.exit(f"error: {out} exists; choose a new corpus directory")
    rnd = random.Random(a.seed)
    src = out / "src"
    src.mkdir(parents=True)
    for f in sorted((HERE / "probes").glob("*.java")):
        shutil.copy(f, src / f.name)
    # Preview probes use the foreign-function API, still a preview in JDK 21; WhiteBox probes
    # compile against HotSpot's test API.
    for sub in ("preview", "whitebox", "c"):
        (src / sub).mkdir()
        for f in sorted(list((HERE / "probes" / sub).glob("*.java")) + list((HERE / "probes" / sub).glob("*.c"))):
            shutil.copy(f, src / sub / f.name)
    instances = []  # (main, args, extra flags, det, expected exit)
    probes = [p for p in PROBES if not a.only or re.search(a.only, p)]
    for name in probes:
        spec = PROBES[name]
        for k in range(a.probe_seeds):
            pseed = rnd.randrange(1, 10**6)
            extra = list(spec.get("extra", [])) + (["--enable-preview", "--enable-native-access=ALL-UNNAMED"] if spec.get("preview") else [])
            if spec.get("whitebox"):
                extra += ["-Xbootclasspath/a:/fuzz/wb", "-XX:+UnlockDiagnosticVMOptions", "-XX:+WhiteBoxAPI"]
            if a.jit_noise:
                extra.append("-Dfz.jitnoise=true")
            if "variants" in spec:
                extra += rnd.choice(spec["variants"])
            args = f"{pseed} {a.scale}"
            if "native" in spec:
                instances.append((name, f"{spec['native']} {args}", None, spec.get("det", True), 0))
                continue
            expect_exit = 0
            if "arg_variants" in spec:
                variant = rnd.choice(spec["arg_variants"])
                args += " " + variant
                expect_exit = spec.get("exit_codes", {}).get(variant, 0)
            instances.append((name, args, extra, spec.get("det", True), expect_exit))
    for k in range(a.random):
        rseed = rnd.randrange(1, 10**9)
        cls = f"R{rseed}"
        (src / f"{cls}.java").write_text(randprog.generate(rseed, cls))
        instances.append((cls, str(rnd.randrange(10**6)), [], True, 0))
    profiles = [p for p in PROFILES if not a.profiles or p in a.profiles.split(",")]
    cases = []
    for main, args, extra, det, expect_exit in instances:
        if extra is None:  # a native probe: no JVM, so no flag profiles
            seed_tag = args.split()[1]
            cases.append({"id": f"{main}.{seed_tag}.native", "main": "/fuzz/bin/cfz", "args": args,
                          "profile": "native", "flags": ["-"], "det": det, "timeout": CASE_TIMEOUT,
                          "exit": 0, "info": PROBES[main].get("info", False), "probe": main})
            continue
        chosen = []
        if "default" in profiles and rnd.random() < 0.5:
            chosen.append("default")
        others = [p for p in profiles if p not in chosen]
        chosen += rnd.sample(others, min(len(others), a.profiles_per_instance - len(chosen)))
        for prof in chosen:
            seed_tag = "_".join([args.split()[0]] + args.split()[2:])
            cid = re.sub(r"[^A-Za-z0-9_.-]", "_", f"{main}.{seed_tag}.{prof}")
            flags = [f.replace("@SEED@", str(rnd.randrange(1, 2**31))) for f in BASE_FLAGS + PROFILES[prof] + extra]
            if a.skip_known and any(re.search(k, cid) for k in KNOWN_ISSUES):
                continue
            cases.append({"id": cid, "main": main, "args": args, "profile": prof,
                          "flags": flags, "det": det, "timeout": PROBES.get(main, {}).get("timeout", CASE_TIMEOUT),
                          "exit": expect_exit})
    rnd.shuffle(cases)
    # Probes that can hang get a simulation of their own, so a hang cuts no other case.
    isolated = [c for c in cases if PROBES.get(c["main"], {}).get("isolate")]
    shared = [c for c in cases if not PROBES.get(c["main"], {}).get("isolate")]
    groups = [shared[i:i + a.batch_size] for i in range(0, len(shared), a.batch_size)] + [[c] for c in isolated]
    batches = {f"b{i:03d}": [c["id"] for c in group] for i, group in enumerate(groups)}
    bdir = out / "batches"
    bdir.mkdir()
    by_id = {c["id"]: c for c in cases}
    for b, ids in batches.items():
        lines = [f"{c['id']}\t{c['timeout']}\t{' '.join(c['flags'])}\t{c['main']}\t{c['args']}"
                 for c in (by_id[i] for i in ids)]
        (bdir / f"{b}.tsv").write_text("\n".join(lines) + "\n")
    shutil.copy(HERE / "runtime" / "Dockerfile", out / "Dockerfile")
    shutil.copy(HERE / "runtime" / "run-batch.sh", out / "run-batch.sh")
    digest = hashlib.sha256()
    for f in sorted(list(src.glob("**/*.java")) + list(src.glob("**/*.c")) + list(bdir.iterdir())
                    + [out / "Dockerfile", out / "run-batch.sh"]):
        digest.update(f.name.encode() + f.read_bytes())
    tag = digest.hexdigest()[:12]
    meta = {"seed": a.seed, "tag": tag, "image": f"jvmfuzz:{tag}", "cases": cases, "batches": batches}
    (out / "corpus.json").write_text(json.dumps(meta, indent=1))
    print(f"GENERATED: {out} ({len(cases)} cases, {len(batches)} batches, image jvmfuzz:{tag})")


# build ---------------------------------------------------------------------

def cmd_build(a):
    meta = load(a.corpus)
    sh(["docker", "buildx", "build", "--load", "-t", meta["image"], a.corpus], env=engine_env())
    print(f"BUILT: {meta['image']}")


# native ----------------------------------------------------------------------

def batch_timeout(meta, b):
    by_id = {c["id"]: c for c in meta["cases"]}
    return sum(by_id[i]["timeout"] for i in meta["batches"][b]) + 120


def cmd_native(a):
    meta = load(a.corpus)
    out = Path(a.corpus) / "native"
    jobs = []
    for run in range(1, a.runs + 1):
        for b in meta["batches"]:
            dest = out / f"r{run}" / f"{b}.out"
            if dest.exists() and not a.force:
                continue
            jobs.append((run, b, dest))
    cpus = list(range(a.jobs))

    def one(job, cpu):
        run, b, dest = job
        dest.parent.mkdir(parents=True, exist_ok=True)
        name = f"jvmfuzz-{meta['tag']}-{b}-r{run}"
        subprocess.run(["docker", "rm", "-f", name], env=engine_env(), capture_output=True)
        t0 = time.time()
        try:
            confine = ["--security-opt", "seccomp=unconfined"] if a.unconfined else []
            p = subprocess.run(["docker", "run", "--rm", "--name", name, f"--cpuset-cpus={cpu}", f"--memory={SIM_MEMORY_MIB}m",
                                f"--memory-swap={SIM_MEMORY_MIB}m", *confine, meta["image"], b],
                               env=engine_env(), capture_output=True, text=True, timeout=batch_timeout(meta, b))
            text = p.stdout + (f"\n@@STDERR\n{p.stderr}" if p.stderr.strip() else "")
        except subprocess.TimeoutExpired as e:
            subprocess.run(["docker", "rm", "-f", name], env=engine_env(), capture_output=True)
            text = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
        dest.write_text(text)
        return f"native r{run} {b} {time.time() - t0:.0f}s"

    with concurrent.futures.ThreadPoolExecutor(a.jobs) as ex:
        free = list(cpus)
        futs = {}
        pending = list(jobs)
        while pending or futs:
            while pending and free:
                cpu = free.pop()
                job = pending.pop(0)
                futs[ex.submit(one, job, cpu)] = cpu
            done, _ = concurrent.futures.wait(futs, return_when=concurrent.futures.FIRST_COMPLETED)
            for f in done:
                free.append(futs.pop(f))
                print(f.result(), file=sys.stderr, flush=True)


# sim -----------------------------------------------------------------------

STATUS = re.compile(r"^([A-Z_]+): ([a-z0-9]+)\s*$", re.M)


def seedbox(a, *args, timeout=None):
    return subprocess.run([a.seedbox, *args], env=engine_env(), capture_output=True, text=True, timeout=timeout)


def sim_batch(a, meta, b, index):
    d = Path(a.corpus) / ("sim-" + a.variant if a.variant else "sim") / b
    d.mkdir(parents=True, exist_ok=True)
    result = d / "result.json"
    if result.exists() and not a.force:
        prior = json.loads(result.read_text())
        if not (a.replay and "replay_id" not in prior):
            return f"sim {b} cached"
    project = {
        "name": f"jvmfuzz-{meta['tag']}-{b}",
        "machines": {},
        "oracle": {"image": meta["image"], "memoryMiB": SIM_MEMORY_MIB, "command": [b]},
    }
    if a.jitter:
        project["oracle"]["cpu"] = {"jitter": f"{a.jitter}%"}
    (d / "seedbox.yaml").write_text(json.dumps(project, indent=1) + "\n")
    info = json.loads(result.read_text()) if result.exists() and not a.force else {}
    t0 = time.time()
    if "id" not in info:
        seed = (meta["seed"] * 1000 + index + a.seed_offset * 7919) % (2**31)
        p = seedbox(a, "run", str(d), "--seed", str(seed), "--wall-time", a.wall_time)
        (d / "run.stdout").write_text(p.stdout)
        (d / "run.stderr").write_text(p.stderr)
        m = STATUS.search(p.stdout)
        info = {"seed": seed, "status": m.group(1) if m else "ERROR", "id": m.group(2) if m else None,
                "exit": p.returncode, "wall_seconds": round(time.time() - t0, 1)}
        if info["id"]:
            logs = seedbox(a, "logs", info["id"], "oracle", "--stream", "stdout")
            (d / "run.out").write_text(logs.stdout)
        result.write_text(json.dumps(info, indent=1))
    if a.replay and info.get("id") and "replay_id" not in info:
        t1 = time.time()
        p = seedbox(a, "replay", info["id"])
        (d / "replay.stdout").write_text(p.stdout)
        (d / "replay.stderr").write_text(p.stderr)
        m = STATUS.search(p.stdout)
        info.update({"replay_status": m.group(1) if m else "ERROR", "replay_id": m.group(2) if m else None,
                     "replay_wall_seconds": round(time.time() - t1, 1)})
        if info["replay_id"]:
            logs = seedbox(a, "logs", info["replay_id"], "oracle", "--stream", "stdout")
            (d / "replay.out").write_text(logs.stdout)
        result.write_text(json.dumps(info, indent=1))
    return f"sim {b} {info.get('status')} {info.get('id')} replay={info.get('replay_status', '-')} {time.time() - t0:.0f}s"


def cmd_sim(a):
    meta = load(a.corpus)
    names = list(meta["batches"])
    if a.batches:
        names = [b for b in names if b in a.batches.split(",")]
    with concurrent.futures.ThreadPoolExecutor(a.jobs) as ex:
        futs = [ex.submit(sim_batch, a, meta, b, list(meta["batches"]).index(b)) for b in names]
        for f in concurrent.futures.as_completed(futs):
            print(f.result(), file=sys.stderr, flush=True)


# report --------------------------------------------------------------------

def parse(path):
    """Returns {case id: {"lines", "exit", "hserr"}} for the framed output in ``path``."""
    cases = {}
    if not path.exists():
        return cases
    cur = None
    # A truncated crash report can leave the next frame marker mid-line.
    text = re.sub(r"(?<!^)(?<!\n)(@@(?:EXIT|CASE|DONE) )", r"\n\1", path.read_text(errors="replace"))
    for line in text.splitlines():
        if line.startswith("@@CASE "):
            cur = {"lines": [], "exit": None, "hserr": []}
            cases[line[7:].strip()] = cur
        elif line.startswith("@@EXIT "):
            parts = line.split()
            if parts[1] in cases:
                cases[parts[1]]["exit"] = int(parts[2])
            cur = None
        elif line.startswith("@@| "):
            if cur is not None:
                cur["hserr"].append(line[4:])
        elif line.startswith("@@"):
            continue
        elif cur is not None:
            cur["lines"].append(line)
    return cases


def same(x, y):
    return x is not None and y is not None and x["exit"] == y["exit"] and x["lines"] == y["lines"]


def diff(x, y, a_name, b_name, limit=40):
    d = list(difflib.unified_diff(x["lines"] + [f"<exit {x['exit']}>"], y["lines"] + [f"<exit {y['exit']}>"],
                                  a_name, b_name, lineterm="", n=1))
    return d[:limit] + ([f"... {len(d) - limit} more diff lines"] if len(d) > limit else [])


def failure_kind(r):
    if r["exit"] is None:
        return "SIM_CUT"  # started but never finished: the simulation ended mid-case
    if r["hserr"] or r["exit"] in (134, 139) or any(l.startswith("# A fatal error") for l in r["lines"]):
        return "SIM_CRASH"
    if r["exit"] == 137:
        return "SIM_TIMEOUT"
    return "SIM_FAIL"


def sim_dirs(corpus):
    """The main simulation directory first, then variants (sim-<name>)."""
    dirs = [corpus / "sim"] if (corpus / "sim").exists() else []
    return dirs + sorted(d for d in corpus.glob("sim-*") if d.is_dir())


def case_verdict(c, ns, s, p, simulated):
    """Verdict and notes for one case in one simulation directory."""
    stable = len(ns) >= 1 and all(same(ns[0], n) for n in ns[1:])
    want = c.get("exit", 0)
    native_ok = bool(ns) and all(n["exit"] == want for n in ns)
    notes = []
    if not ns:
        verdict = "NO_NATIVE"
    elif not native_ok:
        verdict = "NATIVE_FAIL"
        notes += ns[0]["lines"][-15:]
    else:
        verdict = None
    if s is None:
        v2 = "SIM_NOT_REACHED" if simulated else "NO_SIM"
    elif s["exit"] != want:
        v2 = failure_kind(s)
        notes += s["lines"][-25:] + s["hserr"][:60]
    elif c["det"] and stable and ns:
        v2 = "OK" if same(ns[0], s) else "MISMATCH"
        if v2 == "MISMATCH":
            notes += diff(ns[0], s, "native", "sim")
    else:
        v2 = "OK_NONDET"
    if p is not None and s is not None and not same(s, p):
        v2 = "REPLAY_DIVERGE"
        notes += diff(s, p, "sim", "replay")
    if verdict is None or verdict == "NATIVE_FAIL" and v2.startswith("SIM"):
        verdict = v2 if verdict is None else f"{verdict}+{v2}"
    if c.get("info") and verdict == "MISMATCH":
        verdict = "GUEST_DIFF"  # informational: what the guest offers differs from the host
    if c["det"] and native_ok and not stable and verdict.startswith("OK"):
        verdict = "NATIVE_UNSTABLE"
        notes += diff(ns[0], ns[1], "native-r1", "native-r2")
    return verdict, notes


def cmd_report(a):
    corpus = Path(a.corpus)
    meta = load(corpus)
    runs = sorted((corpus / "native").glob("r*")) if (corpus / "native").exists() else []
    native = {}
    for run in runs:
        for b in meta["batches"]:
            for cid, r in parse(run / f"{b}.out").items():
                native.setdefault(cid, []).append(r)
    batch_of = {cid: b for b, ids in meta["batches"].items() for cid in ids}
    dirs = sim_dirs(corpus) or [corpus / "sim"]
    rows, details, simmeta = [], [], {}
    for d in dirs:
        label = d.name
        sim, replay, meta_d = {}, {}, {}
        for b in meta["batches"]:
            sim.update(parse(d / b / "run.out"))
            replay.update(parse(d / b / "replay.out"))
            if (d / b / "result.json").exists():
                meta_d[b] = json.loads((d / b / "result.json").read_text())
        simmeta[label] = meta_d
        for c in meta["cases"]:
            cid = c["id"]
            p = replay.get(cid)
            verdict, notes = case_verdict(c, native.get(cid, []), sim.get(cid), p, batch_of[cid] in meta_d)
            rows.append({"id": cid, "batch": batch_of[cid], "profile": c["profile"], "main": c["main"],
                         "sim": label, "verdict": verdict, "replayed": p is not None})
            if not verdict.startswith("OK") and verdict != "NO_SIM":
                details.append((cid, verdict, notes, label))
    # Profile invariance: one program and argument list must print the same thing under every flag profile.
    groups = {}
    for c in meta["cases"]:
        if c["det"] and native.get(c["id"]) and native[c["id"]][0]["exit"] == c.get("exit", 0):
            groups.setdefault((c["main"], c["args"]), []).append(c)
    variants = []
    for (main, args), cs in groups.items():
        base = native[cs[0]["id"]][0]
        for c in cs[1:]:
            other = native[c["id"]][0]
            if not same(base, other):
                variants.append((cs[0]["id"], c["id"], diff(base, other, cs[0]["profile"], c["profile"], 20)))
    counts = {}
    for r in rows:
        key = r["verdict"] if r["sim"] == "sim" else f"{r['verdict']} ({r['sim']})"
        counts[key] = counts.get(key, 0) + 1
    lines = [f"# JVM fuzz report: {corpus.name}", "",
             f"Corpus seed {meta['seed']}, image `{meta['image']}`, {len(meta['cases'])} cases in {len(meta['batches'])} batches; "
             f"{len(runs)} native run(s) per case.", "", "| Verdict | Cases |", "| --- | --- |"]
    lines += [f"| {k} | {v} |" for k, v in sorted(counts.items())]
    lines += ["", "## Simulations", "", "| Run | Batch | Status | Simulation | Wall s | Replay | Replay simulation |",
              "| --- | --- | --- | --- | --- | --- | --- |"]
    for label, per_batch in simmeta.items():
        for b, m in per_batch.items():
            lines.append(f"| {label} | {b} | {m.get('status')} | {m.get('id')} | {m.get('wall_seconds')} | "
                         f"{m.get('replay_status', '-')} | {m.get('replay_id', '-')} |")
    if details:
        lines += ["", "## Findings", ""]
        for cid, verdict, notes, label in details:
            c = next(x for x in meta["cases"] if x["id"] == cid)
            lines += [f"### {verdict}: `{cid}` ({label}/{batch_of[cid]})", "",
                      f"`java {' '.join(c['flags'])} {c['main']} {c['args']}`", "", "```"] + notes + ["```", ""]
    if variants:
        lines += ["", "## Native profile variance (probe nondeterminism or JIT bug)", ""]
        for x, y, d in variants:
            lines += [f"### `{x}` vs `{y}`", "", "```"] + d + ["```", ""]
    (corpus / "report.md").write_text("\n".join(lines) + "\n")
    (corpus / "report.json").write_text(json.dumps({"counts": counts, "rows": rows, "simulations": simmeta,
                                                     "profile_variants": [[x, y] for x, y, _ in variants]}, indent=1))
    for k, v in sorted(counts.items()):
        print(f"{k}: {v}")
    print(f"profile variants: {len(variants)}")
    print(f"REPORT: {corpus / 'report.md'}")
    bad = [r for r in rows if not r["verdict"].startswith("OK") and r["verdict"] != "NO_SIM"]
    return 1 if bad or variants else 0


# jcstress ------------------------------------------------------------------

JCSTRESS_GROUPS = {
    "api": r"samples\.api\.",
    "jmm-basic": r"samples\.jmm\.basic\.",
    "jmm-advanced": r"samples\.jmm\.advanced\.",
    "primitives": r"samples\.primitives\.",
    "problems": r"samples\.problems\.",
}
JCSTRESS_RESULTS = re.compile(r"\(Results: (\d+) planned; (\d+) passed, (\d+) failed, (\d+) soft errs, (\d+) hard errs\)")


def jcstress_summary(path):
    """Verdict-level summary of one jcstress run; outcome counts vary between runs and are ignored."""
    if not path.exists():
        return None
    text = path.read_text(errors="replace")
    m = JCSTRESS_RESULTS.findall(text)
    exit_m = re.findall(r"@@EXIT jcstress (\d+)", text)
    failed = re.findall(r"^\s*\.*\s*\[(FAILED|ERROR)\] (\S+)", text, re.M)
    return {"planned": int(m[-1][0]) if m else None, "passed": int(m[-1][1]) if m else None,
            "failed": int(m[-1][2]) if m else None, "errors": int(m[-1][3]) + int(m[-1][4]) if m else None,
            "exit": int(exit_m[-1]) if exit_m else None, "crash": "@@HSERR" in text or "# A fatal error" in text,
            "failing_tests": sorted({t for _, t in failed})}


def cmd_jcstress(a):
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    groups = {k: v for k, v in JCSTRESS_GROUPS.items() if not a.groups or k in a.groups.split(",")}
    args = [a.suite, None, "-m", a.mode, "-f", "1", "-fsm", "0", "-af", "NONE", "-spinStyle", "THREAD_YIELD"]

    def native(name, regex):
        dest = out / name / "native.out"
        dest.parent.mkdir(parents=True, exist_ok=True)
        if not dest.exists() or a.force:
            argv = list(args)
            argv[1] = regex
            p = subprocess.run(["docker", "run", "--rm", "--cpuset-cpus=0", f"--memory={SIM_MEMORY_MIB}m", a.image, *argv],
                               env=engine_env(), capture_output=True, text=True)
            dest.write_text(p.stdout + p.stderr)
        return f"jcstress native {name}: {jcstress_summary(dest)}"

    def sim(name, regex, index):
        d = out / name
        d.mkdir(parents=True, exist_ok=True)
        argv = list(args)
        argv[1] = regex
        project = {"name": f"jvmfuzz-jcstress-{name}", "machines": {},
                   "oracle": {"image": a.image, "memoryMiB": SIM_MEMORY_MIB, "command": argv}}
        (d / "seedbox.yaml").write_text(json.dumps(project, indent=1) + "\n")
        info = json.loads((d / "result.json").read_text()) if (d / "result.json").exists() and not a.force else {}
        if "id" not in info:
            t0 = time.time()
            p = seedbox(a, "run", str(d), "--seed", str(a.seed * 100 + index), "--wall-time", a.wall_time)
            (d / "run.stderr").write_text(p.stderr)
            m = STATUS.search(p.stdout)
            info = {"status": m.group(1) if m else "ERROR", "id": m.group(2) if m else None, "wall_seconds": round(time.time() - t0)}
            if info["id"]:
                (d / "run.out").write_text(seedbox(a, "logs", info["id"], "oracle", "--stream", "stdout").stdout)
            (d / "result.json").write_text(json.dumps(info, indent=1))
        if a.replay and info.get("id") and "replay_id" not in info:
            t0 = time.time()
            p = seedbox(a, "replay", info["id"])
            m = STATUS.search(p.stdout)
            info.update({"replay_status": m.group(1) if m else "ERROR", "replay_id": m.group(2) if m else None,
                         "replay_wall_seconds": round(time.time() - t0)})
            if info["replay_id"]:
                (d / "replay.out").write_text(seedbox(a, "logs", info["replay_id"], "oracle", "--stream", "stdout").stdout)
            (d / "result.json").write_text(json.dumps(info, indent=1))
        return f"jcstress sim {name}: {info}"

    with concurrent.futures.ThreadPoolExecutor(max(1, a.jobs)) as ex:
        futs = []
        if a.native:
            futs += [ex.submit(native, n, r) for n, r in groups.items()]
        if a.sim:
            futs += [ex.submit(sim, n, r, i) for i, (n, r) in enumerate(groups.items())]
        for f in concurrent.futures.as_completed(futs):
            print(f.result(), file=sys.stderr, flush=True)
    bad = 0
    lines = ["| Group | Native | Simulation | Replay |", "| --- | --- | --- | --- |"]
    for name in groups:
        d = out / name
        n, s = jcstress_summary(d / "native.out"), jcstress_summary(d / "run.out")
        info = json.loads((d / "result.json").read_text()) if (d / "result.json").exists() else {}

        def fmt(x):
            if x is None:
                return "-"
            state = "CRASH" if x["crash"] else "PASS" if x["exit"] == 0 and not x["failed"] and not x["errors"] else "FAIL"
            extra = f" {','.join(x['failing_tests'])}" if x["failing_tests"] else ""
            return f"{state} {x['passed']}/{x['planned']}{extra}"
        rep = "-"
        if (d / "replay.out").exists() and (d / "run.out").exists():
            rep = "MATCH" if (d / "replay.out").read_text() == (d / "run.out").read_text() else "DIVERGE"
        row = [fmt(n), fmt(s), f"{info.get('replay_status', '-')}, output {rep}"]
        if s and n and (not fmt(s).startswith("PASS") or s["planned"] != n["planned"]) or rep == "DIVERGE":
            bad += 1
        lines.append(f"| {name} | " + " | ".join(row) + " |")
    (out / "report.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    return 1 if bad else 0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gen", help="Generate a corpus directory.")
    g.add_argument("--seed", type=int, default=1, help="Corpus seed.")
    g.add_argument("--out", required=True, help="New corpus directory.")
    g.add_argument("--probe-seeds", type=int, default=1, help="Argument seeds per probe.")
    g.add_argument("--random", type=int, default=20, help="Random programs to generate.")
    g.add_argument("--profiles-per-instance", type=int, default=2, help="Flag profiles per program and arguments.")
    g.add_argument("--profiles", help="Comma-separated profile subset.")
    g.add_argument("--only", help="Regex selecting probes.")
    g.add_argument("--scale", type=int, default=1, help="Probe work multiplier.")
    g.add_argument("--batch-size", type=int, default=12, help="Cases per container run or simulation.")
    g.add_argument("--skip-known", action="store_true", help="Leave out cases matching KNOWN_ISSUES.")
    g.add_argument("--jit-noise", action="store_true",
                   help="Run a thread in every probe that keeps compiling and dropping hidden classes.")
    b = sub.add_parser("build", help="Compile the corpus into its image.")
    b.add_argument("corpus")
    n = sub.add_parser("native", help="Run every batch in a container pinned to one CPU.")
    n.add_argument("corpus")
    n.add_argument("--runs", type=int, default=2, help="Native runs per batch (stability check).")
    n.add_argument("--jobs", type=int, default=4, help="Concurrent containers.")
    n.add_argument("--force", action="store_true", help="Rerun batches with existing output.")
    n.add_argument("--unconfined", action="store_true",
                   help="Run without the engine's seccomp filter, as the guest has none.")
    s = sub.add_parser("sim", help="Run every batch as a Seedbox simulation (server must be running).")
    s.add_argument("corpus")
    s.add_argument("--seedbox", default=os.environ.get("SEEDBOX_BIN", "seedbox"), help="Seedbox executable.")
    s.add_argument("--jobs", type=int, default=2, help="Concurrent simulations.")
    s.add_argument("--wall-time", default="60m", help="Host time limit per simulation.")
    s.add_argument("--replay", action="store_true", help="Also replay each simulation and compare its output.")
    s.add_argument("--batches", help="Comma-separated batch subset.")
    s.add_argument("--force", action="store_true", help="Rerun batches with existing results.")
    s.add_argument("--variant", help="Store results as sim-VARIANT, compared with native like the main run.")
    s.add_argument("--seed-offset", type=int, default=0, help="Shift simulation seeds (for variants).")
    s.add_argument("--jitter", type=int, default=0, help="Oracle CPU jitter percent (for variants).")
    r = sub.add_parser("report", help="Compare native, simulation and replay output.")
    r.add_argument("corpus")
    j = sub.add_parser("jcstress", help="Run jcstress groups natively and/or as simulations, comparing verdicts.")
    j.add_argument("--out", required=True, help="Result directory.")
    j.add_argument("--image", default="jvmfuzz-jcstress:samples", help="Image built from jcstress/Dockerfile.")
    j.add_argument("--suite", default="jcstress-samples", help="Suite jar name inside the image.")
    j.add_argument("--groups", help="Comma-separated subset of " + ",".join(JCSTRESS_GROUPS) + ".")
    j.add_argument("--mode", default="sanity", help="jcstress preset (sanity, quick, default, tough).")
    j.add_argument("--native", action="store_true", help="Run natively in a container on one CPU.")
    j.add_argument("--sim", action="store_true", help="Run as simulations.")
    j.add_argument("--replay", action="store_true", help="Replay each simulation and compare output bytes.")
    j.add_argument("--seed", type=int, default=1, help="Base simulation seed.")
    j.add_argument("--seedbox", default=os.environ.get("SEEDBOX_BIN", "seedbox"), help="Seedbox executable.")
    j.add_argument("--wall-time", default="150m", help="Host time limit per simulation.")
    j.add_argument("--jobs", type=int, default=2, help="Concurrent runs.")
    j.add_argument("--force", action="store_true", help="Rerun groups with existing results.")
    a = ap.parse_args()
    rc = {"gen": cmd_gen, "build": cmd_build, "native": cmd_native, "sim": cmd_sim, "report": cmd_report, "jcstress": cmd_jcstress}[a.cmd](a)
    sys.exit(rc or 0)


if __name__ == "__main__":
    main()
