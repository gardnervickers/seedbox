# Build context is the repository root, so the image can use the SDK crates.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY crates/seedbox-check crates/seedbox-check
COPY evals/retry evals/retry
RUN cd evals/retry && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/retry/target/release/retry-server /retry-server
COPY --from=build /source/evals/retry/target/release/retry-oracle /retry-oracle
