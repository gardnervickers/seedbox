FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY check.c wait.c exclusive-*.S ./
RUN mkdir /out && cc -O2 -std=c11 -static check.c -o /out/check && \
    if cc -dM -E - </dev/null | grep -q '__aarch64__'; then \
      for fixture in epoch overlap consume discard fault; do \
        cc -nostdlib -static -Wl,--build-id=none \
          exclusive-${fixture}.S -o /out/exclusive-${fixture} || exit 1; \
      done; \
      cc -nostdlib -static -Wl,--build-id=none -Wa,--defsym,PAIR=1 \
        exclusive-fault.S -o /out/exclusive-fault-pair && \
      cc -nostdlib -static -Wl,--build-id=none -Wa,--defsym,STOLEN=1 \
        exclusive-fault.S -o /out/exclusive-fault-stolen && \
      cc -O2 -std=c11 -static -pthread wait.c -o /out/wait; \
    fi

FROM scratch
COPY --from=build /out/ /
ENTRYPOINT ["/check"]
