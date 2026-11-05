FROM docker.io/library/alpine:3.23 AS build
RUN apk add --no-cache build-base
COPY faults.c /faults.c
RUN cc -O2 -static -pthread -o /faults /faults.c
FROM docker.io/library/python:3.13-alpine
COPY --from=build /faults /faults
