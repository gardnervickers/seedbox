#!/usr/bin/env bash
set -euo pipefail
task_world="$(cd "$(dirname "$0")" && pwd)"
cargo fmt --manifest-path "$task_world/Cargo.toml" -- --check
cargo clippy --manifest-path "$task_world/Cargo.toml" --locked --offline --all-targets -j 8 -- -D warnings
cargo test --manifest-path "$task_world/Cargo.toml" --locked --offline -j 8
cargo build --manifest-path "$task_world/Cargo.toml" --locked --offline -j 8
