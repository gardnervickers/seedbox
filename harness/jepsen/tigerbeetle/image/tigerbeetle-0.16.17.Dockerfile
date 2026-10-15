# TigerBeetle 0.16.17 for Seedbox: the official image (the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:0.16.17, index sha256:79dcc6095b6dbd600c4e9e958da2bbe794dfbfd78baff30ac4b6f2f948fb9b08) plus the static
# seedbox-jepsen binary, whose tb-node agent runs the replica. The build context
# is the repository root, restricted by the .dockerignore beside this file.
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:1e872730771a04965bc8378959f0d37011d3bd0f5479dd1ba546480e8a555ab3
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
