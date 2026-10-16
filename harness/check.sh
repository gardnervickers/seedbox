#!/usr/bin/env bash
# The required pinned dev shell provisions this tool before checker invocation.
set -euo pipefail
export CARGO_INCREMENTAL=0
script=${BASH_SOURCE[0]}
[[ $script == */* ]] || script=./$script
cd -- "${script%/*}/.."
if [[ ${SEEDBOX_CHECK:-} != /* || ! -f ${SEEDBOX_CHECK:-} || ! -x ${SEEDBOX_CHECK:-} ]]; then
  printf "%s\n" "check: Rust coordinator unavailable; enter the pinned environment with 'nix develop', then rerun 'bash harness/check.sh' (this wrapper never builds or downloads tools)" >&2
  exit 2
fi
exec "$SEEDBOX_CHECK" "$@"
