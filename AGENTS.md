# Working on Seedbox

Start with [README.md](README.md); [design](docs/design.md) and
[internals](docs/internals.md) cover non-obvious execution constraints.

## Scope and review

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
  Elapsed host-time assertions and ordering inferred from timed absence are forbidden.
- In native DynamoRIO C fixtures only, a bounded `nanosleep`/`clock_nanosleep` syscall
  may be the regression subject after faithful event/state alternatives are considered.
  Immediately precede each call with
  `/* seedbox-syscall-subject: nanosleep; bound-ms=N; invariant=... */`
  (or `clock_nanosleep`). Review must verify the actual bound and syscall-specific
  invariant; a marker permits one call, never incidental pacing or a whole function.
  The checker still rejects Rust timer helpers and all unmarked DR fixture sleeps.
- Unit tests stay beside components; public-boundary tests stay in their owning
  crate's `tests/`. Give each scenario one owner; share backend-neutral scenarios.
- Run portable checks in `nix develop`; engine changes also need relevant native
  suites. Portable compiles native cases without running them. Missing requirements
  are blockers. Suite discovery/policy lives in `crates/seedbox-check/src/`.
- CLI conventions live in `crates/seedbox-cli/src/`: reuse clap argument groups,
  validate before preparation, and keep computed defaults in `defaults.rs`.
  Environment selects paths/state, never result-changing inputs.
- Use `output.rs` for status/fields; stdout carries results, stderr plain diagnostics.
  Nonstreaming JSON is one library-shaped document. Raw logs/diagnostics preserve bytes.
- CLI returns contextual errors, never panics/unwrap/exit. Preserve exit codes in
  `EXIT_CODES` and tests; cover reachable flags/defaults/errors in `crates/seedbox/tests/cli.rs`.
- Keep help/examples and both quickstarts accurate. Public docs describe only tricky
  current mechanisms in short sections; do not commit run reports or process diaries.
