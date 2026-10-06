#!/usr/bin/env bash
# Called by accept-runtime.sh after compilation; contains no direct MSR writes.
set -euo pipefail
cli=$1; vmm=$2; kernel=$3; cpu=$4; output=$5
state="$output/state"
capture_msr() {
  python3 - "$cpu" <<'PY'
import sys
sys.path.insert(0, "sim")
import batch, msr54
msr54.show(sorted(batch.siblings(int(sys.argv[1]))))
PY
}
capture_msr > "$output/msr-before.txt"
verify_restore() {
  capture_msr > "$output/msr-after.txt"
  cmp "$output/msr-before.txt" "$output/msr-after.txt"
  if test -d "$state/runs" && test -n "$(find "$state/runs" -name msr-original.json -print -quit)"; then
    echo 'An MSR restoration snapshot remains' >&2
    return 1
  fi
}
trap verify_restore EXIT
run_case() {
  name=$1; compose=$2; expected=$3; expected_status=$4
  set +e
  "$cli" run "$compose" --seed 1009 --cpu "$cpu" --state "$state" \
    --vmm "$vmm" --kernel "$kernel" --guest-init "$output/compose-init" \
    --busybox "$SEEDBOX_BUSYBOX" > "$output/$name.log" 2>&1
  status=$?
  set -e
  cat "$output/$name.log"
  verify_restore
  test "$status" = "$expected_status"
  attempt=$(sed -n 's/^[A-Z]*: //p' "$output/$name.log")
  test -n "$attempt"
  "$cli" inspect "$attempt" --json > "$output/$name.json"
  jq -e --arg expected "$expected" '.outcome == $expected and .report.witness != null' "$output/$name.json" >/dev/null
  set +e
  "$cli" replay "$attempt" --cpu "$cpu" --state "$state" \
    > "$output/$name-replay.log" 2>&1
  status=$?
  set -e
  cat "$output/$name-replay.log"
  verify_restore
  test "$status" = 0
  replay=$(sed -n 's/^[A-Z]*: //p' "$output/$name-replay.log")
  "$cli" inspect "$replay" --json > "$output/$name-replay.json"
  jq -e --arg expected "$expected" '.outcome == $expected and .replay.status == "MATCH"' "$output/$name-replay.json" >/dev/null
}
run_case redis "$output/redis/compose.yaml" PASS 0
run_case etcd "$output/etcd/compose.yaml" PASS 0
sed 's/SEEDBOX_PLANT_FAILURE: "0"/SEEDBOX_PLANT_FAILURE: "1"/' \
  "$output/redis/compose.yaml" > "$output/redis/failure.yaml"
run_case redis-failure "$output/redis/failure.yaml" FAIL 1
sed 's/--writes, "20"/--writes, "20", --plant, missing/' \
  "$output/etcd/compose.yaml" > "$output/etcd/failure.yaml"
run_case etcd-failure "$output/etcd/failure.yaml" FAIL 1
echo 'PASS: registry-image workloads and intentional failures replayed; MSRs restored'
