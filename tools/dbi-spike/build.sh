#!/usr/bin/env bash
# Run from the checkout root inside nix develop. SDK and build stay outside Git.
set -euo pipefail
: "${SEEDBOX_STATIC_CC:?enter the Linux project nix develop shell first}"
: "${SEEDBOX_DBI_SDK:?set to an unpacked DynamoRIO 11.3.0-1 Linux SDK}"
dbi_build=${SEEDBOX_DBI_BUILD:-/tmp/seedbox-dbi-spike/build-native}
dbi_source=$(cd "$(dirname "$0")" && pwd)
dbi_tools=$(nix build --impure --no-link --print-out-paths --expr '
  let f = builtins.getFlake (toString ./.);
      p = import f.inputs.nixpkgs { system = "x86_64-linux"; };
  in p.buildEnv { name = "seedbox-dbi-tools";
                 paths = [ p.cmake p.ninja p.patchelf p.nodejs ]; }')
export PATH="$dbi_tools/bin:$PATH"
cmake -S "$dbi_source" -B "$dbi_build" \
  -DCMAKE_C_COMPILER="$(command -v cc)" \
  -DCMAKE_CXX_COMPILER="$(command -v c++)" \
  -DDynamoRIO_DIR="$SEEDBOX_DBI_SDK/cmake" -DCMAKE_BUILD_TYPE=Release
cmake --build "$dbi_build" -j4
"$SEEDBOX_STATIC_CC" -static -O2 "$dbi_source/workload.c" -o "$dbi_build/workload"
"$SEEDBOX_STATIC_CC" -static -O2 -pthread "$dbi_source/spinlock.c" -o "$dbi_build/spinlock"
readelf -l "$dbi_build/workload" > "$dbi_build/program-headers.txt"
if grep -q INTERP "$dbi_build/program-headers.txt"; then
  echo 'error: workload unexpectedly has a dynamic interpreter' >&2
  exit 1
fi
printf 'build: %s\nnode: %s\n' "$dbi_build" "$dbi_tools/bin/node"
