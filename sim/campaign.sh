#!/usr/bin/env bash
# Usage: campaign.sh WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]]
# Runs seeds in batches of 14 until a consistency failure is found or the
# time budget is spent. Overshoot (infrastructure) failures are retried with a
# larger stepping margin; results are deterministic, so a retry is equivalent.
set -u
cd ~/seedbox-sim
world="$1"; name="$2"; seed="$3"; hours="$4"; max="${5:-1000000}"; keep="${6:-}"
first="$seed"
end=$(( $(date +%s) + hours * 3600 ))
log="runs/$name.campaign.log"
while [ "$(date +%s)" -lt "$end" ] && [ $((seed - first)) -lt "$max" ]; do
  per="${BATCH:-14}"
  seeds=$(seq "$seed" $((seed + per - 1)) | tr '\n' ' ')
  ./batch.sh "$world" "$name" "$seeds" > /dev/null 2>&1
  for s in $seeds; do
    f=$(ls runs/$name-s$s-r*.json | head -1); d=${f%.json}
    res=$(grep -h RESULT "$d/client.console" 2>/dev/null)
    if [ -z "$res" ] && grep -q overshoot "$d.err"; then
      SEEDBOX_MARGIN=98304 ./batch.sh "$world" "$name-retry" "$s" > /dev/null 2>&1
      f=$(ls runs/$name-retry-s$s-r*.json | head -1); d=${f%.json}
      res="(retry) $(grep -h RESULT "$d/client.console" 2>/dev/null)"
    fi
    host=$(python3 -c "import json;print(round(json.load(open('$f'))['host_seconds']))" 2>/dev/null)
    echo "seed=$s host_s=$host $res" >> "$log"
  done
  [ -z "$keep" ] && grep -q "RESULT fail" "$log" && { echo "FOUND" >> "$log"; break; }
  seed=$((seed + per))
done
echo "DONE" >> "$log"
