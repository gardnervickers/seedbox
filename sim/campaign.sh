#!/usr/bin/env bash
# Usage: campaign.sh WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]]
# Runs seeds in batches of 14 until a consistency failure is found or the
# time budget is spent. Overshoot (infrastructure) failures are retried with a
# larger stepping margin; results are deterministic, so a retry is equivalent.
# Every seed must end with a RESULT line; any that does not is logged as
# UNRESOLVED and the campaign exits nonzero. Env passes through to batch.sh.
set -u
here=$(cd "$(dirname "$0")" && pwd)
cd ~/seedbox-sim
world="$1"; name="$2"; seed="$3"; hours="$4"; max="${5:-1000000}"; keep="${6:-}"
first="$seed"
end=$(( $(date +%s) + hours * 3600 ))
log="runs/$name.campaign.log"
unresolved=0
while [ "$(date +%s)" -lt "$end" ] && [ $((seed - first)) -lt "$max" ]; do
  per="${BATCH:-14}"
  seeds=$(seq "$seed" $((seed + per - 1)) | tr '\n' ' ')
  if ! "$here/batch.sh" "$world" "$name" "$seeds" > /dev/null 2>&1; then
    echo "ABORTED batch setup failed for seeds $seeds" >> "$log"
    exit 1
  fi
  for s in $seeds; do
    f=$(ls runs/$name-s$s-r*.json | head -1); d=${f%.json}
    res=$(grep -h RESULT "$d/client.console" 2>/dev/null)
    if [ -z "$res" ] && grep -q overshoot "$d.err"; then
      if ! SEEDBOX_MARGIN=98304 "$here/batch.sh" "$world" "$name-retry" "$s" > /dev/null 2>&1; then
        echo "ABORTED batch setup failed for retry of seed $s" >> "$log"
        exit 1
      fi
      f=$(ls runs/$name-retry-s$s-r*.json | head -1); d=${f%.json}
      res=$(grep -h RESULT "$d/client.console" 2>/dev/null)
      [ -n "$res" ] && res="(retry) $res"
    fi
    host=$(python3 -c "import json;print(round(json.load(open('$f'))['host_seconds']))" 2>/dev/null)
    if [ -z "$res" ]; then
      why=$(tail -n 1 "$d.err" 2>/dev/null | cut -c1-200)
      res="UNRESOLVED ${why:-no result}"
      unresolved=$((unresolved + 1))
    fi
    echo "seed=$s host_s=${host:-?} $res" >> "$log"
  done
  [ -z "$keep" ] && grep -q "RESULT fail" "$log" && { echo "FOUND" >> "$log"; break; }
  seed=$((seed + per))
done
echo "DONE unresolved=$unresolved" >> "$log"
[ "$unresolved" -eq 0 ]
