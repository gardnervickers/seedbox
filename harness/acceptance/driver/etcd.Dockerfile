FROM docker.io/library/golang:1.26-alpine AS build
WORKDIR /source
COPY etcd/go.mod etcd/go.sum ./
RUN go mod download
COPY etcd/main.go ./
RUN CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -trimpath -o /client .
FROM docker.io/library/python:3.13-alpine
WORKDIR /scenario
COPY --from=build /client /client
COPY seedbox ./seedbox
COPY etcd.py ./
ENV PYTHONUNBUFFERED=1
ENTRYPOINT ["python3", "/scenario/etcd.py"]
