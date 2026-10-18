#!/usr/bin/env bash
# Validate a linked ELF64 helper's target and static linkage without executing it.
set -euo pipefail
if [[ $# != 2 ]]; then
  echo 'usage: guest-static-elf.sh EXECUTABLE ELF_MACHINE' >&2
  exit 2
fi
binary=$1
machine=$2
header=$(readelf -h -- "$binary")
if [[ ! "$header" =~ Machine:.*"$machine" || ! "$header" =~ Class:.*ELF64 ]]; then
  echo "$binary: expected ELF64 for $machine" >&2
  exit 1
fi
program_headers=$(readelf -l -- "$binary")
if [[ "$program_headers" == *INTERP* ]]; then
  echo "$binary: static guest helper must not contain INTERP" >&2
  exit 1
fi
dynamic_section=$(readelf -d -- "$binary")
if [[ "$dynamic_section" == *NEEDED* ]]; then
  echo "$binary: static guest helper must not contain NEEDED" >&2
  exit 1
fi
