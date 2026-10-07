#!/usr/bin/env bash
# Side job 2: #110 scoped smoke on its main-merged head, and #105's cancel rerun.
#  lane HC     `host-check.sh --skip-portable` at #110's head (plan: core only;
#              portable passed on Linux at the same SHA). Report PASS/MATCH/no-retry;
#              no timing comparison (other lanes may be running).
#  lane CANCEL #105 head: kill-3.5.3 seed 4000, SIGINT after CANCEL_AFTER seconds
#              (default 90 of ~220 s), want exit 3 and `logs` reading a committed
#              prefix (chunks > 0), plus the machines/ listing for the reviewer.
# Usage: side-2.sh SHA110 SHA105 OUT     (OUT must not exist)
# Env: A, PINNED, LANE_CPUS="HC CANCEL" (default "14 15"; never 0/16), CANCEL_AFTER.
set -uo pipefail
S110=$1 S105=$2 OUT=$3; : "${A:?} ${PINNED:?}"
REPO=${REPO:-$PWD} W=$OUT/pr105 AFTER=${CANCEL_AFTER:-90}
read -r C_HC C_CANCEL <<< "${LANE_CPUS:-14 15}"
for c in $C_HC $C_CANCEL; do
  [[ $c != 0 && $c != 16 ]] || { echo "never CPU 0 or 16"; exit 2; }
  f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && { echo "cpu $c lease is held; set LANE_CPUS"; exit 2; }
done
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin codex/lighter-runs codex/63-timed-logs
  git -C "$REPO" worktree add --detach -q "$W" "$S105"
  cp "$0" "$OUT/side.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$W" --command bash "$OUT/side.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
note "#110 $S110 #105 $S105 $(date -u +%FT%TZ)"
lane_hc() {
  local hc=$OUT/hc; mkdir -p "$hc"
  (cd "$REPO" && git show "$S110:harness/host-check.sh" > "$hc/host-check.sh" \
    && SEEDBOX_KERNEL=$A PINNED_ENGINE=$PINNED HOST_CHECK_DIR=$hc bash "$hc/host-check.sh" --skip-portable "$S110" "$C_HC") > "$OUT/host-check.log" 2>&1
  note "#110 host-check: exit $? $(tail -n 1 "$OUT/host-check.log")"
  local r; r=$(sed -n 's/^[A-Z]*: \([^;]*\).*/\1/p' "$OUT/host-check.log" | tail -n 1); [[ -f $r ]] && sed 's/^/  /' "$r" | tee -a "$SUM"
}
lane_cancel() {
  (cd "$W" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build.log" 2>&1 || { note "#105: BUILD FAIL"; return; }
  local TC=$W/target/release/seedbox TE=$W/crates/seedbox-vmm/target/release/seedbox-vmm s=$OUT/cancel-state pid t0=$SECONDS
  note "#105 engine $(sha256sum "$TE" | cut -c1-12)"
  sb "$TC" run "$W/harness/acceptance/workloads/kill-3.5.3.yaml" --seed 4000 --kernel "$A" --vmm "$TE" --cpu "$C_CANCEL" \
    --state "$s" --allow-dev-engine > "$OUT/cancel.out" 2> "$OUT/cancel.err" & pid=$!
  while kill -0 $pid 2>/dev/null && (( SECONDS - t0 < AFTER )); do sleep 1; done
  if kill -0 $pid 2>/dev/null; then
    for p in $(pgrep -x seedbox); do grep -q kill-3.5.3 /proc/$p/cmdline 2>/dev/null && sudo kill -INT "$p"; done
    note "  SIGINT at $((SECONDS - t0))s"
  else note "  run ended before ${AFTER}s; lower CANCEL_AFTER"; fi
  wait $pid; note "#105 cancel: exit $? $(head -n 1 "$OUT/cancel.out") after $((SECONDS - t0))s"   # want INCOMPLETE, exit 3
  local run; run=$(attempt "$OUT/cancel.out")
  note "  machines/: $(sudo find "$run" -path '*machines*' -printf '%P:%s ' 2>/dev/null | head -c 800)"
  note "  last stderr: $(tail -n 3 "$OUT/cancel.err" | tr '\n' ' ' | head -c 400)"
  sb "$TC" logs "$run" --json > "$OUT/cancel.logs.json" 2> "$OUT/cancel.logs.err"
  note "  logs: exit $? chunks $(jq '.chunks | length' "$OUT/cancel.logs.json" 2>/dev/null) omissions $(jq -c .omissions "$OUT/cancel.logs.json" 2>/dev/null | head -c 600) $(head -n 2 "$OUT/cancel.logs.err" | tr '\n' ' ')"
}
lane_hc & P1=$!; lane_cancel & P2=$!; wait $P1 $P2
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in $C_HC $C_CANCEL; do f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "done $(date -u +%FT%TZ)"; cp "$SUM" "$OUT.txt"; echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $W; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
