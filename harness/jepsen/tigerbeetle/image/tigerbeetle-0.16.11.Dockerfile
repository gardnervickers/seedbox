# TigerBeetle 0.16.11 for Seedbox: the official image (the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:0.16.11, index sha256:9a3366f96f27d53dedbe0459b4b30b6a5b5207a26eef66e9b73e92f35fe10a0b) plus the static
# seedbox-jepsen binary, whose tb-node agent runs the replica. The build context
# is the repository root, restricted by the .dockerignore beside this file.
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:3d64267205b20a9b65acfd32be972e12415889e7a70d33d42410735f5b735714
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
