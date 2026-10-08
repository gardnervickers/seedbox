#!/usr/bin/env bash
# Droplet side of pv-run.sh: install tools, build, run each seed twice,
# diff the pair, replay the first run and write /root/results/summary.txt.
# Inputs come from the environment: WORLD, SEEDS, BUILD_KERNEL, SHA.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
results=/root/results
mkdir -p "$results"

[ -c /dev/kvm ] || modprobe kvm_intel 2>/dev/null || modprobe kvm_amd 2>/dev/null || true
[ -c /dev/kvm ] || { echo "error: no /dev/kvm on this droplet (nested KVM unavailable)"; exit 2; }

apt-get -qq update
apt-get -qq install -y podman curl ca-certificates time >/dev/null
if ! command -v nix >/dev/null; then
  curl -fsSL https://install.determinate.systems/nix | sh -s -- install --no-confirm >/dev/null
fi
# shellcheck disable=SC1091
. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh

cd /root/seedbox
export WORLD SEEDS BUILD_KERNEL SHA results
nix develop --command bash -euo pipefail -c '
  start=$(date +%s)
  if [ "$BUILD_KERNEL" = 1 ]; then bash kernel/build.sh; fi
  cargo build --release --locked -p seedbox
  cargo build-vmm
  echo "build_seconds: $(( $(date +%s) - start ))" > "$results/build.txt"
  sb="$PWD/target/release/seedbox"
  rundir() { sed -n "s/^[A-Z_]*: //p" "$1" | head -n1; }
  status() { grep -m1 "^[A-Z_]*: " "$1" | cut -d: -f1 || echo "no status"; }
  field() { python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2]))" "$1/output/vmm.json" "$2" 2>/dev/null || echo "?"; }
  ok=1
  {
    echo "commit: $SHA"
    echo "world: $WORLD (--timebase pv)"
    echo "cpu: $(grep -m1 "model name" /proc/cpuinfo | cut -d: -f2- | xargs) x$(nproc)"
    cat "$results/build.txt"
  } > "$results/summary.txt"
  for seed in $SEEDS; do
    for n in a b; do
      set +e
      /usr/bin/time -f %e -o "$results/seed$seed-$n.wall" \
        "$sb" run "$WORLD" --timebase pv --seed "$seed" > "$results/seed$seed-$n.out" 2>&1
      echo $? > "$results/seed$seed-$n.rc"
      set -e
    done
    a=$(rundir "$results/seed$seed-a.out"); b=$(rundir "$results/seed$seed-b.out")
    set +e
    /usr/bin/time -f %e -o "$results/seed$seed-replay.wall" \
      "$sb" replay "$a" > "$results/seed$seed-replay.out" 2>&1
    replay_rc=$?
    "$sb" diff "$a" "$b" > "$results/seed$seed-diff.out" 2>&1
    diff_rc=$?
    set -e
    rc_a=$(cat "$results/seed$seed-a.rc"); rc_b=$(cat "$results/seed$seed-b.rc")
    [ "$rc_a" = 0 ] && [ "$rc_b" = 0 ] && [ "$replay_rc" = 0 ] && [ "$diff_rc" = 0 ] || ok=0
    for run in "$a" "$b"; do [ -n "$run" ] && cp "$run/output/vmm.json" "$results/$(basename "$run").vmm.json" 2>/dev/null || true; done
    {
      echo "seed $seed:"
      echo "  run a: $(status "$results/seed$seed-a.out") exit $rc_a, wall $(cat "$results/seed$seed-a.wall")s, vmm host $(field "$a" host_seconds)s"
      echo "  run b: $(status "$results/seed$seed-b.out") exit $rc_b, wall $(cat "$results/seed$seed-b.wall")s, vmm host $(field "$b" host_seconds)s"
      echo "  witness a: $(field "$a" world_witness)"
      echo "  witness b: $(field "$b" world_witness)"
      echo "  diff a b: $(status "$results/seed$seed-diff.out") exit $diff_rc"
      echo "  replay a: $(grep -m1 "^replay:" "$results/seed$seed-replay.out" || status "$results/seed$seed-replay.out") exit $replay_rc, wall $(cat "$results/seed$seed-replay.wall")s"
    } >> "$results/summary.txt"
  done
  if [ "$ok" = 1 ]; then echo "result: OK" >> "$results/summary.txt"; else echo "result: FAILED" >> "$results/summary.txt"; exit 1; fi
'
