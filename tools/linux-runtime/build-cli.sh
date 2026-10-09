#!/usr/bin/env bash
# Compatibility entrypoint. Cargo builds the CLI; build guest assets separately with tools/build-assets.sh.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build -p seedbox --release --locked
