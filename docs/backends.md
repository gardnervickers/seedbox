# Preparation and execution contract

The boundary is a Rust library API, defined in `src/execution.rs`. The CLI only
parses arguments, supplies environment values, calls the library, and formats
results. CLI commands and other Rust applications use the same API.

```text
CLI / Rust caller
    config::load(explicit environment)
    Orchestrator::run(simulation, options, preparer, executor)
        PreparationBackend::prepare -> PreparedInputs + retained content
        write INCOMPLETE manifest
        verify and materialize all declared inputs
        ExecutionBackend::execute -> ExecutionReport + output files
        classify, retain logs, finalize manifest

    Orchestrator::replay(attempt, executor, cancellation)
        verify manifest and materialize recorded inputs
        fresh ExecutionBackend::execute
        classify and compare with the original result/witness
```

The initial manifest is persisted before verification/materialization so
preflight errors can also be recorded. Preparation precedes the initial manifest;
until it finishes there is no resolved run identity.

## Preparation ownership

Preparation owns registry and builder access, platform selection, image metadata,
layer application, source/build snapshots, config file capture, deterministic guest
archives, and launch-helper generation. It must put the bytes in `ContentStore`
before returning references. Registry, Dockerfile, and Nix-produced OCI images
must converge at this boundary. `PreparedImage.manifest` is the platform-specific
manifest digest, not the registry tag or multi-platform index digest. `placement`
maps every service to a machine, and `guests` supplies one prepared boot archive
per machine. Service and image sets must match; the set of placement targets must
exactly equal the guest set. Custom backends can represent several services in
one guest; production OCI preparation currently creates one service per VM.
This library expressiveness is not another supported Compose placement mode.

The preparer must validate descriptor/digest relationships, image platform, all
launch metadata, and guest requirements. `image::resolve_launch` supplies the
shared command/environment override logic. Its `Launch` includes image-declared
volume paths; silently discarding these would change image behavior. The OCI
guest implementation provides per-run RAM-backed storage, read-only configs,
UID/GID lookup, working directory and PATH resolution, and an explicit hosts file
for service-name resolution. The etcd and Redis application paths have passed
Linux acceptance; see [acceptance.md](acceptance.md) for the tested profiles and limits.
Process restart and whole-machine restart must not be
conflated when storage behavior is defined in #3.

Preparation never starts a guest or changes CPU/MSR state. It should complete all
downloads/builds before acquiring any scarce hardware reservation. Source/build
provenance must identify actual context, Dockerfile and builder inputs, including
dirty local source when applicable. Replay consumes retained artifacts and must
not need the original checkout, Docker daemon, or registry.

## Execution ownership

The executor receives only resolved values and materialized content paths. It
must use the requested seed, network and limits; it must not consult ambient
environment for execution-affecting settings. Such settings belong in the
recorded `ExecutionProfile`. Its `profile()` must match the recorded engine bytes,
witness mode and semantic parameters. Performance-only settings are returned
separately by `performance_parameters()`, recorded per attempt and supplied to
`execute`; they are not identity inputs or replay compatibility gates. Timer jitter
and stop tolerance are instead explicit semantic `Simulation.schedule` controls.
Explicit cross-engine replay changes the recorded engine/identity but still
requires matching semantic settings and compares execution evidence.
The executor must execute the verified engine artifact or
verify the executable it actually invokes against that digest. Merely returning
the requested digest from `profile()` is not sufficient.

`execute` blocks until all owned guests/processes have stopped and their outputs
are closed. It owns CPU selection, process lifecycle, timeout enforcement, and
cancellation. It writes only under the supplied output directory and returns a
typed `ExecutionReport`. The orchestrator's elapsed-wall-time check prevents a
late success but cannot interrupt a blocked backend. Wall-time expiry and
cancellation are infrastructure INCOMPLETE results, excluded from replay comparison;
only the simulated-time limit yields semantic TIMEOUT. Therefore a backend must
actively enforce the deadline and observe the cancellation token.

For hardware, reuse the lease protocol already implemented by `harness/batch.py`
(`cpu_locks` and `run_batch`, merged on main). Acquire exclusive nonblocking `flock`
locks on `/tmp/seedbox-msr-cpu-<N>.lock` for every selected core and its SMT siblings,
in ascending CPU order, before any MSR mutation. Open without following symlinks,
keep file descriptors open through cleanup, and do not unlink the lock files. A
Rust executor uses these same files and advisory locks; a separate
`/run` namespace would not coordinate with the existing supervisor.

The current supervisor owns/reaps its VMM process groups, creates a private snapshot
directory while holding the lease, and uses `msr54.py` for exclusive snapshots,
all-or-nothing setup and readback-verified restoration. Keep the lease held until
owned processes have stopped and restoration has been attempted; preserve a failed
restoration snapshot and report infrastructure failure. Honor normal errors and
catchable signals. `host.rs` implements the protocol for the Rust executor;
fixture tests and hardware acceptance cover contention, catchable interruption,
and exact register restoration. The current VMM is a single-process engine;
all other hardware entrypoints must adopt this same protocol before sharing cores.

The versioned VMM report provides `guest_exit_code`, `guest_init_error`, and
`infrastructure_error`. The OCI supervisor reports application completion through
port 0xf4 and bootstrap failure through 0xf5. `State::Exited` also carries sentinel
values for failures, so the adapter never parses its debug string for completion.
The adapter preserves the deterministic
exit witness and optional per-machine full-state digests. Fault history distinguishes
request time from the guest-acknowledged effect time; never substitute the request
or host receipt time for `applied_at_ns`. Full-state keys must match `guests`, not
the set of services. It must preserve logs when a guest or runner fails,
including on timeout, and never infer PASS from a
zero host process status alone.

## Orchestration ownership

Orchestration chooses the explicit seed, requests preparation once, assigns an
identity and a private attempt directory, checks the input closure and profile,
invokes the executor, classifies the result, retains logs, and compares replay.
It never pins cores, writes MSRs, interprets OCI filesystem layers, or talks to KVM.

`PreparationBackend` and `ExecutionBackend` are synchronous `Send + Sync` traits
so callers can use their existing thread pool or async runtime's blocking-task
facility. Concurrent attempts do not imply that a backend can concurrently mutate
host state: its reservation implementation controls that. The separate cancellation
token allows a caller to request shutdown without choosing an async runtime for
the public API.

These contracts and local fixture tests are not evidence of Linux hardware or
guest qualification. The orchestration test backend lives only under `tests/`;
it is never offered as a production execution option.

## Qualification and capability limits

[Draft format and runtime scope](compatibility.md) separates readable artifacts
from execution admission and lists version-bound evidence. V0 reader/writer and
release compatibility promises remain undecided pending qualification.

The tested production profile is Linux/KVM on the AMD Ryzen 5950X workstation,
with the Zen SpeculativeLockMap workaround verified for the leased cores and
SMT siblings. Guest kernel, supervisor, engine and semantic settings are run
identity inputs. Host kernel/microcode changes or a new CPU need fresh
qualification; same-seed equality across hosts is not established. The versioned
host admission and drift refusal implemented for
[issue #5](https://github.com/gardnervickers/seedbox/issues/5) add capability/input
checks and lock-held revalidation. Their Linux collection and the new profile
still require actual acceptance; source checks do not qualify them. Matching the
full policy on another machine admits compatibility, not cross-host determinism.

Clock and entropy evidence admits the tested workloads, not arbitrary Linux
images or JVM/JIT programs. Broader instruction, CPU-feature, clock and entropy
coverage belongs to #5; the Java workloads in issues #6 and #7 need admission
before their historical litmuses can be claimed.

Guest randomness comes from the seeded kernel CRNG only. The engine clears
RDRAND (CPUID leaf 1 ECX bit 30) and RDSEED (leaf 7 EBX bit 18), so the guest
kernel and runtimes that check CPUID before using them (OpenSSL, Rust's
`getrandom` fallback; Go and glibc never use them for randomness) stay
deterministic. On AMD, SVM cannot intercept either instruction: guest code that
executes them without checking CPUID, for example code built with `-mrdrnd`
that calls the intrinsic directly, reads the hardware generator, and its runs
do not replay. Admitting a workload therefore assumes it honours CPUID for
RDRAND and RDSEED. On Intel, KVM makes the hidden instructions raise #UD
instead. `harness/worlds/rdrand.json` and `rdrand-raw.json` show both cases. Retired native-calibration,
10x speedup and 100-microsecond stop targets are not acceptance requirements.

Process restart retains guest tmpfs data. It does not model machine reboot,
power loss, durable block devices or volatile versus flushed writes. Undo
checkpoints are private precise-stop recovery state, not complete VM/world
snapshots or a public reverse-execution interface. These capabilities require
explicit device, storage, RNG, event, controller and incarnation coverage.
The Kafka litmus in #6 needs a declared storage-loss model; a process kill
alone does not lose the OS cache. Stale callbacks and commands must not affect
a new machine incarnation when reboot support is added.

The implemented network timing model permits bounded early ordinary packet
delivery, as documented in [runtime.md](runtime.md); management barriers are
exact. Historical prototype proposals are not additional runtime guarantees.
The historical single-kernel namespace fixture has distinct scheduling, clocks,
memory and storage from independent VMs. Its version-bound
[issue #10 evidence](https://github.com/gardnervickers/seedbox/issues/10) does not
qualify production OCI co-location. The production default remains one service
per VM; broader layout decisions remain [issue #34](https://github.com/gardnervickers/seedbox/issues/34).

The [Mac TCG tool](../tools/mac-tcg/README.md) retains a useful restricted local
experiment. It is not a production execution backend and its scoped witness
is not comparable to KVM witnesses or a full-state digest. Automated setup,
run/replay integration and independent-machine local execution remain open.

The retired research tree and its raw evidence are preserved at
[commit 788fc43](https://github.com/gardnervickers/seedbox/tree/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research).
Earlier standalone prototypes are at commit `bc07f35`. These are historical
inputs, not additional supported backends.
