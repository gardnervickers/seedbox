FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
COPY altstack.c /src/
RUN cc -O2 -std=gnu11 -static -pthread -Wall -Werror /src/altstack.c -o /altstack

FROM scratch
COPY --from=build /altstack /altstack
ENTRYPOINT ["/altstack"]
