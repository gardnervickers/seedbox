#!/usr/bin/env bash
# Build the generic OCI guest supervisor in sim's Linux development shell.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${SEEDBOX_STATIC_CC:?enter nix develop on Linux}"
"$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror -o "$1" "$here/compose-init.c"
