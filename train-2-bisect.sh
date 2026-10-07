#!/usr/bin/env bash
# Bisect train/2's smoke default-replay MISMATCH: engine (#101) vs workload (#110's batch=4).
# Run as gvickers from anywhere; re-enters the train worktree's dev shell.
# Usage: train-2-bisect.sh EVIDENCE OUT
#   EVIDENCE = the kept host-check dir (…/host-check-29fdb9b-cpu7-pNpq)
# Env: A (accepted kernel e8ece7cb), TW (train/2 worktree, built), MW (4ea970a worktree, built)
set -uo pipefail
EV=$1 OUT=$2
: "${A:?} ${TW:?} ${MW:?}"
if [[ -z ${BISECT_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"; cp "$0" "$OUT/bisect.sh"
  exec env BISECT_INNER=1 nix develop "$TW" --command bash "$OUT/bisect.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
TC=$TW/target/release/seedbox MC=$MW/target/release/seedbox
TE=$TW/crates/seedbox-vmm/target/release/seedbox-vmm ME=$MW/crates/seedbox-vmm/target/release/seedbox-vmm
note "train engine $(sha256sum "$TE" | cut -c1-12) main engine $(sha256sum "$ME" | cut -c1-12) $(date -u +%FT%TZ)"

# 0. First divergence in the kept evidence (offline, no guests).
mapfile -t runs < <(sudo find "$EV" -name manifest.json -printf '%T@ %h\n' | sort -n | cut -d' ' -f2)
note "kept runs: ${runs[*]}"
if (( ${#runs[@]} >= 2 )); then
  sb "$TC" diff "${runs[0]}" "${runs[1]}" > "$OUT/kept-diff.txt" 2>&1; note "kept fixed vs default replay diff: exit $?"
  head -n 40 "$OUT/kept-diff.txt" | tee -a "$SUM"
  sb "$TC" diff --json "${runs[0]}" "${runs[1]}" > "$OUT/kept-diff.json" 2>/dev/null
fi

gen() {  # SRC_WORKTREE DEST BATCH
  (cd "$1/examples/faults" && python3 -c "import sys, inspect; from generate import generate
kw = {'batch': int(sys.argv[2])} if 'batch' in inspect.signature(generate).parameters else {}
print(generate(sys.argv[1], **kw) / 'fixed')" "$2" "$3")
}
# One cell: run --full-state, default replay, offline diff.
cell() {  # NAME CLI ENGINE CPU PROJECT
  local n=$1 cli=$2 engine=$3 cpu=$4 project=$5 s=$OUT/state-$1
  sb "$cli" run "$project" --seed 1009 --full-state --memory-mb 512 --kernel "$A" --vmm "$engine" --cpu "$cpu" \
    --state "$s" --allow-dev-engine > "$OUT/$n-run.out" 2> "$OUT/$n-run.err"
  local rc=$?
  sb "$cli" replay "$(attempt "$OUT/$n-run.out")" --cpu "$cpu" --state "$s" --allow-dev-engine > "$OUT/$n-replay.out" 2> "$OUT/$n-replay.err"
  note "$n: run exit $rc $(head -n 1 "$OUT/$n-run.out" | cut -c1-6) replay exit $? $(grep -h '^replay:' "$OUT/$n-replay.out")"
  sb "$TC" diff "$(attempt "$OUT/$n-run.out")" "$(attempt "$OUT/$n-replay.out")" > "$OUT/$n-diff.txt" 2>&1
  note "  $n diff exit $?: $(head -n 3 "$OUT/$n-diff.txt" | tr '\n' ' ')"
}
P4T=$(gen "$TW" "$OUT/proj-b4" 4)          # #110's smoke fixture
P32T=$(gen "$TW" "$OUT/proj-b32" 32)       # #110's workload source, default batch
P32M=$(gen "$MW" "$OUT/proj-main" 32)      # main's workload source
note "projects: b4=$P4T b32=$P32T main=$P32M; b4 '--batch' count $(grep -c -- '--batch' "$P4T/compose.yaml")"
cell train-b4   "$TC" "$TE" 2 "$P4T" &
cell train-b32  "$TC" "$TE" 3 "$P32T" &
cell main-b4    "$MC" "$ME" 4 "$P4T" &
cell main-main  "$MC" "$ME" 5 "$P32M" &
wait
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in 2 3 4 5; do f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"; echo "summary: $OUT.txt"
echo "after posting: sudo rm -rf $OUT $OUT.txt"
