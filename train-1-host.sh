#!/usr/bin/env bash
# Host checks for train/1 (#91, #96, #100, #101), trimmed to what each diff can
# affect (reviews/train1-host-checks.md). No undo-off, margin or timebase
# variants: none of these PRs touches stepping, the counter, undo or timebase.
# Portable checks (fmt, clippy, workspace tests, acceptance portable) already
# ran on the train head off-host, so the host only builds and runs KVM checks.
# Run inside `nix develop` as gvickers, from the main seedbox checkout, with
# cores 2-9 (and siblings 18-25) free. CPU 0/16 are never used.
# Usage: train-host.sh TRAIN_SHA MAIN_SHA OUT      (OUT must not exist)
# Env: A (accepted kernel e8ece7cb), K (disk kernel 8f4f1666),
#      ARTS (dir holding multi.cpio, net.cpio and disk.cpio for world fixtures).
set -uo pipefail
TRAIN=$1 MAIN=$2 OUT=$3
: "${A:?} ${K:?} ${ARTS:?}"
test ! -e "$OUT" || { echo "$OUT exists"; exit 2; }
mkdir -p "$OUT"
REPO=$PWD T=$OUT/train M=$OUT/main
git -C "$REPO" worktree prune
git -C "$REPO" worktree add --detach -q "$T" "$TRAIN"
git -C "$REPO" worktree add --detach -q "$M" "$MAIN"
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
note "train $(git -C "$T" rev-parse --short HEAD) main $(git -C "$M" rev-parse --short HEAD) $(date -u +%FT%TZ)"
for w in "$T" "$M"; do
  (cd "$w" && nice cargo build --release --locked -p seedbox && nice cargo build-vmm) > "$OUT/build-$(basename "$w").log" 2>&1 \
    || { note "BUILD FAIL $w"; exit 1; }
done
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm ME=$M/crates/seedbox-vmm/target/release/seedbox-vmm
TC=$T/target/release/seedbox MC=$M/target/release/seedbox
note "train engine $(sha256sum "$TE" | cut -c1-12), main engine $(sha256sum "$ME" | cut -c1-12)"
mkdir -p "$T/artifacts"
cp "$ARTS"/*.cpio "$T/artifacts/"
(cd "$T" && bash guest/build-disk.sh artifacts/disk.cpio artifacts/ext4.img) > "$OUT/build-disk.log" 2>&1 || note "BUILD FAIL disk fixtures"
note "ext4.img $(sha256sum "$T/artifacts/ext4.img" | cut -c1-12)"

# World fixtures through the batch runner; artifacts/bzImage picks the kernel,
# so only worlds on the same kernel run at once.
world() {  # ENGINE CPU0 WORLD PREFIX SEEDS [extra]
  local engine=$1 cpu=$2 w=$3 prefix=$4 seeds=$5; shift 5
  (cd "$T" && SEEDBOX_ROOT="$T" VMM="$engine" CPU0=$cpu SEEDBOX_TIMEOUT=600 \
    harness/batch.sh "harness/worlds/$w.json" "$prefix" "$seeds" --digest "$@") > "$OUT/world-$prefix.log" 2>&1
  local rc=$? r
  note "world $prefix: exit $rc"
  for r in "$T"/runs/"$prefix"-s*.json; do
    note "  $(basename "$r" .json) $(sudo jq -c 'with_entries(select(.key|test("witness|disk|ledger|digest|status|model";"i")))' "$r" 2>/dev/null)"
  done
}
# Compose runs through the CLI as root.
sb() { sudo -E env "PATH=$PATH" "$@"; }
vmm_of() { echo "${1%/target/release/seedbox}/crates/seedbox-vmm/target/release/seedbox-vmm"; }
attempt() { sed -n 's/^[A-Z_]*: //p' "$1" | tail -n 1; }
compose() {  # NAME CLI KERNEL CPU STATE PROJECT [run flags]
  local name=$1 cli=$2 kernel=$3 cpu=$4 state=$5 project=$6; shift 6
  sb "$cli" run "$project" --kernel "$kernel" --vmm "$(vmm_of "$cli")" --cpu "$cpu" --state "$state" --allow-dev-engine "$@" \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(head -n 1 "$OUT/$name.out")"
}
replay() {  # NAME CLI RUN_OUT CPU STATE   (default settings only)
  local name=$1 cli=$2 from=$3 cpu=$4 state=$5
  sb "$cli" replay "$(attempt "$OUT/$from.out")" --cpu "$cpu" --state "$state" --allow-dev-engine \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(grep -h '^replay:' "$OUT/$name.out")"
}
verify() {  # FUNCTION OUTFILE: examples/faults/verify.py checks on a run directory
  sb python3 -c "import sys; from pathlib import Path; sys.path.insert(0, '$T/examples/faults')
from verify import $1; $1(Path(sys.argv[1])); print('ok')" "$(attempt "$OUT/$2.out")" > "$OUT/$2.verify" 2>&1
  note "  $2 $1: $(tail -n 1 "$OUT/$2.verify")"
}

lane_worlds() {  # cpus 2-5: B1, B2, T1, then #101's net fixtures train vs main
  ln -sfn "$A" "$T/artifacts/bzImage"
  world "$TE" 2 cluster-fixed b1-cluster-fixed "1009"                 # B1: want 4b0b13cc
  ln -sfn "$K" "$T/artifacts/bzImage"
  world "$TE" 2 disk-crash b2-disk-crash "1009" &                     # B2: want 6bb7b8f5, 16 lost
  world "$TE" 3 disk-ext4-torn t1-torn "1009 1009 1010" &             # T1: 1009 twice equal
  wait
  ln -sfn "$A" "$T/artifacts/bzImage"
  world "$TE" 2 net-pause e-net-pause-train "1009" &
  world "$ME" 3 net-pause e-net-pause-main "1009" &
  world "$TE" 4 net-crash e-net-crash-train "1009" &
  world "$ME" 5 net-crash e-net-crash-main "1009" &
  wait
}
lane_control() {  # cpu 6: #91 L1 lifecycle-cases, L2 control-cases, run + default replay
  local s=$OUT/control-state p=$OUT/control-project
  (cd "$T/examples/faults" && python3 -c "import sys; from pathlib import Path
from generate import generate; from run import control_project
p = generate(Path(sys.argv[1]), 1009); control_project(p); control_project(p, 'lifecycle-cases', 'lifecycle_scenario.py')" "$p") \
    > "$OUT/control-generate.log" 2>&1 || note "control project generation FAILED"
  compose l1 "$TC" "$A" 6 "$s" "$p/lifecycle-cases/compose.yaml" --memory-mb 512   # want exit 0
  verify verify_lifecycle_cases l1
  replay l1-replay "$TC" l1 6 "$s"
  verify verify_lifecycle_cases l1-replay
  compose l2 "$TC" "$A" 6 "$s" "$p/control-cases/compose.yaml" --memory-mb 512     # want exit 1 (FAIL by design)
  verify verify_control_cases l2
  replay l2-replay "$TC" l2 6 "$s"
  verify verify_control_cases l2-replay
}
lane_volumes() {  # cpu 7: #96 V1 and #100 T2, on K
  local s=$OUT/vol-state v=$T/harness/acceptance/workloads/volume-crash.yaml
  compose v1 "$TC" "$K" 7 "$s" "$v" --seed 1                  # want PASS, acked=20 found=20
  replay v1-replay "$TC" v1 7 "$s"
  compose t2 "$TC" "$K" 7 "$s" "$v" --seed 1 --torn-writes    # want PASS
  replay t2-replay "$TC" t2 7 "$s"                            # MATCH proves disk_loss is restored
}
lane_diskless() {  # cpu 8: #96 V2, a disk-less workload under the new supervisor, on K
  local s=$OUT/sanity-state
  compose v2 "$TC" "$K" 8 "$s" "$T/harness/acceptance/workloads/sanity.yaml"
  replay v2-replay "$TC" v2 8 "$s"
}
lane_pause() {  # cpu 9: #101 pause witness, host_pause test, wall expiry
  local s=$OUT/pause-state p=$OUT/pause-project
  python3 "$T/examples/faults/generate.py" "$p" > /dev/null
  compose p-base "$TC" "$A" 9 "$s" "$p/fixed" --full-state
  # One run paused for 5 s mid-run must give the unpaused witness.
  sb "$TC" run "$p/fixed" --kernel "$A" --vmm "$TE" --cpu 9 --state "$s" --allow-dev-engine --full-state \
    > "$OUT/p-paused.out" 2> "$OUT/p-paused.err" &
  local pid=$! loc=
  for _ in $(seq 60); do loc=$(sudo find "$s/runs" -path '*/output/host-control' -newer "$OUT/p-base.out" 2>/dev/null | head -n 1); [[ -n $loc ]] && break; sleep 0.5; done
  if [[ -n $loc ]]; then
    sleep 3; sb "$TC" pause "${loc%/output/host-control}" >> "$OUT/p-paused.ctl" 2>&1
    sleep 5; sb "$TC" resume "${loc%/output/host-control}" >> "$OUT/p-paused.ctl" 2>&1
  fi
  wait $pid; note "p-paused: exit $? $(head -n 1 "$OUT/p-paused.out") control: $(tr '\n' ' ' < "$OUT/p-paused.ctl" 2>/dev/null)"
  for r in p-base p-paused; do
    note "$r witness $(sb "$TC" inspect "$(attempt "$OUT/$r.out")" --json | jq -r .report.witness)"
  done
  (cd "$T" && sudo -E env "PATH=$PATH" SEEDBOX_HOST_PAUSE_RUN="$(attempt "$OUT/p-base.out")" SEEDBOX_HOST_PAUSE_STATE="$s" \
    cargo test --release --locked -p seedbox --test host_pause -- --ignored) > "$OUT/host-pause-test.log" 2>&1
  note "host_pause test: exit $? $(grep -h '^test result' "$OUT/host-pause-test.log")"
  # Paused past its wall limit gives INCOMPLETE (exit 3) and leaves no VMM behind.
  local wall=$OUT/wall-project; python3 "$T/examples/faults/generate.py" "$wall" > /dev/null
  sed -i 's/wall_time: [^}]*}/wall_time: 20s}/' "$wall/fixed/compose.yaml"
  sb "$TC" run "$wall/fixed" --kernel "$A" --vmm "$TE" --cpu 9 --state "$s" --allow-dev-engine > "$OUT/p-wall.out" 2> "$OUT/p-wall.err" &
  pid=$!; loc=
  for _ in $(seq 60); do loc=$(sudo find "$s/runs" -path '*/output/host-control' -newer "$OUT/p-paused.out" 2>/dev/null | head -n 1); [[ -n $loc ]] && break; sleep 0.5; done
  [[ -n $loc ]] && sb "$TC" pause "${loc%/output/host-control}" >> "$OUT/p-wall.ctl" 2>&1
  wait $pid; note "p-wall: exit $? (want 3) $(head -n 1 "$OUT/p-wall.out") vmm on cpu 9: $(pgrep -af seedbox-vmm | grep -c -- '--cpu 9' || true)"
}

lane_worlds & lane_control & lane_volumes & lane_diskless & lane_pause & wait
note "B3: not run here. Run 'cargo test-vmm -- --ignored' in $T through ~/bin/seedbox-lease-run.py as for #90, and post its test result line."

# #101 overhead, alone on the host: host seconds, faults seed 1009, main vs train, 3 each.
p=$OUT/overhead-project; python3 "$T/examples/faults/generate.py" "$p" > /dev/null
for i in 1 2 3; do for side in main train; do
  cli=$MC; [[ $side == train ]] && cli=$TC
  compose "o-$side-$i" "$cli" "$A" 9 "$OUT/o-state-$side" "$p/fixed"
  note "o-$side-$i host_seconds $(sudo jq .host_seconds "$(attempt "$OUT/o-$side-$i.out")/output/vmm.json")"
done; done

# Leases, MSRs, leftovers.
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in $(seq 2 9) $(seq 18 25); do printf '%s:%s ' "$c" "$(sudo rdmsr -p "$c" 0xc0011020 2>/dev/null)"; done | tee -a "$SUM"; echo | tee -a "$SUM"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"
echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $T; git -C $REPO worktree remove --force $M; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
