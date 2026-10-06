#!/usr/bin/env bash
# Called by accept-runtime.sh after compilation; contains no direct MSR writes.
set -euo pipefail
cli=$1; vmm=$2; kernel=$3; cpu=$4; output=$5
state="$output/state"
capture_msr() {
  python3 - "$cpu" <<'PY'
import sys
sys.path.insert(0, "harness")
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
  shift 4
  set +e
  "$cli" run "$compose" --seed 1009 --cpu "$cpu" --state "$state" \
    --vmm "$vmm" --kernel "$kernel" --guest-init "$output/compose-init" \
    --busybox "$SEEDBOX_BUSYBOX" "$@" > "$output/$name.log" 2>&1
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
if test "${6:-}" = lifecycle; then
  # Reuse the prepared example sources, but every execution gets a fresh attempt.
  run_case redis-full-state "$output/redis/compose.yaml" PASS 0 --full-state
  jq -e '.replay.full_state_matches == true and (.report.full_state | length) == 2' \
    "$output/redis-full-state-replay.json" >/dev/null
  "$cli" replay "$attempt" --cpu "$cpu" --state "$state" --margin 10000 \
    > "$output/redis-margin-replay.log" 2>&1
  cat "$output/redis-margin-replay.log"
  verify_restore
  changed=$(sed -n 's/^[A-Z]*: //p' "$output/redis-margin-replay.log")
  "$cli" inspect "$changed" --json > "$output/redis-margin-replay.json"
  jq -e '.replay.status == "MATCH" and .replay.full_state_matches == true' \
    "$output/redis-margin-replay.json" >/dev/null
  for undo in 0 16; do
    "$cli" replay "$attempt" --cpu "$cpu" --state "$state" --undo "$undo" \
      > "$output/redis-undo-$undo.log" 2>&1
    cat "$output/redis-undo-$undo.log"
    verify_restore
    changed=$(sed -n 's/^[A-Z]*: //p' "$output/redis-undo-$undo.log")
    "$cli" inspect "$changed" --json > "$output/redis-undo-$undo.json"
    jq -e '.replay.status == "MATCH" and .replay.full_state_matches == true' \
      "$output/redis-undo-$undo.json" >/dev/null
  done

  sed 's/simulated_time: 60s/simulated_time: 1ms/' \
    "$output/redis/compose.yaml" > "$output/redis/timeout.yaml"
  run_case simulated-timeout "$output/redis/timeout.yaml" TIMEOUT 124

  sed 's/wall_time: 15m/wall_time: 1ms/' \
    "$output/redis/compose.yaml" > "$output/redis/wall.yaml"
  set +e
  "$cli" run "$output/redis/wall.yaml" --seed 1009 --cpu "$cpu" --state "$state" \
    --vmm "$vmm" --kernel "$kernel" --guest-init "$output/compose-init" \
    --busybox "$SEEDBOX_BUSYBOX" > "$output/wall.log" 2>&1
  status=$?
  set -e
  cat "$output/wall.log"
  test "$status" = 3
  verify_restore
  wall=$(sed -n 's/^[A-Z]*: //p' "$output/wall.log")
  "$cli" inspect "$wall" --json > "$output/wall.json"
  jq -e '.outcome == "INCOMPLETE" and .report.reason == "WALL_TIME_LIMIT"' "$output/wall.json" >/dev/null
  set +e
  "$cli" replay "$wall" --cpu "$cpu" --state "$state" > "$output/wall-replay.log" 2>&1
  status=$?
  set -e
  cat "$output/wall-replay.log"
  test "$status" = 2
  verify_restore
  wall_replay=$(sed -n 's/^[A-Z]*: //p' "$output/wall-replay.log")
  "$cli" inspect "$wall_replay" --json > "$output/wall-replay.json"
  jq -e '.outcome == "INCOMPLETE" and .replay.status == "NOT_COMPARABLE"' "$output/wall-replay.json" >/dev/null

  # Signal only our CLI, after observing its leased, live VMM child. This checks
  # the real cancellation path, including reaping before exact MSR restoration.
  baseline=$(sed -n 's/^[A-Z]*: //p' "$output/redis-full-state.log")
  "$cli" replay "$baseline" --cpu "$cpu" --state "$state" > "$output/cancel.log" 2>&1 &
  owned=$!
  python3 - "$owned" <<'PY'
import os, signal, sys, time
pid = int(sys.argv[1])
deadline = time.monotonic() + 60
while time.monotonic() < deadline:
    try:
        children = open(f"/proc/{pid}/task/{pid}/children").read().split()
    except FileNotFoundError:
        raise SystemExit("CLI exited before cancellation")
    if children:
        os.kill(pid, signal.SIGTERM)
        break
    time.sleep(.01)
else:
    os.kill(pid, signal.SIGTERM)
    raise SystemExit("No VMM observed before cancellation deadline")
PY
  set +e
  wait "$owned"
  status=$?
  set -e
  cat "$output/cancel.log"
  test "$status" = 2 # Replay NOT_COMPARABLE, independently of INCOMPLETE outcome.
  verify_restore
  cancelled=$(sed -n 's/^[A-Z]*: //p' "$output/cancel.log")
  "$cli" inspect "$cancelled" --json > "$output/cancel.json"
  jq -e '.outcome == "INCOMPLETE" and .report.reason == "CANCELLED" and .replay.status == "NOT_COMPARABLE"' "$output/cancel.json" >/dev/null
  echo 'PASS: full-state, changed-margin/undo replay, timeout, wall limit, cancellation; MSRs restored'
  exit 0
fi
run_case redis "$output/redis/compose.yaml" PASS 0
run_case etcd "$output/etcd/compose.yaml" PASS 0
sed 's/SEEDBOX_PLANT_FAILURE: "0"/SEEDBOX_PLANT_FAILURE: "1"/' \
  "$output/redis/compose.yaml" > "$output/redis/failure.yaml"
run_case redis-failure "$output/redis/failure.yaml" FAIL 1
sed 's/--writes, "20"/--writes, "20", --plant, missing/' \
  "$output/etcd/compose.yaml" > "$output/etcd/failure.yaml"
run_case etcd-failure "$output/etcd/failure.yaml" FAIL 1
echo 'PASS: registry-image workloads and intentional failures replayed; MSRs restored'
