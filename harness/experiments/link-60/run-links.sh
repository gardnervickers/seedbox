#!/usr/bin/env bash
# EXPERIMENT ONLY (#60 link-policy controls, PR #97). Five runs on the AMD host:
#   L1 shaped world, L2 its replay, L3 the same with undo off (SEEDBOX_UNDO=0),
#   P1 plain world on the PR engine, M1 the same plain world on main's engine.
# Run as the normal user on a free core whose SMT sibling is idle:
#   CPU=15 SHA=<exp/60-link-controls head> MAIN=<main sha> bash run-links.sh
set -euo pipefail
CPU="${CPU:?set CPU to a free core (its SMT sibling is leased too)}"
SHA="${SHA:?set SHA to the exp/60-link-controls head}"
MAIN="${MAIN:?set MAIN to the main commit PR #97 is based on}"
CHECKOUT="$HOME/src/github.com/gardnervickers/seedbox"
ACCEPTED_KERNEL="$HOME/seedbox-demo-20261006/artifacts/bzImage"
ACCEPTED_SHA=e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847
S="$HOME/seedbox-link60-$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS="$HOME/seedbox-link60-results"   # small; kept after cleanup
df -h "$HOME" /tmp
[ "$(df --output=avail "$HOME" | tail -1)" -ge $((15 * 1024 * 1024)) ] || { echo "need 15 GiB free in $HOME" >&2; exit 1; }
cleanup() { sudo rm -rf "$S"; echo "removed $S"; df -h "$HOME"; }
trap cleanup EXIT
rm -rf "$RESULTS"; mkdir -p "$S" "$RESULTS"
echo "${ACCEPTED_SHA}  ${ACCEPTED_KERNEL}" | sha256sum -c

# Sources: the host checkout fetches over SSH; clones are local.
git -C "$CHECKOUT" fetch -q origin exp/60-link-controls main
for w in pr main; do git clone -q "$CHECKOUT" "$S/$w"; done
git -C "$S/pr" checkout -q "$SHA"
git -C "$S/main" checkout -q "$MAIN"
test "$(git -C "$S/pr" rev-parse HEAD)" = "$SHA"
test "$(git -C "$S/main" rev-parse HEAD)" = "$MAIN"
E="$S/pr/harness/experiments/link-60"

cd "$S/pr"
nix develop --command bash -euo pipefail -c '
  S="$1"; E="$2"
  (cd "$S/pr" && cargo build-vmm -j 4)
  (cd "$S/main" && cargo build-vmm -j 4)
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -o "$S/link-probe" "$E/link-probe.c"
  bash "$S/pr/guest/build-initramfs.sh" "$S/link.cpio" "$E/link.init" "$S/link-probe:bin/link-probe"
' _ "$S" "$E"
sha256sum "$S/pr/crates/seedbox-vmm/target/release/seedbox-vmm" "$S/main/crates/seedbox-vmm/target/release/seedbox-vmm" "$S/link.cpio" | tee "$RESULTS/inputs.sha256"
mkdir -p "$S/worlds"
for w in shaped plain; do
  sed -e "s|\"KERNEL\"|\"$ACCEPTED_KERNEL\"|" -e "s|\"INITRD\"|\"$S/link.cpio\"|" "$E/$w.json" > "$S/worlds/$w.json"
  cp "$S/worlds/$w.json" "$RESULTS/"
done

run() { # name world root [env...]
  local name=$1 world=$2 root=$3; shift 3
  echo "=== $name $(date -u +%T) world=$world root=$root $*"
  set +e
  env "$@" CPU0="$CPU" SEEDBOX_ROOT="$root" SEEDBOX_TIMEOUT=1800 \
    bash "$S/pr/harness/batch.sh" "$S/worlds/$world.json" "link-$name" "1" --digest
  echo "$name batch rc=$?" | tee -a "$RESULTS/runs.txt"
  set -e
  local out="$root/runs/link-$name-s1-r0"
  mkdir -p "$RESULTS/$name"
  sudo cat "$out.json" > "$RESULTS/$name.json" || true
  sudo cat "$out.status" >> "$RESULTS/runs.txt" || true
  sudo tail -50 "$out.err" > "$RESULTS/$name.err" || true
  for m in a b c; do sudo cat "$out/$m.console" > "$RESULTS/$name/$m.console" 2>/dev/null || true; done
}
run L1 shaped "$S/pr"
run L2 shaped "$S/pr"
run L3 shaped "$S/pr" SEEDBOX_UNDO=0
run P1 plain "$S/pr"
run M1 plain "$S/main"
python3 "$E/analyze.py" "$RESULTS" | tee "$RESULTS/summary.txt"
du -sh "$RESULTS"
echo "results kept in $RESULTS; run state and builds are deleted on exit"
