#!/usr/bin/env bash
# Usage: batch.sh WORLD OUTPREFIX "seed seed ..." [extra VMM args]
# Env: CPU0, VMM, SEEDBOX_ROOT, SEEDBOX_MARGIN, SEEDBOX_TIMEOUT (seconds).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "${SEEDBOX_ROOT:-$HOME/seedbox-sim}"
# The privileged supervisor owns/reaps the actual VMM, holds CPU+SMT locks,
# and verifies restoration before returning. sudo is only its outer launcher.
exec sudo -n env SEEDBOX_ROOT="$PWD" CPU0="${CPU0:-2}" \
 VMM="${VMM:-./target/release/seedbox-vmm}" \
 SEEDBOX_MARGIN="${SEEDBOX_MARGIN:-adaptive}" SEEDBOX_UNDO="${SEEDBOX_UNDO:-256}" \
 SEEDBOX_TIMEOUT="${SEEDBOX_TIMEOUT:-3600}" \
 "${SEEDBOX_PYTHON:-python3}" "$here/batch.py" "$@"
