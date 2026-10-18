# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/compact evals/compact
RUN cd evals/compact && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/compact/target/release/compact-server /compact-server
COPY --from=build /source/evals/compact/target/release/compact-oracle /compact-oracle
