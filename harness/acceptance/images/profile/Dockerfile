# Built from the repository root (see Dockerfile.dockerignore) so the oracle
# links the workspace's own oracle library and the crates it depends on.
FROM docker.io/library/rust:1-alpine AS build
COPY Cargo.toml /src/Cargo.toml
COPY crates/seedbox-engine-core/build.rs /src/crates/seedbox-engine-core/build.rs
COPY crates/seedbox-engine-protocol /src/crates/seedbox-engine-protocol
COPY crates/seedbox-guest /src/crates/seedbox-guest
COPY crates/seedbox-oracle /src/crates/seedbox-oracle
COPY crates/seedbox-spec /src/crates/seedbox-spec
COPY harness/acceptance/images/profile /src/harness/acceptance/images/profile
WORKDIR /src/harness/acceptance/images/profile
RUN cargo build --release --locked

FROM docker.io/library/busybox:1.37
COPY --from=build /src/harness/acceptance/images/profile/target/release/profile-oracle /profile-oracle
ENTRYPOINT ["/profile-oracle"]
