FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY races.c ./
RUN mkdir /out && cc -O2 -std=c11 -static -pthread races.c -o /out/races

FROM scratch
COPY --from=build /out/ /
ENTRYPOINT ["/races"]
