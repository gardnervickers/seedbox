#!/usr/bin/env bash
# EXPERIMENT ONLY (#60 equal-time ordering spike). Four runs on the AMD host:
# placements abc and cba, each a baseline and an exact replay (same seed).
# Run as the normal user on a free core whose SMT sibling is idle:
#   CPU=6 SHA=<exp branch head> bash run-order.sh
set -euo pipefail
CPU="${CPU:?set CPU to a free core (its SMT sibling is leased too)}"
SHA="${SHA:?set SHA to the exp/60-order-trace head}"
ACCEPTED_KERNEL="$HOME/seedbox-demo-20261006/artifacts/bzImage"
ACCEPTED_SHA=e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847
S="$HOME/seedbox-order60-$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS="$HOME/seedbox-order60-results"   # small; kept after cleanup
df -h "$HOME" /tmp
[ "$(df --output=avail "$HOME" | tail -1)" -ge $((10 * 1024 * 1024)) ] || { echo "need 10 GiB free in $HOME" >&2; exit 1; }
cleanup() { sudo rm -rf "$S"; echo "removed $S"; df -h "$HOME"; }
trap cleanup EXIT
rm -rf "$RESULTS"; mkdir -p "$S" "$RESULTS"
echo "${ACCEPTED_SHA}  ${ACCEPTED_KERNEL}" | sha256sum -c

git clone -q https://github.com/gardnervickers/seedbox "$S/seedbox"
git -C "$S/seedbox" fetch -q origin exp/60-order-trace
git -C "$S/seedbox" checkout -q "$SHA"
E="$S/seedbox/harness/experiments/order-60"
cd "$S/seedbox"
nix develop --command bash -euo pipefail -c '
  S="$1"; E="$2"
  cargo build-vmm -j 4
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -o "$S/order-probe" "$E/order-probe.c"
  bash guest/build-initramfs.sh "$S/order.cpio" "$E/order.init" "$S/order-probe:bin/order-probe"
' _ "$S" "$E"
sha256sum crates/seedbox-vmm/target/release/seedbox-vmm "$S/order.cpio" | tee "$RESULTS/inputs.sha256"
mkdir -p "$S/worlds"
for p in abc cba; do
  sed -e "s|\"KERNEL\"|\"$ACCEPTED_KERNEL\"|" -e "s|\"INITRD\"|\"$S/order.cpio\"|" "$E/order-$p.json" > "$S/worlds/order-$p.json"
  cp "$S/worlds/order-$p.json" "$RESULTS/"
done

for run in abc-1 abc-2 cba-1 cba-2; do
  p=${run%-*}
  echo "=== $run $(date -u +%T)"
  set +e
  CPU0="$CPU" SEEDBOX_ROOT="$S/seedbox" SEEDBOX_TIMEOUT=1800 \
    bash harness/batch.sh "$S/worlds/order-$p.json" "order-$run" "1" --digest
  echo "$run batch rc=$?" | tee -a "$RESULTS/runs.txt"
  set -e
  out="$S/seedbox/runs/order-$run-s1-r0"
  mkdir -p "$RESULTS/$run"
  sudo cat "$out.json" > "$RESULTS/$run.json" || true
  sudo cat "$out.status" >> "$RESULTS/runs.txt" || true
  sudo tail -50 "$out.err" > "$RESULTS/$run.err" || true
  sudo cat "$out/order-trace.jsonl" > "$RESULTS/$run/order-trace.jsonl" || true
  for m in a b c; do sudo cat "$out/$m.console" > "$RESULTS/$run/$m.console" 2>/dev/null || true; done
done
python3 "$E/analyze.py" "$RESULTS" | tee "$RESULTS/summary.txt"
du -sh "$RESULTS"
echo "results kept in $RESULTS; run state and builds are deleted on exit"
