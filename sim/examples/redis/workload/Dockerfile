FROM docker.io/library/rust:1-alpine AS build
WORKDIR /source
COPY main.rs ./
RUN rustc --edition=2021 -C opt-level=2 -C target-feature=+crt-static main.rs -o /client
FROM scratch
COPY --from=build /client /client
ENTRYPOINT ["/client"]
