#!/usr/bin/env bash
# Host checks for one merge train, in parallel lanes on separate SMT pairs.
# Run inside `nix develop` as gvickers, from the main seedbox checkout, with
# CPU 0 and its sibling (16) left alone and cores 1-10 free.
# Usage: train-host.sh TRAIN_SHA MAIN_SHA OUT
# Env: A (accepted kernel e8ece7cb), K (disk kernel 8f4f1666),
#      PINNED (stored 52eb engine), ARTS (dir holding multi.cpio and net.cpio
#      for the world fixtures, as used for #90/#101 checks).
set -uo pipefail
TRAIN=$1 MAIN=$2 OUT=$3
: "${A:?} ${K:?} ${PINNED:?} ${ARTS:?}"
test ! -e "$OUT" && mkdir -p "$OUT"
REPO=$PWD T=$OUT/train M=$OUT/main
git -C "$REPO" worktree prune
git -C "$REPO" worktree add --detach -q "$T" "$TRAIN"
git -C "$REPO" worktree add --detach -q "$M" "$MAIN"
SUM=$OUT/summary.txt
note() { echo "$*" | tee -a "$SUM"; }
note "train $(git -C "$T" rev-parse --short HEAD) main $(git -C "$M" rev-parse --short HEAD) $(date -u +%FT%TZ)"
for w in "$T" "$M"; do
  (cd "$w" && cargo build --release --locked -p seedbox && cargo build-vmm) > "$OUT/build-$(basename "$w").log" 2>&1 \
    || { note "BUILD FAIL $w"; exit 1; }
done
TE=$T/crates/seedbox-vmm/target/release/seedbox-vmm ME=$M/crates/seedbox-vmm/target/release/seedbox-vmm
TC=$T/target/release/seedbox MC=$M/target/release/seedbox
note "train engine $(sha256sum "$TE" | cut -c1-12), main engine $(sha256sum "$ME" | cut -c1-12)"
mkdir -p "$T/artifacts"
cp "$ARTS"/*.cpio "$T/artifacts/"
(cd "$T" && bash guest/build-disk.sh artifacts/disk.cpio artifacts/ext4.img) > "$OUT/build-disk.log" 2>&1 || note "BUILD FAIL disk fixtures"
note "ext4.img $(sha256sum "$T/artifacts/ext4.img" | cut -c1-12)"

# World fixtures through the batch runner; KERNEL picks artifacts/bzImage.
world() {  # KERNEL ENGINE CPU0 WORLD PREFIX SEEDS [extra]
  local kernel=$1 engine=$2 cpu=$3 w=$4 prefix=$5 seeds=$6; shift 6
  ln -sfn "$kernel" "$T/artifacts/bzImage"
  (cd "$T" && SEEDBOX_ROOT="$T" VMM="$engine" CPU0=$cpu SEEDBOX_TIMEOUT=600 \
    harness/batch.sh "harness/worlds/$w.json" "$prefix" "$seeds" --digest "$@") >> "$OUT/worlds.log" 2>&1
  note "world $prefix: exit $?"
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
replay() {  # NAME CLI RUN_OUT CPU STATE [replay flags]
  local name=$1 cli=$2 from=$3 cpu=$4 state=$5; shift 5
  sb "$cli" replay "$(attempt "$OUT/$from.out")" --cpu "$cpu" --state "$state" --allow-dev-engine "$@" \
    > "$OUT/$name.out" 2> "$OUT/$name.err"
  note "$name: exit $? $(grep -h '^replay:' "$OUT/$name.out")"
}

lane_check() {  # cpu 1: build, fmt, clippy, portable suite, smoke tier (dev tier)
  (cd "$REPO" && SEEDBOX_KERNEL=$A PINNED_ENGINE=$PINNED HOST_CHECK_DIR=$OUT \
    bash "$T/harness/host-check.sh" "$TRAIN" 1) > "$OUT/host-check.log" 2>&1
  note "host-check: exit $? $(tail -n 1 "$OUT/host-check.log")"
}
lane_worlds() {  # cpus 2-5
  world "$A" "$TE" 2 cluster-fixed b1-cluster-fixed "1009"            # B1: 4b0b13cc
  world "$K" "$TE" 2 disk-crash b2-disk-crash "1009" --trace          # B2/T1: 6bb7b8f5, 16 lost
  world "$K" "$TE" 2 disk-ext4-torn t2-torn "1009 1009 1010 1011" --trace   # T2
  for w in net-pause net-crash; do                                     # #101 item 1
    world "$A" "$TE" 2 "$w" "e-$w-train" "1009"
    world "$A" "$ME" 2 "$w" "e-$w-main" "1009"
  done
}
lane_faults() {  # cpu 6: #91 L1 + L2 (faults matrix, control-cases, lifecycle-cases)
  (cd "$T" && SEEDBOX_ACCEPT_DEV=1 bash crates/seedbox/tests/accept-runtime.sh "$A" 6 "$OUT/faults" faults) \
    > "$OUT/faults.log" 2>&1
  note "accept-runtime faults: exit $? $(grep -h '"status"' "$OUT/faults/acceptance.json" | head -n 1)"
}
lane_volumes() {  # cpu 7: #96 V1-V3, #100 T3, all on K
  local s=$OUT/vol-state v=$T/harness/acceptance/workloads/volume-crash.yaml
  compose v1 "$TC" "$K" 7 "$s" "$v" --seed 1
  replay v1-replay "$TC" v1 7 "$s"
  replay v1-undo0 "$TC" v1 7 "$s" --undo 0
  replay v1-margin "$TC" v1 7 "$s" --undo 0 --margin 16384
  compose v2 "$TC" "$K" 7 "$s" "$v" --seed 4101
  replay v2-replay "$TC" v2 7 "$s"
  compose v3 "$TC" "$K" 7 "$s" "$T/harness/acceptance/workloads/sanity.yaml"
  replay v3-replay "$TC" v3 7 "$s"
  compose t3 "$TC" "$K" 7 "$s" "$v" --seed 1 --torn-writes
  replay t3-replay "$TC" t3 7 "$s"
}
lane_pause() {  # cpu 8: #101 items 2 and 4
  local s=$OUT/pause-state p=$OUT/pause-project
  python3 "$T/examples/faults/generate.py" "$p" > /dev/null
  compose p-base "$TC" "$A" 8 "$s" "$p/fixed" --full-state
  # One run paused for 5 s mid-run must give the unpaused witness.
  sb "$TC" run "$p/fixed" --kernel "$A" --vmm "$TE" --cpu 8 --state "$s" --allow-dev-engine --full-state \
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
  # host_pause ignored test against the full-state base run.
  (cd "$T" && sudo -E env "PATH=$PATH" SEEDBOX_HOST_PAUSE_RUN="$(attempt "$OUT/p-base.out")" SEEDBOX_HOST_PAUSE_STATE="$s" \
    cargo test --release --locked -p seedbox --test host_pause -- --ignored) > "$OUT/host-pause-test.log" 2>&1
  note "host_pause test: exit $? $(grep -h '^test result' "$OUT/host-pause-test.log")"
  # Item 4: paused past its wall limit gives INCOMPLETE and cleans up.
  local wall=$OUT/wall-project; python3 "$T/examples/faults/generate.py" "$wall" > /dev/null
  sed -i 's/wall_time: [^}]*}/wall_time: 20s}/' "$wall/fixed/compose.yaml"
  sb "$TC" run "$wall/fixed" --kernel "$A" --vmm "$TE" --cpu 8 --state "$s" --allow-dev-engine > "$OUT/p-wall.out" 2> "$OUT/p-wall.err" &
  pid=$!; loc=
  for _ in $(seq 60); do loc=$(sudo find "$s/runs" -path '*/output/host-control' -newer "$OUT/p-paused.out" 2>/dev/null | head -n 1); [[ -n $loc ]] && break; sleep 0.5; done
  [[ -n $loc ]] && sb "$TC" pause "${loc%/output/host-control}" >> "$OUT/p-wall.ctl" 2>&1
  wait $pid; note "p-wall: exit $? (want 3) $(head -n 1 "$OUT/p-wall.out") vmm left: $(pgrep -c seedbox-vmm || true)"
}
lane_ignored() {  # cpu 9: B3, every ignored KVM test, through the lease wrapper as for #90
  note "B3: run 'cargo test-vmm -- --ignored' in $T through ~/bin/seedbox-lease-run.py on cpu 9 as for #90, and post its test result line"
}

lane_check & lane_worlds & lane_faults & lane_volumes & lane_pause & lane_ignored & wait

# #101 item 3, alone on the host: host seconds, faults seed 1009, train vs main, 3 each.
p=$OUT/overhead-project; python3 "$T/examples/faults/generate.py" "$p" > /dev/null
for i in 1 2 3; do for side in main train; do
  cli=$MC; [[ $side == train ]] && cli=$TC
  compose "o-$side-$i" "$cli" "$A" 8 "$OUT/o-state-$side" "$p/fixed"
  note "o-$side-$i host_seconds $(sudo jq .host_seconds "$(attempt "$OUT/o-$side-$i.out")/output/vmm.json")"
done; done

# Leases, MSRs, leftovers.
note "vmm processes left: $(pgrep -c seedbox-vmm || true)"
for c in $(seq 1 10) $(seq 17 26); do printf '%s:%s ' "$c" "$(sudo rdmsr -p "$c" 0xc0011020 2>/dev/null)"; done | tee -a "$SUM"; echo | tee -a "$SUM"
note "done $(date -u +%FT%TZ)"
cp "$SUM" "$OUT.txt"
echo "summary: $OUT.txt"
echo "after posting: git -C $REPO worktree remove --force $T; git -C $REPO worktree remove --force $M; sudo rm -rf $OUT $OUT.txt; git -C $REPO worktree prune"
