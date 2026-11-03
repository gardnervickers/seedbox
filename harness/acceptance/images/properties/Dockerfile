# Built from the repository root (see Dockerfile.dockerignore) so the probe
# links the workspace's own guest library.
FROM docker.io/library/rust:1-alpine AS build
COPY Cargo.toml /src/Cargo.toml
COPY crates/seedbox-guest /src/crates/seedbox-guest
COPY harness/acceptance/images/properties /src/harness/acceptance/images/properties
WORKDIR /src/harness/acceptance/images/properties
RUN cargo build --release --locked

FROM docker.io/library/busybox:1.37
COPY --from=build /src/harness/acceptance/images/properties/target/release/property-probe /property-probe
ENTRYPOINT ["/property-probe"]
