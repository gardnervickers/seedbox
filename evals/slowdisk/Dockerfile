# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/slowdisk evals/slowdisk
RUN cd evals/slowdisk && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/slowdisk/target/release/slowdisk-server /slowdisk-server
COPY --from=build /source/evals/slowdisk/target/release/slowdisk-oracle /slowdisk-oracle
