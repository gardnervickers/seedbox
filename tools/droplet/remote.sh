#!/usr/bin/env bash
# Droplet side of pv-run.sh: install tools once, then for each commit build it
# and, for each world and seed, run twice, diff the pair and replay the first
# run. Writes /root/results/summary.txt.
# Inputs come from the environment: SHAS, WORLDS, SEEDS, BUILD_KERNEL. Each
# commit is unpacked in /root/seedbox/<sha>.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
results=/root/results
mkdir -p "$results"

[ -c /dev/kvm ] || modprobe kvm_intel 2>/dev/null || modprobe kvm_amd 2>/dev/null || true
[ -c /dev/kvm ] || { echo "error: no /dev/kvm on this droplet (nested KVM unavailable)"; exit 2; }

start=$(date +%s)
apt-get -qq update
apt-get -qq install -y podman curl ca-certificates time >/dev/null
if ! command -v nix >/dev/null; then
  curl -fsSL https://install.determinate.systems/nix | sh -s -- install --no-confirm >/dev/null
fi
# shellcheck disable=SC1091
. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
{
  echo "cpu: $(grep -m1 "model name" /proc/cpuinfo | cut -d: -f2- | xargs) x$(nproc)"
  echo "setup_seconds: $(( $(date +%s) - start ))"
} > "$results/summary.txt"

podman system service --time=0 unix:///run/seedbox-podman.sock > "$results/podman-api.log" 2>&1 &
engine_pid=$!
trap 'kill "$engine_pid" 2>/dev/null || true' EXIT
export DOCKER_HOST=unix:///run/seedbox-podman.sock
ok=1
for sha in $SHAS; do
  short=${sha:0:12}
  mkdir -p "$results/$short"
  export WORLDS SEEDS BUILD_KERNEL sha out="$results/$short" summary="$results/summary.txt"
  # Use the shipped or locally built kernel; runtime does not download assets.
  export SEEDBOX_KERNEL=/root/seedbox-sim/artifacts/bzImage
  (cd "/root/seedbox/$sha" && nix develop --command bash -euo pipefail -c '
    start=$(date +%s)
    {
      echo
      echo "commit: $sha"
    } >> "$summary"
    if ! { if [ "$BUILD_KERNEL" = 1 ]; then bash kernel/build.sh; fi \
        && nix build .#runtimeArchive --out-link "$out/runtime-assets" \
        && SEEDBOX_BUILD_HELPERS="$out/runtime-assets" cargo build --release --locked -p seedbox && cargo build-vmm; } > "$out/build.log" 2>&1; then
      echo "  build: FAILED (see $out/build.log)" >> "$summary"
      exit 1
    fi
    echo "  build_seconds: $(( $(date +%s) - start ))" >> "$summary"
    sb="$PWD/target/release/seedbox"
    # The CLI embeds helpers built from this commit.
    rundir() { sed -n "s/^[A-Z_]*: //p" "$1" | head -n1; }
    status() { grep -m1 "^[A-Z_]*: " "$1" | cut -d: -f1 || echo "no status"; }
    field() { python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2]))" "$1/output/vmm.json" "$2" 2>/dev/null || echo "?"; }
    ok=1
    for world in $WORLDS; do
      slug=$(echo "${world%/}" | tr / -)
      echo "  world: $world (--timebase pv)" >> "$summary"
      for seed in $SEEDS; do
        p="$out/$slug-seed$seed"
        for n in a b; do
          set +e
          /usr/bin/time -f %e -o "$p-$n.wall" \
            "$sb" run "$world" --timebase pv --seed "$seed" > "$p-$n.out" 2>&1
          echo $? > "$p-$n.rc"
          set -e
        done
        a=$(rundir "$p-a.out"); b=$(rundir "$p-b.out")
        set +e
        /usr/bin/time -f %e -o "$p-replay.wall" "$sb" replay "$a" > "$p-replay.out" 2>&1
        replay_rc=$?
        "$sb" diff "$a" "$b" > "$p-diff.out" 2>&1
        diff_rc=$?
        set -e
        rc_a=$(cat "$p-a.rc"); rc_b=$(cat "$p-b.rc")
        [ "$rc_a" = 0 ] && [ "$rc_b" = 0 ] && [ "$replay_rc" = 0 ] && [ "$diff_rc" = 0 ] || ok=0
        for run in "$a" "$b"; do [ -n "$run" ] && cp "$run/output/vmm.json" "$out/$(basename "$run").vmm.json" 2>/dev/null || true; done
        {
          echo "    seed $seed:"
          echo "      run a: $(status "$p-a.out") exit $rc_a, wall $(cat "$p-a.wall")s, vmm host $(field "$a" host_seconds)s"
          echo "      run b: $(status "$p-b.out") exit $rc_b, wall $(cat "$p-b.wall")s, vmm host $(field "$b" host_seconds)s"
          echo "      witness a: $(field "$a" world_witness)"
          echo "      witness b: $(field "$b" world_witness)"
          echo "      diff a b: $(status "$p-diff.out") exit $diff_rc"
          echo "      replay a: $(grep -m1 "^replay:" "$p-replay.out" || status "$p-replay.out") exit $replay_rc, wall $(cat "$p-replay.wall")s"
        } >> "$summary"
      done
    done
    [ "$ok" = 1 ]
  ') || ok=0
done
echo "total_seconds: $(( $(date +%s) - start ))" >> "$results/summary.txt"
if [ "$ok" = 1 ]; then echo "result: OK" >> "$results/summary.txt"; else echo "result: FAILED" >> "$results/summary.txt"; exit 1; fi
