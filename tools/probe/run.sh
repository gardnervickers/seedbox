#!/usr/bin/env bash
# PROBE ONLY (never merged): run fault workloads twice with one seed under CPU
# hogs on this host, keep the raw stopped-state evidence, and compare it.
set -uo pipefail
root=$PWD
sb=$root/target/release/seedbox
out=$root/target/probe-out
mkdir -p "$out"
export SEEDBOX_STATE=$RUNNER_TEMP/s
grep -m1 "model name" /proc/cpuinfo
"$sb" server start --full-state >"$out/server-start.log" 2>&1 || { cat "$out/server-start.log"; exit 1; }
stash() { # move this run's evidence from the fixed probe directory into $1
  local dest=$1; mkdir -p "$dest"
  shopt -s nullglob
  local f; for f in /tmp/sbx-probe/*; do mv "$f" "$dest/"; done
  shopt -u nullglob
  ls -l "$dest" | head -40
}
rm -rf /tmp/sbx-probe
run_one() { # workload label [seed]
  local w=$1 label=$2 seed=${3:-}
  local dir=$out/$w/$round; mkdir -p "$dir"
  local args=(run "$root/harness/acceptance/workloads/$w.yaml" --wall-time 30m)
  [[ -n $seed ]] && args+=(--seed "$seed")
  local pids=""; for _ in $(seq "$(nproc)"); do yes >/dev/null & pids="$pids $!"; done
  "$sb" "${args[@]}" >"$dir/$label.log" 2>&1
  local status=$?
  kill $pids 2>/dev/null; wait $pids 2>/dev/null
  local id; id=$(sed -n 's/^PASS: //p;s/^FAIL: //p' "$dir/$label.log" | head -1)
  echo "$w round $round $label: exit $status id ${id:-none}" | tee -a "$out/summary.txt"
  echo "$id" >"$dir/$label.id"
  stash "$dir/$label"
}
for round in 1 2; do
  for w in network-faults dns volume-crash profile; do
    run_one "$w" first
    id=$(cat "$out/$w/$round/first.id")
    [[ -n $id ]] || continue
    seed=$("$sb" show "$id" --view definition --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["spec"]["seed"])')
    echo "seed $seed" >>"$out/summary.txt"
    run_one "$w" second "$seed"
    second=$(cat "$out/$w/$round/second.id")
    "$sb" compare "$id" "$second" --json >"$out/$w/$round/compare.json" 2>&1
    echo "$w round $round compare: $(tr -d '\n ' <"$out/$w/$round/compare.json")" | tee -a "$out/summary.txt"
    python3 -I "$root/tools/probe/diff.py" "$out/$w/$round/first" "$out/$w/$round/second" | tee "$out/$w/$round/diff.txt" | head -150
    rm -f "$out/$w/$round"/*/*.probe-ram
  done
done
"$sb" server stop || true
echo "=== SUMMARY ==="; cat "$out/summary.txt"
