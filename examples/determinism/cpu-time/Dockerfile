FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY cputime.c ./
RUN mkdir /out && cc -O2 -std=c11 -static -pthread cputime.c -o /out/cputime

FROM scratch
COPY --from=build /out/ /
ENTRYPOINT ["/cputime"]
