# TigerBeetle 0.17.9 for Seedbox: the official image (the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:0.17.9, index sha256:48f623f9c1e9b6cc44d77ca93634595ae99cce3246ded418763eb1a62eee45e9) plus the static
# seedbox-jepsen binary, whose tb-node agent runs the replica. The build context
# is the repository root, restricted by the .dockerignore beside this file.
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:40c3fa149b98407257b7533ff69ca5dc34399be44d192ead48fabdafca4f4765
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
