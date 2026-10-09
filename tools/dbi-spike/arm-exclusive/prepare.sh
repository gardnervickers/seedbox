#!/usr/bin/env bash
# Run on Linux ARM64 inside the project nix develop shell.
set -euo pipefail
: "${SEEDBOX_STATIC_CC:?enter the project Linux ARM64 nix develop shell}"
if [ "$#" -ne 1 ]; then
    echo 'usage: bash tools/dbi-spike/arm-exclusive/prepare.sh OUTPUT_DIRECTORY' >&2
    exit 2
fi
source_dir=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
"$SEEDBOX_STATIC_CC" -static -O2 -Wall -Wextra -Werror \
    "$source_dir/retry.c" -o "$1/retry"
cp "$source_dir/Dockerfile" "$source_dir/compose.yaml" "$1/"
printf 'context: %s\n' "$1"
