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
baseline network latency, and the application progress budget. Complex test
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
Both use bounded in-memory buffers.

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

## Isolated machine interface

[`seedbox-machine`](../crates/seedbox-machine/src/lib.rs) is a separate Linux-kernel
execution stack for tests and examples. It is not connected to the production
CLI, server or UI. Its TCG backend targets x86-64 Linux on QEMU’s `microvm`
board under software emulation; PV runs a cooperating Linux kernel on the host
architecture.

[`Machine`](../crates/seedbox-machine/src/machine.rs) exposes bounded, stopped
execution and machine snapshots. Each run borrows worker resources, a clock,
continuing entropy, execution pricing and ordered input/output queues. Requests,
responses and stop reasons are part of this contract, not backend transport types.
Concrete factories own boot assets and construct machines from one complete
prepared definition; construction performs no implicit build or asset search.
The bundled PV and TCG assets use `ramfs` for the initramfs root, avoiding a
separate tmpfs quota during unpacking. Its files still consume the VM's physical
RAM; this does not change the capacity or durability of attached disks.

The Driver owns scheduling, canonical time, dependency state and control policy.
Machines report guest termination as values and execution failures as errors.
Guest reset ends a boot; only an explicit caller request starts another one.
Application exit and whole-kernel termination are distinct. Machine snapshots own
accepted guest work, but do not capture the Driver or the external environment.

A complete definition may attach one owned disk image and one network interface
on either PV or TCG.
Disk writes become durable through guest FLUSH; ending a boot loses unflushed
writes, while snapshots preserve both layers without promoting either. Network
interfaces have explicit MAC and IPv4 addresses. The Driver owns simulated frame
delivery, isolated loss/jitter streams and link partitions; host networking never
determines delivery. Positive minimum latency bounds concurrent execution so a
receiver cannot run past a newly emitted frame's arrival.

Machine snapshots include attached device state, not frames still in flight or
routing policy state. Whole-Driver restoration and the legacy UI/API bridge remain
outside this stack's scope. Guest Inspect is explicitly unsupported; the host
event stream is not a complete world-state projection. Backend mechanics and
Linux test inputs are described in [internals](internals.md#reference-tcg-machine).

## Code boundaries

| Package | Responsibility |
| --- | --- |
| `seedbox` | Default executable, native worker dispatch and signing |
| `seedbox-cli` | Command parsing, process environment and CLI output |
| `seedbox-tui`, `seedbox-client` | Interactive UI and HTTP client |
| `seedbox-api`, `seedbox-spec` | Resource types and simulation definitions |
| `seedbox-server` | Catalogue, API, execution ownership |
| `seedbox-application` | Configuration, image preparation, orchestration |
| `seedbox-artifacts` | Offline records, content storage, comparison |
| `seedbox-engine-protocol` | Private worker transport and evidence formats |
| `seedbox-engine-core` | Shared scheduling, faults, devices, deterministic policies |
| `seedbox-driver` | Sole service coordinator, time and control policy |
| `seedbox-machine` | Isolated Machine contract |
| `seedbox-machine-pv`, `seedbox-machine-tcg` | Native CPU/device leaves |
| `seedbox-vmm`, `seedbox-arm64` | Independent low-level fixture workspaces; never service executors |

The application library does not depend on the server or CLI. The engine core
has no hypervisor dependency. Backend adapters perform architecture-specific
operations; shared policies stay in the core.

## Validation

A passing oracle and a matching replay answer different questions. The oracle
checks application behavior. Replay checks whether retained inputs reproduce
recorded results. An engine change also needs independent fresh runs: replay
uses a retained engine and therefore cannot by itself validate a new engine.
