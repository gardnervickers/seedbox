#!/usr/bin/env bash
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$(realpath -m "$1")"
cd "$here/../examples/etcd/workload"
[ -f go.mod ] || { go mod init seedbox/client >/dev/null; go get go.etcd.io/etcd/client/v3@v3.5.17 go.uber.org/zap@v1.27.0 >/dev/null; }
CGO_ENABLED=0 go build -trimpath -ldflags "-s -w -buildid=" -o "$out" .
sha256sum "$out"
