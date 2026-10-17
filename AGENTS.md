# Working on Seedbox

Read [README.md](README.md) first. [Design](docs/design.md) describes the
architecture; [internals](docs/internals.md) explains the tricky mechanisms.

## Pull requests

Changes that touch the user interface must include screenshots in the PR
showing what changed. This includes TUI screens and user-facing CLI output.
Capture and inspect the changed interface; include before/after views when they
help explain the change. Label fixture or test-rendered screenshots clearly.
Every PR must reference its originating issue. Use `Closes #N` only when the
issue is fully resolved; otherwise use `Refs #N`. Keep PR descriptions short.
Open review-ready PRs as non-drafts; reviews and merges go through the user.

## Workspace

- `crates/seedbox` is the default executable package: `cargo run -- <command>` runs the CLI.
  `crates/seedbox-application` owns configuration, preparation and orchestration.
  `seedbox-api` and `seedbox-spec` own transport-independent public types;
  `seedbox-client` owns the local HTTP client; `seedbox-server` owns the catalogue,
  resource lifecycle and execution service. Keep the application library independent
  of the server and CLI. Native worker dispatch remains in the executable.
  Use `target/release/seedbox` for guest runs;
  the bundled engine uses the CLI build profile.
- Build guest inputs with `cargo xtask build-assets`, then the complete CLI with
  `cargo build --release`, inside
  `nix develop`. Linux uses Nix directly; Mac uses a Docker-compatible Linux
  builder with a persistent Nix store. Outputs live together under `build/`;
  `build/assets` changes atomically after a successful asset build.
  Cargo embeds prepared `build/assets` by default; explicit `SEEDBOX_BUILD_HELPERS`
  and `SEEDBOX_BUILD_KERNEL` override those inputs.
  Complete builds embed the architecture-matched kernel in the executable. Runtime commands never run
  Nix, Cargo or asset builders. Replay uses retained inputs. CLI lookup owns
  environment access; library callers supply paths and cancellation explicitly.
- Application images are built/pulled/exported using Bollard through the user's
  Docker-compatible engine socket. The CLI resolves connections; library callers
  supply an explicit client. The engine owns build caching; `ocirender` merges
  exported image layers as archives without extracting guest paths on the host.
  Preparation never starts application containers. Replay never contacts the engine.
- `crates/seedbox-vmm` is Linux-only and deliberately **not** a workspace member.
  It is its own workspace with its own `Cargo.lock`, because cargo unifies
  dependency features across one build. The public CLI links its library only
  on Linux x86-64; the running CLI executable is the engine of every run and
  replay. Standalone low-level fixtures keep a separate lockfile. Build
  and test those from the repository root with the aliases in `.cargo/config.toml`:
  `cargo build-vmm` and `cargo test-vmm`. Both already pass `--locked`; adding
  it again is an error. It builds into `crates/seedbox-vmm/target`. Public
  runtime commands never invoke Cargo or launch a separate VMM.
  Don't set a global
  `target-dir` in `.cargo/config.toml`: cargo applies it by current directory,
  so it would also move standalone crate builds and break their scripts.
- New crates go in `crates/<name>/` and inherit `version`, `edition` and
  `publish` from `[workspace.package]`. Put a dependency used by two or more
  workspace crates in `[workspace.dependencies]`. Commit both lockfiles.
- Historical experiments and evidence are preserved at commit `788fc43`;
  the earlier standalone prototypes are at `bc07f35`. Keep new runtime work
  in the current crates, guest inputs and harness rather than reviving old
  prototypes. `tools/mac-tcg/` retains the experimental local Mac runner,
  its own pinned Nix shell and portable tests; it is not a production backend.
- `harness/` holds YAML acceptance scenarios with guest Python drivers, portable
  checks, and the batch runner for low-level world fixtures. Application
  acceptance uses `seedbox run` / `seedbox replay` directly; do not add a host
  scenario runner to acceptance or `harness/check.sh`. `kernel/` and `guest/`
  hold guest build inputs.
- `crates/seedbox-control-client` is the Rust guest client for the control RPC,
  mirroring `python/seedbox/control.py`.
- Jepsen-style reproductions live in `harness/jepsen/` (generated scenarios)
  and `crates/seedbox-jepsen` (oracles, clients, checkers, generator and the
  seed campaign runner). Write their oracles, checkers, generators and campaign
  tooling in Rust; use Python only for a real reason, such as a system whose
  only client library is Python. Linearizability checks use `wgl` models.
  The crate is its own workspace, outside `harness/check.sh`, because it
  depends on the private `wgl-checker` repository: use `cargo jepsen` and
  `cargo test-jepsen`, and `cargo jepsen build-oracle` before running a
  scenario. Its campaign runner is research tooling, not acceptance.

## Crate and test boundaries

`seedbox-engine-protocol` owns worker launch/configuration, the private shared
host-wait clock transport, and retained evidence formats. It has no CPU backend, scheduler or device policy. `seedbox-artifacts`
owns offline records, content storage, logs, comparison and bundles; it depends
on protocol types, never engine-core, a hypervisor, Bollard or Tokio. The
application crate reexports retained APIs and owns parsing, preparation,
orchestration and `engine_adapter` request/report adaptation. Native modules own
host admission and launching. Standalone workspaces have protocol wrappers with
canonical source links, just like their engine-core and control wrappers.

Construct real worlds through `World::new(WorldInputs { ... })`; keep coordinator
bookkeeping private. Use read-only evidence accessors and guarded lifecycle
commits instead of exposing mutable coordinator fields.

Unit tests live alongside their component under `src/`; integration tests live
in the owning crate's `tests/` and exercise public boundaries or interactions.
Give each scenario one owner. Keep format and offline artifact tests out of the
application crate unless they specifically exercise application composition.
Hardware tests remain feature-controlled and compiled by portable checks.

## Shared engine policies, queues and transport

`crates/seedbox-engine-core` owns deterministic RNG, disk, links, connection,
trigger, fault, control and network-counter policies, plus bounded split-ring
validation/completion, modern virtio MMIO register transitions and console/block/network models. It has no CPU backend or
UART dependency. Adapters pass actual transmitted bytes to its management framer;
legacy PC divisor-register writes remain in `seedbox-vmm/src/management_uart.rs`.
Keep mechanical policy moves separate from behavioral changes.

Queue adapters supply the guest physical RAM base and retain their existing
length, direction and diagnostic policy. ARM queue field names and serialization
order remain part of retained state evidence. KVM console and block completion
feed every used-ring write to their existing dirty-range journals; do not drop
or reorder those entries. The shared MMIO transport serves ARM devices and KVM
console/block/network; its adapter identities and common register contract are
defined in `crates/seedbox-engine-core/src/virtio_mmio.rs`. Preserve error
precedence, cursor resets and the
explicit ARM state projection. Adapters handle typed Notify/Reset effects and
retain model-reset ownership. The shared console owns handshake, backlogs, counters
and queue processing; adapters supply GPA base, dirty callbacks and event projection.
Preserve partial-error mutation and IRQ timing in `virtio_console.rs`.
The shared block model owns request decoding, disk effects and DMA, with shared
descriptor admission and completed-prefix semantics in `virtio_block.rs`. Use explicit disk
and statistics accessors in platform callers.
The shared network model owns packet DMA, backlogs and counters; use the common
admission and completed-prefix contract in `virtio_network.rs`.
The x86 PV watchdog (`machine/pv.rs`) is a diagnostic: it interrupts a run
with no exit to report where the guest is and resumes it, and must never
publish a plan, inject an interrupt or adopt guest time at that landing.
The shared world coordinator owns scheduling, routing, control/trigger/fault
ordering, lifecycle policy and world snapshots. Machine adapters embed its single
`MachineState` and supply stopped execution and physical lifecycle operations.
Keep hard limits exact, including owed I/O; preserve the lifecycle commit sites
relative to fallible architecture actions. See `docs/internals.md`.
ARM uses this same coordinator through stopped, stamped worker primitives; no
second world scheduler belongs in the worker. Cold run/replay require no snapshot.
x86 runs every world through the windowed scheduler (`window/`,
`world_windowed.rs`, [docs/internals.md](docs/internals.md)):
the coordinator applies control, trigger and fault boundaries, lifecycle,
forks and checkpoints between rounds, on the calling thread, from committed
state in a fixed order. arm64 runs its world through the same windowed
scheduler (`application/world.rs`), serially or on a pool; the legacy loop
(`world::simulate`) has no backend caller and stays only for its tests. Worker
count is a performance setting.
A failed crash transaction poisons the retained World and terminates owned worker
transports; retries must fail before any worker activity.

The standalone backends compile the canonical sources through their in-workspace
`engine-core` packages, with distinct package names just like their control
packages. Each `src` link points to the canonical directory; its build script
watches that whole directory, including new modules. The canonical packages and
the VMM wrappers share one build script, `crates/seedbox-engine-core/build.rs`,
which resolves the canonical source through the wrapper's link. Build scripts track the automatic build-only `SEEDBOX_BUILD_ROOT` from `.cargo/config.toml`
to invalidate a reused target when switching checkouts, even if source timestamps
predate the cache. Run Cargo from the checkout root for caching; missing or unrelated
config safely forces a source rebuild on every invocation, including older checkouts.
The application `build-runtime.rs` uses the same invalidation and embeds explicitly
supplied guest inputs. The executable build script owns native signing. Keep inherited package and
dependency keys aligned across workspaces. Portable tests import
the canonical crate; do not restore sibling source inclusion for these policies.

## Determinism

- A run's witness and a `seedbox replay` MATCH are how determinism is checked.
  Server-owned performance settings (watchdog interval and machine workers) must never change results.
- Determinism is about results, not source or build bytes. Runs never retain
  the engine executable; replay always uses the current engine and compares
  results. Each run records the engine's Git revision (`-dirty` with
  uncommitted changes) as provenance only: nothing gates, compares or keys
  caches on it. Don't add compiler wrappers, reproducible-build checks, or
  formatting exceptions (`#[rustfmt::skip]`, `rustfmt.toml`) to keep engine
  bytes stable. Reformatting, moving code and changing build flags are
  ordinary changes; run `cargo fmt` on the VMM like any other crate.
- The CLI runs worlds through its own `__engine` worker entry points. The worker
  reads one JSON launch request (`seedbox_engine_core::launch::WorkerRequest`,
  written as `output/worker-request.json`); the standalone VMM builds the same
  request from its flags for low-level fixtures. Platform/version metadata never
  gates replay.

## Checks

Engine changes need relevant native hardware checks as well as portable tests.

Run the suites for your platform inside `nix develop` before committing:

```sh
bash harness/check.sh                 # portable
bash harness/check.sh linux           # Linux KVM host
bash harness/check.sh mac             # Apple Silicon
```

Suites are named, never auto-detected: `portable`, `linux-kvm`,
`linux-guests`, `linux-oci`, `linux-apps`, `linux-pause`, `mac-hvf`,
`mac-guests`, `mac-apps`, `linux-apps-long`, `mac-apps-long`, plus the `linux`
and `mac` groups. The `-long` suites run `harness/acceptance/long`, workloads that
take tens of minutes each (real JVM brokers); they are never in a group, and the
`applications` corpus never reads that directory. `portable` compiles
every feature-gated test with `--no-run` and executes only portable tests; it
opens no KVM/HVF device and needs no root or registry access. It always runs
first and must pass before hardware execution. A named suite whose host
requirements are missing exits 2 before any build. `--list` checks requirements
without building.

See `harness/check.py` for suite requirements, discovery and fixture inputs.
Keep VMM engine tests compiled by default; conditional `ignore` attributes enable
hardware execution through features. Other feature-gated tests must be built by
the portable `--no-run` checks. Add no per-test registration lists. The runner
builds as the current user and signs disposable HVF test copies. Workers use
normal host scheduling; Seedbox does not reserve cores or pin CPUs. Check other revisions in a Git worktree.

## Native project and resolved simulation definitions

The current authoring format is `seedbox.yaml`, with `machines` and a dedicated
`oracle`. Keep network latency and each machine's virtual CPU (`cpu.speed`, `cpu.jitter` on that machine or the oracle) optional; advanced behavior
belongs in the oracle/control API. Do not add execution wall-time or simulated-time
limits to either definition. Host timeouts are request options (`--wall-time`).
The server resolves omitted defaults and image references before execution.
Resolved definitions inline structured settings and reference binary content by
hash, with one simulation-wide architecture. Provenance annotations are not
ownership. Do not introduce dependencies on a prior simulation ID for replay.
TUI edits use one working copy; saves and submissions freeze independent snapshots.
Use one deterministic override timeline for unchanged and edited runs; oracle calls
are generated by the oracle and must not be replayed a second time as overrides.
See `crates/seedbox-spec/src/project.rs`. Earlier Compose details below describe the legacy
reader and must not expand the native authoring surface.

## CLI

The CLI lives in `crates/seedbox/src/cli/`:

- `mod.rs`: argument definitions and one function per command.
- `output.rs`: status lines, `key: value` fields and their value formats.
- `defaults.rs`: how each runtime input is found when no flag is given.
- `backends.rs`: platform backend selection and guest-asset lookup.
- `diagnostics.rs`: completed server-owned raw console/management/worker evidence.
- `bundles.rs`: server export/import and atomic no-clobber output publication.
- `signals.rs`: cancellation.
- `simulations.rs`: server-client commands and durable control receipts.
- `observe.rs`: read-only observation (`logs`, `events`, `get world`, `get timeline`, `wait`).
- `faults.rs`: bounded YAML/JSON fault input before connecting to the server.
- `playback_args.rs`: positional playback-control arguments and rate validation.

`src/main.rs` dispatches the private retained worker before calling `cli::main()`.
`engine_worker.rs` is a process boundary alongside the public CLI. The library
(everything else in `src/`)
never parses arguments or reads environment variables, and prints only cleanup
failures from `Drop` that it cannot return. The CLI is that boundary.

Follow these rules when you change the CLI, and update this section if you
deliberately change one.

### Arguments

- Required flags are a last resort. Every runtime input needs a sensible
  default or a search, so `seedbox run` works with no arguments on a set-up
  host. The only required inputs are those with no possible default (`init
  DIRECTORY`, `prepare --output`, `compare BASELINE SIMULATION`, `import BUNDLE`).
- A command's main object is positional: the project path (default `.`,
  and a directory resolves to its `seedbox.yaml`), a simulation ID, or an import bundle file.
  `run`, `explore`, `rerun` and `replay` submit through the server; clients never
  launch execution directly. `replay ID` reruns then compares; `rerun ID` reports
  the oracle verdict. `export ID` saves a completed resolved resource; `import BUNDLE`
  registers a fresh independent completed resource without executing it. Imports
  are non-idempotent; never automatically retry an uncertain upload.
- Flags are long kebab-case. Don't add short flags except `-o`/`-h`/`-V`.
  Boolean flags are presence-only (`--json`, `--detach`).
- A choice from a fixed set uses `#[derive(ValueEnum)]` with a doc comment on
  each variant. Never use string matching followed by `unreachable!`.
- Validate ranges and formats in a `value_parser`, so a bad value is a
  clap usage error that names the flag, not a runtime error later.
- Reuse the shared `#[derive(Args)]` groups (`ComposeArg`,
  `StateArg`, `RuntimeInputs`) instead of redeclaring flags.
  When several commands need a new option, add a group.
- Environment variables (`env = "SEEDBOX_<NAME>"`) say where things are: paths
  and state directory. Never back a parameter that changes results (seed,
  memory, full-state, faults, schedule) with an environment variable; it would
  change a run's identity without appearing on the command line.

### Defaults for a new input

1. Declare it as `Option<PathBuf>` with `env = "SEEDBOX_<NAME>"` in the
   relevant group.
2. Add a resolver in `defaults.rs` that returns `existing("--flag", path)` for
   an explicit value and otherwise searches in a fixed, documented order.
3. On failure, return `missing(what, &searched, fix)`, which lists every
   location tried and one way to fix it.
4. Resolve it in `RuntimeInputs::backends` or earlier, before any slow work
   (image pulls and builds). Fail fast. `run` and `explore` check immutable runtime
   inputs and backend host admission before
   preparation.
5. Document the default in CLI help.

### Help text

- Every argument and subcommand has a doc comment. Write it as a short phrase
  starting with a capital letter and ending with a period. A subcommand's line
  is imperative ("Run ...", "Show ...") and names any host requirement in
  parentheses: "(Linux KVM host, root)".
- When clap can't show a default (an `Option` with a computed default), end the
  doc with `[default: ...]` in clap's format.
- Group flags under the existing `next_help_heading` sections: the default
  options, "Runtime inputs", and "Performance (never changes results)". A flag
  takes the heading of the group declared before it, so flatten
  general-purpose groups (`StateArg`) before `RuntimeInputs`.
- `run`, `replay` and `compare` show `EXIT_CODES` in `after_help`.
- Check the result with `cargo run -- help <command>`.

### Output

- Resource lists and `get` print compact tables by default. `describe` and
  `playback` print full YAML. `--json` preserves structured documents without
  a status-line prefix.
- stdout carries results only. A command prints one status line
  `WORD: detail`, where `WORD` is the SCREAMING_SNAKE_CASE serde name
  produced by `word()` (`PASS`, `MATCH`, `VALID`, `CREATED`, ...). Any
  further lines are lowercase `key: value` (`replay: MATCH`, `error: ...`).
  Print both through `cli::output` rather than `println!`.
- `logs ID MACHINE` writes selected application bytes to stdout with no decoration
  or added newlines. Omission diagnostics go to stderr; `--json` emits
  newline-delimited records with stream, time, incarnation and byte arrays.
  `events ID` also emits newline-delimited records. Both print retained history
  and exit; `--follow` keeps streaming until completion, like `kubectl logs`.
  Logs keep bounded history in a shared 64 MiB server budget and events in their
  own 16 MiB budget, for completed simulations too. Observation commands never
  block by default; `wait ID --for CONDITION` is the explicit way to block.
  `diagnostics ID [NAME]` lists completed raw evidence or streams the exact retained
  file bytes. Its `--json` prints one metadata or lossless hex-payload document.
  Diagnostic names are allowlisted record keys, never client host paths.
- `run` and `rerun` print `OUTCOME: simulation-id`; detached submission prints
  `CREATED: simulation-id`. `replay` and `compare` print
  `MATCH|MISMATCH|NOT_COMPARABLE: simulation-id`. Exactly one status line is
  printed. Comparison does not change the stored oracle verdict. Scripts should
  prefer `--json` and resource metadata instead of relying on host run paths.
- Apart from live streaming commands, `--json` prints exactly one pretty JSON document to stdout and nothing else.
  Its field names come from the library's serde types; don't rename them in
  the CLI.
- Progress and diagnostics go to stderr: lowercase, plain text, no colour,
  no emoji.

### Errors and exit status

- CLI code returns `seedbox::Error`. It never panics, calls `unwrap`, or calls
  `process::exit`. `main` prints `error: <message>` and exits 2.
- Use `Error::Setup` for a problem the user can fix (a missing file, a missing
  tool, the wrong host, a permission). It prints without a category prefix.
  Its message starts lowercase, names the path, flag or value involved, and
  says how to fix it: `--kernel /x/bzImage: not found`, or `guest assets missing;
  run cargo xtask build-assets`.
- Never let a bare `io::Error` from a user-supplied path or a spawned program
  reach the user. Check the path first (`existing`), or add context as
  `WorkerLaunch::spawn` (`worker_process.rs`) and `host::msr_error` do.
- Exit codes are a fixed contract (`EXIT_CODES` and the acceptance scripts): 0 PASS/MATCH, 1 FAIL/MISMATCH/DIFFERENT, 2
  ERROR/NOT_COMPARABLE/usage/setup, 3 INCOMPLETE, 124 TIMEOUT. Don't change
  what a code means. A new code must be added to all four places.

### Tests and docs

- Every new flag, default and error path that the test build can reach gets a
  case in `crates/seedbox/tests/cli.rs`. Those tests run `CARGO_BIN_EXE_seedbox`
  through `seedbox_with`, which clears the environment (so no `SEEDBOX_*`,
  `CARGO*` or `SUDO_USER` leaks in) and sets `HOME` and `PATH` explicitly.
  Check exact stderr for setup errors. Usage errors come from clap, so checking
  for the key phrase is enough.
- Pure resolution logic gets unit tests in `defaults.rs`.
- When behaviour changes, update CLI help and any affected examples. Keep the
  root README quickstart and the generated project quickstart in
  `crates/seedbox-application/templates/README.md` accurate.

`doctor` checks KVM device access and the guest
kernel/VMM/runtime inputs. It uses guest-kernel/VMM defaults; host-kernel
bytes are not checked. VMM resolution is lookup-only and must never invoke a
compiler or launch the worker. Success reports
only the exact cleaned-up probe observations; the library does
not read arguments/environment or print. It creates no state/run and never prepares
images or executes guests. Compatibility readiness is not replay qualification.

## Current stage

Obsolete formats, APIs and CLI commands are replaced outright. There is no backward
compatibility requirement; do not add legacy readers, aliases or migration shims.

- Seedbox is under heavy development. Don't add pins, allowlists,
  qualification lists, admission policies or similar gates; #142 and #143
  removed the engine, kernel, CPU and host-policy pins. Provenance such as the
  engine revision is recorded, never checked.
  When a test depends on such a check, change or drop the test. Text elsewhere
  about qualification or re-pinning is historical.
- Replay runs the current engine. A MATCH against an older run shows the current
  engine reproduces its results; a MISMATCH after an engine change is either an
  intended change in results or a determinism bug. Checkpoints whose snapshot
  format an engine no longer reads fail to restore.
- There is one engine mode: PV time with automatic application progress
  from binary instrumentation (`docs/internals.md`). Do not require
  application-specific flags or environment overrides to make replay succeed.
  ARM uses guest-epoch software exclusive reservations; host interruptions and
  backend maintenance must not advance their epoch. Keep the shared PV word,
  kernel EL0 return hook and translator commit checks aligned. Kernel polling
  through `cpu_relax` makes bounded PV progress on both architectures; raw
  assembly retry loops remain outside that coverage (see `docs/internals.md`).
  `.github/workflows/pv-tests.yml` runs its checks on GitHub-hosted Ubuntu
  x86-64 runners.
- `crates/seedbox-vmm` builds only on Linux x86-64, so a Mac pre-check covers
  none of it. A VMM change needs `cargo build-vmm` and
  `cargo test-vmm --features kvm-tests` on a Linux KVM host. Without the
  feature, KVM tests compile but remain ignored.

If no local server is running, an interactive client asks before starting one.
The server stays running for subsequent commands. Noninteractive callers must
start `seedbox serve` explicitly. Use `seedbox shutdown` to stop the server and
its active work gracefully.

The running server uses an in-memory catalogue. Execution files are session-scoped
under `state/sessions/session-*`; shutdown removes them after workers finish,
and startup reclaims abandoned sessions while holding the server lock. Never
load an old catalogue or delete unrelated offline run data.

The simulation input program is separate from application configuration and host playback.
See `docs/internals.md` for authored keys, due-time admission, same-time ordering,
checkpoint cursor ownership and API-owned timeline presentation. Future inputs never
reserve engine fault IDs; clients never reconstruct an execution spec to edit a timeline.

## Documentation

Keep public documentation in README.md, docs/design.md, and docs/internals.md.
README covers getting started; design covers architecture and principles;
internals explains mechanisms whose ordering or state is easy to get wrong.
Use short, factual sections and link to the owning code. Do not commit run
reports, implementation diaries, or duplicate command references. Use CLI help
for options. The template README is the quickstart generated by `seedbox init`.
