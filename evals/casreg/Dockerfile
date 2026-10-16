# Build context is the repository root, so the image can use the SDK crates.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY crates/seedbox-check crates/seedbox-check
COPY evals/casreg evals/casreg
RUN cd evals/casreg && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/casreg/target/release/casreg-server /casreg-server
COPY --from=build /source/evals/casreg/target/release/casreg-oracle /casreg-oracle
