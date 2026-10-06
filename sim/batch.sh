#!/usr/bin/env bash
# Usage: batch.sh WORLD OUTPREFIX "seed seed ..." [extra args]
# Runs one simulation per seed in parallel, each pinned to its own core with
# the Zen SpecLockMap workaround set on that core and its SMT sibling.
# Env: CPU0 (first core, default 2), VMM (binary), SEEDBOX_MARGIN.
set -u
here=$(cd "$(dirname "$0")" && pwd)
cd ~/seedbox-sim
world="$1"; prefix="$2"; seeds=($3); shift 3
P=/etc/profiles/per-user/gvickers/bin/python3
cpus=(); sibs=()
i=0
for s in "${seeds[@]}"; do c=$((${CPU0:-2} + i)); cpus+=($c); sibs+=($c,$(cut -d, -f2 /sys/devices/system/cpu/cpu$c/topology/thread_siblings_list)); i=$((i+1)); done
all=$(IFS=,; echo "${sibs[*]}")
# Branch counts are not deterministic without the workaround: never run without it.
if ! sudo -n $P "$here/msr54.py" set "$all" > /dev/null; then
  echo "batch: could not set the counter workaround on cpus $all; no runs started" >&2
  exit 1
fi
i=0
for s in "${seeds[@]}"; do
  out="runs/$prefix-s$s-r$i"
  timeout 3600 sudo -n env SEEDBOX_MARGIN="${SEEDBOX_MARGIN:-adaptive}" taskset -c ${cpus[$i]} "${VMM:-./sim/vmm/target/release/seedbox-vmm}" --config "$world" --seed "$s" --out "$out" "$@" > "$out.json" 2> "$out.err" &
  i=$((i+1))
done
wait
after=$(sudo -n $P "$here/msr54.py" show "$all")
sudo -n $P "$here/msr54.py" restore "$all" > /dev/null
echo "workaround after runs: $after"
