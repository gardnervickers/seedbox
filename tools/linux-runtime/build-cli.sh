#!/usr/bin/env bash
# Compatibility entrypoint. Cargo builds the CLI; guest assets download on first use.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build -p seedbox --release --locked
