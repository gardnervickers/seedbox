# keel-s3dd for linux/arm64, built from a keel checkout with its pinned
# toolchain (rust-toolchain.toml: 1.96.1). keel has no public remote, so a
# Seedbox scenario cannot name a build context for it: build.sh exports the
# checkout's committed tree (git archive), builds this file against it through
# the engine and tags the result localhost/seedbox-jepsen/keel-s3dd:current,
# which the scenarios name. Seedbox resolves that local tag to the image ID
# when it prepares a run; replay uses the retained image, never the tag.
FROM docker.io/library/rust:1.96.1-slim-bookworm@sha256:e18a79fc84dfcfc3ab5ba72290398a644c135c97eaa881447fddc354ee4701a3 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p keel-s3dd \
    && cp target/release/keel-s3dd /keel-s3dd

FROM docker.io/library/debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
COPY --from=builder /keel-s3dd /usr/local/bin/keel-s3dd
ARG KEEL_REVISION=unknown
LABEL org.opencontainers.image.revision=$KEEL_REVISION
ENTRYPOINT ["/usr/local/bin/keel-s3dd"]
