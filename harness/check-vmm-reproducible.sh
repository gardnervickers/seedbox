#!/usr/bin/env bash
# Run in the project dev shell on Linux; no KVM/root needed. Only committed
# inputs are checked. Both builds use the same toolchain and fresh target dirs.
set -euo pipefail
if [[ $# -gt 1 ]]; then
  echo 'usage: bash harness/check-vmm-reproducible.sh [REV]' >&2
  exit 2
fi
if [[ $(uname -s) != Linux ]]; then
  echo 'error: VMM reproducibility requires Linux; run in the project dev shell on the Linux host' >&2
  exit 2
fi
cd "$(dirname "$0")/.."
source_root=$(pwd -P)
revision=$(git rev-parse --verify "${1:-HEAD}^{commit}")
scratch=$(mktemp -d "${TMPDIR:-/tmp}/seedbox-vmm-repro.XXXXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "revision: $revision"
sysroot=$(rustc --print sysroot)
first="$scratch/first"
second="$scratch/a longer checkout with spaces/second"
for checkout in "$first" "$second"; do
  mkdir -p "$checkout"
  git archive "$revision" | tar -x -C "$checkout"
  (cd "$checkout" && cargo build-vmm -v 2>&1 | tee "$checkout/build.log" >&2)
done
binary=crates/seedbox-vmm/target/release/seedbox-vmm
first_hash=$(sha256sum "$first/$binary")
second_hash=$(sha256sum "$second/$binary")
echo "first sha256: ${first_hash%% *}"
echo "second sha256: ${second_hash%% *}"
strings "$first/$binary" > "$scratch/first.strings"
strings "$second/$binary" > "$scratch/second.strings"
status=0
if [[ ${first_hash%% *} != "${second_hash%% *}" ]]; then
  echo 'error: VMM binaries differ; per-crate -C metadata follows' >&2
  for checkout in "$first" "$second"; do
    echo "checkout: $checkout" >&2
    sed -nE 's/.*--crate-name ([^ ]+).* -C metadata=([[:xdigit:]]+).*/\1 metadata=\2/p' "$checkout/build.log" >&2
  done
  echo 'error: VMM binaries differ; differing embedded paths follow' >&2
  # Include paths within RPATH lists and debug strings, not just leading '/'.
  for name in first second; do
    LC_ALL=C grep -F / "$scratch/$name.strings" | LC_ALL=C sort -u > "$scratch/$name.paths" || true
  done
  diff -u "$scratch/first.paths" "$scratch/second.paths" >&2 || true
  status=1
fi
# Equality alone could hide shared checkout, registry or toolchain paths.
leak_patterns=(-e "$scratch" -e "$source_root" -e "$sysroot")
for path in "${HOME:-}" "${CARGO_HOME:-${HOME:-}/.cargo}"; do
  # HOME=/ would match every absolute path, including retained Nix libraries.
  if [[ -n "$path" && "$path" != / ]]; then
    leak_patterns+=(-e "$path")
    if [[ -d "$path" ]]; then
      leak_patterns+=(-e "$(cd "$path" && pwd -P)")
    fi
  fi
done
if [[ -d "$sysroot" ]]; then leak_patterns+=(-e "$(cd "$sysroot" && pwd -P)"); fi
for name in first second; do
  if grep -F "${leak_patterns[@]}" "$scratch/$name.strings" >&2; then
    echo "error: $name binary retains an absolute build path" >&2
    status=1
  fi
done
if [[ $status == 0 ]]; then echo 'reproducible: byte-identical VMM binaries, no local build paths'; fi
exit "$status"
