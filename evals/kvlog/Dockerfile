# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/kvlog evals/kvlog
RUN cd evals/kvlog && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/kvlog/target/release/kvlog-server /kvlog-server
COPY --from=build /source/evals/kvlog/target/release/kvlog-oracle /kvlog-oracle
