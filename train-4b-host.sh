#!/usr/bin/env bash
# Train/4 re-run of #105's post lanes only, on the kept train-4 dir (built
# binaries, generated faults projects, and the f-pre-run with its state).
# A dev-unpinned run replays only on its own engine (#95: "development replay
# always compares the exact retained artifact pair"), so a post-engine replay of
# the pre run is NOT_COMPARABLE by design. Instead:
#   f-post-run   post engine, same project, --full-state; want PASS and the same
#                world witness as f-pre-run (#105 claims witnesses are untouched)
#   f-post1/2    two replays of f-post-run on the post engine; want MATCH
#   logs --json  of f-post-run, f-post1 and f-post2 byte-identical and non-empty,
#                no unindexed-suffix omission; rollbacks > 0 in at least one run
#   host seconds f-post-run vs f-pre-run and f-post* vs f-pre replay (flag >5%)
# Usage: train-4b-host.sh OUT        (OUT = the kept train-4 dir)
# Env: A (accepted kernel), LANE_CPU (default 3; never 0 or 16).
set -uo pipefail
OUT=$1; : "${A:?}"
T=$OUT/train P=$OUT/pre CPU=${LANE_CPU:-3}
[[ $CPU != 0 && $CPU != 16 ]] || { echo "LANE_CPU: never 0 or 16"; exit 2; }
f=/tmp/seedbox-msr-cpu-$CPU.lock; [[ -e $f ]] && ! flock -n "$f" true && { echo "cpu $CPU lease is held; set LANE_CPU"; exit 2; }
if [[ -z ${TRAIN_INNER:-} ]]; then
  test -x "$T/target/release/seedbox" && test -f "$OUT/f-pre-run.out" || { echo "$OUT is not a kept train-4 dir"; exit 2; }
  test ! -e "$OUT/b" || { echo "$OUT/b exists"; exit 2; }
  mkdir -p "$OUT/b"; cp "$0" "$OUT/b/train-host.sh"
  exec env TRAIN_INNER=1 nix develop "$T" --command bash "$OUT/b/train-host.sh" "$@"
fi
B=$OUT/b SUM=$OUT/b/summary.txt s=$OUT/faults-state g=$OUT/faults-projects
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm TC=$T/target/release/seedbox
note() { echo "$*" | tee -a "$SUM"; }
sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
note "train $(git -C "$T" rev-parse HEAD) post engine $(sha256sum "$TE" | cut -c1-12) cpu $CPU $(date -u +%FT%TZ)"
note "from train-4: $(grep -h '^f-pre' "$OUT/summary.txt" | tr '\n' ';')"
t0=$SECONDS
sb "$TC" run "$g/fixed" --kernel "$A" --vmm "$TE" --cpu "$CPU" --state "$s" --allow-dev-engine --full-state \
  > "$B/f-post-run.out" 2> "$B/f-post-run.err"
note "f-post-run: exit $? $(head -n 1 "$B/f-post-run.out") $((SECONDS - t0))s"   # want PASS
post=$(attempt "$B/f-post-run.out") pre=$(attempt "$OUT/f-pre-run.out")
sb "$TC" diff "$pre" "$post" > "$B/diff.out" 2>&1
note "  pre vs post: $(grep -h '^world witness:' "$B/diff.out")"; grep -h '^result:' "$B/diff.out" | head -n 5 | sed 's/^/  /' | tee -a "$SUM"
for name in post1 post2; do
  t0=$SECONDS
  sb "$TC" replay "$post" --cpu "$CPU" --state "$s" --allow-dev-engine > "$B/f-$name.out" 2> "$B/f-$name.err"
  note "f-$name: exit $? $(grep -h '^replay:' "$B/f-$name.out") $((SECONDS - t0))s"   # want MATCH
done
for name in post-run post1 post2; do
  r=$(attempt "$B/f-$name.out")
  sb "$TC" logs "$r" --json > "$B/f-$name.logs.json" 2> "$B/f-$name.logs.err"
  note "  f-$name logs: exit $? $(jq -c '.chunks | group_by(.stream) | map({(.[0].stream|tostring): {records: length, bytes: (map(.length)|add)}}) | add' "$B/f-$name.logs.json" 2>/dev/null) omissions $(jq -c .omissions "$B/f-$name.logs.json" 2>/dev/null)"
  note "  f-$name rollbacks: $(sudo find "$r" -name vmm.json -exec jq -c '[.. | .rollbacks? // empty]' {} + 2>/dev/null | tr '\n' ' ')"
done
if [[ $(jq '.chunks | length' "$B/f-post-run.logs.json" 2>/dev/null || echo 0) -gt 0 ]] \
   && cmp -s "$B/f-post-run.logs.json" "$B/f-post1.logs.json" && cmp -s "$B/f-post1.logs.json" "$B/f-post2.logs.json"; then
  note "  logs --json: IDENTICAL across run and both replays (non-empty)"
else note "  logs --json: DIFFER or EMPTY"; fi
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
[[ -e $f ]] && ! flock -n "$f" true && note "lease $CPU still HELD"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT-b.txt"; echo "summary: $OUT-b.txt"
