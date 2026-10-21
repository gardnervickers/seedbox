# Working on Seedbox

Start with [README.md](README.md); [design](docs/design.md) and
[internals](docs/internals.md) cover non-obvious execution constraints.

## Scope and review

Seedbox is a testing utility for cooperative users and reasonably configured applications. Users provide supported images and dependencies, required permissions, and sufficient resources; when prerequisites are missing, normal setup or runtime errors are appropriate.

We do not need to support every possible application. If someone intentionally tries to break Seedbox or exercises pathological behavior, failure is acceptable; return a clear error where practical, but do not add architecture, workarounds, compatibility infrastructure, or special detection solely to support such cases. Keep supported workloads executing correctly with deterministic replay, normal cleanup, and clear error reporting; this is not permission to silently corrupt normal results.

- There is no backward-compatibility requirement: replace obsolete formats/APIs;
  do not add migration shims, qualification lists or provenance gates.
- CLI and `seedbox.yaml` interface changes need Gardner's approval; internal APIs
  have no separate approval/freeze process.
- Keep PRs short and review-ready/non-draft; reviews and merges go through the user.
  UI changes, including CLI output, need captured/inspected screenshots; label fixtures.

## Code boundaries

- `seedbox-server` owns resource lifetimes; only `seedbox-driver` coordinates service
  execution. `seedbox-machine` owns the contract/guest inputs; PV/TCG crates own leaves.
- `seedbox-driver-execution` owns retained Driver adaptation, runtime packages, native
  workers, transport and worker control. Application owns OCI preparation/orchestration.
- `seedbox-cli` owns arguments, environment and output; libraries take explicit inputs.
  `seedbox` owns native worker dispatch/signing. Keep application independent of server/CLI.
- `seedbox-engine-core` owns deterministic policies, not scheduling/devices;
  `seedbox-engine-protocol` owns wire/evidence types, not policy. `seedbox-artifacts`
  owns offline data; keep it independent of core, hypervisors, Bollard and Tokio.
- New crates inherit workspace version/edition/publish; shared dependencies belong
  in `[workspace.dependencies]`. Update `Cargo.lock` for dependency changes.

## Runtime and determinism

- Build inside `nix develop`; README owns the build sequence. Runtime lookup never
  builds. Replay consumes retained inputs without the image engine or source project.
- Direnv initializes missing submodules before Nix; use `tools/dev-flake.sh` for
  direct shell entry. Existing checkouts/edits stay untouched. Edit DR in the
  submodule, publish its commits first, then commit the Seedbox gitlink. See internals.
- Image preparation never starts application containers or extracts guest paths onto
  the host; see `crates/seedbox-application/src/oci.rs` and its archive renderer.
- Keep Cargo targets private per agent; never configure a global target-dir.
  Preserve build-only `SEEDBOX_BUILD_ROOT` checkout invalidation in build scripts.
- Check witnesses, replay MATCH and independent fresh runs. Worker counts/watchdogs
  cannot change results. Do not stamp source/build identity, wrap compilers or add
  formatting exceptions to stabilize engine bytes.
- Keep automatic progress application-independent. Host interruptions/maintenance
  cannot advance guest time or ARM reservation epochs; see internals.
- Definitions have one architecture and independent binary ownership, not a prior
  simulation-ID dependency. Oracle observations are not replay override inputs.
  Host execution limits are request options, not definition fields; see
  `crates/seedbox-spec/src/project.rs` and its sibling `definition.rs`.
- The catalogue/scratch are session-scoped; never reload old catalogues or delete
  unrelated offline data. Imports are non-idempotent; do not retry uncertain uploads.

## Tests and CLI

- Tests use positive signals/events for readiness, pacing and ordering, never
  incidental host sleeps. Generous containment timeouts bound positive completion.
  Generic elapsed host-time assertions and ordering inferred from timed absence
  are forbidden; the subject-timer exception below is limited to its stated oracles.
- In native DynamoRIO regression fixtures, retained original timer-accuracy oracles
  may compare independent real clocks during already-existing timer workloads:
  remaining time, expiry/cadence, legitimacy of observed expiries, and the signal
  storm's clock-derived expected total. Preserve the original populations, options,
  tolerances and noise/slack rules; report inconclusive observations explicitly,
  never as a pass. This permits subject-timer clock comparisons only, not added
  fixed sleeps/blocking, generic duration assertions, timed readiness/ordering,
  or success inferred from an event's timed absence. Sleep calls remain subject
  to the separate bounded syscall rule below.
- In native DynamoRIO C fixtures only, a bounded `nanosleep`/`clock_nanosleep` syscall
  may be the regression subject after faithful event/state alternatives are considered.
  Immediately precede each call with
  `/* seedbox-syscall-subject: nanosleep; bound-ms=N; invariant=... */`
  (or `clock_nanosleep`). Review must verify the actual bound and syscall-specific
  invariant; a marker permits one call, never incidental pacing or a whole function.
  The checker rejects Rust timer helpers and known sleep spellings in downstream
  DR Rust, C/header, assembly, Python, Go, JavaScript, Java and C# fixtures, including
  C function-address assignments, named sleep syscalls with casts and zero-fd
  select delays. It ignores comments, literals and ordinary C prototypes.
  This source lint does not resolve arbitrary wrappers, imports, generated code,
  numeric assembly syscall IDs or every language/API; review enforces the same
  readiness rule throughout the suite. A move into another language is no exemption.
- Unit tests stay beside components; public-boundary tests stay in their owning
  crate's `tests/`. Give each scenario one owner; share backend-neutral scenarios.
- Do not use `#[path]` or `include!` of Rust sources; move files to the default
  module location. Test helpers shared by several binaries go in the owning
  crate's `test_support` module behind `test-support` (enabled by a self
  dev-dependency); a helper for one binary is a child of a `tests/<name>/main.rs`
  target. Keep `#[path]` only where no layout works (e.g. a plain-rustc root),
  with a comment saying why.
- Run portable checks in `nix develop`; engine changes also need relevant native
  suites. Portable compiles native cases without running them. Missing requirements
  are blockers. Suite discovery/policy lives in `crates/seedbox-check/src/`.
- Ordinary Seedbox CI remains automatic. The expensive native DynamoRIO workflow
  is manual-only; after updating `vendor/dynamorio`, dispatch it for both x86-64
  and ARM64. See internals for local commands and hosted workflow details.
- CLI conventions live in `crates/seedbox-cli/src/`: reuse clap argument groups,
  validate before preparation, and keep computed defaults in `defaults.rs`.
  Environment selects paths/state, never result-changing inputs.
- Use `output.rs` for status/fields; stdout carries results, stderr plain diagnostics.
  Nonstreaming JSON is one library-shaped document. Raw logs/diagnostics preserve bytes.
- CLI returns contextual errors, never panics/unwrap/exit. Preserve exit codes in
  `EXIT_CODES` and tests; cover reachable flags/defaults/errors in `crates/seedbox/tests/cli.rs`.
- Keep help/examples and the README's etcd instructions accurate. Public docs describe
  only tricky current mechanisms in short sections; do not commit run reports or process diaries.
