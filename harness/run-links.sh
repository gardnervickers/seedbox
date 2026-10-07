#!/usr/bin/env bash
# Directed-link policy controls (PR #97). Six runs on the AMD host:
#   L1 shaped world, L2 its replay,
#   P1 plain world on the PR engine, M1 the same plain world on main's engine,
#   U1 the short shaped world with a 200 us stop tolerance, U2 with undo off.
# Undo off steps up to the whole margin (~16k branches) at every precise stop,
# about 60x the stepping of undo on in the zero-tolerance world (L1: 166k
# precise stops, 400 s), so the undo-off pair uses a stop tolerance to make
# most slices end at natural exits. U1/U2 use a 600 s timeout.
# Run as the normal user on distinct free physical cores with idle SMT siblings:
#   CPUS="2 3 4 5 6 7" SHA=<PR head> MAIN=<main sha> bash harness/run-links.sh
# Subsets: RUNS="L1 U1" CPUS="14 15" (default: all six lanes).
# Optional NETWORK_TRACE_ROOT holds L1/network.jsonl, L2/network.jsonl from an
# external route capture; current VMM --trace does not record routed arrivals,
# so without one links.py reports the exact bounds as covered by link_worlds.rs.
# To test another engine (a train head), add ENGINE=<commit> and FETCH="<refs>":
# the PR side is then ENGINE's tree plus the link fixtures from SHA, and
# MAIN should be the engine's base (the run compares P1 against it).
set -euo pipefail
select_lanes() {
  read -r -a lanes <<< "${RUNS-L1 L2 P1 M1 U1 U2}"
  read -r -a cores <<< "${CPUS-}"
  [[ ${#lanes[@]} -gt 0 ]] || { echo 'RUNS must select at least one lane' >&2; return 2; }
  local i j lane
  for lane in "${lanes[@]}"; do
    case "$lane" in L1|L2|P1|M1|U1|U2) ;; *) echo "unknown lane: $lane" >&2; return 2 ;; esac
  done
  [[ ${#cores[@]} -eq ${#lanes[@]} ]] || { echo 'CPUS must supply one distinct physical core per selected lane' >&2; return 2; }
  for ((i=0; i<${#lanes[@]}; i++)); do
    [[ ${cores[i]} =~ ^(0|[1-9][0-9]*)$ ]] || { echo "invalid CPU: ${cores[i]}" >&2; return 2; }
    for ((j=0; j<i; j++)); do
      [[ ${lanes[i]} != "${lanes[j]}" ]] || { echo "duplicate lane: ${lanes[i]}" >&2; return 2; }
      [[ ${cores[i]} != "${cores[j]}" ]] || { echo "duplicate CPU: ${cores[i]}" >&2; return 2; }
    done
  done
}

allocate_directories() {
  local results_root=$1 scratch_parent=$2
  mkdir -p "$results_root" || return 1
  RESULTS=$(mktemp -d "$results_root/run.XXXXXXXX") || return 1
  S=$(mktemp -d "$scratch_parent/seedbox-link60.XXXXXXXX") || return 1
}

run_lane() {
  local lane=$1 cpu=$2
  case "$lane" in
    L1|L2) run "$cpu" "$lane" shaped "$S/pr" 1800 ;;
    P1) run "$cpu" "$lane" plain "$S/pr" 1800 ;;
    M1) run "$cpu" "$lane" plain "$S/main" 1800 ;;
    U1) run "$cpu" "$lane" shaped-tol-short "$S/pr" 600 ;;
    U2) run "$cpu" "$lane" shaped-tol-short "$S/pr" 600 SEEDBOX_UNDO=0 ;;
  esac
}

run_lanes() {
  local i rc=0
  pids=()
  for ((i=0; i<${#lanes[@]}; i++)); do
    run_lane "${lanes[i]}" "${cores[i]}" > "$RESULTS/${lanes[i]}.log" 2>&1 &
    pids+=("$!")
  done
  # Join every supervisor, even if one fails, before analyzing or cleanup.
  for i in "${pids[@]}"; do
    wait "$i" || rc=1
  done
  pids=()
  return "$rc"
}

# Sourcing exposes only the selection/dispatch helpers for portable tests.
if [[ ${BASH_SOURCE[0]} != "$0" ]]; then return; fi
select_lanes
SHA="${SHA:?set SHA to the PR fixture commit}"
MAIN="${MAIN:?set MAIN to the main commit PR #97 is based on}"
CHECKOUT="$HOME/src/github.com/gardnervickers/seedbox"
ACCEPTED_KERNEL="$HOME/seedbox-demo-20261006/artifacts/bzImage"
ACCEPTED_SHA=e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847
RESULTS_ROOT="$HOME/seedbox-link60-results"
allocate_directories "$RESULTS_ROOT" "$HOME"
pids=()
cleanup() {
  # Never delete a live child's files: on interruption these bounded host runs
  # finish through batch.py's normal reap/MSR restoration before removal.
  trap '' INT TERM HUP
  local pid
  for pid in "${pids[@]}"; do wait "$pid" || true; done
  sudo rm -rf -- "$S"
  echo "removed $S; results kept in $RESULTS"
  df -h "$HOME"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
df -h "$HOME" /tmp
[ "$(df --output=avail "$HOME" | tail -1)" -ge $((15 * 1024 * 1024)) ] || { echo "need 15 GiB free in $HOME" >&2; exit 1; }
echo "${ACCEPTED_SHA}  ${ACCEPTED_KERNEL}" | sha256sum -c

# Sources: the host checkout fetches over SSH; clones are local.
ENGINE="${ENGINE:-$SHA}"
git -C "$CHECKOUT" fetch -q origin codex/60-link-policy main ${FETCH:-}
for w in pr main; do git clone -q "$CHECKOUT" "$S/$w"; done
git -C "$S/pr" checkout -q "$ENGINE"
git -C "$S/main" checkout -q "$MAIN"
test "$(git -C "$S/pr" rev-parse HEAD)" = "$(git -C "$CHECKOUT" rev-parse "$ENGINE^{commit}")"
test "$(git -C "$S/main" rev-parse HEAD)" = "$MAIN"
if [ "$ENGINE" != "$SHA" ]; then
  git -C "$S/pr" checkout -q "$SHA" -- harness/run-links.sh harness/links.py harness/fixtures/links harness/worlds/shaped.json harness/worlds/plain.json harness/worlds/shaped-tol-short.json
  git -C "$S/pr" diff --cached --stat
fi
echo "engine=$(git -C "$S/pr" rev-parse HEAD) fixture=$SHA main=$MAIN" | tee "$RESULTS/engine.txt"
E="$S/pr/harness/fixtures/links"

cd "$S/pr"
nix develop --command bash -euo pipefail -c '
  S="$1"; E="$2"
  (cd "$S/pr" && cargo build-vmm -j 4)
  (cd "$S/main" && cargo build-vmm -j 4)
  "$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -o "$S/link-probe" "$E/link-probe.c"
  bash "$S/pr/guest/build-initramfs.sh" "$S/link.cpio" "$E/link.init" "$S/link-probe:bin/link-probe"
  bash "$S/pr/guest/build-initramfs.sh" "$S/link-short.cpio" "$E/link-short.init" "$S/link-probe:bin/link-probe"
' _ "$S" "$E"
sha256sum "$S/pr/crates/seedbox-vmm/target/release/seedbox-vmm" "$S/main/crates/seedbox-vmm/target/release/seedbox-vmm" "$S/link.cpio" "$S/link-short.cpio" | tee "$RESULTS/inputs.sha256"
mkdir -p "$S/worlds"
for w in shaped plain shaped-tol-short; do
  initrd="$S/link.cpio"
  [[ $w != shaped-tol-short ]] || initrd="$S/link-short.cpio"
  sed -e "s|\"KERNEL\"|\"$ACCEPTED_KERNEL\"|" -e "s|\"INITRD\"|\"$initrd\"|" "$S/pr/harness/worlds/$w.json" > "$S/worlds/$w.json"
  cp "$S/worlds/$w.json" "$RESULTS/"
done

run() { # cpu name world root timeout [env...]
  local cpu=$1 name=$2 world=$3 root=$4 timeout=$5; shift 5
  echo "=== $name $(date -u +%T) world=$world root=$root $*"
  set +e
  env "$@" CPU0="$cpu" SEEDBOX_ROOT="$root" SEEDBOX_TIMEOUT="$timeout" \
    bash "$S/pr/harness/batch.sh" "$S/worlds/$world.json" "link-$name" "1" --digest --trace
  local rc=$?
  echo "$name batch rc=$rc" > "$RESULTS/$name.status"
  set -e
  local out="$root/runs/link-$name-s1-r0"
  mkdir -p "$RESULTS/$name"
  sudo cat "$out.json" > "$RESULTS/$name.json" || true
  sudo cat "$out.status" >> "$RESULTS/$name.status" || true
  sudo tail -50 "$out.err" > "$RESULTS/$name.err" || true
  for m in a b c; do
    sudo cat "$out/$m.console" > "$RESULTS/$name/$m.console" 2>/dev/null || true
    sudo cat "$out/$m.trace" > "$RESULTS/$name/$m.trace" 2>/dev/null || true
  done
  # A routed-frame capture must belong to this exact lane/run, in routing order.
  # Device traces or a witness digest cannot substitute for these observations.
  if [[ $name == L1 || $name == L2 ]] && [[ -n ${NETWORK_TRACE_ROOT:-} ]]; then
    cp "$NETWORK_TRACE_ROOT/$name/network.jsonl" "$RESULTS/$name/network.jsonl" || return 1
  elif [[ -f $out/network.jsonl ]]; then
    sudo cat "$out/network.jsonl" > "$RESULTS/$name/network.jsonl"
  fi
  return "$rc"
}
batch_rc=0
run_lanes || batch_rc=$?
for lane in "${lanes[@]}"; do cat "$RESULTS/$lane.status" >> "$RESULTS/runs.txt"; done
analysis_rc=0
python3 "$S/pr/harness/links.py" "$RESULTS" "${lanes[@]}" | tee "$RESULTS/summary.txt" || analysis_rc=$?
du -sh "$RESULTS"
echo "results kept in $RESULTS; run state and builds are deleted on exit"

(( batch_rc == 0 && analysis_rc == 0 ))
