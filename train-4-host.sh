#!/usr/bin/env bash
# Host checks for train/4: train/3 (6cac3a9) + #105 timed logs (ac7d7a9).
# #105's list is its first reviewer's (PR comment 6043165117), plus B1 feature-off
# equality (cluster-fixed 1009 == 4b0b13cc) and host-check core on the head.
# "pre" = the train commit before #105 (TRAIN^1), "post" = TRAIN.
# Portable checks passed at this SHA on Linux (harness/mac-check.sh), so
# host-check runs with --skip-portable.
# Run as gvickers from the main seedbox checkout; re-enters the train worktree's
# own `nix develop`. Needs cores 2-7 (siblings 18-23) free and network for the
# python:3.13-alpine image.
# Usage: train-4-host.sh TRAIN_SHA OUT     (OUT must not exist)
# Env: A (accepted kernel e8ece7cb), ARTS (dir with multi.cpio, net.cpio, disk.cpio),
#      PINNED (stored 52eb engine, for host-check's wrong-kernel control).
set -uo pipefail
TRAIN=$1 OUT=$2
: "${A:?} ${ARTS:?} ${PINNED:?}"
REPO=${REPO:-$PWD} T=$OUT/train P=$OUT/pre
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin claude/train-4
  git -C "$REPO" worktree prune
  git -C "$REPO" worktree add --detach -q "$T" "$TRAIN"
  git -C "$REPO" worktree add --detach -q "$P" "$TRAIN^1"
  cp "$0" "$OUT/train-host.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$T" --command bash "$OUT/train-host.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
note "train $(git -C "$T" rev-parse HEAD) pre $(git -C "$P" rev-parse HEAD) $(date -u +%FT%TZ)"
for w in "$T" "$P"; do
  (cd "$w" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build-$(basename "$w").log" 2>&1 \
    || { note "BUILD FAIL $w"; exit 1; }
done
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm TC=$T/target/release/seedbox
PE=$P/crates/seedbox-vmm/target/release/seedbox-vmm PC=$P/target/release/seedbox
note "post engine $(sha256sum "$TE" | cut -c1-12) pre engine $(sha256sum "$PE" | cut -c1-12)"
mkdir -p "$T/artifacts"; cp "$ARTS"/*.cpio "$T/artifacts/"; ln -sfn "$A" "$T/artifacts/bzImage"

sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }

lane_b1() {  # cpu 2: feature-off witness equality (want 4b0b13cc, as train/3)
  (cd "$T" && SEEDBOX_ROOT="$T" VMM="$TE" CPU0=2 SEEDBOX_TIMEOUT=600 \
    harness/batch.sh harness/worlds/cluster-fixed.json b1-cluster-fixed 1009 --digest) > "$OUT/world-b1.log" 2>&1
  note "world b1: exit $? $(sudo jq -c 'with_entries(select(.key|test("witness";"i")))' "$T"/runs/b1-cluster-fixed-s1009-r0.json 2>/dev/null)"
}
lane_faults() {  # cpu 3: #105 items 1-2, sequential so host seconds compare
  local s=$OUT/faults-state g=$OUT/faults-projects t0 run
  python3 "$T/examples/faults/generate.py" "$g" > "$OUT/faults-generate.log" 2>&1 || { note "faults: generate FAILED"; return; }
  t0=$SECONDS
  sb "$PC" run "$g/fixed" --kernel "$A" --vmm "$PE" --cpu 3 --state "$s" --allow-dev-engine --full-state \
    > "$OUT/f-pre-run.out" 2> "$OUT/f-pre-run.err"
  note "f-pre-run: exit $? $(head -n 1 "$OUT/f-pre-run.out") $((SECONDS - t0))s"   # want PASS
  run=$(attempt "$OUT/f-pre-run.out")
  for name in post1 post2 pre; do
    local extra=(); [[ $name == post* ]] && extra=(--vmm "$TE" --allow-engine-change)
    local bin=$TC; [[ $name == pre ]] && bin=$PC
    t0=$SECONDS
    sb "$bin" replay "$run" --cpu 3 --state "$s" --allow-dev-engine "${extra[@]}" > "$OUT/f-$name.out" 2> "$OUT/f-$name.err"
    note "f-$name: exit $? $(grep -h '^replay:' "$OUT/f-$name.out") $((SECONDS - t0))s"   # want MATCH
  done
  for name in post1 post2; do
    local r; r=$(attempt "$OUT/f-$name.out")
    sb "$TC" logs "$r" --json > "$OUT/f-$name.logs.json" 2> "$OUT/f-$name.logs.err"
    note "  f-$name logs: exit $? $(jq -c '.chunks | group_by(.stream) | map({(.[0].stream|tostring): {records: length, bytes: (map(.length)|add)}}) | add' "$OUT/f-$name.logs.json" 2>/dev/null) omissions $(jq -c .omissions "$OUT/f-$name.logs.json" 2>/dev/null)"
    note "  f-$name rollbacks: $(sudo find "$r" -name vmm.json -exec jq -c '[.. | .rollbacks? // empty]' {} + 2>/dev/null | tr '\n' ' ')"
  done
  cmp -s "$OUT/f-post1.logs.json" "$OUT/f-post2.logs.json" && note "  post logs --json: IDENTICAL" || note "  post logs --json: DIFFER"
}
lane_kvm_fixtures() {  # cpu 4: #105 item 3 plus the reviewer's ordering proof (ignored KVM tests, leased)
  (cd "$T" && sb python3 harness/check-timed-logs-kvm.py 4) > "$OUT/timed-logs-kvm.log" 2>&1
  note "check-timed-logs-kvm: exit $? $(grep -h '^test result' "$OUT/timed-logs-kvm.log" | tail -n 1)"   # want 3 passed
  local bin
  bin=$(cd "$T" && cargo test-vmm --no-run --message-format=json 2>/dev/null \
    | jq -r 'select(.executable != null and (.target.kind | index("bin"))) | .executable' | tail -n 1)
  # committed_console_discards_rolled_back_candidate_before_matcher_feed + trigger_world_tests
  (cd "$T" && sb python3 harness/test-triggers.py 4 "$bin") > "$OUT/test-triggers.log" 2>&1
  note "test-triggers: exit $? $(grep -h '^test result' "$OUT/test-triggers.log" | tr '\n' ' ')"
}
lane_cancel() {  # cpu 5: #105 item 4; interrupt a post run after guest output, want exit 3 + committed prefix
  local s=$OUT/cancel-state pid
  sb "$TC" run "$T/harness/acceptance/workloads/kill-3.5.3.yaml" --seed 4000 --kernel "$A" --vmm "$TE" --cpu 5 \
    --state "$s" --allow-dev-engine > "$OUT/cancel.out" 2> "$OUT/cancel.err" & pid=$!
  sleep 25
  for p in $(pgrep -x seedbox); do grep -q kill-3.5.3 /proc/$p/cmdline 2>/dev/null && sudo kill -INT "$p"; done
  wait $pid; note "cancel: exit $? $(head -n 1 "$OUT/cancel.out")"   # want INCOMPLETE, exit 3
  sb "$TC" logs "$(attempt "$OUT/cancel.out")" --json > "$OUT/cancel.logs.json" 2> "$OUT/cancel.logs.err"
  note "  cancel logs: exit $? chunks $(jq '.chunks | length' "$OUT/cancel.logs.json" 2>/dev/null) omissions $(jq -c .omissions "$OUT/cancel.logs.json" 2>/dev/null) $(head -n 3 "$OUT/cancel.logs.err" | tr '\n' ' ')"
}

lane_b1 & P1=$!; lane_faults & P2=$!; lane_kvm_fixtures & P3=$!; lane_cancel & P4=$!
wait $P1 $P2 $P3 $P4
(cd "$REPO" && SEEDBOX_KERNEL=$A PINNED_ENGINE=$PINNED HOST_CHECK_DIR=$OUT bash "$T/harness/host-check.sh" --skip-portable "$TRAIN" 7) > "$OUT/host-check.log" 2>&1
note "host-check: exit $? $(tail -n 1 "$OUT/host-check.log")"
hc=$(sed -n 's/^[A-Z]*: \([^;]*\).*/\1/p' "$OUT/host-check.log" | tail -n 1); [[ -f $hc ]] && tee -a "$SUM" < "$hc"

note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in 2 3 4 5 7; do f=/tmp/seedbox-msr-cpu-$c.lock; [[ -e $f ]] && ! flock -n "$f" true && note "lease $c still HELD"; done
note "leases checked; MSRs are checked by each run's lease readback"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"
echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $T; git -C $REPO worktree remove --force $P; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
