#!/usr/bin/env bash
# Run inside nix develop ./sim. Hardware options are seedbox acceptance options.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
cd "$here/.."
tier=${1:-portable}; shift "$(( $# > 0 ? 1 : 0 ))"
case "$tier" in portable|smoke|full) ;; *) echo 'expected portable, smoke or full' >&2; exit 2;; esac
while IFS= read -r -d '' script; do bash -n "$script"; done < <(find sim -name target -prune -o -name '*.sh' -print0)
python3 -m unittest discover -s sim/tests -p test_msr54.py
cargo test --locked --manifest-path sim/seedbox/Cargo.toml
if test "$(uname -s)" = Linux; then cargo test --release --manifest-path sim/vmm/Cargo.toml; fi
if test "$tier" = portable; then exit 0; fi
cargo build --release --locked --manifest-path sim/seedbox/Cargo.toml
exec sudo -n sim/seedbox/target/release/seedbox acceptance "$tier" --sim sim "$@"
