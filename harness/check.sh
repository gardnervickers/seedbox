#!/usr/bin/env bash
# Run every available native suite; use portable for checks without hardware.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 harness/check.py "$@"
