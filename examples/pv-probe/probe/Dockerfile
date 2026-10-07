FROM docker.io/library/golang:1.26-alpine AS build
WORKDIR /source
COPY go.mod main.go ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -o /probe .
FROM scratch
COPY --from=build /probe /probe
ENTRYPOINT ["/probe"]
