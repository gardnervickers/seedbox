# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/bitrot evals/bitrot
RUN cd evals/bitrot && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/bitrot/target/release/bitrot-server /bitrot-server
COPY --from=build /source/evals/bitrot/target/release/bitrot-oracle /bitrot-oracle
