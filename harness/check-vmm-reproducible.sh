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
first="$scratch/first"
second="$scratch/a longer checkout with spaces/second"
for checkout in "$first" "$second"; do
  mkdir -p "$checkout"
  git archive "$revision" | tar -x -C "$checkout"
  (cd "$checkout" && cargo build-vmm)
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
  echo 'error: VMM binaries differ; differing embedded paths follow' >&2
  # Include paths within RPATH lists and debug strings, not just leading '/'.
  for name in first second; do
    LC_ALL=C grep -F / "$scratch/$name.strings" | LC_ALL=C sort -u > "$scratch/$name.paths" || true
  done
  diff -u "$scratch/first.paths" "$scratch/second.paths" >&2 || true
  status=1
fi
# Equality alone could hide a shared inherited Nix shell checkout path.
for name in first second; do
  if grep -F -e "$scratch" -e "$source_root" "$scratch/$name.strings" >&2; then
    echo "error: $name binary retains an absolute checkout path" >&2
    status=1
  fi
done
if [[ $status == 0 ]]; then echo 'reproducible: byte-identical VMM binaries, no checkout paths'; fi
exit "$status"
