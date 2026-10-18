#!/usr/bin/env bash
# Build the delores docker-kv image the delores scenarios name, through the
# engine at DOCKER_HOST, with the checkout's own examples/docker-kv/Dockerfile
# against its committed tree (uncommitted changes are not included):
#
#   harness/jepsen/delores/image/build.sh [DELORES_CHECKOUT]   # default ../delores
#
# A Seedbox scenario names only local build contexts, and the Dockerfile must
# sit inside its context, so the image is built here and the scenarios name
# its local tag; Seedbox resolves the tag to the image ID when it prepares a
# run, and replay uses the retained image.
# Tags localhost/seedbox-jepsen/delores-docker-kv:current and :<revision>.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
delores="${1:-$here/../../../../../delores}"
delores="$(cd "$delores" && pwd)"
revision="$(git -C "$delores" rev-parse --short=12 HEAD)"
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
git -C "$delores" archive --format=tar HEAD | tar -x -C "$context"
image=localhost/seedbox-jepsen/delores-docker-kv
podman --url "$DOCKER_HOST" build --platform linux/arm64 \
  --label "org.opencontainers.image.revision=$revision" \
  -f "$context/examples/docker-kv/Dockerfile" -t "$image:current" -t "$image:$revision" "$context"
echo "BUILT: $image:current ($revision)"
