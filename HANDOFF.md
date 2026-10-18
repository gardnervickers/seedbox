# DynamoRIO fork and base-CI handoff

Status: 2026-10-18. Use OpenAI models only until further notice.

## Scope and current stop

This branch, `handoff/dr-fork-and-base-ci`, starts at Seedbox
`a6500b20c4bb00b24efb005757d68d42c91f6d5e` on `driver-service`
([PR #320](https://github.com/gardnervickers/seedbox/pull/320)). Its only new
file is this handoff; no DynamoRIO source is consolidated into this branch.
PR #320 initially stood at `66bb1720b0c37191d0795b450911e092a341bcdb`.
[PR #317](https://github.com/gardnervickers/seedbox/pull/317) supplies source
inventory, not correctness acceptance or the integration architecture.

The owner created public <https://github.com/gardnervickers/dynamorio>;
`gardner-codex` WRITE access was confirmed. Publication of independently reviewed
work and standard free native x86/ARM CI is authorized in principle, but is
**PAUSED by the owner pending Pocket's cross-project review-routing fix**.
The prepared DR changes have not been published or dispatched. Do not resume
publication merely because repository access works.

The sole hard publication stop is review routing: a Seedbox-rooted coordinator
cannot request the recorded independent review of the actual DR candidate in
its owning repository. Supplying its DR worktree does not change project lookup.
Restore explicit project selection while retaining repository validation,
permissions, exact candidate identity and review evidence in the owning project.
Do not copy Git objects, fabricate commits, vendor source, or introduce a
submodule merely to bypass this tooling limitation.

## Seedbox base: what landed and what remains unresolved

Changes already pushed to `driver-service` since its initial head:

- Network fixture handles denied loopback configuration in restricted CI.
- KVM PMU admission accepts absent Intel architectural PMU only with sufficient
  evidence; AMD leaf A zero alone is not sufficient. The final fix
  `fec4493bab90db11384715e93dc9fe90b4f18c2d` also accepts exact global
  `enable_pmu=N` evidence for every vendor, retains explicit per-VM disable
  when supported, and adds regression cases and hosted CPU/PMU diagnostics.
- etcd failure collection parses the CLI's summary array, not an API page.
- `a6500b20c4bb00b24efb005757d68d42c91f6d5e` changes only README.md to describe
  the current workflow; its independent review passed.

The PMU fix's independent code review found no defects and full portable checks
passed; focused native checks passed. Hosted proof was originally outstanding:

- [Run 39136089988](https://github.com/gardnervickers/seedbox/actions/runs/39136089988)
  is **overall cancelled**, not an overall pass. Its
  [test job 117700814506](https://github.com/gardnervickers/seedbox/actions/runs/39136089988/job/117700814506)
  **succeeded**: application checks and all four etcd checks passed on AMD EPYC
  7763, KVM `enable_pmu=N`, PMU capability 0.
- [Successor run 39141183905](https://github.com/gardnervickers/seedbox/actions/runs/39141183905)
  failed. Portable/application checks and TCG etcd passed. PV etcd completed with
  oracle exit 0, but fresh baseline/repeat comparison reported
  `witnessMatches=false` and `fullStateMatches=false`; outcomes, fault history,
  simulated time and oracle bytes matched. This run used AMD EPYC 9V74 with
  the same reported PMU settings. CPU model is an investigation variable,
  **not an established cause**. The two runs tested distinct PR merge commits;
  their source heads differ only by README, but their complete prepared inputs
  are not an identical-input control.

Witnesses incorporate full-state digests, so these two false comparison fields
are not independent proof of two faults. Existing retained artifacts contain no
per-machine state-part/page hashes or raw pages that localize the difference.
Capture limits and cleanup lost that diagnostic detail; this does not imply
internal state evidence was absent. [Issue #304](https://github.com/gardnervickers/seedbox/issues/304)
shares the observable signature, not a proven common cause.

**Pending owner approval, not implemented:** add failure-only, bounded capture
before server cleanup: both internal reports and stopped-state manifests,
machine/part digests, page hashes for differing machines, and a capped set of
raw differing pages where obtainable. Reserve artifact slots and explicit
file/total-size limits. Preserve seed, epoch, resolved inputs and CPU/PMU evidence.
Then run **one targeted diagnostic PV baseline/repeat pair**, not retries until
green. Keep oracle, witness and full-state assertions unchanged; localize the
first differing state part before proposing an engine fix. A passing attempt
alone does not resolve the recorded mismatch.

## DynamoRIO recovery locations and exact candidates

These commits exist in the **separate local DR repository**, not in this Seedbox
branch. Preserve the worktrees and their local commits until publication is
safely completed.

Original repository: `/home/gvickers/src/github.com/lbradstreet/dynamorio`.
Execution-reference fork snapshot:
`c28cfef9ffc46c43c711990ffd2fabb677ef1b49`.

| Candidate | Recovery location | Contents and evidence |
| --- | --- | --- |
| `0f38ad32298d6b9a9977164555fbaacae1e627df` | `/home/gvickers/.pi-pocket/worktrees/slots/slot-028f86a906e6fa777da9e5a7`, branch `pocket/agent-f7c39fbffe312665f74ea991` | Two-line `AARCHXX` guard around `app_sigreturn_target` in `core/unix/signal.c`. Preserves ARM32 and ARM64; excludes unused helper elsewhere without suppressing warnings. Parent is the fork snapshot above. |
| `068aa472c2466beaa3053c45bd61d4d62c4a7cb7` | `/home/gvickers/.pi-pocket/worktrees/slots/slot-374f74be5d7bca3da77bbf20`, branch `pocket/agent-1cd990d1679d67e38a9c4052` | Includes the guard as parent, plus `.github/workflows/ci-native-baseline.yml` and three files under `suite/ci/`: workflow README, selector/accounting helper and tests. Six helper tests, actionlint and diff checks passed. No real ARM build/run or all-target candidate build is claimed. |

Guard validation: core and `drrun` built, echo smoke passed, **13/14 focused
CTests passed**; `api.detach_signal` failed and remains required. The earlier
`BUILD_EXT=OFF` all-target configuration failed on missing `drmgr.h`.
Neither result is blanket baseline success. ARM32/ARM64 behavioral validation,
debug builds and the full extension/client regression suite remain unproven.

Read the candidate's `suite/ci/README-native-baseline.md` before resuming. Its
publication section predates the now-created public destination and verified
WRITE access; use the current owner pause above. Local evidence:
`/tmp/dr-baseline/report.md`, `/tmp/dr-baseline/commit-manifest.tsv`,
`/tmp/dr-baseline-fix/results.md`, and `/tmp/dr-native-ci/`.
These are host-local recovery files, not portable retained artifacts.

## Source choice and native CI still to finish

The observed upstream snapshot was
`17dc7500d92815234b1a6a0c535a3d23ea210e59`; the established common base is
`a842f0eb3463f2c1846d082ecac0b4b2d5f894c9`. The fork has 218 nonmerge commit
identities after that base, **not 218 unique or accepted fixes**. Patch equivalence,
reverts and supersessions need semantic review. The fork HEAD also includes
three post-pin commits plus a merge: keep execveat production/consumer changes
and IcNull oracle improvements distinct. The inventory pin was
`356c1821045a4b5678d0813e132074324623df6e`. Finish the maintained source ancestry
and branch choice explicitly; testing fork HEAD does not approve its full stack.

The prepared workflow is manual-only and checks out the immutable dispatch SHA.
It uses standard public-repository `ubuntu-24.04` and `ubuntu-24.04-arm` runners
(native x86_64/aarch64; documented 4 CPU, 16 GB RAM, 14 GB SSD), not paid larger
runners. It enables extensions/clients/tests, requires smoke execution and exact
CTest accounting, preserves failures including `api.detach_signal`, and does
not retry failed tests. Static helper success is not native runner proof.

Recursive dependency acquisition via Robo transport failed before obtaining
inputs because non-Gardner repository owners are blocked. Resolve the authorized
transport/provisioning path for recorded elfutils/libipt/zlib gitlinks; do not
fall back to human credentials or change managed authentication. Before any
publication, review inherited push-to-master and custom ARM workflows/triggers
with the owner so publication cannot start unintended jobs. Confirm Actions
policy permits the pinned actions and standard ARM runner; a prior policy API
403 left policy unknown.

## Resume order

1. Wait for the Pocket review fix and owner release of the pause; restore
   independent review of the exact DR guard/workflow candidate in its repository.
2. Resolve source ancestry, dependency acquisition and inherited-trigger safety.
   Then publish only the reviewed safe branch and manually dispatch the authorized
   standard native workflow. Record source/dependency SHAs and run URLs.
3. Obtain real native x86 and ARM results. Diagnose baseline test/configuration
   failures without hiding them, suppressing warnings, weakening required tests,
   or treating skips/unavailable hardware as success.
4. Curate DR fix families one invariant and regression at a time, retaining
   supersession/revert semantics and both-architecture proof for shared changes.
   No blanket DR import is approved.
5. Integrate accepted immutable DR inputs into the **current Driver** later,
   preserving executable-FD ownership, exclusive reservation epochs and client
   contracts. Native DR tests are not Seedbox KVM/HVF or Driver qualification.
   Keep time policy, lean-client work and unrelated PR features separate.

The separate Seedbox diagnostic-capture proposal also awaits explicit owner
approval. This handoff authorizes no diagnostic code, rerun, push, merge,
publication, dispatch, paid runner or deployment. No tests/builds were run for
this documentation-only commit, and no push was performed.
