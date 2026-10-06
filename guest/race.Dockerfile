FROM docker.io/library/alpine:3.23 AS build
RUN apk add --no-cache build-base
WORKDIR /source
COPY race.c .
RUN cc -O2 -static -pthread -o /race race.c
FROM docker.io/library/alpine:3.23
COPY --from=build /race /race
