#!/usr/bin/env bash
# Compatibility entrypoint for the complete native build.
set -euo pipefail
cd "$(dirname "$0")/../.."
exec python3 tools/build.py "$@"
