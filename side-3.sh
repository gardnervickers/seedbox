#!/usr/bin/env bash
# Side job 3: #105 cancel during a REPLAY (no preparation phase), per its reviewer.
#  1. Full run of kill-3.5.3 seed 4000 with #105's CLI and engine (want a result, kept).
#  2. Replay it; poll <state>/runs/*/output/machines/ until some *.console.chunks
#     is > 8 bytes and some *.console is non-empty, then SIGINT 2 s later.
#  Want: exit 3 INCOMPLETE, non-empty raw consoles, (size-8) % 40 == 0 for every
#  .chunks, `logs --json` with timed chunks and at most one "unindexed raw bytes"
#  omission per stream. Bugs: a truncated/oversized index error, or a non-empty
#  console with no .chunks key.
# Usage: side-3.sh SHA105 OUT    (OUT must not exist)
# Env: A, LANE_CPU (default 15; never 0/16).
set -uo pipefail
S105=$1 OUT=$2; : "${A:?}"
REPO=${REPO:-$PWD} W=$OUT/pr105 CPU=${LANE_CPU:-15}
[[ $CPU != 0 && $CPU != 16 ]] || { echo "never CPU 0 or 16"; exit 2; }
f=/tmp/seedbox-msr-cpu-$CPU.lock; [[ -e $f ]] && ! flock -n "$f" true && { echo "cpu $CPU lease is held; set LANE_CPU"; exit 2; }
if [[ -z ${TRAIN_INNER:-} ]]; then
  test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
  mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin codex/63-timed-logs
  git -C "$REPO" worktree add --detach -q "$W" "$S105"
  cp "$0" "$OUT/side.sh"
  exec env TRAIN_INNER=1 REPO="$REPO" nix develop "$W" --command bash "$OUT/side.sh" "$@"
fi
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
sb() { sudo -E env "PATH=$PATH" "$@"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
(cd "$W" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build.log" 2>&1 || { note "BUILD FAIL"; exit 1; }
TC=$W/target/release/seedbox TE=$W/crates/seedbox-vmm/target/release/seedbox-vmm s=$OUT/state
note "#105 $(git -C "$W" rev-parse HEAD) cli $(sha256sum "$TC" | cut -c1-12) engine $(sha256sum "$TE" | cut -c1-12) cpu $CPU $(date -u +%FT%TZ)"
t0=$SECONDS
sb "$TC" run "$W/harness/acceptance/workloads/kill-3.5.3.yaml" --seed 4000 --kernel "$A" --vmm "$TE" --cpu "$CPU" \
  --state "$s" --allow-dev-engine > "$OUT/run.out" 2> "$OUT/run.err"
note "run: exit $? $(head -n 1 "$OUT/run.out") $((SECONDS - t0))s"
base=$(attempt "$OUT/run.out")
[[ -n $base ]] || { note "no run directory; stop"; exit 1; }
before=$(sudo ls "$s/runs" | sort)
t0=$SECONDS hit=""
sb "$TC" replay "$base" --cpu "$CPU" --state "$s" --allow-dev-engine > "$OUT/replay.out" 2> "$OUT/replay.err" & pid=$!
while kill -0 $pid 2>/dev/null && (( SECONDS - t0 < 300 )); do
  for d in $(comm -13 <(echo "$before") <(sudo ls "$s/runs" | sort)); do
    m=$s/runs/$d/output/machines
    if sudo find "$m" -name '*.console.chunks' -size +8c 2>/dev/null | grep -q . && sudo find "$m" -name '*.console' -size +0c 2>/dev/null | grep -q .; then hit=$d; fi
  done
  [[ -n $hit ]] && break; sleep 0.2
done
if [[ -n $hit ]]; then
  sleep 2
  for p in $(pgrep -x seedbox); do grep -q replay /proc/$p/cmdline 2>/dev/null && sudo kill -INT "$p"; done
  note "  SIGINT 2 s after chunks appeared ($((SECONDS - t0))s into the replay)"
else note "  no chunks seen before the replay ended or 300 s passed"; fi
wait $pid; note "replay cancel: exit $? $(head -n 1 "$OUT/replay.out") after $((SECONDS - t0))s"   # want INCOMPLETE, exit 3
r=$(attempt "$OUT/replay.out"); r=${r:-$s/runs/$hit}
note "  ls -la output/:"; sudo ls -la "$r/output" 2>&1 | sed 's/^/    /' | tee -a "$SUM"
note "  machines/:"; sudo find "$r/output/machines" -type f -printf '    %P %s\n' 2>/dev/null | sort | tee -a "$SUM"
bad=0; while read -r size name; do (( (size - 8) % 40 == 0 )) || { bad=1; note "  BAD index size $name $size"; }; done \
  < <(sudo find "$r/output/machines" -name '*.chunks' -printf '%s %P\n' 2>/dev/null)
note "  .chunks sizes (size-8)%40==0: $([[ $bad == 0 ]] && echo all || echo NO)"
note "  record logs keys: $(sb "$TC" inspect "$r" --json 2>/dev/null | jq -c '.logs | keys')"
sb "$TC" logs "$r" --json > "$OUT/logs.json" 2> "$OUT/logs.err"
note "  logs: exit $? chunks $(jq -c '.chunks | group_by(.machine + "/" + (.stream|tostring)) | map({(.[0].machine + "/" + (.[0].stream|tostring)): length}) | add' "$OUT/logs.json" 2>/dev/null) $(head -n 2 "$OUT/logs.err" | tr '\n' ' ')"
note "  omissions: $(jq -c .omissions "$OUT/logs.json" 2>/dev/null | head -c 1200)"
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
[[ -e $f ]] && ! flock -n "$f" true && note "lease $CPU still HELD"
note "done $(date -u +%FT%TZ)"; cp "$SUM" "$OUT.txt"; echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $W; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
