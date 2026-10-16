#!/usr/bin/env bash
# Usage: batch.sh WORLD OUTPREFIX "seed seed ..." [extra VMM args]
# Env: VMM, SEEDBOX_ROOT, SEEDBOX_TIMEOUT (seconds). SEEDBOX_ROOT
# defaults to this checkout; seedbox-batch owns the other defaults.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
checkout=$(cd "$here/.." && pwd)
# Resolve relative SEEDBOX_ROOT against the caller's cwd, as before.
run_root=$(cd "${SEEDBOX_ROOT:-$checkout}" && pwd)
# Build as the ordinary caller, before the existing privilege boundary.
# Cargo reads this checkout's .cargo/config.toml by its working directory.
cd "$checkout"
cargo build --manifest-path "$checkout/Cargo.toml" -p seedbox-test \
  --bin seedbox-batch --release --locked --target-dir "$checkout/target"
cd "$run_root"
# sudo drops the environment, so forward the nonempty settings the caller gave.
forward=(SEEDBOX_ROOT="$PWD")
for name in VMM SEEDBOX_TIMEOUT; do
  if [ -n "${!name:-}" ]; then forward+=("$name=${!name}"); fi
done
# The privileged supervisor owns/reaps every VMM and its descendants.
# sudo is only its outer launcher.
exec sudo -n env "${forward[@]}" "$checkout/target/release/seedbox-batch" "$@"
