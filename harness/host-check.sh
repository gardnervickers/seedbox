#!/usr/bin/env bash
# One engine PR's full host check on one leased SMT pair: build, source checks
# and the AMD smoke tier, each timed. Several checks can run at once on
# different pairs. Run inside `nix develop` from any checkout of this
# repository, as the user who owns it; the smoke tier asks sudo for root.
set -euo pipefail
usage() {
  cat >&2 <<'EOF'
Usage: harness/host-check.sh REV CPU
  REV   commit to check (a worktree is made for it)
  CPU   first CPU of the SMT pair to lease; never 0 or its sibling
Environment:
  SEEDBOX_KERNEL     guest kernel (required; the accepted bzImage)
  SEEDBOX_BUSYBOX    static busybox (set by nix develop)
  PINNED_ENGINE      a pinned seedbox-vmm; set it to check an unpinned engine
                     as the development tier (--allow-dev-engine)
  HOST_CHECK_DIR     where to work [default: $HOME]
EOF
  exit 2
}
[[ $# -eq 2 && $2 =~ ^[0-9]+$ ]] || usage
REV=$1 CPU=$2
: "${SEEDBOX_KERNEL:?set SEEDBOX_KERNEL to the accepted bzImage}"
: "${SEEDBOX_BUSYBOX:?set SEEDBOX_BUSYBOX or enter nix develop}"
siblings=$(cat "/sys/devices/system/cpu/cpu$CPU/topology/thread_siblings_list")
if [[ ,$siblings, == *,0,* ]]; then
  echo "cpu $CPU: its pair ($siblings) includes cpu 0, which the host keeps" >&2
  exit 2
fi
REPO=$(git rev-parse --show-toplevel)
SHA=$(git -C "$REPO" rev-parse --verify "$REV^{commit}")
WORK=${HOST_CHECK_DIR:-$HOME}/host-check-${SHA:0:7}-cpu$CPU
SRC=$WORK/src
mkdir "$WORK"
git -C "$REPO" worktree add --detach --quiet "$SRC" "$SHA"
SUMMARY=$WORK/summary.txt
echo "host-check ${SHA:0:12} cpu $CPU ($siblings) kernel $(sha256sum "$SEEDBOX_KERNEL" | cut -c1-12)" | tee "$SUMMARY"

# Builds and source checks run niced and unpinned: they never change results,
# and the leased pair stays free for the guests.
step() {
  local name=$1 started status=0
  shift
  started=$(date +%s)
  (cd "$SRC" && "$@") > "$WORK/$name.log" 2>&1 || status=$?
  printf '%-8s %s %4ss\n' "$name" "$([[ $status == 0 ]] && echo pass || echo FAIL)" \
    "$(($(date +%s) - started))" | tee -a "$SUMMARY"
  if [[ $status != 0 ]]; then
    tail -n 20 "$WORK/$name.log" >&2
    echo "FAIL: $WORK (delete it once the failure is reported)"
    exit 1
  fi
}
step build nice cargo build --release --locked -p seedbox
step vmm nice cargo build-vmm
step fmt cargo fmt -p seedbox --check
step clippy nice cargo clippy -p seedbox --all-targets --locked -- -D warnings
step suite nice bash harness/acceptance.sh portable

init=$(ls -t "$SRC"/target/release/build/seedbox-*/out/compose-init | head -n 1)
dev=()
if [[ -n ${PINNED_ENGINE:-} ]]; then dev=(--allow-dev-engine --pinned-engine "$PINNED_ENGINE"); fi
step smoke sudo python3 harness/smoke/amd.py --cli "$SRC/target/release/seedbox" \
  --engine "$SRC/crates/seedbox-vmm/target/release/seedbox-vmm" --kernel "$SEEDBOX_KERNEL" \
  --guest-init "$init" --busybox "$SEEDBOX_BUSYBOX" --cpu "$CPU" --tool-path "$PATH" \
  --output "$WORK/smoke" "${dev[@]}"
grep '^phase ' "$WORK/smoke.log" | tee -a "$SUMMARY"

# Passed: keep only the summary, beside the work directory.
cp "$SUMMARY" "$WORK.txt"
git -C "$REPO" worktree remove --force "$SRC"
sudo rm -rf "$WORK"
echo "PASS: $WORK.txt"
