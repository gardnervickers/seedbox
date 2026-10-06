#!/usr/bin/env bash
# Compatibility arguments: WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]].
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${SEEDBOX_ROOT:-$here/..}"
extra=()
if test -n "${6:-}"; then extra+=(--keep-going); fi
cli=${SEEDBOX_CLI:-$here/seedbox/target/release/seedbox}
cpu0=${CPU0:-2}; count=${BATCH:-6}
mkdir -p runs
exec sudo -n "$cli" campaign "$1" --output "runs/$2" --first-seed "$3" \
  --seconds "$(($4 * 3600))" --max-seeds "${5:-1000000}" \
  --cpus "${SEEDBOX_CPUS:-$cpu0-$((cpu0 + count - 1))}" \
  --vmm "${VMM:-$here/vmm/target/release/seedbox-vmm}" \
  --timeout "${SEEDBOX_TIMEOUT:-3600}" --margin "${SEEDBOX_MARGIN:-adaptive}" \
  --undo "${SEEDBOX_UNDO:-256}" "${extra[@]}"
