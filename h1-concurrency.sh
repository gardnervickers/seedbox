#!/usr/bin/env bash
# H1: how many seedbox jobs the 5950X runs at once.
# Run as gvickers from the main checkout with nothing else using leases 1-14
# (checked below; the script stops if any is held). Unpinned engine, hence
# --allow-dev-engine.
# Usage: h1-concurrency.sh KERNEL OUT     (OUT must not exist; REV defaults to origin/main)
# Self-contained: makes a worktree at REV, enters its dev shell and builds.
set -euo pipefail
K=$1; OUT=$2; S=$OUT/state; REV=${REV:-origin/main}; REPO=${REPO:-$PWD}
if [[ -z ${H1_INNER:-} ]]; then
  test ! -e "$OUT"; mkdir -p "$OUT"
  git -C "$REPO" fetch -q origin main; git -C "$REPO" worktree prune
  git -C "$REPO" worktree add --detach -q "$OUT/src" "$REV"
  cp "$0" "$OUT/h1.sh"
  cd "$OUT/src"
  exec env H1_INNER=1 REPO="$REPO" nix develop "$OUT/src" --command bash -c \
    'nice cargo build --release --locked -p seedbox >/dev/null && nice cargo build-vmm >/dev/null && exec bash "$0" "$@"' "$OUT/h1.sh" "$@"
fi
cd "$OUT/src"
SB=(sudo -E env "PATH=$PATH" "SEEDBOX_KERNEL=$K" "$PWD/target/release/seedbox")
held() { flock -n "$1" true && echo free || echo HELD; }

# 0. Read-only inventory.
{ date -u; git rev-parse HEAD; uname -r; nproc; free -m; df -m /home /tmp
  lscpu | grep -E 'Model name|Thread|Core|Socket|NUMA'
  for f in /tmp/seedbox-msr-cpu-*.lock; do echo "$f $(held "$f")"; done
  pgrep -af 'seedbox|batch.py' || echo 'no seedbox processes'
  uptime; sha256sum "$K"; } > "$OUT/inventory.txt" 2>&1
for c in $(seq 1 14); do f=/tmp/seedbox-msr-cpu-$c.lock
  [ -e "$f" ] && [ "$(held "$f")" = HELD ] && { echo "lease $c held; host busy, stopping"; exit 1; }; done

python3 examples/faults/generate.py "$OUT/project" > /dev/null
# 1. One full-state baseline; preparation is paid once, here.
/usr/bin/env time -v -o "$OUT/baseline.time" "${SB[@]}" run "$OUT/project/fixed" --full-state --cpu 1 --state "$S" --allow-dev-engine \
  > "$OUT/baseline.out" 2> "$OUT/baseline.err" || true
RUN=$(sed -n 's/^PASS: //p' "$OUT/baseline.out"); test -d "$RUN"
"${SB[@]}" inspect "$RUN" --json | jq -r .report.witness > "$OUT/baseline.witness"
du -sm "$S" "$RUN" > "$OUT/baseline.du"

# 2. N concurrent retained replays, one per physical core (cpu c, sibling c+16).
sample() { while :; do echo "$(date +%s) $(free -m | awk '/Mem:/{print $3, $7}') $(df -m --output=used /home | tail -1) $(ps -C seedbox-vmm -o rss= | awk '{s+=$1} END {print s+0}')"; sleep 1; done; }
for N in 1 4 8 14; do
  D=$OUT/n$N; mkdir -p "$D"
  sample > "$D/samples.txt" & SAMPLER=$!   # epoch used_mb avail_mb home_used_mb vmm_rss_kb
  T0=$(date +%s.%N); PIDS=()
  for i in $(seq 1 "$N"); do
    ( s=$(date +%s.%N); rc=0
      "${SB[@]}" replay "$RUN" --state "$S" --cpu "$i" --allow-dev-engine > "$D/r$i.out" 2> "$D/r$i.err" || rc=$?
      echo "$rc $(echo "$(date +%s.%N) - $s" | bc)" > "$D/r$i.code" ) & PIDS+=($!)
  done
  wait "${PIDS[@]}"; T1=$(date +%s.%N); kill "$SAMPLER"
  peak=$(awk 'BEGIN{u=0;r=0} {if($2>u)u=$2; if($5>r)r=$5} END{print "peak_used_mb=" u " peak_vmm_rss_mb=" int(r/1024)}' "$D/samples.txt")
  echo "N=$N wall_s=$(echo "$T1 - $T0" | bc) $peak" | tee "$D/summary.txt"
  for i in $(seq 1 "$N"); do
    read -r rc secs < "$D/r$i.code"
    dir=$(sed -n 's/^[A-Z_]*: //p' "$D/r$i.out"); st=? w=?
    if [ -d "$dir" ]; then j=$("${SB[@]}" inspect "$dir" --json)
      st=$(jq -r .replay.status <<<"$j"); w=$(jq -r .report.witness <<<"$j"); fi
    same=$([ "$w" = "$(cat "$OUT/baseline.witness")" ] && echo yes || echo NO)
    echo "r$i cpu=$i exit=$rc secs=$secs replay=$st witness_same=$same" | tee -a "$D/summary.txt"
  done
done

# 3. Leases, MSRs and processes afterwards.
{ for f in /tmp/seedbox-msr-cpu-*.lock; do echo "$f $(held "$f")"; done
  for c in $(seq 1 14) $(seq 17 30); do printf '%s:%s ' "$c" "$(sudo rdmsr -p "$c" 0xc0011020 2>/dev/null || echo '?')"; done; echo
  pgrep -af seedbox-vmm || echo 'no VMM processes'; df -m /home; } > "$OUT/after.txt"

# 4. Keep only the text results; delete run state (cleanup rule).
mkdir -p "$OUT.results"; cp "$OUT"/*.txt "$OUT"/baseline.* "$OUT.results/"
for d in "$OUT"/n*; do mkdir -p "$OUT.results/$(basename "$d")"; cp "$d"/summary.txt "$d"/samples.txt "$OUT.results/$(basename "$d")/"; done
git -C "$REPO" worktree remove --force "$OUT/src"; sudo rm -rf "$OUT"; git -C "$REPO" worktree prune; df -m /home | tee -a "$OUT.results/after.txt"
echo "results in $OUT.results"
