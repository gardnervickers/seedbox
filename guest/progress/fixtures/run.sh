#!/usr/bin/env bash
# Historical packing negative control; the guest acceptance gate is exclusives.yaml.
# Run the exclusive-sequence fixtures natively on an aarch64 Linux host under
# DynamoRIO with the progress client, packed and unpacked, and count the
# progress charges (getpid) each makes. Usage: run.sh DR_DIR CLIENT_SO [GCC]
set -euo pipefail
dr=$1 client=$2 cc=${3:-gcc}
here=$(cd "$(dirname "$0")" && pwd)
work=${WORK:-$(mktemp -d)}
mkdir -p "$work"
for fixture in poll split weakcas; do
    for shape in packed unpacked; do
        define=; [ $shape = packed ] && define=-DPACKED
        # Load/store-exclusive CAS, not LSE or outline atomics, so the weak CAS can fail.
        "$cc" -O2 -pthread -march=armv8-a+nolse -mno-outline-atomics $define \
            "$here/$fixture.c" -o "$work/$fixture-$shape"
    done
done
for shape in packed unpacked; do
    for fixture in poll split weakcas; do
        for options in "-no_ldstex2cas -unsafe_build_ldstex" "-no_ldstex2cas"; do
            log=$work/$fixture-$shape.log
            if strace -f -c -e trace=getpid -o "$work/strace" \
                "$dr/bin64/drrun" -opt_speed -disable_traces $options -c "$client" --instructions-per-progress-call 50000 --progress-points-per-block 1 -- \
                "$work/$fixture-$shape" >"$log" 2>"$log.err"; then rc=0; else rc=$?; fi
            getpids=$(awk '/getpid/ {print $4}' "$work/strace" | head -1)
            printf '%-7s %-8s %-32s rc=%s getpid=%-8s %s %s\n' "$fixture" "$shape" \
                "$options" "$rc" "${getpids:-0}" "$(cat "$log")" "$(grep -o 'seedbox progress:.*' "$log.err" | head -1)"
        done
    done
done
