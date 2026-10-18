FROM docker.io/library/alpine:3.22 AS build
RUN apk add --no-cache build-base linux-headers
COPY runtime-probe.c /runtime-probe.c
RUN cc -static -O2 -Wall -Wextra -Werror /runtime-probe.c -o /runtime-probe -lrt

FROM docker.io/library/busybox:1.37
COPY --from=build /runtime-probe /runtime-probe
RUN mkdir /data && chmod 0777 /data
ENTRYPOINT ["/runtime-probe", "runtime"]
