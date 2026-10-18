# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/replog evals/replog
RUN cd evals/replog && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/replog/target/release/replog-replica /replog-replica
COPY --from=build /source/evals/replog/target/release/replog-oracle /replog-oracle
