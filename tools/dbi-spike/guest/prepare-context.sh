#!/usr/bin/env bash
# Construct only an image build context; never executes application containers.
set -euo pipefail
: "${SEEDBOX_DBI_SDK:?set to the unpacked SDK}"
: "${SEEDBOX_DBI_BUILD:?set to the built experiment directory}"
destination=${1:?provide an empty destination directory}
mkdir -p "$destination"
if [ -n "$(ls -A "$destination")" ]; then
  echo 'error: context destination must be empty' >&2
  exit 1
fi
source_dir=$(cd "$(dirname "$0")" && pwd)
cp "$source_dir/Dockerfile" "$source_dir/"*.yaml "$destination/"
cp "$source_dir/../workload.js" "$SEEDBOX_DBI_BUILD/libbudget.so" \
   "$SEEDBOX_DBI_BUILD/workload" "$SEEDBOX_DBI_BUILD/spinlock" "$destination/"
mkdir -p "$destination/sdk/bin64" "$destination/sdk/lib64/release" \
         "$destination/sdk/ext/lib64/release"
cp "$SEEDBOX_DBI_SDK/bin64/drrun" "$destination/sdk/bin64/"
cp "$SEEDBOX_DBI_SDK/lib64/release/"*.so "$destination/sdk/lib64/release/"
cp "$SEEDBOX_DBI_SDK/ext/lib64/release/libdrmgr.so" \
   "$SEEDBOX_DBI_SDK/ext/lib64/release/libdrreg.so" "$destination/sdk/ext/lib64/release/"
