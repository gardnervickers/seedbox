# Simulation configuration migration

Implementation decisions for the [server plan](simulation-server-plan.md).
Run, exploration, rerun and replay use the server API. Replay accepts a simulation
ID and compares a new execution with its source. Importing an offline bundle into
the server catalogue still needs migration; the table distinguishes retained
formats from public options.

The Compose parser now fixes stop tolerance at zero and page timer/deferred TX
at false. `check.exit_code` is rejected in Compose; oracle exit zero passes and
every nonzero exit fails. Fresh API submissions enforce those defaults as well. Retained input
records preserve old settings for replay. Examples and acceptance Compose files
use the reduced authoring surface. Fixing the formerly configurable stop
tolerance to zero changes results for worlds that used nonzero early-delivery
tolerance; fresh runs are not expected to match those older worlds.

## Ownership

`seedbox-spec` contains resolved configuration shared by preparation, retained
artifacts and the public API; it depends only on Serde. `seedbox-api` contains
public resources and wire contracts with no engine or transport dependency.
`seedbox-client` owns HTTP over Unix sockets. `seedbox-server` owns the catalogue,
subscriptions and application execution service. The `seedbox` executable owns
environment discovery, server startup and presentation. The application library
lives in `seedbox-application`, allowing the server to depend on orchestration
without a dependency cycle. The existing
artifact and engine protocol crates retain their separate responsibilities.

## Option inventory and decisions

Consumers below refer to the current implementation. A value retained for replay
does not automatically become a configurable public API field.

| Input | Current consumer / effect | Decision |
| --- | --- | --- |
| Compose path | CLI/config parser; local image/config/build inputs | Keep positional, default `.`; resolve client paths before submission. |
| Images, entrypoint, command, environment, working directory, user, config mounts, volumes | Parser and guest preparation; application behavior | Keep Compose authoring. Freeze resolved inputs before execution; no later client-environment reads. |
| Platform | Preparation architecture validation | Discover native guest architecture; reject incompatible explicit images. |
| Seed | World scheduler and guest randomness; result-changing | Keep one optional authoring value/CLI override; allocate once when omitted and publish effective seed. |
| `check.service` | Workload exit selection | Keep oracle machine identity; call it `oracle` in the public resource. |
| `check.exit_code` | Orchestration pass/fail comparison | Remove expected-code configuration. Zero passes; every nonzero exit fails. |
| `control.service` | Guest fault-control owner | Keep scenario controller identity where used. It remains distinct from the host control API. |
| Simulated-time limit | Engine experiment termination | Keep; default 60 simulated seconds. Describe the resolved limit. |
| Wall-time limit | Worker supervision/cancellation | Operational safeguard, default 10 minutes of active execution. Exclude deliberate pacing waits and pauses. Preparation has its own 15-minute operational deadline. These do not alter the oracle verdict. |
| `wall_clock_epoch` | Guest launch clock; result-changing | Keep explicit experiment override; otherwise derive from prepared image timestamps, never host clock. |
| Global network latency/jitter | Shared links; result-changing | Keep experiment parameters; defaults remain 1 ms latency and zero jitter. |
| Directed links, packet loss, bandwidth, queue bound | Shared network policy; result-changing | Keep explicit topology/fault parameters. |
| Schedule timer jitter | Shared scheduler; result-changing | Keep as an experiment parameter, default zero. |
| Schedule stop tolerance | Scheduler stop policy | Default zero; remove authoring override until a user need justifies it. |
| Machine RAM | Guest memory map and Linux/app behavior | Keep resource sizing, default 1024 MiB per machine. |
| `page_timer`, `deferred_tx` | KVM-specific implementation paths | Remove authoring overrides; use existing false defaults until engine changes choose a replacement. Retain actual values in internal reproduction inputs. |
| Torn disk writes | Crash durability policy; result-changing | Keep experiment parameter, default false. |
| Static fault arrays | Already rejected by parser | Remove; use the guest scenario API or interactive controls. |
| Host CPU scheduling | Host scheduling | No Seedbox binding option or lease; external affinity tools remain available. |
| Exploration workers | Host concurrent execution capacity | Server scheduling based on available capacity; no experiment field. Keep seed range/count as batch submission intent. |
| Watchdog milliseconds | Diagnostic interruption cadence | Fixed implementation default, currently 20 ms; no authoring flag. |
| Kernel/VMM/runtime assets/init/mke2fs | CLI discovery, preparation, worker launch | Discover once at server executable boundary; no per-simulation override. Continue to retain exact execution inputs. |
| Docker/Podman endpoint | CLI discovery and OCI transport | Server connection discovery, with explicit server environment override. Never contact the engine for retained-input reruns. |
| State directory / `SEEDBOX_STATE` | Server catalogue and execution storage | User-wide server state by default, with an explicit server location override. Client working directory must not select a different daemon implicitly. |
| Full-state evidence | Execution profile and retained comparisons | Keep the existing exit-witness default initially; the CLI currently selects full state only with `--full-state`. Do not expose that toggle in the public spec. Checkpoint capture retains the state needed for restoration independently of terminal comparison detail. |
| `checkpoint-at-ns`, `branch-seed` | One-shot worker launch operations | Replace paired run flags with checkpoint and branch resource operations. |
| Replay VMM override | Alternate engine comparison | Remove from ordinary rerun. Fresh source changes prepare a new simulation; rerun uses retained engine/inputs. |
| Log machine/stream selectors | Offline log reader | Live logs use positional simulation/machine and stdout/stderr selection. Raw stdout contains only application bytes. |
| Log since/until simulated time | Retained log index | Remove from live API; no historical stream archive. Keep offline artifact inspection separate while retained formats still support it. |
| JSON/full output/follow/detach | CLI presentation and attachment | Client options. `--json` remains one document; event streaming uses explicit JSON-lines output. |
| Bundle byte limit and output paths | Offline import/export | Keep offline; no daemon required. |
| Pause/resume/rate | Host boundary control | Live controls. Default unlimited; zero pauses. Reset pacing anchor on rate changes. |

## Authoring and resolved resources

Fresh submission identifies a Compose project and optional seed. The server
resolves it into an immutable retained-input reference plus seed and designated
oracle machine. The retained reference owns the resolved Compose configuration,
image bytes, guest assets, engine and experiment policy. `describe` includes
effective configuration for inspection; resubmission uses the reference rather
than rebuilding mutable image tags or reading the original project directory.
This reference is local catalogue data, not a portable bundle. Missing retained
inputs produce an explicit error.

Submission of a described resource reads its spec and discards old metadata and
status. Version/kind and unknown spec fields are validated. Resource revision,
stream sequence and committed timeline position are different counters.

Operational execution termination and the oracle verdict remain independent.
An observed oracle exit remains authoritative even if finalization fails. A
simulation that ends before an oracle exit has an incomplete test result.

## Migration checks

Update parsing, defaults, help, README, runtime/CLI documentation and generated
project instructions together with the CLI replacement. Tests should exercise
public behavior and meaningful invariants rather than preserving removed knobs.
Existing hardware acceptance scripts that parse a retained directory must migrate
to the simulation ID and describe API. Offline retained formats remain readable;
their old expected-exit-code fields must not reintroduce configurable pass/fail
into the new API.

Preparation uses a fixed 15-minute server deadline. It is independent of the
simulation's execution budget and has no initial user-facing tuning option.
Expiry cancels owned preparation work and retains a distinct operational reason.
