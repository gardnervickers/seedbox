#!/usr/bin/env bash
# Host checks for train/2: main 4ea970a + #101 (own merge) + #104 + #110, plus
# #100's on-path torn check. Each PR's list is its reviewer's, change-scoped
# (docs/merge-train.md); default replays only.
# Run as gvickers from the main seedbox checkout; the script re-enters the train
# worktree's own `nix develop`. Needs cores 2-9 (siblings 18-25) free and
# network for #104's locked MinIO input fetch. CPU 0/16 are never used.
# Usage: train-2-host.sh TRAIN_SHA PRE_SHA OUT   (PRE = 4ea970a, main before #101; OUT must not exist)
# Env: A (accepted kernel e8ece7cb), K (disk kernel 8f4f1666),
#      ARTS (dir holding multi.cpio, net.cpio, disk.cpio for world fixtures),
#      PINNED (stored 52eb engine, for host-check's wrong-kernel control).
set -uo pipefail
TRAIN=$1 MAIN=$2 OUT=$3
: "${A:?} ${K:?} ${ARTS:?} ${PINNED:?}"
REPO=${REPO:-$PWD} T=$OUT/train M=$OUT/main
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" worktree prune
  git -C "$REPO" worktree add --detach -q "$T" "$TRAIN"
  git -C "$REPO" worktree add --detach -q "$M" "$MAIN"
  cp "$0" "$OUT/train-host.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$T" --command bash "$OUT/train-host.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
note "train $(git -C "$T" rev-parse --short HEAD) pre-#101 $(git -C "$M" rev-parse --short HEAD) $(date -u +%FT%TZ)"
for w in "$T" "$M"; do
  (cd "$w" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build-$(basename "$w").log" 2>&1 \
    || { note "BUILD FAIL $w"; exit 1; }
done
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm ME=$M/crates/seedbox-vmm/target/release/seedbox-vmm
TC=$T/target/release/seedbox MC=$M/target/release/seedbox
note "train engine $(sha256sum "$TE" | cut -c1-12), pre engine $(sha256sum "$ME" | cut -c1-12)"
mkdir -p "$T/artifacts"
cp "$ARTS"/*.cpio "$T/artifacts/"

world() {  # ENGINE CPU0 WORLD_JSON PREFIX SEEDS [extra]
  local engine=$1 cpu=$2 w=$3 prefix=$4 seeds=$5; shift 5
  (cd "$T" && SEEDBOX_ROOT="$T" VMM="$engine" CPU0=$cpu SEEDBOX_TIMEOUT=600 \
    harness/batch.sh "$w" "$prefix" "$seeds" --digest "$@") > "$OUT/world-$prefix.log" 2>&1
  local rc=$? r
  note "world $prefix: exit $rc"
  for r in "$T"/runs/"$prefix"-s*.json; do
    note "  $(basename "$r" .json) $(sudo jq -c 'with_entries(select(.key|test("witness|disk|ledger|digest|status|model";"i")))' "$r" 2>/dev/null)"
  done
}
sb() { sudo -E env "PATH=$PATH" "$@"; }
vmm_of() { echo "${1%/target/release/seedbox}/crates/seedbox-vmm/target/release/seedbox-vmm"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
compose() {  # NAME CLI KERNEL CPU STATE PROJECT [run flags]
  local name=$1 cli=$2 kernel=$3 cpu=$4 state=$5 project=$6; shift 6
  sb "$cli" run "$project" --kernel "$kernel" --vmm "$(vmm_of "$cli")" --cpu "$cpu" --state "$state" --allow-dev-engine "$@" \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(head -n 1 "$OUT/$name.out")"
}
witness() { sb "$TC" inspect "$(attempt "$OUT/$1.out")" --json | jq -r .report.witness; }

lane_worlds() {  # cpus 2-4: B1 core, then #100's on-path torn check on the raw disk-crash world
  ln -sfn "$A" "$T/artifacts/bzImage"
  world "$TE" 2 harness/worlds/cluster-fixed.json b1-cluster-fixed "1009"      # want 4b0b13cc
  ln -sfn "$K" "$T/artifacts/bzImage"
  jq '. + {disk_loss: "flush-stable/seeded-latest-sector-512-half/disk-loss-machine-incarnation/v1"}' \
    "$T/harness/worlds/disk-crash.json" > "$T/harness/worlds/disk-crash-torn.json"
  world "$TE" 2 harness/worlds/disk-crash-torn.json t-disk-crash-torn "1009 1009 1010"
  # want: the two 1009 runs equal; surviving sectors of the 16 unflushed vary by seed
}
lane_minio() {  # cpu 6: #104, four runs on the train engine with A
  local s=$OUT/minio-state in=$OUT/minio-inputs p=$OUT/minio-projects e=$T/examples/minio
  python3 "$e/fetch_inputs.py" "$in" > "$OUT/minio-fetch.log" 2>&1 || { note "minio: input fetch FAILED"; return; }
  python3 "$e/generate.py" "$p" --minio "$in/minio" --wheel-directory "$in/wheels" > "$OUT/minio-generate.log" 2>&1 \
    || { note "minio: generate FAILED"; return; }
  local r=(--seed 0 --memory-mb 2048)
  compose m-qual "$TC" "$A" 6 "$s" "$p/qualification" "${r[@]}"                       # want 0
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-qual.out")" --mode qualification 2>&1 | tail -n 1)"
  compose m-broken "$TC" "$A" 6 "$s" "$p/broken" "${r[@]}"                             # want 1
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-broken.out")" --mode broken 2>&1 | tail -n 1)"
  sb "$TC" replay "$(attempt "$OUT/m-broken.out")" --cpu 6 --state "$s" --allow-dev-engine > "$OUT/m-replay.out" 2> "$OUT/m-replay.err"
  note "m-replay: exit $? $(grep -h '^replay:' "$OUT/m-replay.out")"                  # want 0, MATCH
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-replay.out")" --mode broken --replay 2>&1 | tail -n 1)"
  compose m-fixed "$TC" "$A" 6 "$s" "$p/fixed" "${r[@]}"                               # want 0
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-fixed.out")" --mode fixed 2>&1 | tail -n 1)"
}
lane_pause() {  # cpu 9: #101 pause invariance and wall expiry while paused
  local s=$OUT/pause-state p=$OUT/pause-project
  python3 "$M/examples/faults/generate.py" "$p" > /dev/null
  compose p-base "$TC" "$A" 9 "$s" "$p/fixed" --full-state
  (cd "$T" && sudo -E env "PATH=$PATH" SEEDBOX_HOST_PAUSE_RUN="$(attempt "$OUT/p-base.out")" SEEDBOX_HOST_PAUSE_STATE="$s" \
    cargo test --release --locked -p seedbox --test host_pause -- --ignored) > "$OUT/host-pause-test.log" 2>&1
  note "host_pause test: exit $? $(grep -h '^test result' "$OUT/host-pause-test.log")"
  local wall=$OUT/wall-project; python3 "$M/examples/faults/generate.py" "$wall" > /dev/null
  sed -i 's/wall_time: [^}]*}/wall_time: 10s}/' "$wall/fixed/compose.yaml"
  sb "$TC" run "$wall/fixed" --kernel "$A" --vmm "$TE" --cpu 9 --state "$s" --allow-dev-engine > "$OUT/p-wall.out" 2> "$OUT/p-wall.err" &
  local pid=$! loc=
  for _ in $(seq 60); do loc=$(sudo find "$s/runs" -path '*/output/host-control' -newer "$OUT/p-base.out" 2>/dev/null | head -n 1); [[ -n $loc ]] && break; sleep 0.5; done
  [[ -n $loc ]] && { sleep 2; sb "$TC" pause "${loc%/output/host-control}" >> "$OUT/p-wall.ctl" 2>&1; }
  wait $pid; local rc=$?
  note "p-wall: exit $rc (want 3) $(head -n 1 "$OUT/p-wall.out") vmm on cpu 9: $(pgrep -af seedbox-vmm | grep -c -- '--cpu 9' || true) host-control left: $([[ -n $loc && -e $loc ]] && echo yes || echo no)"
}

lane_worlds & lane_minio & lane_pause & wait

# Alone on the host from here. #101 overhead: pre (4ea970a) vs post (train), 3 each,
# alternating, same faults project (generated from pre, so only #101 differs).
# The train head's CLI and engine equal #101's merge commit: #104 and #110 change only examples and harness.
p=$OUT/overhead-project; python3 "$M/examples/faults/generate.py" "$p" > /dev/null
for i in 1 2 3; do for side in pre post; do
  cli=$MC; [[ $side == post ]] && cli=$TC
  compose "o-$side-$i" "$cli" "$A" 9 "$OUT/o-state-$side" "$p/fixed"
  note "o-$side-$i host_seconds $(sudo jq .host_seconds "$(attempt "$OUT/o-$side-$i.out")/output/vmm.json") witness $(witness "o-$side-$i")"
done; done
# #110 and the train core: host-check.sh (build, fmt, clippy, suite, smoke), alone so the
# smoke phase times compare with #99's receipt (fixed 13.7 s, default 9.5 s, undo-margin 84.1 s).
(cd "$REPO" && SEEDBOX_KERNEL=$A PINNED_ENGINE=$PINNED HOST_CHECK_DIR=$OUT bash "$T/harness/host-check.sh" "$TRAIN" 7) > "$OUT/host-check.log" 2>&1
note "host-check: exit $? $(tail -n 1 "$OUT/host-check.log")"
hc=$(sed -n 's/^[A-Z]*: \([^;]*\).*/\1/p' "$OUT/host-check.log" | tail -n 1); [[ -f $hc ]] && tee -a "$SUM" < "$hc"

note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in $(seq 2 9); do f=/tmp/seedbox-msr-cpu-$c.lock
  [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "leases 2-9 checked; MSRs are checked by each run's lease readback"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"
echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $T; git -C $REPO worktree remove --force $M; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
