# Build context is the repository root, so the image can use the SDK crate.
FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY Cargo.toml ./
COPY crates/seedbox-sdk crates/seedbox-sdk
COPY evals/lazyinit evals/lazyinit
RUN cd evals/lazyinit && cargo build --release --locked
FROM scratch
COPY --from=build /source/evals/lazyinit/target/release/lazyinit-app /lazyinit-app
COPY --from=build /source/evals/lazyinit/target/release/lazyinit-oracle /lazyinit-oracle
