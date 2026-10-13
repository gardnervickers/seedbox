# JVM fuzz corpus

Small JVM programs that each exercise one area of HotSpot, run natively and
under Seedbox and compared. A failure points at one feature and one flag
profile instead of a whole Kafka cluster.

This is a diagnostic tool, like `tools/mac-tcg`; it is not an acceptance suite.

## Pieces

- `probes/*.java`: hand-written probes, one area each (JIT tiers and
  deoptimization, exceptions and stack overflow, GC and references, monitors,
  `java.util.concurrent`, atomics, virtual threads, reflection, invokedynamic,
  class loading and unloading, strings, SIMD and the Vector API, crypto
  intrinsics, zlib, files and `mmap`, loopback sockets and HTTP, child
  processes, signals, safepoint and handshake operations, JFR sampling, clocks).
  Each takes `SEED SCALE`, prints deterministic `key value` lines and ends with
  `OK <probe> <hash>`. `Race` prints schedule- and entropy-dependent values on
  purpose; it is only compared with its replay.
- Translator probes aim at what DynamoRIO's progress client must get right:
  signals landing in compiled code (`SignalBomb`, `InterruptedSyscalls`,
  `AsyncClose`, `MappedSigbus`), code written while other threads run it
  (`IcStorm`, `DeoptStorm`, `Redefine` through a Java agent, `CodeChurn`
  through HotSpot's WhiteBox API, `CodeReuse` in a small code cache), the
  thread register x28 and vector registers (`X28Storm`, `FpRegPressure`),
  copy and encoding stubs (`ArrayCopyStubs`), memory maps and the disk
  (`MmapChurn`, `AddressSpace`, `Durability`, `ClassStorm`), VM operations
  (`SafepointsNative`, `SigquitAttach` through the attach socket), thread and
  process lifetime (`ThreadChurn`, `VthreadStorm`, `ExitMatrix`, `Crash`) and
  timers (`SleepSort`, `Clocks`). JVM features beyond the defaults: foreign-function
  ABI corners and arenas (`FfmAbi`), virtual threads with scoped values and
  structured concurrency (`VthreadScopes`), the attach listener and `agentmain`
  (`AttachLoad`), dynamic CDS (`Cds`), perfdata, heap dumps and NMT (`PerfData`,
  `HeapNmt`), a raw JDWP debugger (`Jdwp`), allocation without TLABs, Epsilon and
  string deduplication (`AllocPaths`), out-of-memory recovery (`OomRecovery`),
  finalization, class-initialization states and `CONSTANT_Dynamic` (`ClassInit`),
  method-handle adapters, buffer atomics, stack overflow with locks held, JFR
  chunks, asynchronous files, vector masks, ZipFS and CheckJNI, TLS 1.3, EdDSA,
  RMI/JMX and the legacy reflection backends. `probes/preview` use the JDK 21
  foreign-function preview; `probes/whitebox` compile against the WhiteBox sources
  the image downloads for its JDK update.
- `probes/c/cfz.c`: native AArch64 probes, one test per `C.<name>` probe, run
  without a JVM: LSE pair atomics, `DC ZVA`, flags and FPCR across progress
  calls, tagged pointers, RW/RX code aliases and moved code, `membarrier`,
  `dlopen` churn, SIGSEGV write barriers, `siglongjmp` out of faults,
  `ucontext` with x28 live, thread cancellation, seccomp `SIGSYS` emulation,
  `SS_AUTODISARM`, `signalfd`, `pselect`, CPU-time timers, `futex_waitv`,
  `EPOLLONESHOT`, pidfds, `clone3`, `memfd` exec, `madvise`/`mremap` and seals,
  `timerfd`, a real `rseq` critical section, raw `io_uring`, `ptrace` register
  reads, `MADV_WIPEONFORK`, `SCM_RIGHTS`, `IFUNC`, robust and PI mutexes,
  `MAP_FIXED_NOREPLACE`, PID namespaces, `getrandom`, `vmsplice`/`tee`, DWARF
  unwinding through signal frames, TLS destructors, static PIE re-exec, vDSO clocks,
  counters, `WFE`/`WFI` and CPU affinity. Probes whose values depend on the host
  (entropy digests, counters, `TPIDRRO_EL0`, exclusive-store failures) are compared
  with the replay only. `probes/c/cfzprocfs.c` (`C.procfs`, replay-only) hashes
  `/proc` files and kernel-visible syscalls one per line after threads, files,
  sockets, a timer and a fork, so a replay or twin-run difference names the
  subsystem. `C.capabilities` lists the host's CPU features, syscalls and files;
  its differences from native are reported as `GUEST_DIFF`, not failures. Run
  native passes with `--unconfined`, or the engine's seccomp filter hides some
  syscalls.
- Other runtimes, as native probes with their own executables (`"exe"`): Go
  asynchronous preemption, stacks and GC (`probes/go`); Node's WebAssembly traps
  and JIT (`probes/js`); .NET tiering and compacting GC (`probes/cs`); BEAM
  reductions and per-process heaps (`probes/erl`); LuaJIT traces and FFI
  (`probes/lua`). The Erlang probes are `explicit`: BEAM runs about 250 times
  slower than real time in a simulation, so they run only when `--only` names them.
- `randprog.py`: seeded random programs (arithmetic edge cases, caught
  arithmetic and bounds exceptions, virtual calls, monitors, string building)
  run through interpreter, C1 and C2 phases with a checksum after each.
- `jvmfuzz.py`: the driver. Each case is one JVM process: program, arguments
  and a flag profile (`PROFILES`: tiers, `-Xcomp`, Serial/Parallel/G1/ZGC/
  Shenandoah, `-XX:-UseLSE`, small code cache, C2 stress seeds, ...). Cases
  are packed into batches; a batch is one container run or one simulation.
- `jcstress/`: OpenJDK jcstress built from its release tag, patched so
  multi-actor tests time-slice on the single guest CPU
  (`-Djcstress.virtualCpus`, with `-af NONE`).

## Use

The steps use the Docker-compatible engine named by `DOCKER_HOST` and, for
simulations, a running server:

```sh
mkdir -m 700 -p /tmp/sbfuzz   # a short path: the server socket lives here
export SEEDBOX_STATE=/tmp/sbfuzz SEEDBOX_BIN=$PWD/target/release/seedbox
$SEEDBOX_BIN serve &

cd tools/jvm-fuzz
./jvmfuzz.py gen --seed 1 --out /tmp/jvmfuzz-1 --random 20 --profiles-per-instance 3
./jvmfuzz.py build /tmp/jvmfuzz-1
./jvmfuzz.py native /tmp/jvmfuzz-1 --runs 2
./jvmfuzz.py sim /tmp/jvmfuzz-1 --replay
./jvmfuzz.py report /tmp/jvmfuzz-1      # writes report.md and report.json

docker buildx build --load -t jvmfuzz-jcstress:samples jcstress
./jvmfuzz.py jcstress --out /tmp/jcstress --native --sim --replay
```

`gen --only REGEX` selects probes; `--profiles a,b` restricts flag profiles.
`gen --jit-noise` adds a thread to every probe that keeps compiling and
dropping hidden classes, so each probe also runs during code modification.
`native` pins each container to one CPU with the guest's memory so JVM
ergonomics match the 1-vCPU guest; `--unconfined` drops the engine's seccomp
filter, which the guest does not have. `sim --variant NAME --seed-offset K
--jitter P` reruns the corpus into `sim-NAME` with other simulation seeds and
CPU jitter; the report gives each variant its own verdicts.

A probe whose expected exit status is not 0 lists it per argument variant
(`exit_codes` in `PROBES`); a probe that can hang its JVM is `isolate`d in a
simulation of its own.

## Verdicts

| Verdict | Meaning |
| --- | --- |
| `OK` | Simulation output equals the (stable) native output. |
| `OK_NONDET` | Output is not expected to match native; only replay is checked. |
| `MISMATCH` | Deterministic case whose simulated output differs from native. |
| `SIM_CRASH`, `SIM_FAIL`, `SIM_TIMEOUT` | The JVM crashed (hs_err included), failed, or hit the per-case limit only in simulation. A wrong exit status, or exit 0 without the `OK` (or `UNSUPPORTED`) line, is `SIM_FAIL`. |
| `SIM_CUT`, `SIM_NOT_REACHED` | The simulation ended during or before the case. |
| `REPLAY_DIVERGE` | The replay printed different output from the original simulation, or lacks the case. |
| `REPLAY_MISMATCH`, `REPLAY_NOT_COMPARABLE`, `REPLAY_ERROR` | `seedbox replay` of the batch did not report `MATCH` (witness or full state), whatever the output; every case of the batch gets it. |
| `NATIVE_FAIL`, `NATIVE_UNSTABLE` | The probe itself is broken or nondeterministic; fix the probe. |
| `GUEST_DIFF` | An informational probe (`C.capabilities`) found the guest differs from the host. |

Probes with `exit_codes` are judged by exit status alone; every other case must
exit 0 and print its `OK` line. A jcstress group passes only when its run
selected tests, printed the results summary and exit status, passed every
planned test (as many as natively) and replayed with `MATCH`; otherwise it is
`FAIL`, `CRASH`, `TRUNCATED` or `NO_TESTS`. `test_jvmfuzz.py` checks these
verdicts on fixture result directories (portable suite).

The report also lists programs whose native output changes with the flag
profile: a probe bug or a HotSpot bug, independent of Seedbox.
