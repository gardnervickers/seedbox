# AMD PR host runner

The optional [NixOS module](../nix/host-runner.nix) installs a pool of GitHub
Actions runners for `gardnervickers/seedbox` on `nixos`, the qualified Ryzen
5950X host. The repo previously had only project development flakes, so this
host module lives under `nix/` and is also exported as
`nixosModules.host-runner`. Gardner imports it into his host configuration and
applies that configuration himself. Nothing in a PR deploys the module.

## Setup

Use a reviewed, immutable Seedbox source revision in the host's system flake,
then import its module into the existing `nixos` entry in nixcfg. For example,
with a `seedbox` flake input pinned to that revision:

```nix
{
  imports = [ inputs.seedbox.nixosModules.host-runner ];
  services.seedbox-host-runner = {
    enable = true;
    maxParallel = 2; # 1..15; one service per simultaneous job
    tokenFile = "/run/secrets/seedbox-runner-token";
    kernel = "/var/lib/seedbox-artifacts/bzImage";
    pinnedEngine = "/var/lib/seedbox-artifacts/seedbox-vmm";
  };
}
```

Alternatively import `seedbox-source/nix/host-runner.nix` directly. The module
uses the system flake's nixpkgs for host packages; each workflow enters the
project dev shell from its trusted workflow revision for source builds. Do not
use `path:.` for a worktree flake. Do not add the runner to Nix `trusted-users`,
`wheel`, Docker's group, or a general sudo rule.

Provide the already qualified kernel and **stored** pinned VMM. Rebuilding the
VMM embeds its build path and changes its hash. Keep the files and every parent
directory owned by root and unwritable by the runner, group, or others; the
wrapper checks this before smoke. Verify their SHA-256 hashes against the
current release pins before enabling the module. This module does not change
kernel pins, release policy, CPU isolation, the host's perf policy or the
qualified-host hardware requirements. It loads `kvm-amd` and `msr` and provisions
regular canonical lock files for CPUs 0..31 without replacing existing inodes.

For registration, use GitHub's repository Settings → Actions → Runners → New
self-hosted runner to obtain a registration token. Gardner places it in the
configured runtime secret file, without a trailing newline, via his existing
secret management. Use a **quoted absolute string** for `tokenFile`; never use
a Nix path literal, `builtins.readFile`, an inline token, a committed secret, or
a secret copied into the Nix store. The NixOS runner service reads registration
credentials in its root configuration phase and hides the token from job
processes. Do not reuse the managed Codex GitHub credential.

A registration token expires after an hour. It is sufficient for initial
registration of the persistent pool; supply a fresh token when configuration
changes cause re-registration. For unattended future re-registration, use a
separate repository-scoped fine-grained PAT stored by the same secret manager,
with only the GitHub runner administration permission required by the NixOS
runner module. Do not put that PAT in workflow environment variables. These
runners are persistent rather than ephemeral, so a one-hour registration token
is not renewed per job. See the [NixOS runner options](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/continuous-integration/github-runner/options.nix).

Gardner runs his normal `nix-switch` for `nixos`. There will be services
`github-runner-seedbox-host-1` through `github-runner-seedbox-host-N`, each with
label `seedbox-amd`. Check their local systemd journals and GitHub's runner list.
Create the `needs-host` repository label. Merge the workflow onto the default
branch before trying the comment trigger. No additional workflow secret is
required: label consumption and reporting use short-lived job `GITHUB_TOKEN`s
only on GitHub-hosted runners.

Before treating the installation as accepted, exercise an owner label, an owner
comment, a rejected nonowner/fork request, overlapping checks on distinct pairs,
a failing fixture, a new head while smoke runs, and runner service restart.
Check that authenticated checkout can obtain a head from this private repository,
that the local commit-presence check succeeds without a second fetch, and that a
missing commit fails before the coordinator runs. Confirm `needs-host` disappears
as the hosted gate accepts the request, including when host setup fails, and that
a push only cancels: the next head requires a new label or command request.
As a negative control, add a branch workflow targeting `seedbox-amd` with an
`if: always()` step; the runner must reject it before that step or any other step
runs. Also test the wrong job name and repository/ref identities. Inspect the
effective sudo rules: only the
installed lease wrapper is allowed, and direct sudo of the stored VMM is denied.
Confirm that root smoke units are gone, canonical locks are free, MSR restoration
receipts pass, and both worktrees and evidence disappear. The portable tests
cannot establish those hardware/systemd/GitHub properties.

## Requests and scheduling

Gardner adds `needs-host` to an open same-repository PR, or comments the exact
standalone command `/host-check`. The
[workflow](../.github/workflows/host-check.yml) gates the request on a
GitHub-hosted runner before allocating a self-hosted job. Label requests must
come from `gardnervickers`; command comments must be authored by him. Fork PRs
are rejected even if Gardner requests them. PR title, branch name and comment
text are never interpolated into a shell command.

The candidate is the PR's full head SHA, not GitHub's synthetic merge commit.
The host rechecks that the PR is still open, same-repository, and on that SHA
before executing it. `actions/checkout` uses its default job token to fetch all
same-repository branch heads (`fetch-depth: 0`) from this private repository,
without retaining credentials (`persist-credentials: false`). The host uses
`git cat-file -e "$SHA^{commit}"` to require the candidate locally and fails
closed if it is absent; it performs no unauthenticated follow-up fetch.
Control scripts and the flake are checked out at the trusted workflow SHA;
`harness/host-check.sh SHA CPU` then creates a detached candidate
worktree and performs its existing build, fmt, clippy, portable and smoke steps.
Candidate `flake.nix`/`flake.lock` changes still need separate validation in the
candidate shell; this runner continues using the trusted revision's tools.

`maxParallel` controls both the number of runner services and shared capacity
locks. Changing it requires Gardner's host rebuild. Other PRs queue in Actions
when every service is busy. Within each job, `harness/host_runner.py` reserves
one slot and a free pair from 1/17 through 15/31. It checks online status,
reciprocal topology, allowed affinity, userspace threads pinned to a pair, and
both existing canonical leases.
CPU 0 and sibling 16 are excluded. Reservations cover the entire host check;
the coordinator's locks are separate from the canonical leases the CLI acquires
and releases for each guest operation. The installed smoke driver also holds
an active-pair guard, so a living smoke campaign blocks pair reuse if its
unprivileged coordinator dies. Lock files are never unlinked. External
non-runner workloads can still race for a canonical lease after selection; a
campaign refuses contention rather than retrying a guest run.

Each request authorizes one head. The GitHub-hosted gate consumes `needs-host`
before dispatching the host job, keeping the self-hosted token read-only. A label
request fails closed if its label disappeared or the head changed since the label
event. A command works without a label and consumes it if present. Host setup
failure or cancellation does not preserve an accepted request for later heads.
A push cancels stale work using one concurrency group per PR and never dispatches
a replacement, even if a label remains. Gardner must re-add the label or comment
`/host-check` for the next head; if a request was cancelled before the gate
consumed its label, remove and re-add it. Closing the PR also cancels its check.
Unrelated labels and nonowner comments cannot cancel a running check. Reporting
checks the current head and never removes labels, so a late result cannot consume
a new request or post a misleading current-head verdict.

Normal PRs use `PINNED_ENGINE` from the module's stored path and build a candidate
VMM, enabling `--allow-dev-engine` in smoke. The unflagged wrong-kernel control
still uses the stored pinned engine. PRs changing
`crates/seedbox/src/admission.rs`, including renames, conservatively select the
pinned tier: `SEEDBOX_VMM` is the stored engine, `PINNED_ENGINE` is absent and the
candidate engine build is skipped. This covers changes to ENGINE, GUEST_KERNEL
and POLICY_VERSION without attempting to parse arbitrary Rust source in CI.
A proposed new pin that disagrees with the stored artifacts fails; this runner
does not qualify or install a new release. Dev-mode success remains explicitly
unqualified evidence.

## Privilege and cleanup

All services run as the dedicated system user `seedbox-runner`, without a login
shell or privileged supplementary groups. Its writable state is under
`/var/lib/seedbox-host`, with per-service checkout and job directories and shared
coordinator locks. The standard NixOS runner's sandbox is relaxed where needed
for sudo, shared `/tmp` leases, process readback and builders; the runner keeps
its scoped writable state. It has no blanket sudo, `sudo env`, `sudo python3`,
`sudo rm`, or `sudo systemctl` permission.

The only passwordless sudo entry is the immutable installed
`seedbox-host-cpu-lease` wrapper. Direct sudo of the stored VMM is not allowed;
every privileged smoke run must go through the wrapper's CPU and path checks.
Development VMMs are executed by the privileged CLI inside smoke; no wildcard
sudo permission for runner-writable binaries is installed. The wrapper accepts
only `smoke WORK CPU INIT dev|pinned`, `clean JOB` and `reap RUNNER-JOBS`. It
validates paths under the configured job roots, excludes pair 0/16, binds the
kernel and pinned engine to root-owned stored paths, and selects the fixed CLI,
supervisor and engine paths for that job. It does not accept shell commands,
arbitrary environment assignments, arbitrary deletion paths, or a caller's
Python script.

The existing smoke driver and public CLI require root for MSR/perf admission and
lease restoration. The wrapper runs an **installed, reviewed** copy of smoke,
its ownership helper and the fixed fixture in a named transient systemd unit,
using a fixed host tool PATH. The installed adapter gives Podman/Buildah job-local
vfs storage, a transient unit-owned `/run` runroot, temporary files and
cgroupfs/chroot configuration, so build
outputs and private tags are removed with the job instead of accumulating in
root's global container store (using the documented
[Podman configuration overrides](https://docs.podman.io/en/latest/markdown/podman.1.html)
and [Buildah isolation setting](https://github.com/podman-container-tools/buildah/blob/main/docs/buildah-build.1.md)).
It does not import the candidate's Python smoke or
fixture code as root. The candidate CLI and, in dev mode, candidate VMM still
execute as root. Consequently this is a trusted-code runner, not a sandbox for
hostile PRs: those binaries could exercise root authority. The restricted sudo
command surface prevents routine accidental general sudo use; it does not make
malicious candidate code safe. Owner authorization, same-repository branches and review of privileged
code are mandatory. Every new head needs a fresh owner request. The runner can
persist changes in its own account across jobs; it cannot provide adversarial
job isolation.

Tokens with issue-write permissions exist only in the GitHub-hosted gate and
reporting jobs. The self-hosted job gets read permissions and checkout does not
retain Git credentials. Runner registration state necessarily remains available to the
runner service. The module installs an immutable `.sh` pre-job hook in the Nix
store and sets `ACTIONS_RUNNER_HOOK_JOB_STARTED` on every runner service. Before
any workflow step, it requires all of:

- `GITHUB_REPOSITORY=gardnervickers/seedbox`.
- `GITHUB_WORKFLOW_REF=gardnervickers/seedbox/.github/workflows/host-check.yml@refs/heads/main`.
- `GITHUB_JOB=host`.
- `GITHUB_EVENT_NAME=pull_request_target` or `issue_comment`.

Missing or mismatched values cause the validator to exit nonzero. The installed
hook then kills its immediate `Runner.Worker` parent, terminating this job before
any workflow steps can run. The listener survives to accept the next job. This
also blocks `if: always()` steps: a failed hook by itself leaves those steps
eligible in the [runner's condition loop](https://github.com/actions/runner/blob/main/src/Runner.Worker/StepsRunner.cs).
Only refused jobs take this path, before any coordinator or privileged smoke
starts; authorized jobs retain their ordinary cancellation and cleanup behavior.
GitHub documents the hook's placement in its [pre-job hook contract](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/run-scripts).
This restriction lives on the runner rather than in branch-editable workflows;
using the runner label in another workflow or in a branch copy of this workflow
does not authorize a job. The allowed main workflow still supplies owner and
same-repository request checks. Hook changes require Gardner's host rebuild;
workflow changes on main remain privileged infrastructure changes. This is
admission control, not isolation from previously authorized malicious code in a
persistent runner account.

The coordinator forwards catchable cancellation to the host-check process,
then invokes bounded root cleanup. Cleanup stops the job's systemd unit, waits
up to its 45-second restoration grace, and only then removes the job tree,
including root-owned smoke outputs. An `always()` workflow step repeats cleanup
and prunes Git worktree metadata. Service `ExecStopPost` also reaps that service's
jobs on restart/stop; the next job reaps leftovers before starting. Sudden power
loss or SIGKILL cannot produce a software cleanup guarantee. A forced unit kill
also cannot prove MSRs were restored: failures remain failures, and the host
must be inspected before calling it qualified again.

Only the step/phase summary `.txt`, including a bounded failure log excerpt, is
uploaded (seven-day retention) and quoted in the PR comment. The comment links
the workflow logs. All bulky state stores, worktrees and raw evidence are
removed on pass and failure. This intentionally changes the manual harness's
failure retention only when run through the coordinator. Manual invocations
still retain failed evidence for inspection. If cleanup fails, the summary says
so and the result fails; local leftovers require host operator inspection.
Cancelled jobs can have an uploaded summary but do not post a current verdict.

## Extra fixture jobs

Keep portable checks in `harness/tests/` and add their test module to
`harness/acceptance.sh`. Those run for every candidate and must not access devices
or sudo. `test_host_runner` covers allocation/capacity, lock identity, process
death, comment quoting, root cleanup path validation, runner-hook admission and
the sole sudo entry. Run `node harness/tests/test_host_workflow.js` separately to
exercise the workflow's actual JavaScript with a fake API, including one-shot
label consumption, stale heads, cancellation events and late reports. Node is
provided by Actions; it is not required by the portable project dev shell.

For additional hardware fixtures, add a finite public-CLI phase to the reviewed
smoke campaign with explicit inputs, wall budget, owned process cleanup,
canonical lease and MSR restoration readback, and a `phase` timing receipt.
Exercise negative controls before counting it as acceptance. Stage the fixture
in the module's `smokeSource` closure and have Gardner rebuild the module so the
new root driver is installed; changing a PR's root script cannot activate it.
Keep fixture operations on the job's assigned `--cpu` and under its fresh
output directory, so existing scheduling and cleanup apply. If a separate job
is necessary, route it through the same capacity/pair reservation and bounded
installed wrapper, rather than introducing an independent CPU selector or a
new sudo permission. Review its required privileged operations and extend the
installed wrapper's fixed interface only if the existing CLI cannot express it.

GitHub event behavior and concurrency are described in the
[official event reference](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows)
and [workflow syntax reference](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax).
