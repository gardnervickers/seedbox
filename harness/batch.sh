#!/usr/bin/env bash
# Usage: batch.sh WORLD OUTPREFIX "seed seed ..." [extra VMM args]
# Env: VMM, SEEDBOX_ROOT, SEEDBOX_TIMEOUT (seconds). SEEDBOX_ROOT
# defaults to this checkout; batch.py owns the other defaults.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${SEEDBOX_ROOT:-$here/..}"
# sudo drops the environment, so forward the nonempty settings the caller gave.
forward=(SEEDBOX_ROOT="$PWD")
for name in VMM SEEDBOX_TIMEOUT; do
  if [ -n "${!name:-}" ]; then forward+=("$name=${!name}"); fi
done
# The privileged supervisor owns/reaps every VMM and its descendants.
# sudo is only its outer launcher.
exec sudo -n env "${forward[@]}" "${SEEDBOX_PYTHON:-python3}" "$here/batch.py" "$@"
