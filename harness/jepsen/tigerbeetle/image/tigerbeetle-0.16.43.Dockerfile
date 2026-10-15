# TigerBeetle 0.16.43 for Seedbox: the official image (the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:0.16.43, index sha256:aac07eea19a720580a54cbfaba4bf5f90b7c53bfc0805996860bf0c6c26b6ce7) plus the static
# seedbox-jepsen binary, whose tb-node agent runs the replica. The build context
# is the repository root, restricted by the .dockerignore beside this file.
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:593479dbe5aef42c1ee931fee2e99de584a2f3c2483afa6493259c2406bc4417
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
