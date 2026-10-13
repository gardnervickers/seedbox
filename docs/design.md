# Design

Seedbox runs real Linux programs while controlling time, entropy, packet
delivery, and disk faults. Its purpose is to reproduce failures and test how
applications respond to faults. A simulation is one execution with one seed.

## Application and oracle

Each machine has its own Linux guest. The oracle is a separate guest program
that waits for application readiness, drives work, injects scripted faults, and
checks results. Its exit code determines the test verdict. Execution errors and
replay mismatches are reported separately from that verdict.

`seedbox.yaml` describes machines, the oracle, initial files and volumes,
baseline network latency, and each machine's CPU speed and jitter. Each CPU
defaults to 1 GHz with zero jitter, including the oracle. Complex test
logic belongs in the oracle. The oracle can use the guest control API; the TUI
can inject faults through the server API.

The authoring schema is defined in [seedbox-spec](../crates/seedbox-spec/src/project.rs).
The [Python control client](../python/seedbox/) provides the guest API.

## Clients and server

The CLI and TUI submit work to a server. The server owns preparation, execution,
resource lifetimes, logs, events, and playback control. Closing a detached
client leaves accepted work running. Deleting a simulation stops its execution
and deletes its owned resources. Independent reruns and branches are separate
simulations.

The TUI gets machine state, fault descriptions, parameter schemas, logs, and
events from the API. Execution policy belongs in the server and engine so
another client can control the same simulation without implementing that policy.
Application logs contain application output. Events describe simulator activity.
Both use bounded in-memory buffers: 64 MiB shared for application logs and 16 MiB
for events, including completed and imported simulations. Eviction is visible as
delivery gaps. Workers publish at most 64 output chunks of up to 2 KiB per
machine per round and count the rest as a machine gap, which depends only on
execution. Between rounds the worker waits for the server to accept each log and
event datagram, which delays only wall time. If the server accepts none for two
seconds, the worker stops waiting until it accepts one again; datagrams lost
meanwhile become source gaps, which differ between runs. Frame batches for
visualization never wait. `diagnostics ID` exposes completed raw
console/management streams, chunk indexes and worker output separately, by exact
retained name and digest.

## Definitions and execution state

The client translates `seedbox.yaml` into a submission. The server fills omitted
defaults, resolves images, and prepares guest inputs. A resolved definition
contains its seed, initial clock, architecture, machine configuration, starting
state, and external fault overrides. Structured data is inline; binary assets
are referenced by content digest. A definition does not require its source
simulation to remain in the catalogue. Annotations can record its origin.

Submitting a definition captures its inputs. Later edits create a different
simulation; they do not change an already submitted one. The TUI maintains an
editable working copy and can save or submit it. Removing a fault from that copy
does not undo a fault already applied to the live simulation.

The server catalogue is in memory. Execution uses session scratch directories,
which are removed on graceful shutdown; startup cleans abandoned sessions.
The binary cache survives restarts. Saved definitions can reuse it, but copying
YAML to another server does not transfer the referenced bytes.

A portable bundle transfers a completed resolved simulation, its binary closure,
independent checkpoint definitions, retained result evidence and available bounded
observations. Import creates a fresh completed resource without executing anything.
Source IDs and host paths are provenance, never dependencies or restored ownership.
A checkpoint branch resolves its inputs independently of the imported source.
Import is not idempotent or resumable; a full upload may be accepted after the client
disconnects. Export is completed-only, and files are published without overwriting
an existing destination. Catalogue persistence and cache pruning are separate concerns.

Deleting a simulation reclaims the cache objects it owned that no remaining
resource references: captured checkpoint state, imported binaries and edited boot
archives. Reusable inputs (kernels, prepared images and guests) are never owned.

## Deterministic execution

A shared coordinator orders machine execution, network delivery, controls, and
faults. Platform backends supply CPU execution, guest memory, devices, and
snapshot operations. The same scheduler serves x86 KVM and ARM backends.
Host worker scheduling and watchdog interruptions must not affect results.

Simulated time comes from guest operations and instrumentation. It is separate
from elapsed host time. Pausing and playback rate control when the host executes
work; they do not change the simulation's definition. A wall-time limit bounds
one execution attempt.

Oracle actions execute as part of the oracle program. External fault overrides
are additional ordered inputs. Replay applies those inputs through the same
scheduler. After an edit, the unchanged prefix should reproduce; later outcomes
may differ. Logs and events are observations, not instructions to inject again.

## Code boundaries

| Package | Responsibility |
| --- | --- |
| `seedbox` | CLI, native worker dispatch, process environment |
| `seedbox-tui`, `seedbox-client` | Interactive UI and HTTP client |
| `seedbox-api`, `seedbox-spec` | Resource types and simulation definitions |
| `seedbox-server` | Catalogue, API, execution ownership |
| `seedbox-application` | Configuration, image preparation, orchestration |
| `seedbox-artifacts` | Retained records, content storage, comparison, portable archives |
| `seedbox-engine-protocol` | Private worker transport and evidence formats |
| `seedbox-engine-core` | Shared scheduling, faults, devices, deterministic policies |
| `seedbox-vmm`, `seedbox-arm64` | Native machine execution |

The application library does not depend on the server or CLI. The engine core
has no hypervisor dependency. Backend adapters perform architecture-specific
operations; shared policies stay in the core.

## Validation

A passing oracle and a matching replay answer different questions. The oracle
checks application behavior. Replay checks whether retained inputs reproduce
recorded results. Runs keep no engine executable; replay always uses the current
engine, so replaying an older run checks the new engine against recorded results.
Each run records the engine's source revision as provenance, never as an input.

Server comparison checks resolved execution semantics and retained result evidence,
not private run IDs. Image names and descriptive engine/host metadata do not replace
actual binary assets and policies. Missing evidence remains `NOT_COMPARABLE`; changed
results remain `MISMATCH`.
