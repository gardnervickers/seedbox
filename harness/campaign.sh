#!/usr/bin/env bash
# Usage: campaign.sh WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]]
# Runs seeds in batches of 14 until a consistency failure is found or the
# time budget is spent. PMU overshoot failures are retried, in one parallel
# batch per batch, with a larger margin.
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
  # First pass: accept completed attempts; collect PMU overshoots for one
  # parallel retry batch (each on its own core, fresh output).
  list=($seeds); entries=(); errs=(); retry=(); retry_at=()
  for i in "${!list[@]}"; do
    s=${list[$i]}
    f="runs/$prefix-s$s-r$i.json"; d=${f%.json}
    entry=$(python3 "$here/campaign-result.py" "$f" "$s" 2>/dev/null) || entry=""
    entries[$i]=$entry; errs[$i]="$d.err"
    if [ -z "$entry" ] && grep -q overshoot "$d.err"; then
      retry+=("$s"); retry_at+=("$i")
    fi
  done
  if [ "${#retry[@]}" -gt 0 ]; then
    if ! SEEDBOX_MARGIN=98304 "$here/batch.sh" "$world" "$prefix-retry" "${retry[*]}" > /dev/null 2>&1; then
      echo "ABORTED batch setup failed for retry of seeds ${retry[*]}" >> "$log"
      exit 1
    fi
    for j in "${!retry[@]}"; do
      s=${retry[$j]}; i=${retry_at[$j]}
      f="runs/$prefix-retry-s$s-r$j.json"; d=${f%.json}
      entry=$(python3 "$here/campaign-result.py" "$f" "$s" 2>/dev/null) || entry=""
      [ -n "$entry" ] && entry="${entry%% *} (retry) ${entry#* }"
      entries[$i]=$entry; errs[$i]="$d.err"
    done
  fi
  for i in "${!list[@]}"; do
    s=${list[$i]}; entry=${entries[$i]}
    if [ -z "$entry" ]; then
      host="?"
      why=$(tail -n 1 "${errs[$i]}" 2>/dev/null | cut -c1-200)
      res="UNRESOLVED ${why:-no valid completed result}"
      unresolved=$((unresolved + 1))
    else
      host="${entry%% *}"
      res="${entry#* }"
      [[ "$res" == *"RESULT fail"* ]] && found=1
    fi
    echo "seed=$s host_s=$host $res" >> "$log"
  done
  [ -z "$keep" ] && [ "$found" -eq 1 ] && { echo "FOUND" >> "$log"; break; }
  seed=$((seed + per))
done
echo "DONE unresolved=$unresolved" >> "$log"
[ "$unresolved" -eq 0 ]
