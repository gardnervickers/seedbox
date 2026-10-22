#!/usr/bin/env bash
# Prepare the native TCG suite's explicit inputs inside the pinned dev shell.
# Source this file to prepare inputs for a focused cargo test invocation.
set -euo pipefail
script=${BASH_SOURCE[0]}
[[ $script == */* ]] || script=./$script
cd -- "${script%/*}/.."

seedbox_flake=$(bash tools/dev-flake.sh)
inputs=$(nix build --impure --no-link --print-out-paths \
  --file nix/build-inputs.nix --argstr flake "$seedbox_flake" \
  --arg names '[ "machineTcgAssets" "machineTcgTestFixtures" "machinePvTestFixtures" "tcgQemu" ]')
export SEEDBOX_TCG_ASSETS=$inputs/machineTcgAssets
export SEEDBOX_TCG_QEMU=$inputs/tcgQemu/bin/qemu-system-x86_64
export SEEDBOX_TCG_KERNEL=$SEEDBOX_TCG_ASSETS/kernel
export SEEDBOX_TCG_INIT=$SEEDBOX_TCG_ASSETS/init
export SEEDBOX_TCG_FIRMWARE=$SEEDBOX_TCG_ASSETS/firmware
fixtures=$inputs/machineTcgTestFixtures
export SEEDBOX_TCG_SERVICE_INITRD=$fixtures/service-initrd
export SEEDBOX_TCG_CONTROL_INITRD=$fixtures/control-initrd
export SEEDBOX_TCG_DRIVER_CONTROL_APPLICATION=$fixtures/bin/driver-simulation-control
export SEEDBOX_TCG_PREPARED_FIXTURE=$fixtures/bin/prepared-fixture
export SEEDBOX_TCG_CLOCK_APPLICATION=$fixtures/bin/clock-faults
export SEEDBOX_TCG_TERMINAL_APPLICATION=$fixtures/bin/kernel-terminal
export SEEDBOX_TCG_NETWORK_APPLICATION=$fixtures/bin/network-probe
export SEEDBOX_TCG_DISK_PROBE=$fixtures/bin/disk-probe
export SEEDBOX_TCG_DISK_IMAGE=$fixtures/disk/ext4.img
# Shared x86 service scenarios retain the fixture package's PV-named inputs
# even when the selected execution backend is TCG.
export SEEDBOX_CPU_LOOP_FIXTURE=$inputs/machinePvTestFixtures/bin/cpu-loop
export SEEDBOX_PV_CLOCK_APPLICATION=$inputs/machinePvTestFixtures/bin/clock-faults
export SEEDBOX_PV_NETWORK_APPLICATION=$inputs/machinePvTestFixtures/bin/network-probe
# Public-service scenarios consume the same complete retained runtime package.
cargo xtask build-assets >/dev/null
if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  exec bash harness/check.sh machine-tcg "$@"
fi
