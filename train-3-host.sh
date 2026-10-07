#!/usr/bin/env bash
# Host checks for train/3: main f160994 + #101 + #97 + #106 + #109 + #104 + #110.
# Each PR's list is its reviewer's, change-scoped (docs/merge-train.md).
# Portable checks passed at this SHA on Linux (harness/mac-check.sh), so
# host-check runs with --skip-portable.
# Run as gvickers from the main seedbox checkout; the script re-enters the train
# worktree's own `nix develop`. Needs cores 2-7 (siblings 18-23) free, network
# for #104's locked MinIO inputs, and >=15 GiB free for run-links.sh.
# Usage: train-3-host.sh TRAIN_SHA MAIN_SHA OUT     (OUT must not exist)
# Env: A (accepted kernel e8ece7cb), ARTS (dir with multi.cpio, net.cpio, disk.cpio),
#      PINNED (stored 52eb engine, for host-check's wrong-kernel control).
set -uo pipefail
TRAIN=$1 MAIN=$2 OUT=$3
: "${A:?} ${ARTS:?} ${PINNED:?}"
REPO=${REPO:-$PWD} T=$OUT/train
LINKS_SHA=14aab0c3a0f176588eb3ae55bab1de5dfa32dbfa
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin claude/train-3 exp/60-link-controls
  git -C "$REPO" worktree prune
  git -C "$REPO" worktree add --detach -q "$T" "$TRAIN"
  cp "$0" "$OUT/train-host.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$T" --command bash "$OUT/train-host.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
note "train $(git -C "$T" rev-parse HEAD) main $MAIN $(date -u +%FT%TZ)"
(cd "$T" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build.log" 2>&1 \
  || { note "BUILD FAIL"; exit 1; }
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm TC=$T/target/release/seedbox
note "train engine $(sha256sum "$TE" | cut -c1-12)"
mkdir -p "$T/artifacts"; cp "$ARTS"/*.cpio "$T/artifacts/"; ln -sfn "$A" "$T/artifacts/bzImage"

sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
compose() {  # NAME CPU STATE PROJECT [run flags]
  local name=$1 cpu=$2 state=$3 project=$4; shift 4
  local t0=$SECONDS
  sb "$TC" run "$project" --kernel "$A" --vmm "$TE" --cpu "$cpu" --state "$state" --allow-dev-engine "$@" \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(head -n 1 "$OUT/$name.out") $((SECONDS - t0))s"
}
replay() {  # NAME FROM CPU STATE [replay flags]
  local name=$1 from=$2 cpu=$3 state=$4; shift 4
  local t0=$SECONDS
  sb "$TC" replay "$(attempt "$OUT/$from.out")" --cpu "$cpu" --state "$state" --allow-dev-engine "$@" \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(grep -h '^replay:' "$OUT/$name.out") $((SECONDS - t0))s"
}

lane_b1() {  # cpu 2: feature-off witness equality for #101/#97/#106/#109 (want 4b0b13cc)
  (cd "$T" && SEEDBOX_ROOT="$T" VMM="$TE" CPU0=2 SEEDBOX_TIMEOUT=600 \
    harness/batch.sh harness/worlds/cluster-fixed.json b1-cluster-fixed 1009 --digest) > "$OUT/world-b1.log" 2>&1
  note "world b1: exit $? $(sudo jq -c 'with_entries(select(.key|test("witness";"i")))' "$T"/runs/b1-cluster-fixed-s1009-r0.json 2>/dev/null)"
}
lane_links() {  # cpu 3: #97's link controls on the train engine (L1==L2, U1==U2, P1==M1, ALL PASS)
  local w=$OUT/links; git -C "$REPO" worktree add --detach -q "$w" "$LINKS_SHA"
  (cd "$w/harness/experiments/link-60" && CPU=3 SHA=$LINKS_SHA ENGINE=$TRAIN FETCH=claude/train-3 MAIN=$MAIN \
    bash run-links.sh) > "$OUT/links.log" 2>&1
  note "links: exit $? $(grep -h 'ALL PASS\|FAIL' "$OUT/links.log" | tail -n 3 | tr '\n' ' ')"
  git -C "$REPO" worktree remove --force "$w"
}
lane_connections() {  # cpu 4: #106
  local s=$OUT/conn-state
  compose c61 4 "$s" "$T/harness/acceptance/workloads/connections.yaml" --seed 61 --full-state   # want PASS
  replay c61-replay c61 4 "$s"                                                                      # want MATCH
  replay c61-undo0 c61 4 "$s" --undo 0                                                              # want MATCH, record time
}
lane_triggers() {  # cpu 5: #109
  local s=$OUT/trig-state bin
  bin=$(cd "$T" && cargo test-vmm --no-run --message-format=json 2>/dev/null \
    | jq -r 'select(.executable != null and (.target.kind | index("bin"))) | .executable' | tail -n 1)
  (cd "$T" && sudo -E env "PATH=$PATH" python3 harness/test-triggers.py 5 "$bin") > "$OUT/test-triggers.log" 2>&1
  note "test-triggers: exit $? $(tail -n 2 "$OUT/test-triggers.log" | tr '\n' ' ')"
  compose trig 5 "$s" "$T/harness/acceptance/workloads/triggers.yaml" --full-state                 # want PASS
  replay trig-replay trig 5 "$s"                                                                    # want MATCH
  replay trig-undo0 trig 5 "$s" --undo 0                                                            # want MATCH
}
lane_minio() {  # cpu 6: #104 at 488bfd6; the qualification run's first request proves MinIO listens
  local s=$OUT/minio-state in=$OUT/minio-inputs p=$OUT/minio-projects e=$T/examples/minio
  python3 "$e/fetch_inputs.py" "$in" > "$OUT/minio-fetch.log" 2>&1 || { note "minio: input fetch FAILED"; return; }
  python3 "$e/generate.py" "$p" --minio "$in/minio" --wheel-directory "$in/wheels" > "$OUT/minio-generate.log" 2>&1 \
    || { note "minio: generate FAILED"; return; }
  local r=(--seed 0 --memory-mb 2048)
  compose m-qual 6 "$s" "$p/qualification" "${r[@]}"                                               # want 0
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-qual.out")" --mode qualification 2>&1 | tail -n 1)"
  compose m-broken 6 "$s" "$p/broken" "${r[@]}"                                                     # want 1
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-broken.out")" --mode broken 2>&1 | tail -n 1)"
  replay m-replay m-broken 6 "$s"                                                                   # want MATCH
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-replay.out")" --mode broken --replay 2>&1 | tail -n 1)"
  compose m-fixed 6 "$s" "$p/fixed" "${r[@]}"                                                       # want 0
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-fixed.out")" --mode fixed 2>&1 | tail -n 1)"
}

lane_links & LINKS=$!
lane_b1 & P1=$!; lane_connections & P2=$!; lane_triggers & P3=$!; lane_minio & P4=$!
wait $P1 $P2 $P3 $P4
# Core + #110: host-check smoke on cpu 7 once the short lanes are done (links runs
# on alone on cpu 3), so its phase times compare with #99's receipt
# (fixed 13.7 s, default 9.5 s, undo-margin 84.1 s).
(cd "$REPO" && SEEDBOX_KERNEL=$A PINNED_ENGINE=$PINNED HOST_CHECK_DIR=$OUT bash "$T/harness/host-check.sh" --skip-portable "$TRAIN" 7) > "$OUT/host-check.log" 2>&1
note "host-check: exit $? $(tail -n 1 "$OUT/host-check.log")"
hc=$(sed -n 's/^[A-Z]*: \([^;]*\).*/\1/p' "$OUT/host-check.log" | tail -n 1); [[ -f $hc ]] && tee -a "$SUM" < "$hc"
wait $LINKS

note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in 2 3 4 5 6 7; do f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "leases 2-7 checked; MSRs are checked by each run's lease readback"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"
echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $T; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
