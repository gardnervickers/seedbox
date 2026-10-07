#!/usr/bin/env bash
# Side checks after train/3 and train/4 (one job, two lanes on separate cores):
#  minio  #104 on main: train/3's MinIO evidence ran on #97's engine, and #97
#         changes jitter>0 worlds (MinIO uses 200us jitter), so it reruns on
#         main + #104 (claude/check-104). Four runs + verify, replay MATCH.
#  cancel #105: (a) engine and (b) retained keys of train/4's cancelled run, then
#         a rerun that sends SIGINT only once a guest console has bytes.
# Usage: side-checks.sh CHECK104_SHA TRAIN4_DIR OUT    (OUT must not exist)
# Env: A (accepted kernel), LANE_CPUS="MINIO CANCEL" (default "14 15"; never 0/16).
set -uo pipefail
C104=$1 T4=$2 OUT=$3; : "${A:?}"
REPO=${REPO:-$PWD} W=$OUT/c104
read -r C_MINIO C_CANCEL <<< "${LANE_CPUS:-14 15}"
for c in $C_MINIO $C_CANCEL; do
  [[ $c != 0 && $c != 16 ]] || { echo "never CPU 0 or 16"; exit 2; }
  f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && { echo "cpu $c lease is held; set LANE_CPUS"; exit 2; }
done
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  test -f "$T4/cancel.out" || { echo "$T4 is not the kept train-4 dir"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin claude/check-104
  git -C "$REPO" worktree add --detach -q "$W" "$C104"
  cp "$0" "$OUT/side.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$W" --command bash "$OUT/side.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
note "check-104 $(git -C "$W" rev-parse HEAD) $(date -u +%FT%TZ)"

lane_cancel() {
  local TC=$T4/train/target/release/seedbox TE=$T4/train/crates/seedbox-vmm/target/release/seedbox-vmm
  local old; old=$(attempt "$T4/cancel.out")
  note "cancel (a) recorded engine: $(sb "$TC" inspect "$old" --json 2>/dev/null | jq -c '.identity.inputs.profile.engine' | cut -c1-120) train-4 engine $(sha256sum "$TE" | cut -c1-12)"
  note "cancel (b) files: $(sudo find "$old" -path '*machines*' -printf '%P:%s ' 2>/dev/null | head -c 600)"
  note "cancel (b) record logs keys: $(sb "$TC" inspect "$old" --json 2>/dev/null | jq -c '.logs | keys' | head -c 400)"
  local s=$OUT/cancel-state pid hit="" t0=$SECONDS
  sb "$TC" run "$T4/train/harness/acceptance/workloads/kill-3.5.3.yaml" --seed 4000 --kernel "$A" --vmm "$TE" --cpu "$C_CANCEL" \
    --state "$s" --allow-dev-engine > "$OUT/cancel.out" 2> "$OUT/cancel.err" & pid=$!
  while kill -0 $pid 2>/dev/null && (( SECONDS - t0 < 600 )); do
    hit=$(sudo find "$s" -name '*.console' -size +0c 2>/dev/null | head -n 1); [[ -n $hit ]] && break; sleep 1
  done
  [[ -n $hit ]] && { sleep 3; for p in $(pgrep -x seedbox); do grep -q kill-3.5.3 /proc/$p/cmdline 2>/dev/null && sudo kill -INT "$p"; done; }
  wait $pid; note "cancel rerun: exit $? $(head -n 1 "$OUT/cancel.out") console seen after $((SECONDS - t0))s: ${hit:-none}"   # want INCOMPLETE, exit 3
  sb "$TC" logs "$(attempt "$OUT/cancel.out")" --json > "$OUT/cancel.logs.json" 2> "$OUT/cancel.logs.err"
  note "  cancel logs: exit $? chunks $(jq '.chunks | length' "$OUT/cancel.logs.json" 2>/dev/null) omissions $(jq -c .omissions "$OUT/cancel.logs.json" 2>/dev/null | head -c 400)"
}
lane_minio() {
  (cd "$W" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build.log" 2>&1 || { note "minio: BUILD FAIL"; return; }
  local TC=$W/target/release/seedbox TE=$W/crates/seedbox-vmm/target/release/seedbox-vmm
  local s=$OUT/minio-state in=$OUT/minio-inputs p=$OUT/minio-projects e=$W/examples/minio r=(--seed 0 --memory-mb 2048)
  note "minio engine $(sha256sum "$TE" | cut -c1-12)"
  python3 "$e/fetch_inputs.py" "$in" > "$OUT/minio-fetch.log" 2>&1 || { note "minio: input fetch FAILED"; return; }
  python3 "$e/generate.py" "$p" --minio "$in/minio" --wheel-directory "$in/wheels" > "$OUT/minio-generate.log" 2>&1 || { note "minio: generate FAILED"; return; }
  run() { local name=$1 proj=$2 t0=$SECONDS
    sb "$TC" run "$p/$proj" --kernel "$A" --vmm "$TE" --cpu "$C_MINIO" --state "$s" --allow-dev-engine "${r[@]}" > "$OUT/$name.out" 2> "$OUT/$name.err"
    note "$name: exit $? $(head -n 1 "$OUT/$name.out") $((SECONDS - t0))s"; }
  run m-qual qualification;  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-qual.out")" --mode qualification 2>&1 | tail -n 1)"   # want 0
  run m-broken broken;       note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-broken.out")" --mode broken 2>&1 | tail -n 1)"        # want 1
  sb "$TC" replay "$(attempt "$OUT/m-broken.out")" --cpu "$C_MINIO" --state "$s" --allow-dev-engine > "$OUT/m-replay.out" 2> "$OUT/m-replay.err"
  note "m-replay: exit $? $(grep -h '^replay:' "$OUT/m-replay.out")"                                                                 # want MATCH
  note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-replay.out")" --mode broken --replay 2>&1 | tail -n 1)"
  run m-fixed fixed;         note "  $(sb python3 "$e/verify.py" "$(attempt "$OUT/m-fixed.out")" --mode fixed 2>&1 | tail -n 1)"          # want 0
}
lane_cancel & P1=$!; lane_minio & P2=$!; wait $P1 $P2
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in $C_MINIO $C_CANCEL; do f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "done $(date -u +%FT%TZ)"; cp "$SUM" "$OUT.txt"; echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $W; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
