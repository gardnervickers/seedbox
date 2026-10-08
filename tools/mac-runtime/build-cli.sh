#!/usr/bin/env bash
# Compatibility entrypoint. Normal Cargo builds perform asset preparation/signing.
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build -p seedbox --release --locked
