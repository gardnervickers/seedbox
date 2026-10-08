#!/usr/bin/env bash
# Run every available native suite; use portable for checks without hardware.
set -euo pipefail
exec python3 "$(dirname "$0")/check.py" "$@"
