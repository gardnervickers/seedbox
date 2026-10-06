#!/usr/bin/env bash
# Usage: acceptance.sh portable|smoke|full|pin [options]   (see --help)
# Tiers and checks: issue #4 and the "Acceptance suite" section of sim/README.md.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
# Import this checkout's package regardless of the caller's working directory.
exec "${SEEDBOX_PYTHON:-python3}" -c \
  'import sys; sys.path.insert(0, sys.argv.pop(1)); from acceptance.__main__ import main; sys.exit(main())' \
  "$here" "$@"
