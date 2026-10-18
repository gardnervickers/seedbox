#!/usr/bin/env bash
# Build the keel-s3dd image the keel scenarios name, through the engine at
# DOCKER_HOST, from the committed tree of a keel checkout (uncommitted changes
# are not included):
#
#   harness/jepsen/keel/image/build.sh [KEEL_CHECKOUT]   # default ../keel
#
# keel's workspace has private git dependencies (dev-dependencies, but cargo
# resolves the whole workspace), so the host vendors every crate into the
# context first and the build runs offline.
# Tags localhost/seedbox-jepsen/keel-s3dd:current and :<keel revision>.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
keel="${1:-$here/../../../../../keel}"
keel="$(cd "$keel" && pwd)"
revision="$(git -C "$keel" rev-parse --short=12 HEAD)"
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
git -C "$keel" archive --format=tar HEAD | tar -x -C "$context"
mkdir -p "$context/.cargo" "$context/.seedbox"
(cd "$context" && cargo vendor --locked --versioned-dirs .seedbox/vendor) > "$context/.cargo/config.toml"
cp "$here/Dockerfile" "$context/.seedbox/Dockerfile"
image=localhost/seedbox-jepsen/keel-s3dd
podman --url "$DOCKER_HOST" build --platform linux/arm64 --build-arg "KEEL_REVISION=$revision" \
  -f "$context/.seedbox/Dockerfile" -t "$image:current" -t "$image:$revision" "$context"
echo "BUILT: $image:current ($revision)"
