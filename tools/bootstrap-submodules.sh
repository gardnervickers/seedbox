#!/usr/bin/env bash
# Initialize missing asset source checkouts without updating existing work.
set -euo pipefail

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

root=$(git -C "${1:-.}" rev-parse --show-toplevel) ||
  fail 'cannot find Seedbox checkout; run this helper from its Git worktree'
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

initialize_missing() {
  local repo=$1 listing entry metadata path child prefix
  listing=$(mktemp "$scratch/index.XXXXXX")
  git -C "$repo" ls-files --stage -z > "$listing" ||
    fail "cannot read submodule gitlinks in $repo; repair its Git index and rerun asset preparation"
  while IFS= read -r -d '' entry; do
    case "$entry" in
      '160000 '*) ;;
      *) continue ;;
    esac
    metadata=${entry%%$'\t'*}
    path=${entry#*$'\t'}
    child=$repo/$path
    [[ ${metadata##* } == 0 ]] ||
      fail "unmerged submodule $child; resolve its gitlink conflict and rerun asset preparation"
    if [[ ! -e "$child/.git" && ! -L "$child/.git" ]]; then
      # No --recursive: that would also reset initialized descendants. Git
      # resolves the URL and checks out this one recorded index gitlink.
      git -C "$repo" submodule update --init --checkout -- "$path" >&2 ||
        fail "cannot initialize submodule $child; check its .gitmodules URL and access, then rerun bash tools/bootstrap-submodules.sh from the Seedbox checkout"
    fi
    # A directory alone is not an initialized checkout. Reject broken Git
    # pointers rather than letting Git silently discover the parent repository.
    [[ -e "$child/.git" ]] ||
      fail "invalid submodule checkout $child; repair its .git pointer and rerun asset preparation"
    prefix=$(git -C "$child" rev-parse --show-prefix) ||
      fail "invalid submodule checkout $child; repair its Git repository and rerun asset preparation"
    [[ -z "$prefix" ]] && git -C "$child" rev-parse --verify HEAD >/dev/null ||
      fail "invalid submodule checkout $child; restore a valid local HEAD and rerun asset preparation"
    initialize_missing "$child"
  done < "$listing"
}

initialize_missing "$root"
