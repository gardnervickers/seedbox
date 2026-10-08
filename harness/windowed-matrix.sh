#!/usr/bin/env bash
# Windowed-scheduler evidence (docs/windowed-scheduler.md). Run as root in the
# dev shell on a KVM host:
#
#   sudo -E env PATH="$PATH" bash harness/windowed-matrix.sh KERNEL VMM BASE_VMM CPUS
#
# KERNEL is a PV-capable guest kernel (kernel/build.sh). VMM is this revision's
# `cargo build-vmm` output and BASE_VMM the preceding engine's. CPUS lists one
# CPU per machine thread, e.g. 4,5,6,7; the Zen counter workaround is set on
# them and their SMT siblings and restored on exit.
#
# For each timebase and world (harness/worlds/window2.json, window4.json):
# - the old scheduler under VMM and BASE_VMM must agree (feature-off equality);
# - windowed runs with 1..len(CPUS) threads, and with reversed completion
#   order, must agree with each other on world, network and full-state
#   witnesses, machine times and the committed end.
# Prints one timing row per run; exits nonzero on any disagreement.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
kernel=$(realpath "$1"); vmm=$(realpath "$2"); base=$(realpath "$3"); cpus=$4
seed=${SEED:-7}
out=$(mktemp -d "${TMPDIR:-/tmp}/windowed-matrix.XXXXXX")
siblings=$(for c in ${cpus//,/ }; do cat /sys/devices/system/cpu/cpu$c/topology/thread_siblings_list; done | tr '\n' ',' | sed 's/,$//')
msr_state="$out/msr54.json"
SEEDBOX_MSR_STATE="$msr_state" python3 "$here/msr54.py" set "$siblings" >/dev/null
cleanup() {
    SEEDBOX_MSR_STATE="$msr_state" python3 "$here/msr54.py" restore "$siblings" >/dev/null || echo "msr54 restore failed: $siblings" >&2
    rm -rf "$out"
}
trap cleanup EXIT
bash "$here/../guest/build-initramfs.sh" "$out/window.cpio" "$here/../guest/window.init" >/dev/null
first=${cpus%%,*}
IFS=, read -ra cpu_list <<< "$cpus"

run() { # NAME ENGINE TASKSET-CPUS [VMM ARGS...]
    local name=$1 engine=$2 set=$3; shift 3
    mkdir -p "$out/$name"
    taskset -c "$set" "$engine" --config "$out/world.json" --seed "$seed" --out "$out/$name" --digest "$@" \
        > "$out/$name/vmm.json" 2> "$out/$name/stderr" || { echo "$name failed:" >&2; tail -n 5 "$out/$name/stderr" >&2; exit 1; }
}

for world in window2 window4; do
    python3 - "$here/worlds/$world.json" "$out/world.json" "$kernel" "$out/window.cpio" <<'PY'
import json, sys
w = json.load(open(sys.argv[1]))
w["kernel"] = sys.argv[3]
for m in w["machines"]:
    m["initrd"] = sys.argv[4]
json.dump(w, open(sys.argv[2], "w"))
PY
    machines=$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["machines"]))' "$out/world.json")
    for tb in pv counter; do
        names=()
        run "$world-$tb-base" "$base" "$first" --timebase "$tb"
        run "$world-$tb-old" "$vmm" "$first" --timebase "$tb"
        for ((t = 1; t <= ${#cpu_list[@]} && t <= machines; t++)); do
            set=$(IFS=,; echo "${cpu_list[*]:0:t}")
            args=(--timebase "$tb" --scheduler windowed)
            if ((t > 1)); then args+=(--machine-workers "$t" --machine-cpus "$set"); fi
            run "$world-$tb-w$t" "$vmm" "$set" "${args[@]}"
            names+=("$world-$tb-w$t")
            last=$t
        done
        # Reversed completion order: machine i waits (n - i) x 300 us per run.
        SEEDBOX_WINDOWED_SKEW_US=300 run "$world-$tb-w${last}r" "$vmm" "$set" "${args[@]}"
        names+=("$world-$tb-w${last}r")
        python3 - "$out" "$world-$tb-base" "$world-$tb-old" "${names[@]}" <<'PY'
import json, sys
out, base, old, *windowed = sys.argv[1:]
load = lambda n: json.load(open(f"{out}/{n}/vmm.json"))
def key(r):
    s = r.get("scheduler", {})
    return (r["outcome"], r["world_witness"], r["network_witness"], s.get("committed_ns"),
            [(m["time_ns"], m["state_digest"]) for m in r["machines"]])
def row(n, r):
    s = r.get("scheduler", {})
    print(f"{n:24} {r['host_seconds']:7.2f}s world {r['world_witness'][:12]} "
          f"rounds {s.get('rounds', '-')} migrations {s.get('migrations', '-')}")
b, o = load(base), load(old)
row(base, b); row(old, o)
ok = key(b) == key(o)
if not ok:
    print(f"MISMATCH: {old} differs from the preceding engine")
ref = load(windowed[0])
for n in windowed:
    r = load(n); row(n, r)
    if key(r) != key(ref):
        print(f"MISMATCH: {n} differs from {windowed[0]}"); ok = False
sys.exit(0 if ok else 1)
PY
    done
done
echo "windowed matrix: all witnesses agree"
