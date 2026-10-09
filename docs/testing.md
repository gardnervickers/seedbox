# Running and adding tests

Tests are grouped into named platform suites. You pick the suites for the
machine you are on; the runner never selects tests by probing the host.

```sh
nix develop --command bash harness/check.sh   # portable (the default)
bash harness/check.sh linux-kvm               # any Linux KVM host, including droplets
bash harness/check.sh linux --cpu 14          # the AMD host
bash harness/check.sh mac                     # Apple Silicon
bash harness/check.sh linux-counter --list    # check requirements, build nothing
bash harness/check.sh --help                  # list suites and groups
```

`portable` always runs first, and a portable failure stops hardware execution.
Every named suite then runs. If a named suite's host requirements are missing
(no `/dev/kvm`, no guest PMU, no root, missing tools or fixture inputs), the
runner prints why and exits 2 before building anything. Failed checks exit 1.
There are no retries, fallback passes or silently skipped suites.

The runner builds as the current user, discovers Cargo test executables and runs
them. Linux tests that need root use existing passwordless sudo; the runner
never prompts or runs Cargo as root. Mac native tests run from signed disposable
copies, after their Cargo build has finished.

## Suites

| Suite | Platform and requirements | Cargo feature / coverage |
| --- | --- | --- |
| `portable` | Any development host | Root workspace, ARM mocks/policies, Python suites, guest source checks, YAML validation; also x86 backend unit tests on Linux x86-64 |
| `linux-kvm` | Linux x86-64 or ARM64, usable `/dev/kvm` | Native backend `kvm-tests`; no PMU or root required |
| `linux-counter` | Linux x86-64 KVM, guest PMU, root | VMM `counter-tests`; exact stops, checkpoints, forks, committed logs and triggers on a leased core. Implies `linux-kvm` |
| `linux-guests` | Linux KVM plus fixture kernel/initrd | Backend `guest-tests`; real guest determinism/checkpoint controls. Implies `linux-counter` on x86-64, `linux-kvm` on ARM64 |
| `linux-oci` | Linux x86-64, root, Skopeo, umoci, Podman, registry access | CLI `oci-tests`; real image import and Dockerfile behavior |
| `linux-apps` | Linux x86-64 KVM, root, OCI tools | CLI `application-tests`; every acceptance YAML runs twice, diff matches, replay matches, with full state. PV always; counter too when `linux-counter` is selected |
| `linux-pause` | Linux x86-64 KVM, root, supplied retained run/store | CLI `retained-run-tests`; pause/resume replay |
| `mac-hvf` | Apple Silicon, macOS 15+, HVF and codesign | ARM `hvf-tests`; native CPU/state and cancellation primitives |
| `mac-guests` | `mac-hvf` plus ARM fixture kernel/initrd | ARM `guest-tests`. Implies `mac-hvf` |
| `mac-apps` | `mac-hvf` plus Docker or Podman | CLI `application-tests` on ARM PV. Implies `mac-hvf` |

Groups: `linux` is `linux-kvm linux-counter linux-guests linux-oci linux-apps`;
`mac` is `mac-hvf mac-guests mac-apps`. `linux-pause` needs a retained run and is
never part of a group. `--cpu` applies to `linux-counter` and `linux-apps`.
Both apps suites need the guest kernel and runtime assets: `GH_TOKEN` or
`GITHUB_TOKEN` for the private release download, a populated Seedbox cache, or
`SEEDBOX_KERNEL` and `SEEDBOX_RUNTIME_ASSETS`. Otherwise they report unavailable.

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
downloads and caches current guest assets. The low-level guest and pause suites
fail with exit 2 when their fixture inputs are missing. Every hardware run prints a fresh report directory
under `target/check-reports/`, which survives development-shell cleanup.

| Suite | Input variables | Checkout defaults |
| --- | --- | --- |
| x86 entropy checkpoint | `SEEDBOX_TEST_KERNEL`, `SEEDBOX_TEST_INITRD` | `artifacts/bzImage`, `artifacts/entropy-fork.cpio` |
| ARM guest fixture | `SEEDBOX_ARM64_KERNEL`, `SEEDBOX_ARM64_INITRD` | `artifacts/arm64-hres/Image`, `artifacts/arm64-hres/init.cpio` |
| Applications | No guest-asset inputs required; optional CLI overrides remain supported | Release CLI downloads/cache, native container tools |
| Pause/resume | `SEEDBOX_HOST_PAUSE_RUN`, `SEEDBOX_HOST_PAUSE_STATE` | No default retained run |

Applications use a release CLI built from the current checkout. Linux runs PV
scenarios, plus counter scenarios when `linux-counter` is also selected. Mac runs ARM PV scenarios.
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
matching suite is selected. Neither path needs virtualization to compile.
The runner does not check or stamp Git revisions or checkout cleanliness.

## Adding a test

1. Add ordinary Rust `#[test]` functions to the appropriate crate. They run by
   default when no special host suite is required.
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

These additions need no runner edits. A new suite needs one feature, one entry in
`SUITES` and its requirements in `harness/check.py`. Do not register individual test names or change
scopes from a PR's changed-file list. Features control hardware execution while
portable builds retain type checking.
Unconditional `#[ignore]` remains appropriate for existing diagnostics outside
the maintained suite.

To check a different revision, check it out in a Git worktree and run the same
command there. Low-level standalone world campaigns remain separate diagnostic
tools, documented in `harness/worlds/README.md`.
