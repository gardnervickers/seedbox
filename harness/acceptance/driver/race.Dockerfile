FROM docker.io/library/alpine:3.23 AS build
RUN apk add --no-cache build-base
COPY race.c /race.c
RUN cc -O2 -static -pthread -o /race /race.c
FROM docker.io/library/python:3.13-alpine
COPY --from=build /race /race
COPY race.py /race.py
ENTRYPOINT ["python3", "/race.py"]
