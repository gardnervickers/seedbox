# Preparation and execution contract

The boundary is a Rust library API, defined in `src/execution.rs`. The CLI only
parses arguments, supplies environment values, calls the library, and formats
results. Both a future CLI command and another Rust application use the same API.

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
manifest digest, not the registry tag or multi-platform index digest.

The preparer must validate descriptor/digest relationships, image platform, all
launch metadata, and guest requirements. `image::resolve_launch` supplies the
shared command/environment override logic. Its `Launch` includes image-declared
volume paths; silently discarding these would change image behavior. The upcoming
guest implementation needs an explicit per-run writable-storage lifetime,
read-only configs, UID/GID lookup, working directory and PATH resolution, and
service-name resolution. Process restart and whole-machine restart must not be
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
witness mode and parameters. It must execute the verified engine artifact or
verify the executable it actually invokes against that digest. Merely returning
the requested digest from `profile()` is not sufficient.

`execute` blocks until all owned guests/processes have stopped and their outputs
are closed. It owns CPU selection, process lifecycle, timeout enforcement, and
cancellation. It writes only under the supplied output directory and returns a
typed `ExecutionReport`. The orchestrator's elapsed-wall-time check prevents a
late success but cannot interrupt a blocked backend. Therefore a backend must
actively enforce the deadline and observe the cancellation token.

For hardware, acquire the common host reservation **before any CPU/MSR changes**.
The reservation must cover every selected core and SMT sibling, and must be shared
by all hardware entrypoints, including batch/campaign/performance runs. Save the
exact original state under the reservation and restore it before release on
normal return, errors, and catchable interruption. Restoration must finish only
after owned VMM processes have stopped. The existing scripts' shared
`msr54-saved.json` file is not an appropriate per-run state store. No new runner
should bypass those scripts with a separate lock that they do not also honor.
Agree on that lock and lease ownership with #1 before enabling hardware execution.

The current VMM report is not yet this API. It needs an explicit distinction
between workload exit, guest failure, limit, and quiescence, together with the
designated workload's numeric exit code. Current `State::Exited` also carries
sentinel values for failures, so parsing its debug string cannot establish a
successful application exit. The adapter must preserve fault history, deterministic
exit witness and optional per-machine full-state digests. It must preserve logs
when a guest or runner fails, including on timeout, and never infer PASS from a
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

These are integration contracts, not evidence that hardware reservation, OCI
import, or kernel qualification is already implemented. The contract test backend
lives only under `tests/`; it is never offered as a production execution option.
