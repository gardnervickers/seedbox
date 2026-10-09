# Running and adding tests

From the project development shell, one command runs every available suite:

```sh
nix develop --command bash harness/check.sh
```

The runner reports the host and every selected or unavailable capability. It
builds as the current user, discovers Cargo test executables, and runs them.
Linux tests that need root use existing passwordless sudo; the runner never
prompts or runs Cargo as root. Mac native tests run from signed disposable
copies, after their Cargo build has finished.

```sh
bash harness/check.sh portable                  # No virtualization or root
bash harness/check.sh --list                    # Inspect selection without tests
bash harness/check.sh --require hvf             # Require Mac hardware coverage
bash harness/check.sh --require kvm             # Require KVM coverage
bash harness/check.sh --require counter --cpu 14 # Require leased PMU coverage
```

`--require` is repeatable. Missing required capabilities exit 2; failed checks
exit 1. Default auto mode runs all available suites and explains unavailable
ones. A failed portable check prevents hardware execution. A selected suite
that fails stays failed; there are no retries or fallback passes.

## Environments and features

| Capability | Environment | Cargo feature / coverage |
| --- | --- | --- |
| Portable | Any development host | Root workspace, ARM mocks/policies, Python suites, guest source checks, YAML validation; also x86 backend unit tests on Linux x86-64 |
| `kvm` | Linux x86-64 or ARM64, usable `/dev/kvm` | Native backend `kvm-tests`; no PMU required |
| `counter` | Linux x86-64 KVM, usable guest PMU, root | VMM `counter-tests`; exact stops, checkpoints, forks, committed logs and triggers |
| `hvf` | Apple Silicon, macOS 15+, HVF and codesign | ARM `hvf-tests`; native CPU/state and cancellation primitives |
| `oci` | Linux x86-64, root, Skopeo, umoci, Podman, registry access | CLI `oci-tests`; real image import and Dockerfile behavior |
| `guests` | Native backend plus fixture kernel/initrd | Backend `guest-tests`; real guest determinism/checkpoint controls |
| `applications` | Linux x86 KVM plus OCI tools, or Mac HVF plus Docker/Podman | CLI `application-tests`; every acceptance YAML runs twice, diff matches, replay matches, with full state |
| `retained-run` | Linux x86-64 KVM, root, supplied retained run/store | CLI `retained-run-tests`; pause/resume replay |

AMD and Intel run the same counted-execution tests. The engine selects its
counter profile through CPUID. The test supervisor leases a free core and its
SMT siblings, applies the AMD workaround where required, and restores exact MSR
values after reaping every owned process. Intel uses no AMD MSR writes. This
test selection makes no claim about untested Intel hardware or cross-host replay.

Platform dependencies remain selected by Cargo target OS/architecture. Features
enable test execution; they do not switch the production CPU backend or pin a
host. `counter-tests` includes `kvm-tests`. Root and standalone backend workspaces
are invoked separately, using their existing aliases, lockfiles and target directories.

## Guest and application inputs

Application coverage needs no local kernel or runtime bundle: the release CLI
downloads and caches current guest assets. The low-level guest and retained-run
suites join the default command when their fixture inputs are present. Use
`--require guests`, `--require applications`, or `--require retained-run` when
their absence should fail the job. Every run prints a fresh report directory
under `target/check-reports/`, which survives development-shell cleanup.

| Suite | Input variables | Checkout defaults |
| --- | --- | --- |
| x86 entropy checkpoint | `SEEDBOX_TEST_KERNEL`, `SEEDBOX_TEST_INITRD` | `artifacts/bzImage`, `artifacts/entropy-fork.cpio` |
| ARM guest fixture | `SEEDBOX_ARM64_KERNEL`, `SEEDBOX_ARM64_INITRD` | `artifacts/arm64-hres/Image`, `artifacts/arm64-hres/init.cpio` |
| Applications | No guest-asset inputs required; optional CLI overrides remain supported | Release CLI downloads/cache, native container tools |
| Pause/resume | `SEEDBOX_HOST_PAUSE_RUN`, `SEEDBOX_HOST_PAUSE_STATE` | No default retained run |

Applications use a release CLI built from the current checkout. Linux runs PV
and, where the PMU is available, counter scenarios. Mac runs ARM PV scenarios.
Linux Dockerfile builds use `--build-epoch 0` for repeatable build timestamps.
Mac uses its container builder's ordinary timestamp policy; that CLI option is
Linux-only. Each CLI step streams to its report log and prints its progress.
Application guests use 2048 MiB, including space for the JVM image's boot archive.
Image pulls, application builds and guest execution can be substantially slower
than native unit tests. Their assertions belong to the guest workload and public
CLI; the test runner does not interpret application protocols.

Portable checks compile all Cargo features with `--no-run`, then run default
portable tests without hardware. VMM checkpoint, trigger and machine tests stay
compiled by default; `cfg_attr(..., ignore)` enables execution only when the
matching capability is selected. Neither path needs virtualization to compile.
The runner does not check or stamp Git revisions or checkout cleanliness.

## Adding a test

1. Add ordinary Rust `#[test]` functions to the appropriate crate. They run by
   default when no special host capability is required.
2. Keep VMM engine test bodies compiled by default and use conditional
   `ignore` attributes for hardware execution. Other suites may gate hardware
   modules with their existing feature, for example `kvm-tests`; the portable
   suite must compile those features with `--no-run`. Retain target OS/architecture
   guards for platform APIs. Use `required-features` in `Cargo.toml` when a
   complete integration target needs a capability.
3. Put Python tests in `harness/tests/test_*.py` or `python/tests/test_*.py`.
   Standard unittest discovery finds new files. Declare genuine platform
   requirements with `unittest.skipUnless` in the tests themselves.
4. Add application cases as `harness/acceptance/workloads/*.yaml` with guest-owned
   checks. Both validation and application acceptance discover new files.

These additions need no runner edits. A new capability needs one feature and
one capability selection rule. Do not register individual test names or change
scopes from a PR's changed-file list. Features control hardware execution while
portable builds retain type checking.
Unconditional `#[ignore]` remains appropriate for existing diagnostics outside
the maintained suite.

To check a different revision, check it out in a Git worktree and run the same
command there. Low-level standalone world campaigns remain separate diagnostic
tools, documented in `harness/worlds/README.md`.
