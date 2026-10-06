#!/usr/bin/env bash
# Usage: campaign.sh WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]]
# Runs seeds in batches of 14 until a consistency failure is found or the
# time budget is spent. PMU overshoot failures are retried with a larger margin.
# Each attempt has fresh output. A successful process, terminal JSON and RESULT
# are all required; otherwise log UNRESOLVED and exit nonzero.
set -u
here=$(cd "$(dirname "$0")" && pwd)
cd "${SEEDBOX_ROOT:-$HOME/seedbox-sim}" || exit 1
world="$1"; name="$2"; seed="$3"; hours="$4"; max="${5:-1000000}"; keep="${6:-}"
first="$seed"
end=$(( $(date +%s) + hours * 3600 ))
mkdir -p runs || exit 1
log="runs/$name.campaign.log"
unresolved=0
found=0
while [ "$(date +%s)" -lt "$end" ] && [ $((seed - first)) -lt "$max" ]; do
  per="${BATCH:-14}"
  seeds=$(seq "$seed" $((seed + per - 1)) | tr '\n' ' ')
  # Preserve previous evidence; never reuse its console, JSON or status files.
  attempt=$(mktemp -d "runs/$name-batch.XXXXXX") || exit 1
  prefix="${attempt#runs/}/run"
  if ! "$here/batch.sh" "$world" "$prefix" "$seeds" > /dev/null 2>&1; then
    echo "ABORTED batch setup failed for seeds $seeds" >> "$log"
    exit 1
  fi
  i=0
  for s in $seeds; do
    f="runs/$prefix-s$s-r$i.json"; d=${f%.json}
    entry=$(python3 "$here/campaign-result.py" "$f" "$s" 2>/dev/null) || entry=""
    if [ -z "$entry" ] && grep -q overshoot "$d.err"; then
      if ! SEEDBOX_MARGIN=98304 "$here/batch.sh" "$world" "$prefix-retry" "$s" > /dev/null 2>&1; then
        echo "ABORTED batch setup failed for retry of seed $s" >> "$log"
        exit 1
      fi
      f="runs/$prefix-retry-s$s-r0.json"; d=${f%.json}
      entry=$(python3 "$here/campaign-result.py" "$f" "$s" 2>/dev/null) || entry=""
      [ -n "$entry" ] && entry="${entry%% *} (retry) ${entry#* }"
    fi
    if [ -z "$entry" ]; then
      host="?"
      why=$(tail -n 1 "$d.err" 2>/dev/null | cut -c1-200)
      res="UNRESOLVED ${why:-no valid completed result}"
      unresolved=$((unresolved + 1))
    else
      host="${entry%% *}"
      res="${entry#* }"
      [[ "$res" == *"RESULT fail"* ]] && found=1
    fi
    echo "seed=$s host_s=$host $res" >> "$log"
    i=$((i+1))
  done
  [ -z "$keep" ] && [ "$found" -eq 1 ] && { echo "FOUND" >> "$log"; break; }
  seed=$((seed + per))
done
echo "DONE unresolved=$unresolved" >> "$log"
[ "$unresolved" -eq 0 ]
