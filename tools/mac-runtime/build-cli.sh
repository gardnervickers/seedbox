#!/usr/bin/env bash
# Compatibility entrypoint; the CLI build is the same on every platform.
exec bash "$(dirname "$0")/../linux-runtime/build-cli.sh" "$@"
