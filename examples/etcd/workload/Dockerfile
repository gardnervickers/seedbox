FROM docker.io/library/golang:1.26-alpine AS build
WORKDIR /source
COPY go.mod go.sum ./
RUN go mod download
COPY main.go ./
RUN CGO_ENABLED=0 GOOS=linux go build -trimpath -o /client .
FROM scratch
COPY --from=build /client /client
ENTRYPOINT ["/client"]
