#!/usr/bin/env bash
# Compatibility entry for the portable stage; seedbox-check owns its policy.
# Public CLI exits: run 0/1/2/3/124; replay and diff 0/1/2.
set -euo pipefail
if [[ ${1:-portable} != portable || $# -gt 1 ]]; then
  printf "%s\n" 'Usage: harness/acceptance.sh portable; see README.md for seedbox run commands' >&2
  exit 2
fi
script=${BASH_SOURCE[0]}
[[ $script == */* ]] || script=./$script
exec "$BASH" "${script%/*}/check.sh" --portable-stage "${1:-portable}"
