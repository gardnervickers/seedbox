#!/usr/bin/env bash
# Run PV worlds for one or more commits on a throwaway DigitalOcean droplet,
# then delete the droplet.
#
# Runs from any machine with doctl (authenticated), ssh and a seedbox checkout.
# Setup and each build cost minutes before the first run, so batch: pass every
# queued commit, world and seed to one invocation. For each commit, world and
# seed: run twice, check the two world witnesses with `seedbox diff`, and replay
# the first run. Results land in --out; exit 0 only if every build succeeded,
# every run passed, every diff matched and every replay matched.
set -euo pipefail

usage() {
  cat <<'EOF'
usage: tools/droplet/pv-run.sh [options]

  --ref REF          commit to test, shipped from this checkout; repeat for more [default: HEAD]
  --world PATH       Compose file or directory, relative to the repo; repeat for more [default: examples/redis]
  --seeds "N ..."    seeds to run [default: 1]
  --kernel PATH      guest bzImage to ship [default: $SEEDBOX_KERNEL, then ~/seedbox-sim/artifacts/bzImage]
  --build-kernel     build the guest kernel on the droplet for each commit (needed when a REF changes kernel/)
  --size SLUG        droplet size [default: s-4vcpu-8gb-240gb-intel]
  --region SLUG      droplet region [default: nyc3]
  --context NAME     doctl auth context [default: $SEEDBOX_DO_CONTEXT, else doctl's current one]
  --ssh-keys IDS     comma-separated DO SSH key IDs [default: $SEEDBOX_DO_SSH_KEYS, else every key on the account]
  --identity FILE    private key for ssh [default: ssh's own choice]
  --out DIR          results directory [default: target/droplet-pv/<time>]
  --keep             leave the droplet running (prints how to delete it)
EOF
}

refs=()
worlds=()
seeds=1
kernel=""
build_kernel=0
size=s-4vcpu-8gb-240gb-intel
region=nyc3
context="${SEEDBOX_DO_CONTEXT:-}"
ssh_keys="${SEEDBOX_DO_SSH_KEYS:-}"
identity=""
out=""
keep=0
while [ $# -gt 0 ]; do
  case "$1" in
    --ref) refs+=("$2"); shift 2 ;;
    --world) worlds+=("$2"); shift 2 ;;
    --seeds) seeds="$2"; shift 2 ;;
    --kernel) kernel="$2"; shift 2 ;;
    --build-kernel) build_kernel=1; shift ;;
    --size) size="$2"; shift 2 ;;
    --region) region="$2"; shift 2 ;;
    --context) context="$2"; shift 2 ;;
    --ssh-keys) ssh_keys="$2"; shift 2 ;;
    --identity) identity="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --keep) keep=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option $1" >&2; usage >&2; exit 2 ;;
  esac
done

for tool in doctl ssh scp git tar; do
  command -v "$tool" >/dev/null || { echo "$tool not found; install it" >&2; exit 2; }
done
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(git -C "$here" rev-parse --show-toplevel)"
[ ${#refs[@]} -gt 0 ] || refs=(HEAD)
[ ${#worlds[@]} -gt 0 ] || worlds=(examples/redis)
shas=()
for ref in "${refs[@]}"; do
  sha="$(git -C "$repo" rev-parse --verify "$ref^{commit}")" \
    || { echo "--ref $ref: not a commit in this checkout" >&2; exit 2; }
  case " ${shas[*]} " in *" $sha "*) continue ;; esac
  shas+=("$sha")
  for world in "${worlds[@]}"; do
    case "$world" in *[[:space:]]*) echo "--world $world: paths with spaces are not supported" >&2; exit 2 ;; esac
    git -C "$repo" cat-file -e "$sha:${world%/}" 2>/dev/null \
      || { echo "--world $world: not in $ref ($sha)" >&2; exit 2; }
  done
  if [ "$build_kernel" = 0 ] && ! git -C "$repo" diff --quiet "$sha" origin/main -- kernel/ 2>/dev/null; then
    echo "warning: $ref changes kernel/ relative to origin/main; consider --build-kernel" >&2
  fi
done
if [ "$build_kernel" = 0 ]; then
  kernel="${kernel:-${SEEDBOX_KERNEL:-$HOME/seedbox-sim/artifacts/bzImage}}"
  [ -f "$kernel" ] || { echo "--kernel $kernel: not found; pass --kernel or --build-kernel" >&2; exit 2; }
fi
out="${out:-$repo/target/droplet-pv/$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$out"

do_() { doctl ${context:+--context "$context"} "$@"; }
if [ -z "$ssh_keys" ]; then
  ssh_keys="$(do_ compute ssh-key list --format ID --no-header | paste -sd, -)"
  [ -n "$ssh_keys" ] || { echo "no SSH keys on the DigitalOcean account; add one or pass --ssh-keys" >&2; exit 2; }
fi
ssh_opts=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
  -o ConnectTimeout=10 -o BatchMode=yes -o ServerAliveInterval=30)
[ -n "$identity" ] && ssh_opts+=(-i "$identity")

name="seedbox-pv-${shas[0]:0:8}-$(date -u +%H%M%S)"
started=$(date +%s)
echo "creating droplet $name ($size, $region)" >&2
id="$(do_ compute droplet create "$name" --size "$size" --region "$region" \
  --image ubuntu-24-04-x64 --ssh-keys "$ssh_keys" --tag-name seedbox-pv \
  --wait --format ID --no-header)"
gone() {
  # DigitalOcean keeps listing a deleted droplet for a few seconds.
  for _ in $(seq 30); do
    do_ compute droplet get "$id" >/dev/null 2>&1 || return 0
    sleep 5
  done
  return 1
}
cleanup() {
  status=$?
  minutes=$(( ($(date +%s) - started + 59) / 60 ))
  if [ "$keep" = 1 ]; then
    echo "droplet $id kept; delete it with: doctl ${context:+--context $context }compute droplet delete -f $id" >&2
  elif do_ compute droplet delete -f "$id" >/dev/null && gone; then
    echo "droplet $id deleted after about $minutes min" >&2
  else
    echo "error: could not confirm droplet $id was deleted; check: doctl compute droplet list --tag-name seedbox-pv" >&2
    status=1
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
ip="$(do_ compute droplet get "$id" --format PublicIPv4 --no-header)"
echo "droplet $id at $ip" >&2

for _ in $(seq 60); do
  ssh "${ssh_opts[@]}" "root@$ip" true 2>/dev/null && break
  sleep 5
done
ssh "${ssh_opts[@]}" "root@$ip" true

for sha in "${shas[@]}"; do
  echo "shipping $sha" >&2
  git -C "$repo" archive --format=tar "$sha" \
    | ssh "${ssh_opts[@]}" "root@$ip" "mkdir -p /root/seedbox/$sha && tar -x -C /root/seedbox/$sha"
done
scp "${ssh_opts[@]}" -q "$here/remote.sh" "root@$ip:/root/pv-remote.sh"
if [ "$build_kernel" = 0 ]; then
  ssh "${ssh_opts[@]}" "root@$ip" 'mkdir -p /root/seedbox-sim/artifacts'
  scp "${ssh_opts[@]}" -q "$kernel" "root@$ip:/root/seedbox-sim/artifacts/bzImage"
fi

echo "building and running on the droplet (about 7 minutes of setup and first build, then a few minutes per extra commit)" >&2
set +e
ssh "${ssh_opts[@]}" "root@$ip" \
  "SHAS='${shas[*]}' WORLDS='${worlds[*]}' SEEDS='$seeds' BUILD_KERNEL=$build_kernel bash /root/pv-remote.sh" \
  > "$out/remote.log" 2>&1
remote=$?
set -e
scp "${ssh_opts[@]}" -q -r "root@$ip:/root/results/." "$out/" || true
{
  echo "droplet: $size $region ($(ssh "${ssh_opts[@]}" "root@$ip" "grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs" 2>/dev/null))"
  echo "results: $out"
} >&2
[ -f "$out/summary.txt" ] && cat "$out/summary.txt" || tail -n 40 "$out/remote.log" >&2
exit "$remote"
