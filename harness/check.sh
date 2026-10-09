#!/usr/bin/env bash
# Run the named platform suites (default portable); see --help for the list.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 harness/check.py "$@"
