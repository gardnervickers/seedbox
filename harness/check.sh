#!/usr/bin/env bash
# Named portable and system suites, implemented and tested in Rust.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo xtask check "$@"
