# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/bank evals/bank
RUN cd evals/bank && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/bank/target/release/bank-server /bank-server
COPY --from=build /source/evals/bank/target/release/bank-oracle /bank-oracle
