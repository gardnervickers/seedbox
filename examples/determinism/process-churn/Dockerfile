FROM alpine:3.22 AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY churn.c ./
RUN cc -O2 -std=c11 -static -pthread -Wall -Wextra churn.c -o /churn

# Not scratch: phase a runs /bin/sh, seq, sort, uniq and wc from busybox.
FROM alpine:3.22
COPY --from=build /churn /churn
ENTRYPOINT ["/churn"]
