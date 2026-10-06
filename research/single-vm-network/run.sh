#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
exec sudo -n env RESULTS_DIR="${RESULTS_DIR:-results}" SEED="${SEED:-1009}" "$(command -v python3)" research/single-vm-network/run_guard.py "$@"
