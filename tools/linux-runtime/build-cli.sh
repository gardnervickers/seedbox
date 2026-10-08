#!/usr/bin/env bash
# Compatibility entrypoint. Cargo now owns guest assets on both platforms.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build -p seedbox --release --locked
