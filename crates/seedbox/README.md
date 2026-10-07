# Seedbox orchestration

This Rust crate implements [issue #2](https://github.com/gardnervickers/seedbox/issues/2)'s
Compose-shaped application configuration and OCI run/replay path. The CLI exposes
`init`, `validate`, `prepare`, `run`, `inspect`, `replay`, `export`, and `import`. `OciPreparer` and
`VmmExecutor` implement separate programmatic preparation/execution interfaces;
the orchestration API can be called directly by future simulation schedulers.

The checked-in [etcd](../../examples/etcd/compose.yaml) and
[Redis](../../examples/redis/compose.yaml) examples passed real KVM workloads and
intentional-failure replays on the supported workstation. Full-state replay,
changed-margin replay, deterministic timeout, wall expiry, SIGTERM cancellation,
lease contention and exact MSR restoration were also exercised. See the
[acceptance record](../../docs/acceptance.md) and [runtime instructions](../../docs/runtime.md) for exact
inputs and limits. The runtime process and network API is described in
[the fault operations section](#structured-faults) and [recovery examples](../../examples/faults/README.md).

## Build and validate

From the repository root (`seedbox` is the workspace's default package):

```sh
nix develop
cargo test --locked
cargo run -- init etcd-demo --template etcd
cargo run -- validate etcd-demo
cargo run -- validate etcd-demo --json
cargo run -- inspect .seedbox/runs/<attempt> --json
```

The crate is independent of the Linux-only VMM crate and builds on the flake's
Darwin and Linux development hosts. The workspace lockfile is checked in. Validation
does not start guests, access registries, build images, or acquire a host lease.
It does check the existence of referenced local Dockerfiles and config files.
`--json` prints the resolved environment, so its output may contain values the
caller considers sensitive. Plain validation output does not print those values.

## Supported authoring subset

This is a deliberately small subset of the [Compose specification](https://compose-spec.github.io/compose-spec/05-services.html).
Validation errors include the affected path, for example
`$.services.workload.volumes: unsupported field`. Accepted authoring syntax does
not establish image/kernel compatibility; preparation must check that separately.

| Field | Accepted form and meaning |
| --- | --- |
| `name` | Lowercase Compose project name; defaults to the source directory name. |
| `services` | 1–254 services with lowercase DNS-label names. Preparation explicitly places services on machines; multiple services may share one guest. |
| `image` | Nonempty registry reference, without a transport prefix. Exact reference grammar and platform compatibility are preparation checks. |
| `build` | Local context string, or `{context, dockerfile}`. Context is relative to the Compose file; Dockerfile is relative to context. Remote contexts and build options are rejected. |
| `platform` | `linux/amd64`, also the default. |
| `entrypoint`, `command` | Null/missing, a string, or an argv list. String values are shell-word split but never implicitly run in a shell. |
| `environment` | String/null mapping or `NAME=value`/`NAME` list. Empty values, passthrough, and removal are distinct. Numbers and booleans must be quoted. Duplicate list entries are rejected. |
| `working_dir`, `user` | Explicit strings overriding image defaults. Working directories must be normalized absolute guest paths. Unknown users/groups fail guest bootstrap as infrastructure errors. |
| Top-level `configs` | Named `{file: local-path}` entries. |
| Service `configs` | Names, or `{source, target}`. Default target is `/<name>`. Read-only file semantics are required of preparation/execution. Permission/ownership overrides are rejected. |
| `networks` | One private network, with no driver/host options. Service lists or empty option maps are accepted. Default is `default`; a differently named network must be explicitly selected by each service. |
| `x-seedbox.version` | Required integer `1`. |
| `x-seedbox.seed` | Optional unsigned 64-bit fixed seed. The library run option overrides it; absence of both is an error. Zero is valid. |
| `x-seedbox.limits` | Required positive `simulated_time` and `wall_time`. |
| `x-seedbox.network` | Optional `latency` (default `1ms`), `jitter` (default `0ns`) and directed `links` baselines (default `[]`). Jitter must be less than latency. See [directed policy](../../docs/runtime.md#directed-link-policy). |
| `x-seedbox.schedule` | Optional `timer_jitter` and `stop_tolerance`, each defaulting to `0ns`. These are semantic schedule exploration settings. |
| `x-seedbox.check` | Required known `service` and optional `exit_code` (default `0`, range 0–255). |
| `x-seedbox.control` | Optional `{service: scenario}` selects the guest controller for the runtime RPC API. |
| `x-seedbox.faults` | Omitted or `[]` only. Fault operations are issued through the runtime API; declarative schedules are future work. |

The current VMM may deliver scheduled events/frames up to `early = min(latency/2, stop_tolerance)` early, never late. Effective one-way delay lies in `[latency - early, latency + jitter]`. Timer jitter and stop tolerance are retained in the simulation identity and passed explicitly to the executor.

Durations use `ns`, `us`, `ms`, `s`, `m`, or `h`, including exact decimals such as
`1.25ms`. Fractional nanoseconds and overflow are rejected. `image` and `build`
cannot be combined until pull/build precedence is implemented. Duplicate mapping
keys, custom YAML tags and merges, unknown extension fields, Compose interpolation,
`.env`, `env_file`, host integrations, ports, volumes, restart policies, healthchecks,
and dependency ordering are unsupported. Unknown fields fail rather than disappear.
`$$` escapes a literal dollar, including inside explicitly invoked shell commands.

The library takes a `BTreeMap<String, String>` for environment passthrough. It never
reads the ambient process environment; the CLI supplies that map at its boundary.
A missing passthrough variable removes the corresponding image default. A literal
empty string sets an empty value.

`image::resolve_launch` combines the supported service overrides with launch
metadata from the [OCI image configuration](https://github.com/opencontainers/image-spec/blob/main/config.md).
Null/missing entrypoint or command inherits the corresponding image default;
empty lists/strings clear it. A non-null entrypoint override also discards the
image's default command unless the service supplies a command. Image `Env`, `User`,
and `WorkingDir` are retained unless overridden. Image-declared anonymous volume
paths are carried into `Launch`, so a preparer must implement their lifetime
explicitly. No OCI layer fetching/unpacking is implemented in this module.

## Programmatic interface

The orchestration layer has no global scheduler, process launch, KVM dependency, or CLI calls.
`PreparationBackend` and `ExecutionBackend` are `Send + Sync` and object-safe.
See [the backend contract](../../docs/backends.md) before implementing them.

```rust,no_run
use seedbox::{
    config,
    execution::{CancellationToken, ExecutionBackend, PreparationBackend},
    orchestration::{Orchestrator, Run, RunOptions},
    store::ContentStore,
    Result,
};
use std::{collections::BTreeMap, path::Path};

fn run_and_replay(
    preparer: &dyn PreparationBackend,
    executor: &dyn ExecutionBackend,
) -> Result<(Run, Run)> {
    let simulation = config::load(Path::new("compose.yaml"), &BTreeMap::new())?;
    let runner = Orchestrator::new(ContentStore::new("store"), "runs");
    let first = runner.run(
        &simulation,
        RunOptions { seed: Some(1009), ..Default::default() },
        preparer,
        executor,
    )?;
    // Replay cannot call a preparer: it accepts only an executor and stored input.
    let replay = runner.replay(&first.directory, executor, &CancellationToken::default())?;
    Ok((first, replay))
}
```

Calls are synchronous and runtime-independent. An async application can use its
blocking-task facility; cancellation is a clonable token the backend must honor.
Concurrent calls use private attempt directories and a shared content store.
The caller supplies the executor's CPU selection. Core reservations, process
lifecycle and restoration belong to the executor. Multi-core scheduling is not
implemented by this crate yet. Signal handlers exist only at the CLI boundary;
programmatic callers control the cancellation token.

## Retained inputs and results

A run identity hashes the format version, seed, resolved configuration/environment,
prepared image manifests/configs/layers/guest archives, kernel, execution engine,
semantic execution profile, explicit service-to-machine placement, auxiliary input
hashes, and source/build provenance. Changing an identity input produces a new identity.
Performance settings returned by `ExecutionBackend::performance_parameters` are
recorded per attempt and passed explicitly to execution, but excluded from identity
and profile matching. Only settings with qualified witness invariance belong there
(e.g. `SEEDBOX_MARGIN` and `SEEDBOX_UNDO`). Each execution receives a distinct
attempt directory even when its identity is the same. Source paths remain as
provenance in the manifest; the executor receives verified private artifact copies
and cannot obtain original paths from its request.

The content store verifies SHA-256 on reads and refuses missing or corrupted
artifacts. It never repairs corruption implicitly. Replay verifies the entire
declared input closure before calling the backend, and never resolves image tags
or rebuilds. Semantic profile drift always fails before guest execution. Engine
drift fails by default. Explicit `replay_with_options` with
`ReplayOptions { allow_engine_change: true, ..Default::default() }` permits a different
engine whose bytes are already in the content store. The attempt records that mode
and the actual new engine digest, gets a new identity, and still compares outcomes
and witnesses against the baseline. It does not relax semantic settings or kernel
identity. A preparer is responsible for declaring **all** dependencies; this crate cannot detect undeclared
inputs hidden inside a backend implementation.

`manifest.json` holds a checksummed record, starting as `INCOMPLETE` before execution.
Updates use atomic replacement. Backend output files remain in the attempt
directory, including on backend errors.
Content-store retention defaults to 256 MiB per file and 1 GiB total per attempt;
`Orchestrator::with_log_policy` configures these caps. The manifest records the policy
and every omitted file/reason. This limits duplicate storage, not original output
growth; the executor must enforce its output disk budget. Artifact hashing and
copying stream with a 64 KiB buffer. `ContentStore::read` is limited to 8 MiB of
metadata; use `copy_to` or `materialize` for larger inputs. The checksum detects
corruption, not adversarial replacement by someone able to rewrite the store.

| Outcome | Meaning |
| --- | --- |
| `PASS` | The designated workload completed with its expected exit code and the requested witness data. |
| `FAIL` | It completed with a different exit code and the requested witness data. |
| `ERROR` | Backend/preflight failure, invalid result, or missing required witness data. |
| `TIMEOUT` | The deterministic simulated-time limit was reached. |
| `INCOMPLETE` | Host wall-time expiry, cancellation, quiescence, missing workload completion, or an interrupted process with only its initial manifest. |

Errors before preparation completes are returned as `Err`, because no resolved
run identity exists yet. Once the attempt manifest exists, ordinary execution or
input verification errors are persisted as `ERROR`; failures to write the manifest
itself still return `Err`.

Replay records its application outcome separately from `MATCH`, `MISMATCH`, or
`NOT_COMPARABLE`. A reproduced `FAIL` can be `MATCH`. Comparison checks the exact
workload exit, outcome, execution witness, and fault history. Optional full-state
digests are compared separately; `null` explicitly means no full-state comparison
was available. Full-state keys are machine names validated against prepared placement,
not service names. A profile requesting full-state evidence cannot pass without every
machine's digest. Simulated-time timeouts can reproduce when witness evidence is
available. Wall-time expiry and other incomplete/infrastructure-error attempts are
`NOT_COMPARABLE`. A post-hoc wall deadline check may therefore mark an attempt
incomplete but never create a semantic TIMEOUT.

Fault history records both `requested_at_ns` and `applied_at_ns`. The latter must
come from the guest acknowledgement of the actual effect, not host receipt time.
Changed effect timing is a replay mismatch; effects before their requests or for
unknown services are rejected.

The draft run-manifest format is now version 2; earlier version 1 records are not
migrated. Compose authoring remains `x-seedbox.version: 1`.

See [current draft readers/writers and runtime scope](../../docs/compatibility.md).
`inspect --json` prints the typed record with nested `identity.version`, not the
manifest checksum envelope. Inspection/transfer do not establish execution
admission. Release format compatibility is still a V0 gate.

## Structured faults

The fault API is a versioned RPC interface, with a Python binding for ordinary
scenario programs running inside the simulated world. Select a controller with
`x-seedbox.control: {service: scenario}`. Compose launches the services; Python
issues operations after observing application progress:

```python
from seedbox import World

with World.connect() as world:
    wait_for_acknowledged_writes(32)  # Application-specific readiness/progress.
    world.stop("server")
    world.sleep(1)
    world.restart("server")
    wait_for_server_ready()
    cut = world.partition(["client"], ["server"])
    world.sleep(3)
    world.heal(cut)
    check_acknowledged_values()
```

These application helpers are illustrative; the [working Python example](../../examples/faults/README.md)
implements them over real TCP. The controller is outside the fault model: faults
cannot stop it or cut its links, and management messages do not use the faulted
application network. Its normal completion determines the configured check outcome.

Operations validate retained service/machine targets. Stop waits for process-group
termination and reap; restart waits for successful exec. Partition returns an
independent opaque handle. A link remains cut until all applicable handles are
healed. Already routed packets stay queued; cuts drop subsequent sends.

The RPC uses committed guest boundaries and fixed simulated delivery latency.
Request admission and reply delivery use exact stops, even with ordinary packet
early delivery enabled. Replies contain request, admission, effect and delivery
timestamps. Missing/rejected supervisor effects are infrastructure errors.

Replay runs the retained controller program again and compares the resulting
history and witness. `inspect()` exposes topology, machine state, active handles
and packet counters. `delivered` counts receiver NIC acceptance, including bounded
backlog, independently of IRQ notification; full-backlog/unready drops are separate.

See the [Python API](../../python/README.md) and [public wire protocol](../../python/PROTOCOL.md)
for custom language bindings. Future declarative tools can call this same API.
No control configuration preserves the prior no-control serialization/witness path.

## Follow-on scope

The low-level kernel/network and one-VM fixtures retain their own inputs and
batch runner. Application acceptance uses direct YAML runs; the old application
campaign runner has been removed. Multi-core dispatch, whole-machine persistent
storage, and broad guest image qualification are separate work. The supported Compose path runs one service per VM with
explicit host/runtime inputs and RAM-backed storage.

## Retained run bundles

`bundle::export` and `bundle::import` transfer the existing manifest and declared
content closure without preparation or execution. Import returns an ordinary
`orchestration::Run` for inspect/replay, preserves original metadata and opaque
retained logs, and keeps capture omissions explicit. It publishes into fresh run
storage using bounded, verified streaming and no-follow filesystem operations.
See the [runtime bundle contract](../../docs/runtime.md#bundles) for format/limits
and qualification boundaries.
