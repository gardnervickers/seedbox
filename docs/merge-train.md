# Merge trains

The AMD host is the bottleneck for engine PRs. Instead of one host run per PR,
PRs whose review is clean and whose only missing piece is host evidence ride a
train: one branch, one host job, then ordinary merges with no further host time.

1. **Build the train.** `claude/train-N` is current main plus each ready PR,
   merged in the order the reviewers' merge notes give (for example, a stacked
   PR's old base head before main). Resolve conflicts the way the PR's own
   main merge will, so main plus the PRs in order equals the train tree. A fix
   the train needs (a stale test, a harness flag) is a separate "Train fix"
   commit; it must also land in the PR it belongs to before that PR merges.
   Each PR enters as its own merge commit, so the train just before and just
   after any PR exists as a commit; a check that compares those prefixes
   builds an engine at each one.
2. **Check it before host time.** On the train head: `cargo fmt -p seedbox --check`,
   strict clippy, `cargo test --workspace --locked` and
   `bash harness/acceptance.sh portable` (which runs `cargo test-vmm`).
3. **Run one host job.** `harness/host-check.sh` on the train head, plus the
   union of each PR's own host checks as its reviewer listed them, written as
   one script that runs independent checks at once on separate SMT pairs
   (never CPU 0 or its sibling). Every run uses `--allow-dev-engine` until
   the train's engine is pinned. Each PR's host list covers only what its
   diff can affect, plus one witness-equality core, with no unrelated flag
   combinations (no undo or margin variants for a PR that doesn't touch
   stepping). Each PR's evidence is its reviewer's list. A PR that claims to
   be witness-neutral must show witnesses equal to the train before it. A PR
   that changes witnesses on purpose has its reviewer name the fixtures that
   move, and every other fixture must still be equal. Compose runs must PASS
   and replay MATCH. Two checks belong on every list, because reviews have
   shown these are where PRs fail on the host:
   - **The host fact behind the PR's claims.** A PR that assumes something
     about the environment (a kernel option, a tool inside the guest image,
     a hash that has to match across hosts) runs a check that exercises that
     assumption on the host. Unit tests on another machine can't show it.
   - **Feature-off witness equality.** An engine change gated behind a flag
     or config shows that, with its feature off, the world fixtures it could
     touch give the same witnesses as the train before it.

   **Lanes.** Concurrent replays scale nearly linearly on the host: 14 at once
   took 14.1 s against 10.1 s for one (H1: about 10x throughput, 39% slower
   each, about 900 MB of host RAM per run). So a train runs every check as
   its own lane on its own core, and splits one PR's list across cores when
   its runs are independent (a run and its replays stay together; separate
   worlds don't). Trains use cores 2-13, one lane per core with its SMT
   sibling leased. Cores 14-15 are for side jobs and builds outside a train.
   Builds that finish before any lane starts may use the idle lane cores;
   builds that overlap running lanes use core 1. Lane cores are a
   parameter of the train script, so a train can start beside a straggler on
   whatever cores are free. Any undo-off or margin check uses a short world
   and a timeout of 600 s or less.

   Timings compare only like with like: two arms of a timing comparison (a
   PR's run against the train before it) run at the same time on the same
   load, or both alone. Never compare a lane's seconds with an earlier
   receipt taken at a different lane count.

   A run on a dev-unpinned engine replays only on that engine, so a check of
   "the new engine reproduces the old run" is instead a fresh run on the new
   engine with its world witness compared against the old run's, plus replays
   of the new run on its own engine.
4. **On a pass.** Post the train evidence on each PR, linking the train.
   Each reviewer then merges its PRs in train order: merge main into the PR
   the same way, check that its tree equals the train's prefix for that PR,
   run the portable checks, and squash-merge. No further host run.
   If main gains commits while the train runs that don't touch
   `crates/seedbox-vmm/`, `kernel/` or `guest/`, the evidence still stands:
   merge the new main into each PR as usual, run the portable checks, and
   compare the tree against the train prefix plus those commits. If main
   gains an engine, kernel or guest change, rebuild the train and rerun the
   host job.
5. **On a failure.** Find the owner of the failed check; if that's unclear,
   bisect the train by halves. Drop the culprit, rebuild the train from the
   rest, and rerun. The culprit goes back to its author with the evidence.
   PRs stacked on the culprit leave with it. A PR that doesn't contain the
   culprit and doesn't touch `crates/seedbox-vmm/`, `kernel/` or `guest/`
   keeps its evidence only if its checks ran on main's engine (no engine PR
   ahead of it in the train), or the train showed feature-off witness
   equality for the worlds its checks ran. Otherwise rerun just its scoped
   checks (`host-check.sh --plan`) on main plus the PR. Its engine equals
   main's, so `cluster-fixed` is skipped and this is usually one core tier
   on a side core. For example, #97 deliberately changes worlds with network
   jitter, so when it left train/3 the MinIO runs of #104 and the faults
   smoke tier of #110 reran on main. If the culprit's fix changes only its own
   fixtures or checker, the engine is unchanged: the rest of the train's
   evidence for it and the PRs stacked on it stands, and only the failed
   check reruns.
6. **Clean up.** Delete the train's worktrees and run directories on the
   host once the evidence is posted, and delete the train branch once its
   PRs have merged or left the train.
