#!/usr/bin/env bash
set -euo pipefail
probe_root="$(cd "$(dirname "$0")" && pwd)"
round_root="$(dirname "$probe_root")"
mode="${1:-prepare}"
mkdir -p "$round_root/runs/w5b"
out="$(mktemp -d "$round_root/runs/w5b/$mode.XXXXXX")"
mkdir -p "$out"
case "$mode" in prepare|portable|amd|intel) ;; *) echo 'mode must be prepare, portable, amd or intel' >&2; exit 2 ;; esac
rustc --version > "$out/rustc.txt"
cargo --version > "$out/cargo.txt"
nasm -v > "$out/nasm.txt"
uname -a > "$out/uname.txt"
if [ "$mode" != prepare ]; then
    # Serialize even the small remote builds with measurements from other slices.
    exec 9>"$round_root/measurement.lock"
    if ! flock -n 9; then echo 'measurement lock busy; retry after other slice finishes' >&2; exit 75; fi
fi
for package in rng tsc; do
    cargo test --locked -j 4 --manifest-path "$probe_root/$package/Cargo.toml"
    cargo build --locked -j 4 --manifest-path "$probe_root/$package/Cargo.toml"
    cargo fmt --check --manifest-path "$probe_root/$package/Cargo.toml"
done
"$probe_root/rng/target/debug/seedbox-rng-rewrite-kvm-probe" --admission-audit > "$out/admission-audit.json"
if [ "$mode" = prepare ]; then exit 0; fi
test "$(uname -s)" = Linux && test "$(uname -m)" = x86_64
lscpu > "$out/lscpu.txt"
cat /proc/cpuinfo > "$out/cpuinfo.txt"
test -r /dev/kvm && test -w /dev/kvm
if [ "$mode" = intel ]; then
    grep -q GenuineIntel "$out/cpuinfo.txt" || { echo 'Intel hardware acceptance requires GenuineIntel' >&2; exit 42; }
    rng_args=()
elif [ "$mode" = portable ]; then
    rng_args=(--synthetic-ud-smoke)
else
    grep -q AuthenticAMD "$out/cpuinfo.txt" || { echo 'AMD fixture requires AuthenticAMD' >&2; exit 42; }
    rng_args=(--portable-rewrite-smoke)
fi
rng="$probe_root/rng/target/debug/seedbox-rng-rewrite-kvm-probe"
tsc="$probe_root/tsc/target/debug/seedbox-tsc-kvm-probe"
for bin in "$rng" "$tsc"; do sha256sum "$bin"; done > "$out/binaries.sha256"
for trial in 1 2; do
    "$rng" "${rng_args[@]}" --seed 42 > "$out/rng-42-$trial.json" 2> "$out/rng-42-$trial.stderr"
    "$tsc" --seed 42 > "$out/tsc-42-$trial.json" 2> "$out/tsc-42-$trial.stderr"
done
cmp "$out/rng-42-1.json" "$out/rng-42-2.json"
cmp "$out/tsc-42-1.json" "$out/tsc-42-2.json"
"$rng" "${rng_args[@]}" --seed 43 > "$out/rng-43.json" 2> "$out/rng-43.stderr"
"$tsc" --seed 43 > "$out/tsc-43.json" 2> "$out/tsc-43.stderr"
# Verify actual consumed results change, rather than just the seed field.
for mechanism in rng tsc; do
    jq -S '.world.outputs' "$out/$mechanism-42-1.json" > "$out/$mechanism-42-outputs.json"
    jq -S '.world.outputs' "$out/$mechanism-43.json" > "$out/$mechanism-43-outputs.json"
    if cmp -s "$out/$mechanism-42-outputs.json" "$out/$mechanism-43-outputs.json"; then
        echo 'seed sensitivity failed in guest outputs' >&2; exit 1
    fi
done
sha256sum "$out"/*.json > "$out/results.sha256"
printf 'fresh equality, consumed seed sensitivity and built-in negative controls passed; mode=%s\n' "$mode"
