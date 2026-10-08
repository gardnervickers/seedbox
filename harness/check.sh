#!/usr/bin/env bash
# Run every available native suite; use portable for checks without hardware.
set -euo pipefail
cd "$(dirname "$0")/.."
printf 'revision: %s\n' "$(git rev-parse HEAD)"
if [[ -n $(git --no-optional-locks status --porcelain) ]]; then
  echo 'checkout: modified'
else
  echo 'checkout: clean'
fi
exec python3 harness/check.py "$@"
