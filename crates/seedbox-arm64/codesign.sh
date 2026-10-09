#!/usr/bin/env bash
# Sign the exact CLI or discovered native test binary that will run.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
if [ "$#" -eq 0 ]; then set -- "$here/target/release/seedbox-arm64"; fi
for binary in "$@"; do
  test -f "$binary" || { echo "binary not found: $binary" >&2; exit 2; }
  /usr/bin/codesign --force --sign - --entitlements "$here/entitlements.plist" "$binary"
  /usr/bin/codesign --verify --strict "$binary"
done
