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
2. **Check it before host time.** On the train head: `cargo fmt -p seedbox --check`,
   strict clippy, `cargo test --workspace --locked` and
   `bash harness/acceptance.sh portable` (which runs `cargo test-vmm`).
3. **Run one host job.** `harness/host-check.sh` on the train head, plus the
   union of each PR's own host checks as its reviewer listed them, written as
   one script that runs independent checks at once on separate SMT pairs
   (never CPU 0 or its sibling). Every run uses `--allow-dev-engine` until
   the train's engine is pinned. Judge each PR's evidence on same-engine
   equality: world fixtures without a guest supervisor must equal main's
   witnesses exactly, and Compose runs must PASS and replay MATCH.
4. **On a pass.** Post the train evidence on each PR, linking the train.
   Each reviewer then merges its PRs in train order: merge main into the PR
   the same way, check that its tree equals the train's prefix for that PR,
   run the portable checks, and squash-merge. No further host run.
5. **On a failure.** Find the owner of the failed check; if that's unclear,
   bisect the train by halves. Drop the culprit, rebuild the train from the
   rest, and rerun. The culprit goes back to its author with the evidence.
6. **Clean up.** Delete the train's worktrees and run directories on the
   host once the evidence is posted, and delete the train branch once its
   PRs have merged or left the train.
