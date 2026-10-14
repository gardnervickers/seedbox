# The image every Jepsen oracle and Rust client machine runs: a small base plus
# the static binary `seedbox-jepsen build-oracle` writes to build/jepsen/ARCH.
# Scenario files are copied read-only into guests, so the binary comes in an
# image layer. The build context is the repository root, restricted by
# guest.Dockerfile.dockerignore to that binary.
FROM docker.io/library/busybox:1.37.0-musl@sha256:5cec3fc171c87218698e85a52af7087de727372aae264a787b8112901a5b0092
ARG TARGETARCH
COPY build/jepsen/${TARGETARCH}/seedbox-jepsen /jepsen/seedbox-jepsen
