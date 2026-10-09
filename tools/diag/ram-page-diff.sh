#!/usr/bin/env bash
# DIAGNOSTIC (branch fable/ram-page-diag): reproduce the "one RAM page differs
# while witnesses match" finding from #182 and show the differing bytes.
#
# Builds the CLI from this checkout (its engine dumps a sparse copy of guest
# RAM next to the page hashes when --full-state is on), runs WORKLOAD twice,
# replays the first run, then compares the three RAM images page by page.
#
# usage: tools/diag/ram-page-diff.sh WORKLOAD TIMEBASE [STATE_DIR] [extra seedbox run flags...]
#   e.g. tools/diag/ram-page-diff.sh harness/acceptance/workloads/runtime-jvm.yaml pv
#        tools/diag/ram-page-diff.sh harness/acceptance/workloads/kill-3.5.3.yaml counter
# Run inside `nix develop` at the repository root on a KVM host.
set -uo pipefail
workload="$1"; timebase="$2"; state="${3:-/var/tmp/ram-diag-$(date +%s)}"; shift 3 2>/dev/null || shift $#
mkdir -p "$state"
log="$state/report.txt"
exec > >(tee -a "$log") 2>&1
echo "revision: $(git rev-parse HEAD)"
echo "workload: $workload timebase: $timebase state: $state"
cargo build --release -q || { echo "build failed"; exit 2; }
sb=target/release/seedbox
run() {
  local out
  out="$("$sb" run "$workload" --timebase "$timebase" --full-state --state "$state" "$@" 2>&1)"
  echo "$out" | grep -v '^[A-Z]*: ' | tail -n 20 >&2
  echo "$out" | sed -n 's/^[A-Z]*: //p'
}
echo "--- run A"; A="$(run "$@")"; echo "A: $A"
echo "--- run B"; B="$(run "$@")"; echo "B: $B"
echo "--- replay A"
R="$("$sb" replay "$A" --state "$state" 2>&1 | tee /dev/stderr | sed -n 's/^[A-Z]*: //p')"; echo "R: $R"
echo "--- diff A B"; "$sb" diff "$A" "$B"; "$sb" diff --json "$A" "$B" > "$state/diff-AB.json"
echo "--- diff A R"; "$sb" diff "$A" "$R"; "$sb" diff --json "$A" "$R" > "$state/diff-AR.json"
echo "--- state digest parts (A, B, R)"
for d in "$A" "$B" "$R"; do
  python3 - "$d" <<'PY'
import json, sys, glob, os
d = sys.argv[1]
m = json.load(open(os.path.join(d, 'manifest.json')))['record']
rep = m.get('report') or {}
print(os.path.basename(d), 'outcome', m.get('outcome'), 'witness', (rep.get('witness') or '')[:16])
for name, st in sorted((rep.get('full_state') or {}).items()):
    print('  ', name, str(st)[:80])
PY
done
echo "--- page-by-page RAM comparison"
python3 tools/diag/ram-page-compare.py "$state/pages" "A=$A" "B=$B" "R=$R"
echo "--- RAM dumps (sparse) left in place:"; du -sh "$A"/output/machines/*.ram "$B"/output/machines/*.ram "$R"/output/machines/*.ram 2>/dev/null
echo "report: $log ; extracted pages: $state/pages"
