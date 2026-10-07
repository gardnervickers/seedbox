#!/usr/bin/env bash
# Portable PR/merge-train gate before the AMD host checks.
set -euo pipefail
if [[ $# -gt 1 || ${1:-} == -* ]]; then
  echo 'Usage: harness/mac-check.sh [REV|BRANCH]' >&2
  exit 2
fi
if [[ $(uname -s) != Darwin ]]; then
  echo 'mac-check requires macOS; run the Linux checks on the AMD host' >&2
  exit 2
fi
repo=$(git rev-parse --show-toplevel)
runner=$(cd "$(dirname "$0")" && pwd)/mac-check-steps.sh
source_dir=$repo
work=
cleanup() {
  local status=$?
  trap - EXIT
  if [[ -n $work ]]; then
    if [[ $status != 0 ]]; then
      echo "FAIL: worktree kept at $source_dir" >&2
      exit "$status"
    fi
    if [[ -e $source_dir/.git ]]; then
      if ! git -C "$repo" worktree remove --force "$source_dir"; then
        echo "failed to unregister temporary worktree: $source_dir" >&2
        echo "FAIL: worktree kept at $source_dir" >&2
        exit 1
      fi
    fi
    rm -rf "$work" || status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ $# -eq 1 ]]; then
  revision=$(git -C "$repo" rev-parse --verify --end-of-options "$1^{commit}")
  work=$(mktemp -d "${TMPDIR:-/tmp}/seedbox-mac-check.XXXXXX")
  source_dir=$work/src
  git -C "$repo" worktree add --detach --quiet "$source_dir" "$revision"
else
  revision=$(git -C "$repo" rev-parse HEAD)
  echo "mac-check checkout: $repo (including local changes)" >&2
fi
echo "mac-check revision: $revision"
# Use this checkout's runner even when checking a revision predating the gate.
# Run in the caller's project development shell.
(cd "$source_dir" && bash "$runner")
