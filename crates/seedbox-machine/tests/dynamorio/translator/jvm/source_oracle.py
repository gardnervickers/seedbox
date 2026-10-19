#!/usr/bin/env python3
# **********************************************************
# Copyright (c) 2026 Google, Inc.    All rights reserved.
# **********************************************************

# Redistribution and use in source and binary forms, with or without
# modification, are permitted provided that the following conditions are met:
#
# * Redistributions of source code must retain the above copyright notice,
#   this list of conditions and the following disclaimer.
#
# * Redistributions in binary form must reproduce the above copyright notice,
#   this list of conditions and the following disclaimer in the documentation
#   and/or other materials provided with the distribution.
#
# * Neither the name of Google, Inc. nor the names of its contributors may be
#   used to endorse or promote products derived from this software without
#   specific prior written permission.
#
# THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
# AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
# IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
# ARE DISCLAIMED. IN NO EVENT SHALL GOOGLE, INC. OR CONTRIBUTORS BE LIABLE
# FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
# DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
# SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
# CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
# LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
# OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH
# DAMAGE.

# Extracted semantic/JIT oracle; no upstream runner or runtime build machinery.
import os
import re

BASE_JVM = ["-Xmx384m", "-Xss1m", "-XX:-UsePerfData", "-XX:-CreateCoredumpOnCrash",
            # Fixed ergonomics (GC, compiler threads) whatever the CPU set.
            "-XX:ActiveProcessorCount=2"]

# Unlimited traps: every implicit null check stays a signal, every trap
# deoptimizes and recompiles (Seedbox's "deoptstorm").
DEOPTSTORM = ["-XX:PerBytecodeTrapLimit=1000000", "-XX:PerMethodTrapLimit=1000000",
              "-XX:PerMethodRecompilationCutoff=-1", "-XX:PerBytecodeRecompilationCutoff=-1"]

SCENARIOS = {
    "IcNull": {
        "jvm": ["-XX:CompileCommand=quiet",
                "-XX:CompileCommand=dontinline,*::icsite",
                "-XX:CompileCommand=dontinline,*::f",
                # No receiver or argument profiles: C2 keeps a real virtual call
                # and does not speculate on the receiver (with argument
                # profiling, some runs stopped raising the NPE in icsite).
                "-XX:TypeProfileWidth=0", "-XX:TypeProfileLevel=0",
                "-XX:CompileThresholdScaling=0.05"],
        "ops": 100000, "threads": 4, "c2_target": r"JitStress\$S\d::icsite",
    },
    "DeoptCycle": {
        "jvm": DEOPTSTORM + ["-XX:-OmitStackTraceInFastThrow",
                             "-XX:CompileThresholdScaling=0.05"],
        "ops": 40000, "threads": 3, "c2_target": r"JitStress\$DeoptCycle::dc[123]",
    },
    "JfrNpe": {
        "jvm": ["-XX:+UseZGC"],
        "ops": 300000, "threads": 3, "c2_target": r"JitStress\$JfrNpe::step",
    },
    "Redefine": {
        "jvm": [],
        "ops": 20, "threads": 1, "c2_target": r"JitStressRTarget::capture",
    },
}

# Floors: counts from -Xlog (see count_log) a run must reach, or its verdict is
# INSUFFICIENT_STRESS.  Calibrated on JDK 25.0.4 (release) for the default
# ops; scaled by ops/default.
FLOORS = {
    # npe_through_icsite: NPEs that unwound through icsite, i.e. null receivers
    # reaching the callee's inline cache check (C2 leaves that null check
    # implicit; the scenario has no other way to raise one there).
    "IcNull": {"compiles": 200, "c2_icsites": 8, "npe_through_icsite": 2000,
               "class_unloads": 20, "made_not_entrant": 50},
    # c2_target: C2 (re)compiles of dc1-3; 5-7 with a release DR, 4 with a debug one.
    "DeoptCycle": {"compiles": 100, "deopts": 100, "made_not_entrant": 50, "c2_target": 3},
    # C2 turns step's NPE into a branch to its handler (no exception object, no
    # log line): the implicit null checks are not countable from -Xlog.
    "JfrNpe": {"compiles": 50, "c2_target": 1, "execution_samples": 1},
    "Redefine": {"compiles": 50, "c2_target": 2, "redefinitions": 20},
}

XLOG = ("-Xlog:jit+compilation=debug,deoptimization=debug,exceptions=info,"
        "codecache=info,class+unload=info,redefine+class=info"
        ":file={log}:uptime,tid,level,tags:filecount=0")


def log(msg):
    print(msg, flush=True)


def count_log(path, stdout, c2_target=None):
    """Counts completed events (not requests) in the -Xlog file."""
    c = {k: 0 for k in ("compiles", "c2_compiles", "made_not_entrant", "deopts",
                        "npe_created", "npe_through_icsite", "compiled_exceptions",
                        "class_unloads", "redefinitions", "c2_target")}
    c2_icsites = set()
    if os.path.exists(path):
        with open(path, errors="replace") as f:
            for line in f:
                # "[uptime][tid][level][tags   ] message"
                tags, _, msg = line.partition("] ")
                msg = msg.strip()
                if "[jit,compilation" in tags:
                    if "made not entrant" in msg:
                        c["made_not_entrant"] += 1
                        continue
                    c["compiles"] += 1
                    # "<id> <flags> <level> <method> (<size> bytes)"
                    m = re.match(r"\d+\s+[%sbn!]*\s*(\d)\s+(\S+)", msg)
                    if m and m.group(1) == "4":
                        c["c2_compiles"] += 1
                        if c2_target and re.search(c2_target, m.group(2)):
                            c["c2_target"] += 1
                        m2 = re.search(r"JitStress\$(S\d)::icsite", m.group(2))
                        if m2:
                            c2_icsites.add(m2.group(1))
                elif "[deoptimization" in tags:
                    if "trap_bci=" in msg:
                        c["deopts"] += 1
                elif "[exceptions" in tags:
                    if msg.startswith("Exception <a 'java/lang/NullPointerException'"):
                        c["npe_created"] += 1
                    if "thrown in C1 compiled method" in msg or "thrown in C2 compiled method" in msg \
                            or re.match(r"Exception <a [^>]*> \(0x", msg):
                        c["compiled_exceptions"] += 1
                    if "NullPointerException" in msg and "'icsite'" in msg and "[Exception ()" in msg:
                        c["npe_through_icsite"] += 1
                elif "[class,unload" in tags:
                    c["class_unloads"] += 1
    c["c2_icsites"] = len(c2_icsites)
    m = re.search(r"executionSamples=(\d+)", stdout or "")
    c["execution_samples"] = int(m.group(1)) if m else 0
    m = re.search(r"redefinitions=(\d+)", stdout or "")
    c["redefinitions"] = int(m.group(1)) if m else 0
    return c
