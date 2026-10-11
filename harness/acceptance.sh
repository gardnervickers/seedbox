#!/usr/bin/env bash
# Compatibility entrypoint for portable Rust checks.
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "$#" -eq 0 ]; then set -- portable; fi
exec cargo xtask check "$@"
