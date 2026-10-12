FROM docker.io/library/alpine:3.22 AS build
RUN apk add --no-cache build-base
WORKDIR /source
COPY server.c .
RUN cc -O2 -std=c11 -static -Wall -Wextra -Werror server.c -o /server
FROM scratch
COPY --from=build /server /server
ENTRYPOINT ["/server"]
