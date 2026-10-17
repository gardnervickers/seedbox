# Image for harness/acceptance/workloads/clock-step.yaml: the cfztime probe on Alpine.
FROM docker.io/library/alpine:3.23 AS build
RUN apk add --no-cache build-base
COPY cfztime.c /cfztime.c
RUN cc -O2 -static -pthread -o /cfztime /cfztime.c
FROM docker.io/library/alpine:3.23
COPY --from=build /cfztime /cfztime
ENTRYPOINT ["/cfztime", "9100"]
