# Working on Seedbox

Seedbox runs Compose applications as a deterministic world of real Linux
machines on KVM. Read [README.md](README.md) first; it has the layout table.
[docs/vision.md](docs/vision.md) says where the project is going.

## Workspace

- `crates/seedbox` is the default package: `cargo run -- <command>` runs the CLI.
  Use `cargo run --release -- <command>` or the release executable for guest runs;
  the bundled engine uses the CLI build profile.
- Linux x86-64 asset builds use `cargo build --release` inside
  `nix develop`. The optional `SEEDBOX_LINUX_RUNTIME_BUNDLE` build input embeds
  only guest kernel/static helpers plus notices/provenance. Runtime extraction
  stays bounded and private, with an owned lifetime through input retention.
  CLI resolution owns environment lookup; the library receives explicit inputs.
  Doctor extracts only the selected kernel and never creates state/prepares
  images. Replay must not substitute installed package bytes for retained inputs.
  Linux package execution/reproducibility requires the final merge train.
- Apple Silicon application builds use `cargo build --release` inside
  `nix develop`; Cargo prepares guest assets and signs executable outputs. It packages the audited ARM kernel, static helpers and
  Linux preparation tools into the signed CLI. Runtime preparation uses a Docker/Podman endpoint and a Linux filesystem-tools
  container, with no second Seedbox executable. Replay must never require that
  engine or invoke Cargo. Mac application profiles use `arm64-world-v1`
  with their actual guest inputs and diagnostic HVF host observations. CLI
  platform selection lives in `cli/backends.rs`; the library
  receives explicit inputs and cancellation.
- `crates/seedbox-vmm` is Linux-only and deliberately **not** a workspace member.
  It is its own workspace with its own `Cargo.lock`, because cargo unifies
  dependency features across one build. The public CLI links its library only
  on Linux x86-64 and retains the complete CLI executable as the new engine.
  Standalone low-level fixtures keep a separate lockfile and identity. Build
  and test those from the repository root with the aliases in `.cargo/config.toml`:
  `cargo build-vmm` and `cargo test-vmm`. Both already pass `--locked`; adding
  it again is an error. It builds into `crates/seedbox-vmm/target`. Public
  runtime commands never invoke Cargo or search for a separate VMM by default.
  Don't set a global
  `target-dir` in `.cargo/config.toml`: cargo applies it by current directory,
  so it would also move standalone crate builds and break their scripts.
  These two aliases select `harness/vmm-rustc.py` for path remapping and removal
  of the Nix dev shell's synthetic checkout RPATH; keep that wrapper isolated
  from other builds. Keep release debug info enabled. On Linux, verify committed
  inputs with `bash harness/check-vmm-reproducible.sh [REV]` in the dev shell.
  The wrapper includes its content hash in Cargo's compiler fingerprint to
  invalidate older unremapped builds and later wrapper revisions.
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
  scenario runner. `kernel/` and `guest/` hold guest build inputs.

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
console/block/network; its compatibility profiles are documented in
`docs/virtio-transport.md`. Preserve error precedence, cursor resets and the
explicit ARM state projection. Adapters handle typed Notify/Reset effects and
retain model-reset ownership. The shared console owns handshake, backlogs, counters
and queue processing; adapters supply GPA base, dirty callbacks and event projection.
Preserve partial-error mutation and IRQ timing documented in `docs/virtio-console.md`.
The shared block model owns request decoding, disk effects and DMA, with descriptor
profiles and partial-error semantics in `docs/virtio-block.md`. Use explicit disk
and statistics accessors in platform callers.
The shared network model owns packet DMA, backlogs and counters; preserve the
profile-specific admission, partial-error and IRQ order in `docs/virtio-network.md`.
The core PV progress engine owns watchdog policy and cycle proof; architecture
adapters supply stepping and complete comparison primitives. Preserve the
proof/diagnostic boundary in `docs/progress-proof.md`; ARM support must not assume
a PMU or treat an unsupported dirty log as proof that RAM is unchanged.
The shared world coordinator owns scheduling, routing, control/trigger/fault
ordering, lifecycle policy and world snapshots. Machine adapters embed its single
`MachineState` and supply stopped execution and physical lifecycle operations.
Keep hard limits exact, including owed I/O; preserve the lifecycle commit sites
relative to fallible architecture actions. See `docs/world-coordinator.md`.
ARM uses this same coordinator through stopped, stamped worker primitives; no
second world scheduler belongs in the worker. Cold run/replay require no snapshot.
A failed crash transaction poisons the retained World and terminates owned worker
transports; retries must fail before any worker activity.

The standalone backends compile the canonical sources through their in-workspace
`engine-core` packages, with distinct package names just like their control
packages. Each `src` link points to the canonical directory; its build script
watches that whole directory, including new modules. Canonical and wrapper build
scripts track the automatic build-only `SEEDBOX_BUILD_ROOT` from `.cargo/config.toml`
to invalidate a reused target when switching checkouts, even if source timestamps
predate the cache. Run Cargo from the checkout root for caching; missing or unrelated
config safely forces a source rebuild on every invocation, including older checkouts.
The CLI `build-runtime.rs` uses the same invalidation and resolves Git and guest
inputs from the runtime package directory. Keep this input out of library sources
and runtime identity.
Keep inherited package and
dependency keys aligned across workspaces. These inputs change engine bytes and
require final Linux reproducibility/merge-train validation. Portable tests import
the canonical crate; do not restore sibling source inclusion for these policies.

## Identity and determinism

- The VMM binary's hash, the guest kernel, the guest supervisor
  (`guest/compose-init.c`, embedded by `crates/seedbox/build-runtime.rs`) and the
  semantic parameters together make up a run's identity. Any change to
  `crates/seedbox-vmm` produces a new engine: its source, its `Cargo.toml` or
  lockfile, its `[profile.release]`, or even reformatting (which moves panic
  line numbers; `crates/seedbox-vmm/rustfmt.toml` disables formatting). Build
  standalone fixtures only with `cargo build-vmm`. The linked CLI uses the
  root lockfile and records an explicit internal-worker contract and simulator
  version; historical profiles without it retain the standalone launch contract.
  Platform/version/qualification metadata never gates development replay.
  Don't tidy the VMM as a side effect of other
  work. `crates/seedbox-control/**`, `harness/vmm-rustc.py` and the VMM alias
  flags in `.cargo/config.toml` also determine engine bytes and identity. The
  VMM reaches the shared control source through its in-workspace control package so
  Cargo's crate metadata does not hash an absolute checkout path; its inherited
  workspace package and dependency keys must match the root workspace.
- Performance settings (`--margin`, `--undo`) must never change results.

## Checks

Engine PRs that need host evidence go through a merge train
([docs/merge-train.md](docs/merge-train.md)).

Portable checks must pass at the PR or merge-train head before spending AMD
host time. `bash harness/mac-check.sh REV` is the fast pre-check for a Mac or
any other non-KVM machine; it also works on the host. Run inside the project's
Nix dev shell. Omit `REV` to check the current checkout, including local changes.
The steps are fmt, clippy and `harness/acceptance.sh portable`, which includes
workspace tests and both Python unittest suites (plus VMM tests on Linux).
The first stdout line is `revision: FULL_SHA`, followed by the host check's
timed step/phase summary format. Workspace builds use a persistent cache under
`$XDG_CACHE_HOME`, or `~/Library/Caches` on macOS and `~/.cache` elsewhere;
override it with `SEEDBOX_PRECHECK_TARGET` or `CARGO_TARGET_DIR`. A revision uses
a temporary detached worktree under `$TMPDIR`, deleted on a pass; a failure keeps it and prints
its path. Any failed step makes the gate exit nonzero; all steps still run.
After a Linux pre-check pass, `bash harness/host-check.sh --skip-portable REV CPU` skips fmt,
clippy and the portable suite (including workspace tests), while retaining VMM
build/tests and host-specific checks. A Mac pass alone does not justify the skip.
Match the full SHA printed by both gates.
See [contributor checks](README.md#contributor-checks).

Run these before committing:

```sh
cargo fmt -p seedbox --check
cargo clippy -p seedbox --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test-vmm --features kvm-tests  # Linux; the feature needs /dev/kvm
bash harness/acceptance.sh portable
```

The `seedbox-vmm` crate has existing clippy findings. Don't fix them in an
unrelated change (see above).

## CLI

The CLI lives in `crates/seedbox/src/cli/`:

- `mod.rs`: argument definitions, dispatch and output.
- `defaults.rs`: how each runtime input is found when no flag is given.
- `signals.rs`: cancellation.

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
  DIRECTORY`, `prepare --output`, `diff RUN_A RUN_B`).
- A command's main object is positional: the Compose path (default `.`,
  and a directory resolves to its `compose.yaml`) or the run directory.
- Flags are long kebab-case. Don't add short flags except `-o`/`-h`/`-V`.
  Boolean flags are presence-only (`--json`, `--full-state`).
- A choice from a fixed set uses `#[derive(ValueEnum)]` with a doc comment on
  each variant. Never use string matching followed by `unreachable!`.
- Validate ranges and formats in a `value_parser`, so a bad value is a
  clap usage error that names the flag, not a runtime error later.
- Reuse the shared `#[derive(Args)]` groups (`ComposeArg`, `CpuArg`,
  `StateArg`, `RuntimeInputs`, `Performance`) instead of redeclaring flags.
  When several commands need a new option, add a group.
- Environment variables (`env = "SEEDBOX_<NAME>"`) say where things are: paths,
  CPU, state directory. Never back a parameter that changes results (seed,
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
   (image pulls, builds, CPU leases). Fail fast. `run` and `explore` check immutable runtime
   inputs and backend host admission before
   preparation; no transient perf descriptor, affinity or CPU reservation survives
   that check.
5. Add it to the defaults table in README.md's quick start.

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
  general-purpose groups (`CpuArg`, `StateArg`) before `RuntimeInputs`.
- `run`, `replay` and `diff` show `EXIT_CODES` in `after_help`.
- Check the result with `cargo run -- help <command>`.

### Output

- stdout carries results only. A command prints one status line
  `WORD: detail`, where `WORD` is the SCREAMING_SNAKE_CASE serde name
  produced by `word()` (`PASS`, `MATCH`, `VALID`, `CREATED`, ...). Any
  further lines are lowercase `key: value` (`replay: MATCH`, `error: ...`).
- `logs` without `--json` writes only the selected raw bytes to stdout, with no
  decoration or added newlines. Its lowercase `logs:` status and `omission:`
  diagnostics go to stderr; `--json` includes chunk metadata and `bytes_hex`.
- Scripts find the run directory with `sed -n 's/^[A-Z]*: //p'` over both
  output streams of `run` and `replay`. Exactly one line may match
  `^[A-Z]*: `, and it must be `OUTCOME: run-directory`.
- `--json` prints exactly one pretty JSON document to stdout and nothing else.
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
  says how to fix it: `--kernel /x/bzImage: not found`, or `skopeo not found;
  install it or enter nix develop`.
- Never let a bare `io::Error` from a user-supplied path or a spawned program
  reach the user. Check the path first (`existing`), or add context as
  `oci::spawn_error` does.
- Exit codes are a fixed contract (`EXIT_CODES`, README.md, docs/runtime.md,
  and the acceptance scripts): 0 PASS/MATCH, 1 FAIL/MISMATCH/DIFFERENT, 2
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
- When behaviour changes, update `seedbox help`, README.md (quick start and
  defaults table), docs/runtime.md, and `crates/seedbox/templates/README.md`.
  The last one is copied into every project `seedbox init` creates.

`doctor` shares execution admission and guest-kernel/VMM defaults; host-kernel
bytes are not checked. VMM resolution is lookup-only and must never invoke a
compiler or launch the worker. Success reports
only the exact cleaned-up probe observations and selected core; the library does
not read arguments/environment or print. It creates no state/run and never prepares
images or executes guests. Compatibility readiness is not replay qualification.

## Current stage

- Seedbox is under heavy development. Don't add pins, allowlists,
  qualification lists, admission policies, provenance markers or similar
  gates; #142 and #143 removed the engine, kernel, CPU and host-policy pins.
  When a test depends on such a check, change or drop the test. Text elsewhere
  about qualification or re-pinning is historical.
- An engine change (see above) gives every later run a new engine hash. Old
  runs still replay on their retained engine, but can't be replayed on the new
  one: to compare, make a fresh run and check its witness against the old run's.
- The default `counter` timebase needs an AMD Zen KVM host with guest perf
  counting. `--timebase pv` needs only KVM, so it also runs on cloud VMs with
  nested KVM and no PMU; `tools/droplet/pv-run.sh` runs batched PV checks on a
  throwaway DigitalOcean droplet.
- `crates/seedbox-vmm` builds only on Linux x86-64, so a Mac pre-check covers
  none of it. A VMM change needs `cargo build-vmm` and
  `cargo test-vmm --features kvm-tests` on a Linux KVM host. Without the
  feature, the KVM tests are skipped, not failed.
