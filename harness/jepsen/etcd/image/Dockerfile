# etcd 3.4.3 for linux/arm64. The official v3.4.3-arm64 tag holds aarch64 binaries
# but labels its image config amd64, which Seedbox rejects; copy its unchanged
# static binaries into a correctly labelled image.
FROM gcr.io/etcd-development/etcd:v3.4.3-arm64@sha256:7a49fa1974d5c562f7732ac378cf094d3d7c38c4b6a82c96955a6bf7bcfa951d AS etcd
FROM scratch
COPY --from=etcd /usr/local/bin/etcd /usr/local/bin/etcdctl /usr/local/bin/
ENV ETCD_UNSUPPORTED_ARCH=arm64
ENTRYPOINT ["/usr/local/bin/etcd"]
