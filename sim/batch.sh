#!/usr/bin/env bash
# Compatibility arguments: WORLD OUTPUT_NAME "seed seed ..." [--digest].
# Results now use the common run manifests and runs/OUTPUT_NAME/report.json.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${SEEDBOX_ROOT:-$here/..}"
world=$1; name=$2; read -r -a seeds <<< "$3"; shift 3
extra=()
for arg in "$@"; do
  case "$arg" in --digest) extra+=(--full-state);; *) echo "unsupported argument: $arg" >&2; exit 2;; esac
done
cli=${SEEDBOX_CLI:-$here/seedbox/target/release/seedbox}
mkdir -p runs
seed_list=$(IFS=,; echo "${seeds[*]}")
cpu0=${CPU0:-2}
exec sudo -n "$cli" batch "$world" --seeds "$seed_list" --output "runs/$name" \
  --cpus "${SEEDBOX_CPUS:-$cpu0-$((cpu0 + ${#seeds[@]} - 1))}" \
  --vmm "${VMM:-$here/vmm/target/release/seedbox-vmm}" \
  --timeout "${SEEDBOX_TIMEOUT:-3600}" --margin "${SEEDBOX_MARGIN:-adaptive}" \
  --undo "${SEEDBOX_UNDO:-256}" "${extra[@]}"
