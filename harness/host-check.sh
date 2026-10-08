#!/usr/bin/env bash
# Change-scoped host checks; --plan is portable and never touches the host.
set -euo pipefail
exec python3 "$(dirname "$0")/host_check.py" "$@"
