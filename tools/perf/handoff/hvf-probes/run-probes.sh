#!/usr/bin/env bash
# Run from checkout root inside nix develop. Uses the installed modern Apple SDK.
set -euo pipefail
P="$PWD/build/perf/hvf-probes"
/usr/bin/clang -mmacosx-version-min=15.0 -Wall -Wextra -Wno-format \
  -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
  -framework Hypervisor "$P/probe.c" "$P/guest.S" -o "$P/probe"
bash crates/seedbox-arm64/codesign.sh "$P/probe"
"$P/probe" 0 | tee "$P/el2-off.txt"
"$P/probe" 1 | tee "$P/el2-on.txt"
python3 "$P/build-linux.py"
podman run --rm --pull=never --platform linux/arm64 --security-opt seccomp=unconfined \
  -v "$P/out:/p:ro" docker.io/library/alpine:3.22 /p/jit 2>&1 | tee "$P/jit-native.txt"
for variant in ordinary fake; do
  EXTRA=(); if [ "$variant" = fake ]; then EXTRA=(-fake_ctr_dic); fi
  podman run --rm --pull=never --platform linux/arm64 --security-opt seccomp=unconfined \
    -v "$P/out:/p:ro" -v "$P/shipped:/seedbox/progress:ro" docker.io/library/alpine:3.22 \
    /seedbox/progress/bin64/drrun -opt_speed -disable_traces -ldstex2cas \
    -no_hw_cache_consistency "${EXTRA[@]}" -c /p/libic_counter.so -- /p/jit \
    2>&1 | tee "$P/jit-$variant.txt"
done
