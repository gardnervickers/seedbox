# The static oracle and client binary for Jepsen-style scenarios. Run
# `seedbox-jepsen build-oracle`, which assembles the build context (this crate,
# its path dependencies and vendored sources) and exports the binary.
FROM docker.io/library/rust:1.99.0-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/target \
    cargo build --release --locked --offline --manifest-path crates/seedbox-jepsen/Cargo.toml --target-dir /target \
    && cp /target/release/seedbox-jepsen /seedbox-jepsen

FROM scratch
COPY --from=build /seedbox-jepsen /seedbox-jepsen
