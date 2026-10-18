#!/usr/bin/env bash
# Build the delores jepsen-node image the delores-node scenarios name, through
# the engine at DOCKER_HOST, with the checkout's own
# examples/jepsen-node/Dockerfile against its committed tree:
#
#   harness/jepsen/delores/node-image/build.sh DELORES_CHECKOUT [REV]
#
# REV defaults to HEAD. Tags localhost/seedbox-jepsen/delores-jepsen-node:current
# and :<revision>. See ../image/build.sh for why scenarios name a local tag.
set -euo pipefail
delores="$(cd "$1" && pwd)"
revision="$(git -C "$delores" rev-parse --short=12 "${2:-HEAD}")"
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
git -C "$delores" archive --format=tar "$revision" | tar -x -C "$context"
image=localhost/seedbox-jepsen/delores-jepsen-node
podman --url "$DOCKER_HOST" build --platform linux/arm64 \
  --label "org.opencontainers.image.revision=$revision" \
  -f "$context/examples/jepsen-node/Dockerfile" -t "$image:current" -t "$image:$revision" "$context"
echo "BUILT: $image:current ($revision)"
