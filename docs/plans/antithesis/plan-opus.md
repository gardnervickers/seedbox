# Seedbox product and architecture plan, informed by Antithesis

Sources: Antithesis docs fetched 2026-10-16 (`pages/*.md`, cited by filename) and Seedbox `main` @ ec80ea9f (cited as `path:line`). If a claim is an inference rather than something I read in code or docs, it is marked **(inferred)**.

## Summary

Seedbox already has the hard part: a deterministic multi-machine simulator for real Linux guests. That includes PV time, whole-binary progress instrumentation, seeded network/clock/lifecycle faults, a replayable override timeline, checkpoints, branches with a new branch identity, one-boot multi-seed forks, a server, a CLI and a TUI. What it lacks is everything that turns many executions into **findings**:

- **A property model.** Only the oracle exit code exists today.
- **A guest event channel.** No SDK or assertions.
- **Default failure detectors.** Crashes, OOM and so on.
- **An automatic fault generator.** Faults are only scripted or interactive.
- **Guided exploration.** `explore` is a sweep over consecutive seeds.
- **Triage tooling.** Aggregation, minimisation, causality.

The plan is three phases:

- **P0:** make a single run produce properties and findings, under generated faults that include storage faults.
- **P1:** build a deterministic branching explorer driven by assertion, event and coverage novelty.
- **P2:** add triage and debugging: shrinking, causality analysis, and exec/extract at a moment.

The main design choice: a branch from a checkpoint is expressed as a **`reseed` override at a boundary position**. Any explored path then flattens into one cold, linear definition, and checkpoints become caches that replay never needs.

---

## 1. Antithesis capability map

Status: **have** / **partial** / **missing**.

| Group | Antithesis capability | Doc page(s) | Seedbox status and evidence |
|---|---|---|---|
| Environment | Hermetic deterministic hypervisor running containers; no internet | `introduction__how_antithesis_works.md`, `setup__overview.md` | **Have.** Per-machine Linux guest VMs, controlled time, entropy and packets (`docs/design.md`; `crates/seedbox-engine-core/src/world.rs`) |
| Environment | Docker Compose / Kubernetes orchestration | `setup__docker_compose.md`, `setup__kubernetes.md` | **Partial.** Native `seedbox.yaml` with machines plus oracle (`crates/seedbox-spec/src/project.rs:10`). A legacy Compose reader exists. No k8s |
| Environment | x86-64 only, Skylake CPU, 10 GB RAM, CPU "strobing" | `configuration__the_antithesis_environment.md` | **Have, broader.** x86 KVM and arm64 HVF. Per-machine `memoryMiB`, `cpu.speed`/`cpu.jitter` (`project.rs:150-190`). Throttle fault (`crates/seedbox-control/src/lib.rs:180`). **Single vCPU per guest** (`docs/internals.md`, software exclusive reservations section) |
| Environment | `/dev/random` replaced with platform entropy; kernel RNG controlled | `configuration__the_antithesis_environment.md`, `release_notes.md` | **Have.** Seeded guest CRNG reseed at release and on fork (`world.rs:1256`, `kernel/apply.py:190`) |
| Environment | Setup mode / streaming setup logs; `snouty validate` | `setup__setup_mode.md`, `ai__snouty.md` | **Partial.** `validate`, `doctor`, `prepare`, preparation stage events (`crates/seedbox/src/cli/mod.rs:106-135`, `crates/seedbox-api/src/events.rs:9`) |
| Environment | Detect "running in Antithesis" (`ANTITHESIS_OUTPUT_DIR`) | `configuration__the_antithesis_environment.md` | **Missing.** No documented guest marker (inferred from `guest/compose-init.c`) |
| Workload | Test templates: `first_`/`parallel_driver_`/`singleton_`/`serial_`/`anytime_`/`eventually_`/`finally_` commands scheduled by the platform | `product__writing_tests__test_templates*.md` | **Missing.** One oracle program owns all driving (`docs/design.md`, "Application and oracle") |
| Workload | Platform-controlled randomness (`get_random`, `random_choice`) that steers exploration | `reference__sdk__generate_randomness.md`, `reference__sdk__cpp__random.md` | **Partial.** Guest `getrandom` is seeded and reseeded per branch. No structured choice points |
| Workload | Custom faults run at random intervals | `product__writing_tests__controlling_faults__fault_types.md` | **Partial.** The oracle can do anything at its own times via the control API (`python/seedbox/control.py`) |
| Properties | `always`, `alwaysOrUnreachable`, `sometimes`, `reachable`, `unreachable`, plus numeric/compound variants | `product__writing_tests__assertions.md`, `reference__sdk__cpp__assert.md` | **Missing** |
| Properties | Assertion catalog (declare at startup so never-hit ones fail) | `reference__sdk__fallback__assert.md`, `reference__sdk__assertion_cataloging.md` | **Missing** |
| Properties | Fallback SDK: JSONL to `$ANTITHESIS_OUTPUT_DIR/sdk.jsonl`; `ANTITHESIS_SDK_LOCAL_OUTPUT` | `reference__sdk__fallback*.md`, `reference__sdk__rust.md:75`, `reference__sdk__go.md:94` | **Missing.** The app-to-engine path today is console bytes only (`guest/compose-init.c:489`, `crates/seedbox-engine-core/src/triggers.rs:1`) |
| Properties | Lifecycle `setup_complete`, named events | `reference__sdk__fallback__lifecycle.md` | **Partial.** `MachineReady` observation (`crates/seedbox-engine-protocol/src/observation.rs:49`). No app-named events |
| Properties | Default properties: no crashes (signals), no unexpected exits, peak memory, Go/Rust/Java panic patterns, setup checks | `reference__default_properties.md` | **Partial.** `MachineExited{code}` and oracle verdict only (`observation.rs:49-90`) |
| Exploration | Branching multiverse from interesting moments; RL guidance from coverage and assertions | `introduction__how_antithesis_works.md`, `product__debugging__advanced_multiverse_debugging__moment_branch.md`, `best_practices__sometimes_assertions.md` | **Partial (primitives only).** Checkpoint plus branch with new branch identity (`crates/seedbox-application/src/engine_adapter/world.rs:250-256`). One-boot multi-seed fork (`crates/seedbox-vmm/src/lib.rs:892-975`, `crates/seedbox-engine-protocol/src/launch.rs:48`). `explore` is consecutive seeds (`crates/seedbox-application/src/exploration.rs:1`, `cli/mod.rs:252`). No guidance |
| Exploration | Coverage instrumentation, per language | `product__writing_tests__instrumentation__coverage_instrumentation.md` | **Missing.** But every application thread already runs under a DynamoRIO client (`guest/progress/progress.c:170`) |
| Faults | Network: latency, partitions (stopped/slowed/jammed), clogs, restore | `..._fault_types.md` | **Have, richer.** Partition and shape with loss, bandwidth, queue, reorder, duplicate, burst loss, spikes, TCP RST (`seedbox-control/src/lib.rs:16-80,145-161`). Connection-scoped drop (`crates/seedbox-engine-core/src/connections.rs`) |
| Faults | Node pause / kill / stop / throttle | `..._fault_types.md` | **Have.** Pause, Resume, Crash, Reboot, Stop, Restart, PauseProcess, Throttle (`seedbox-control/src/lib.rs:318-410`) |
| Faults | Clock skips | `..._fault_types.md` | **Have.** Per-machine ClockSkew and ClockDrift, which is finer than Antithesis's global skips (`seedbox-control/src/lib.rs:359-384`) |
| Faults | Thread pausing (needs instrumentation); CPU modulation | `..._fault_types.md`, `..._pause_faults.md` | **Partial.** Seeded progress jitter and timer jitter (`project.rs:160`, `crates/seedbox-spec/src/lib.rs:168`). No targeted preemption |
| Faults | Faults random by default, continuously | `product__writing_tests__controlling_faults.md` | **Missing.** Faults come only from the oracle, the TUI or overrides |
| Faults | Quiet periods (`ANTITHESIS_STOP_FAULTS`), terminal pause for `eventually`/`finally` | `..._pause_faults.md` | **Missing** |
| Faults | Disk faults | Only "durably synced or lose modified data; talk to your FDE" (`..._fault_types.md`) | **Partial.** Crash drops the volatile layer; opt-in torn subset (`crates/seedbox-engine-core/src/disk.rs:1-10`, `engine_adapter/world.rs:118`). No IOERR, latency or lying flush |
| Faults | Fault events in logs, with vtime | `reference__event_logs.md` | **Have.** `FaultChanged` events, timeline API (`observation.rs:55`, `crates/seedbox-api/src/timeline.rs`) |
| Results | Triage report: findings (New/Ongoing/Resolved/Rare), properties, utilization, environment | `product__understanding_test_results__reports*.md` | **Missing.** Per-simulation outcome only |
| Results | Logs Explorer with regex and temporal queries (preceded-by), multiverse map | `product__understanding_test_results__logs_explorer.md`, `reference__event-set-reference.md` | **Partial.** `logs`/`events` with time and incarnation filters, JSON streams (`cli/mod.rs:364-390`). No query language, no cross-branch view |
| Results | Artifacts at failure (core dumps), time-travel artifact collection | `..._reports__properties.md` | **Missing** |
| Debugging | Multiverse debugger: rewind, branch, run bash in a container, extract file, profiler | `product__debugging__simple_mvd.md`, `..._bash_env.md`, `..._moment_branch.md` | **Partial.** Checkpoint, branch, derive with edited timeline, TUI timeline editing (`crates/seedbox-server/src/http.rs:62-70`, `timeline.rs`). No exec or extract |
| Debugging | Counterfactuals ("if I flip X, does the bug happen?") | `..._cookbook.md` | **Partial.** Derive with edited override timeline plus `compare` (`crates/seedbox-engine-core/src/input_program.rs:38`) |
| Debugging | Causality analysis (bug probability vs. rewind point) | `product__debugging__causality_analysis.md` | **Missing.** Primitives exist (branch from checkpoint with a new seed) |
| Debugging | Retroactive logging | `product__launching_tests.md` | **N/A.** Any moment can be deterministically re-executed; see section 3 |
| Debugging | Determinism checking | (implicit) | **Have, ahead.** Witness, MATCH, divergence interval (`crates/seedbox-artifacts/src/comparison.rs:17-46`) |
| Workflow | API/CLI launch, CI action, webhooks, nightly vs dev loop, ephemeral runs | `product__launching_tests.md`, `workflows*.md` | **Partial.** Local server API plus CLI with `--json` (`http.rs:47-83`). No CI integration docs |
| AI | Agent skills (research, setup, workload, triage, debug, mutation testing), Snouty | `ai__skills.md`, `ai__snouty.md` | **Missing.** The etcd example has a `-plant` checker sanity control (`examples/etcd/workload/main.go:214`) |

---

## 2. Where Seedbox can be better, not just equal

I sort these by how real the advantage is.

**Real, strong.**

1. **Storage faults at block-request granularity.**
   - The engine handles every virtio-blk request synchronously in `Block::notify` (`crates/seedbox-engine-core/src/virtio_block.rs:172-225`). The disk model already separates volatile from stable state (`disk.rs:1-10`).
   - Seedbox can therefore:
     - fail a specific write or flush with IOERR (fsyncgate-class bugs);
     - acknowledge a flush without persisting it (a lying disk);
     - crash at the k-th flush, or while the volatile layer is non-empty;
     - add flush latency.
   - All of this is deterministic and needs no guest cooperation. Antithesis documents nothing beyond node restart durability.
   - Random-time crashes mostly land when nothing is unflushed. Event-relative storage crashes hit the WAL-tail and rename-atomicity window directly.

2. **Explicit, editable, minimisable fault schedules.**
   - Seedbox's faults are keyed overrides in one timeline (`crates/seedbox-spec/src/definition.rs:99`). Checkpoints carry the input cursor (`input_program.rs:38`, `replace_pending`).
   - That makes delta-debugging a failing schedule down to a minimal fault set mechanical, and it can restart from the checkpoint just before the first changed override.
   - Antithesis offers probabilistic causality analysis but documents no minimisation.

3. **Whole-binary coverage with no rebuild.**
   - Every application thread, including JIT code, already runs under DynamoRIO (`docs/internals.md`, "Progress from translated blocks").
   - Adding edge coverage to that client covers any language without per-language instrumentors, `/symbols` catalogs or LTO caveats (`..._coverage_instrumentation.md`).
   - Caveat: hashes must be module-relative because guest ASLR differs per seed. Enabling coverage may also change translator syscall counts, and so timing (see section 4b).

4. **Local, offline, open, multi-architecture.**
   - Runs on a laptop (arm64 HVF, x86 KVM). Branching is local and fast, with no core-hour billing.
   - Antithesis debug sessions take 10–30 minutes to "rewarm" (`product__debugging__simple_mvd.md`).
   - This is a real dev-loop advantage and a weakness at scale (see below).

5. **Non-perturbing host-side inspection.**
   - Seedbox holds disk sectors (stable plus volatile), per-link frame fates (`observation.rs:96-123`), connection incarnations and fault preview (`crates/seedbox-server/src/fault_preview.rs`).
   - Files can be extracted from a checkpoint's disk without running anything in the guest. Packet traces need no tcpdump.
   - Antithesis extracts files by running commands in a branch.

**Real but modest.**

6. **Event-anchored faults and branching.** Seedbox already arms a fault on a console marker (`seedbox-control/src/lib.rs:187-235`, `triggers.rs`). Generalising predicates to SDK events, assertions, connection opens, lifecycle and block operations allows "from every leader-election moment, try each of K faults within 0–50 ms". The limits:
   - **Precision.** Console- and SDK-derived events are seen only at committed round boundaries; the round is bounded by minimum network latency, 1 ms by default (`docs/internals.md`, "Parallel scheduling"), and guest output buffering adds more. Only device-local predicates (block requests) are exact.
   - **Ranking.** Guided random search finds many of the same states. Anchoring mostly helps users encode intuition.

7. **Antithesis compatibility as an adoption lever.**
   - Antithesis SDKs write JSONL to `ANTITHESIS_SDK_LOCAL_OUTPUT` when run outside Antithesis (`reference__sdk__rust.md:75`, `reference__sdk__python.md:114`). Seedbox can ingest that format, so existing instrumented code works unmodified.
   - Test templates are a directory convention that a guest-side runner can execute.
   - Needs a licensing and trademark check; the SDK repos are believed MIT-licensed **(inferred)**.

**Conditional or skeptical.**

8. **"Fault exactly when a timeout fires."**
   - The VMM sees only the kernel's aggregated next clockevent (`kernel/seedbox.c:205-231`), which is dominated by kernel ticks, not application timeouts.
   - The useful version is *timeout-aware parameters*:
     - a kernel hook records the durations of timed waits (futex, epoll, poll, nanosleep, timerfd);
     - the generator then picks pause and delay durations just below or above observed timeouts (lease, election, heartbeat).
   - It is plausible, but it is a heuristic. It belongs after guidance (P2).

**Not advantages; be honest.**

- **Scale.** Antithesis runs many VMs in parallel for hours and explores tens of thousands of timelines.
- **Guidance maturity.** Antithesis uses RL guidance.
- **Single-vCPU guests.** Seedbox cannot expose intra-machine SMP memory-ordering bugs. Interleavings come only from preemption timing.
- **Ecosystem.** Seedbox has no k8s support, no web UI, no findings history.

---

## 3. Recommended functionality, prioritized

### P0: a run produces properties and findings

| # | Item | User-visible behaviour | Why | Depends | Size |
|---|---|---|---|---|---|
| P0.1 | **Guest SDK channel and property model** | Apps or oracles emit assertions and events by writing JSONL (Antithesis fallback schema, `reference__sdk__fallback__schema.md`) to `/run/seedbox/sdk.jsonl`. The supervisor sets `ANTITHESIS_SDK_LOCAL_OUTPUT` to that path. `seedbox get properties ID` lists each property: pass/fail, counts, first failing example with position. `events` includes `sdk` records | Properties are the unit of findings, and the guidance signal for P1. Compatibility gives day-one SDKs in Go, Rust, Python, Java and C++ | none | L |
| P0.2 | **Default properties** | No fatal signal in any process; no OOM kill; no unexpected machine exit; every machine became ready; oracle passed. Optional regex log properties (Rust/Go panics) via a request option | Antithesis's out-of-box value: "many customers find bugs at this stage" (`product__writing_tests.md`) | P0.1 types; kernel hooks on x86 and ARM | M |
| P0.3 | **Seeded fault generator** | `seedbox run --faults profile.yaml` (bounded YAML, reusing `cli/faults.rs`). The server materialises a seeded override schedule (kinds, rates, durations, targets) into the resolved definition. The schedule is visible and editable in the TUI timeline and replayable without the generator | Random faults are Antithesis's default. Materialising keeps one override timeline and keeps the engine policy-free | none | M |
| P0.4 | **Quiet periods** | Oracle control op `quiet(duration)` and `seedbox.quiet()` in the Python client. Generated overrides falling in a quiet window are rejected at admission, with a recorded reason. Optional "heal everything now" | Needed for liveness/eventual checks (`..._pause_faults.md`) | P0.3 | S |
| P0.4b | **Setup-gated fault start** | The SDK `setup_complete` gates generated faults; the generator does not start before readiness | Avoids wasting runs during boot | P0.1 | S |
| P0.5 | **Storage faults** | New actions:<br>• `disk_error{target, op, sectors?, ppm or count}` (IOERR on read, write or flush)<br>• `disk_latency{target, op, latency, jitter}`<br>• `disk_lie{target}` (flush acknowledged but not persisted)<br>• `disk_corrupt_read{target, sectors, mode}` (silent corruption, opt-in)<br>New trigger predicates: crash at the k-th flush, or at a flush or write while volatile data exists. All of these are in the fault catalogue, preview and TUI | Finds fsync-error handling, WAL torn tails and checksum gaps. This is the clearest differentiator (section 2.1) | P0.3 for generation | M–L |
| P0.6 | **Findings and reproduction** | A failing property creates a Finding: property, simulation, boundary position, definition, `seedbox replay` command. `seedbox get findings`, `describe finding`, and export as a bundle | Turns runs into actionable bugs | P0.1 | S–M |

Deliberately not in P0:

- **A web UI.** The TUI plus `--json` is enough.
- **A Seedbox-branded SDK per language.** Use the fallback schema first.
- **Findings history across server restarts.** It conflicts with the in-memory catalogue rule; see section 5.

### P1: guided branching exploration

| # | Item | User-visible behaviour | Why | Depends | Size |
|---|---|---|---|---|---|
| P1.1 | **`reseed` override and path flattening** | A new override action `reseed{identity}` re-derives host RNG streams and guest CRNG at a committed boundary. Any branch becomes `seed + overrides` and replays cold | Explored paths become ordinary definitions. Replay never needs a snapshot ("Cold run/replay require no snapshot", AGENTS.md) | existing branch-identity and fork-entropy (`world.rs:1256`, `kernel/apply.py:190`) | M |
| P1.2 | **Branching explorer v1 (server-driven)** | `seedbox explore DIR --branches N --faults profile.yaml`. It boots once, checkpoints at setup complete, repeatedly picks a node, branches with `reseed` plus a fresh fault suffix for a simulated horizon, scores novelty, and keeps checkpoints of novel nodes. Prints `PASS`/`FAIL: campaign-id` with findings. The existing seed sweep becomes `--strategy seeds` | Antithesis's core: amplify rare states by restarting from them (`best_practices__sometimes_assertions.md`) | P0.1, P0.3, P1.1 | L |
| P1.3 | **Coverage from the DynamoRIO client** | Edge coverage (module-relative hashed edges) in a per-machine bitmap that the VMM reads at boundaries. Feeds the explorer. `seedbox get coverage` shows edges over branches (Antithesis "Utilization") | Generic guidance for code with no assertions | P1.2 | L |
| P1.4 | **Generalised triggers** | `arm_trigger{on: console_marker or sdk_event or assertion or block_op or connection or lifecycle, nth, delay, fault: any Action}`. Triggers can be armed by overrides as well as by the oracle | Event-relative faults for every fault kind. Lifts the "initial surface" limit (`seedbox-control/src/lib.rs:212`) | P0.1, P0.5 | M |
| P1.5 | **Seeded preemption ("thread pausing")** | Definition-level `preempt_ppm`. At a progress call the kernel forces a reschedule on a seeded draw | Interleaving diversity inside one machine (`..._fault_types.md`, thread pausing) | none | M |
| P1.6 | **In-worker snapshot tree** | Same CLI, 10–100x more branches per hour **(inferred)**. The worker keeps many dirty-page-delta snapshots and runs branches without relaunching | Throughput is the explorer's lifeblood | P1.2 | XL |
| P1.7 | **Test-template runner** | A guest-side Rust binary (`seedbox-composer`) run as the oracle entrypoint. It executes `/opt/antithesis/test/v1/<template>/` or `/opt/seedbox/test/<template>/` commands with Antithesis scheduling rules. Random choices come from `getrandom`; it calls `quiet` for `eventually_`/`finally_` | Lets the platform steer workload composition. Porting path from Antithesis. Stays inside the oracle per AGENTS.md | P0.4 | M |

Deliberately not in P1:

- **RL or learned guidance.** Start with deterministic novelty scoring.
- **Userspace PRNG interception.**
- **Multi-vCPU guests.**

### P2: triage and debugging

| # | Item | Behaviour | Depends | Size |
|---|---|---|---|---|
| P2.1 | **Fault-schedule shrinking** | `seedbox shrink ID` runs ddmin over generated overrides and reseeds, never over oracle actions. Each probe starts from the checkpoint before the first removed entry. Prints the minimal definition | P0.6, P1.1 | M |
| P2.2 | **Causality analysis** | `seedbox causality ID [--points 20 --trials 16]`: at positions along the failing path, run K `reseed` branches and plot failure probability against time with nearby logs (`product__debugging__causality_analysis.md`) | P1.2 | M |
| P2.3 | **Moment debugging** | `seedbox debug ID --at 12.5s --machine n1 -- netstat -tn` creates a derived simulation with an `exec` override, streams output, and prints the new simulation ID. `seedbox extract ID --at T --machine M --path /data/wal` reads files host-side from the disk layers at a checkpoint (ext4 reader), falling back to exec+cat | Checkpoint plus derive | M–L |
| P2.4 | **Event query** | `seedbox events --query 'assert.id=="x" && !preceded_by(fault.partition, 2s)'` over a simulation or campaign (`reference__event-set-reference.md`, `with_last`) | P0.1 | M |
| P2.5 | **Timeout-aware fault parameters** | Kernel hook records timed-wait durations into a PV-page histogram. The generator samples pause, partition and shape durations around the observed values ±ε | P0.3 | M |
| P2.6 | **Agent skills and docs** | Skills for setup, property research, triage via `--json`, and mutation testing (like `ai__skills.md`) | P0 | S |

Deliberately not in P2:

- **Kubernetes.**
- **A multiverse map UI.** A TUI tree view at most.
- **Retroactive logging.** Rerunning a window with verbose flags via a derived branch is equivalent, because everything is deterministic.
- **Custom faults "invoked at random intervals" by the platform.** The oracle already does this.

---

## 4. Required architecture

### 4a. Exploration engine

**Ownership.**

| Piece | Owner | Notes |
|---|---|---|
| Pure policy: node scoring, selection, fault-suffix generation, budget accounting | New crate `crates/seedbox-explore` | No Tokio, server or hypervisor dependencies. Unit-tested against `world_mock`-style fixtures |
| Campaign resource, scheduling and persistence of node and checkpoint references | `crates/seedbox-server/src/exploration/` | Calls `seedbox-explore` |
| Resource types `Campaign`, `CampaignStatus`, `Node`, `Finding` | `crates/seedbox-api` | |
| Seed sweep | `crates/seedbox-application/src/exploration.rs` | Stays as `--strategy seeds` |
| Application library | — | Stays independent of server and CLI |

**Core types** (in `seedbox-explore`):

```rust
struct CampaignSpec { source: DefinitionDigest, explore_seed: u64, budget: Budget,
                      profile: FaultProfile, horizon_ns: u64, strategy: Strategy }
struct Budget { branches: u64, simulated_ns: u64 }      // never wall time
struct NodeId(u64);                                      // dense, assigned in fold order
struct Node { id: NodeId, parent: Option<NodeId>, position: BoundaryPosition,
              path: Vec<Override>,                       // flattened: reseeds + faults
              features_new: FeatureSet, depth: u32, picks: u32, cache: CheckpointRef }
struct BranchPlan { parent: NodeId, ordinal: u32, reseed: u64, suffix: Vec<Override> }
struct BranchResult { plan: BranchPlan, end: BoundaryPosition, features: FeatureSet,
                      property_events: Vec<PropertyEvent>, verdict: Option<Outcome> }
enum Feature { Edge(u32 /*hash*/, u8 /*bucket*/), Assert(String, bool),
               SometimesFirst(String), Event(String), Fault(String),
               Lifecycle(String, u8), DiskCrashWithVolatile(String) }
```

**Per-campaign loop.** The loop is generation-synchronous, so it is deterministic.

1. **Root.** Run the source definition to the first SDK `setup_complete`, or to `MachineReady` for all machines, then capture a checkpoint. That is node 0.
2. **Generation g.** Pick K plans from the frontier with RNG domain `explore/<explore_seed>/gen/<g>`:
   - **Weight** = (sum of 1/count(f) over the features the node discovered first) + a bonus if the node is where a `sometimes`/`reachable` assertion was first satisfied (Antithesis's rule) − a decay for `picks`. Depth is capped.
   - **Reseed:** `reseed = H(explore_seed, node, ordinal)`.
   - **Suffix:** generated from the `FaultProfile` over `[position, position + horizon)`. It is keyed `x/<node>/<ordinal>/<n>` so override keys are unique.
3. **Execute.** Run the K plans on any number of workers.
4. **Fold.** Fold results **sorted by (parent, ordinal)**:
   - update feature counts;
   - create child nodes for plans with new features, checkpointing at `end`;
   - record findings.

Worker count only changes wall time, which follows the AGENTS rule that performance settings never change results. Wall time may end a campaign early only as a request option; the campaign then ends `INCOMPLETE`, like `--wall-time`.

**Execution v1 (P1.2).** Each plan is an ordinary server simulation:

- `Start::Checkpoint` from the parent's durable checkpoint (`definition.rs:111`);
- `overrides = parent.path ++ [reseed at position] ++ suffix`; `Start::validate_overrides` already enforces that the consumed prefix is preserved (`definition.rs:120-160`);
- run with a request option `--until-position` (simulated horizon), then checkpoint and stop.

The horizon is a **request option**, not a definition field, which follows the "no simulated-time limits in definitions" rule.

**Execution v2 (P1.6).** A `WorkerRequest` gains an `explore` mode that generalises the existing `fork_seeds` loop (`seedbox-vmm/src/lib.rs:908-975`):

- The worker owns a `SnapshotTree`: a content-addressed 4 KiB page store with per-node page maps, built by hashing dirty pages at capture. Restore writes only the pages that differ between the current node and the target.
- Device, coordinator and input-program state come from `WorldSnapshot`, which already captures `input_program`, `control`, `branch_identity` and `faults` (`world.rs:1405-1430`).
- Branch commands arrive over host control; the worker returns per-branch `BranchResult`s.
- The existing Undo supports only one baseline ("Multiple held snapshots cannot share Undo's single dirty-page baseline", `world.rs:1436`), and `LiveCheckpoints` caps holding at 8 (`checkpoint.rs:124`). Both must change. Spill old nodes to the server's content cache.

**Coordinator and windowed scheduler.** Branch points are only committed round boundaries with no owed I/O (`docs/internals.md`, "Checkpoints and lifecycle"). `reseed` is applied by the coordinator between rounds, in the fixed order alongside control, trigger and fault boundaries. It must also:

- reseed every named RNG domain with the new branch identity, as checkpoint branches do today (`checkpoint::branch_identity`, `engine_adapter/world.rs:253`);
- bump the PV entropy generation so each guest kernel takes new entropy (`kernel/apply.py:190-222`);
- reseed timer, progress and speed jitter as `release_plan` does (`world.rs:1257-1265`).

**Determinism.**

| Risk | Mitigation |
|---|---|
| A checkpoint restore diverges from the cold path | Every node is reproducible cold (`seed + path`). Checkpoints are caches. A sampled cold re-execution check (`compare`) runs at a configurable rate |
| `reseed` at a position differs from a branch started at a checkpoint at that position | Define checkpoint branching *as* `reseed`. Implement once, and have the existing `Checkpoint{branch_seed}` path call it |
| Explorer nondeterminism (hash maps, completion order) | `BTreeMap` everywhere; fold in sorted order; selection RNG on its own domain |
| Feature extraction reading guest memory perturbs the guest | Read only at stopped boundaries, read-only, never writing the vCPU (same discipline as the watchdog, `docs/internals.md`, "Observation without guest progress") |

### 4b. Guest SDK, property model and getting events out deterministically

**Guest side.**

- `compose-init.c` creates `/run/seedbox/sdk.jsonl` as a FIFO before starting the application. It adds `ANTITHESIS_SDK_LOCAL_OUTPUT=/run/seedbox/sdk.jsonl` and `SEEDBOX_SDK=/run/seedbox/sdk.jsonl` to the process environment.
- Preparation adds these variables to the resolved `Process.environment` (`definition.rs:61`), so they are visible and part of identity rather than a hidden supervisor behaviour.
- The supervisor's poll loop (`service_channels`, `compose-init.c:739`) drains the FIFO as it drains stdout and stderr (`relay_output`, `:489`). It emits a third application-output stream type, `sdk`, on the management port, with a 64 KiB line bound.
- Readers that open late, or writers that write before the reader opens, are handled because the supervisor opens the read end first.

**Engine side.**

- New `crates/seedbox-engine-core/src/sdk.rs`: a streaming JSONL decoder fed **only from committed management bytes**, the same rule as `triggers.rs:1-2`. It produces `SdkRecord { machine, incarnation, time_ns, seq, kind: Declare | Assert{..} | Setup{details} | Event{name, details} | Malformed }`.
- New observation `Kind::Sdk(SdkRecord)` (`observation.rs:49`). Retained SDK records go into run evidence as an `sdk` log owned by `seedbox-artifacts`.
- The engine does no aggregation: it is mechanism only.

**Property model.** New `seedbox-artifacts/src/properties.rs`, offline and pure:

```rust
enum PropertyKind { Always, AlwaysOrUnreachable, Sometimes, Reachable, Unreachable, Default(DefaultKind) }
struct Property { id: String, message: String, kind: PropertyKind, declared: bool,
                  pass: u64, fail: u64, first_pass: Option<Example>, first_fail: Option<Example> }
struct Example { simulation: SimulationId, position: BoundaryPosition, machine: String, details: Value }
fn evaluate(records: impl Iterator<Item=SdkRecord>, defaults: &[DefaultEvent]) -> BTreeMap<String, Property>
fn merge(a, b) -> ...   // campaign aggregation; commutative and associative, so fold order doesn't matter
```

The semantics follow `product__writing_tests__assertions.md`:

- `always` fails on any false and also fails if never hit when declared `must_hit`;
- `sometimes` passes on any true;
- `unreachable` fails on any hit.

Numeric and compound guidance forms (`ALWAYS_GREATER_THAN`, `SOMETIMES_ALL`) arrive as ordinary JSONL with details; treat them as boolean properties first.

**Default properties.** Guest-kernel hooks on both architectures publish compact records to a per-machine PV event ring that the VMM drains at stopped exits. The records are fatal-signal delivery (pid, comm, signal), OOM kill (victim, RSS) and segfault address. The ring is deterministic guest state.

The alternative is printing to the console; the ring is better because it doesn't pollute application logs. The supervisor adds "main process exited with code/signal".

**Server, API and CLI.**

- `GET /v1/simulations/{id}/properties`, `GET /v1/campaigns/{id}/properties`.
- `seedbox get properties ID` prints a compact table; `--json` prints one document.

**Exit status.** Recommended rule: `run` exits **1 (FAIL)** if the oracle fails *or* any `always`/`unreachable`/default property fails. Sometimes-never-hit is reported but is not a FAIL by default. This is a decision for the user (section 5).

**Coverage (P1.3).**

- The DynamoRIO client (`guest/progress/progress.c`) adds AFL-style edge instrumentation: `map[(prev ^ cur) & mask]++`, where `cur = hash(module_path) ^ (pc − module_base)`. That is ASLR-independent.
- The map is a 64 KiB page set that the seedbox kernel exposes to every instrumented process (for example a `/dev/seedbox-cov` mmap). The VMM reads it at boundaries via its guest-physical address published on the time page.
- Translator-inserted instructions are not counted as application progress, so virtual time is unchanged by the instrumentation itself.
- **Risk:** DynamoRIO's extra mmaps and code-cache growth are syscalls and memory that *are* charged. Coverage on and off may therefore produce different results.
- **Recommendation:** coverage is always on (keeping "one engine mode"). If its cost is too high, make it a resolved-definition field under `simulator`, never an environment variable or performance flag.

**Determinism risks for 4b.**

- SDK writes are ordinary guest work, so emission is deterministic.
- The engine sees bytes only when committed, so property events are identical across replays.
- Malformed lines are recorded, never fatal.
- The SDK channel must never block the application on a slow host: it is a FIFO drained by the supervisor, with a bounded pipe. A full pipe blocks the writer *deterministically*, because the supervisor's drain schedule is itself guest-deterministic.

### 4c. Fault model expansion

**New `seedbox-control` actions** (`lib.rs:106,318`), with matching fault-catalogue category `Storage` (`crates/seedbox-server/src/fault_catalogue.rs:9`) and previews:

```rust
DiskError   { target, op: DiskOp /*read|write|flush|any*/, sectors: Option<Range<u64>>,
              ppm: Option<u64>, count: Option<u32> }          // healable handle
DiskLatency { target, op: DiskOp, latency_ns: u64, jitter_ns: u64 } // healable
DiskLie     { target }                                         // flush acks without stabilising
DiskCorruptRead { target, sectors: Range<u64>, mode: Zero | BitFlip } // opt-in, healable
Quiet       { duration_ns: u64 }                               // oracle-only, P0.4
Reseed      { identity: u64 }                                  // overrides only, P1.1
Exec        { target, argv: Vec<String>, timeout_ns: u64 }     // overrides in derived branches, P2.3
```

**Where storage faults execute.**

- A `BlockFaults` policy is passed into `Block::notify` (`virtio_block.rs:172`), alongside a per-machine RNG stream `disk-fault/<machine>/<incarnation>` (the same domain convention as `disk-loss/...`, `disk.rs:58-75`).
- The policy is part of each machine's immutable round assignment. The coordinator installs and removes it only between rounds.
- Decisions therefore depend only on machine-local state, so the parallel windowed scheduler stays exact.
- IOERR uses the existing `S_IOERR` path and keeps the completed-prefix semantics (`docs/internals.md`, "Device completion and disk state").
- `DiskLie` makes `flush()` acknowledge without moving sectors to the stable layer. A later crash then applies the existing `disk_loss` policy.
- Latency needs an I/O completion delay. The block model currently completes synchronously. Reuse the "owed I/O cost" mechanism, charging the machine (`docs/internals.md`, "Progress from translated blocks", x86 owed charge) rather than inventing an asynchronous device. Treat this as **M** effort with care around checkpoint "no owed I/O".

**Generalised triggers** (`triggers.rs`, `control.rs:146`).

```rust
enum Predicate {
  ConsoleMarker{source, marker},                       // existing
  Sdk{source, name: Option<String>, assert_id: Option<String>, condition: Option<bool>},
  Block{source, op: DiskOp, nth: u32, sectors: Option<Range<u64>>, volatile_nonempty: bool},
  Connection{from, to, dst_port: Option<u16>, event: Open | NthFrame(u32)},
  Lifecycle{target, transition: MachineTransition},
}
ArmTrigger { on: Predicate, match_timeout_ns, delay_ns, fault: Action /*any*/ }
```

Evaluation and timing:

| Predicate source | Where it is matched | Effect time |
|---|---|---|
| Console and SDK | At coordinator commit, from committed bytes | `max(match_time + delay, next boundary)` (as today) |
| Connection | Where the classifier already runs, at routing | Next boundary |
| Lifecycle | At commit | Next boundary |
| Block | **Device-local** in `notify` | See below |

For a block predicate:

- an IOERR effect applies to *that* request;
- a `Crash` effect makes the machine park at that exit and report `TriggerFired{at}`; the coordinator commits the crash at round end with effect time `at`.
- This is consistent because no other machine can observe the crashing machine's post-`at` state within the round: round length is bounded by minimum latency, and frames it sent before `at` keep their routing.
- The disk's volatile layer is captured exactly at `at`.
- This needs a new park reason in `world_windowed.rs`, analogous to `park_for_fork` (`world_windowed.rs:1172`).

Triggers are armed either by the oracle or by keyed overrides, so they sit in the one override timeline: they are editable, minimisable and replayable. Raise `MAX_TRIGGERS` (64, `lib.rs:204`) for generated profiles, or recycle fired slots.

**Fault generator (P0.3).**

- `FaultProfile` lives in `seedbox-explore`, with a YAML reader in the CLI. It defines:
  - kinds with weights;
  - mean inter-arrival time;
  - duration distributions, including `around_timeouts` in P2.5;
  - target selection (machines, links, connection-scoped);
  - storage rules;
  - trigger templates, for example "crash `{machine}` at flush k ∈ [1, 20] while volatile is non-empty".
- The server samples it with domain `faults/<seed>` (or a per-branch domain) and writes keyed overrides into the resolved definition before execution.
- **Replay never runs the generator**, consistent with "oracle calls ... must not be replayed a second time as overrides" and "one deterministic override timeline".
- Admission rejects overrides that are invalid in the current state (for example, crashing an already-crashed machine). The rejection is recorded and the cursor advances (`docs/internals.md`, "Fault ordering and replay").

**Seeded preemption (P1.5).**

- At each progress call (`guest/progress/progress.c:44`, the kernel's progress-call path in `kernel/seedbox.c`), the kernel draws from the existing per-machine jitter stream. With probability `preempt_ppm` it sets `need_resched`.
- `preempt_ppm` is a resolved-definition field next to `cpu`, because it changes results.
- **Risk:** it consumes the same stream as `cpu.jitter`. Use a separate PV seed slot so enabling preemption doesn't change jitter draws.

### 4d. Results: findings, triage, reproduction, multiverse debugging

**Finding.**

```rust
struct Finding { id, property: String, kind: PropertyKind, campaign: Option<CampaignId>,
                 simulation: SimulationId, position: BoundaryPosition,
                 definition: Definition /* cold: seed + flattened overrides */,
                 minimized: Option<Definition>, examples: u64, findability: f64 }
```

- **Grouping:** findings are grouped by property ID, the same as Antithesis ("one property per unique message").
- **Findability:** within a campaign, fraction of branches that hit the property. This is a cheap analogue of "Rare" (`..._reports__findings.md`).
- **Ownership:** the server catalogue (session-scoped). `export` writes a bundle with the definition and referenced assets (`seedbox-artifacts/src/bundle.rs`).

**Reproduction.**

- `seedbox replay FINDING` runs the cold definition until the finding's position, which is a request option (a stop position, not a definition limit). It reports `MATCH` when both the witness and the property event agree.
- Because the path is flattened (P1.1), a finding never depends on a checkpoint or a campaign still existing. That honours "do not introduce dependencies on a prior simulation ID for replay".

**Shrinking (P2.1).**

- ddmin over `overrides` whose origin is `Program` (generated or authored), plus `reseed` entries. Oracle actions are never touched.
- Each probe derives a simulation (the existing `derive` endpoint, `http.rs:62`) starting from the latest cached checkpoint before the first changed entry. That is valid because the consumed prefix is unchanged (`definition.rs:120`).
- The property must fail at any position, not necessarily the original one.
- The output is a minimal cold definition plus a `compare` divergence against the original (`seedbox-artifacts/src/comparison.rs:27`).

**Causality (P2.2).**

- Choose positions p₀…pₙ along the failing path, at checkpoints or at re-executed boundaries.
- At each, run T branches with `reseed(H(finding, i, t))` and no change to the suffix of authored overrides. Generated suffixes are optionally regenerated per trial.
- Report P(fail | p_i), with logs and events in each interval where the probability jumps.
- This is Antithesis's coin-flip method (`product__debugging__causality_analysis.md`) built on P1 machinery. Output is `--json` plus a TUI sparkline.

**Multiverse debugging.**

- **Moment** = `(simulation, BoundaryPosition)`.
- **Rewind** = derive from the nearest earlier checkpoint, or from the cold start, and run to the position. This is deterministic; no "rewarm" is needed beyond re-execution.
- **Exec:**
  - an `Exec` override handled by the supervisor's control path (`compose-init.c:691`) forks the command in the application's namespace and user;
  - stdout and stderr come back as a distinct output stream;
  - it exists only in derived simulations, so the original timeline is untouched;
  - the derived simulation is itself replayable.
- **Extract:** read the target's disk layers at a checkpoint in the host (stable plus volatile, `disk.rs:86-97`) with a Rust ext4 reader **(inferred crate choice)**. No guest perturbation at all, which is an advantage over Antithesis's exec-based extract.
- **TUI:** add a properties pane, a findings list and a campaign tree (nodes as rows with feature counts). Selecting a node opens its simulation in the existing timeline view.

**CLI conventions.**

- New commands: `explore` (extended), `shrink`, `causality`, `debug`, `extract`, `get properties|findings|campaigns`, `describe finding|campaign`.
- They follow the existing rules: a single status word (`PASS`/`FAIL`/`MATCH`/`INCOMPLETE`), long kebab-case flags, `ValueEnum` for `--strategy`, budgets validated in a `value_parser`, no environment variables for anything that changes results.

**Determinism summary.**

| Feature | Risk | Preservation |
|---|---|---|
| SDK | Host-side parsing timing | Parse only committed bytes; aggregation offline |
| Generated faults | Generator drift across versions | Materialised into overrides; replay never regenerates |
| Storage faults | Parallel workers | Policy in immutable round assignment, per-machine RNG stream |
| Block-triggered crash | Mid-round lifecycle | Park at exit; coordinator commits at the recorded time |
| `reseed` | Divergence from checkpoint branch | Single implementation; checkpoint branch defined as reseed |
| Explorer | Worker completion order | Generation-synchronous fold in sorted order |
| Coverage | Translator syscalls change timing | Always on, or a definition field; never toggled per run |
| Exec / debug | Perturbs timeline | Derived simulations only; it is an override, so replayable |

---

## 5. Open questions and risks

1. **Exit-code meaning with properties.**
   - **Question:** should a failing `always`/default property make `run` exit 1 even when the oracle passes?
   - **Recommendation:** yes. Sometimes-never-hit is reported but is not FAIL unless `--require-sometimes` is set.
2. **Where the fault profile lives.**
   - **Recommendation:** a request input (`--faults`) materialised into overrides, keeping `seedbox.yaml` minimal per AGENTS.md.
   - **Alternative:** a `faults:` section in `seedbox.yaml`. It would expand the authoring surface, which AGENTS.md currently discourages.
3. **Coverage always-on vs. a definition flag.** Measure the overhead on etcd and the JVM before deciding. Either way it cannot be a performance setting.
4. **Environment injection for the SDK.** Adding `ANTITHESIS_SDK_LOCAL_OUTPUT` to every process changes behaviour for apps that use Antithesis SDKs. Do we always inject it, or only per machine (`sdk: true`)? A per-machine flag is also an authoring-surface change.
5. **Compatibility naming.** Supporting `ANTITHESIS_*` variables and `/opt/antithesis/test/v1` needs a trademark and licence check. Offer `SEEDBOX_*` equivalents in either case.
6. **Findings history across sessions.**
   - The catalogue is in-memory and "never load an old catalogue" (AGENTS.md).
   - New/Resolved/Rare tracking across nightly runs needs a durable findings store.
   - That would be an **explicit rule change**: a separate, append-only findings file that is never a catalogue.
   - **Recommendation:** defer, and use exported bundles.
7. **Stop positions and horizons as request options.**
   - Branch horizons and replay stop positions must stay outside `Definition` to honour "no simulated-time limits in definitions".
   - The consequence: a finding's reproduction is `(definition, stop position)`. Confirm this is acceptable for `replay`/`compare` semantics.
8. **Snapshot tree memory on Mac.** HVF guests are at most 3 GiB each (README). A deep tree over a 3-machine world needs page dedup and spill to the cache. Is P1.6 Linux-first?
9. **Single vCPU.** Accept the class of missed SMP bugs, or plan multi-vCPU (XL, and it touches the software reservation design)? Recommendation: accept for now and document it.
10. **Kernel hooks on two architectures.** Fatal-signal, OOM, preemption, coverage-map and timed-wait hooks each need x86 and ARM patches (`kernel/apply.py`, `kernel-arm64/apply.py`) plus native hardware checks. That is the main schedule risk for P0.2, P1.3 and P1.5.
11. **Explorer throughput before P1.6.** v1 relaunches a worker per branch from a durable checkpoint. It may be too slow to beat a plain seed sweep on small systems. Benchmark v1 against `--strategy seeds` on etcd before investing in guidance tuning.
12. **Precision claims for event-relative faults.** Market only block-level targeting as exact. Console/SDK-anchored faults have round-boundary granularity (at least the minimum network latency) plus guest output buffering.
