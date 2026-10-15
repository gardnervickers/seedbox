# TigerBeetle multiversion releases 0.16.25 to 0.16.32 for Seedbox upgrade scenarios:
# the official image of 0.16.25 (its /tigerbeetle) plus every release's
# /tigerbeetle under /opt/tigerbeetle/RELEASE/, each the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:RELEASE pinned by digest (image index digests
# in the comments). Each binary bundles its four previous releases. The tb-node
# agent copies the starting release onto the data volume and `install RELEASE`
# replaces it there; a production replica polls the binary and upgrades itself.
# The build context is the repository root, restricted by the .dockerignore
# beside this file.
# 0.16.25 index sha256:e5521c91c6a2a34a69f7c3719f917b157f096bf6059485721a3807334a55ecee
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:cddd292e9a2274cdf60d42eccc23db7f846253f31fdb58fe2b6b93f48bb68665 AS r25
# 0.16.26 index sha256:3d0b2076418941257f68126f47e31314641bb79d4398f18f0558aacf86f2696c
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:4e03b16592b7304b022969fe927160539bf4c49f84a3add7f0185dec3ad3b4b8 AS r26
# 0.16.27 index sha256:adeb2f720ce2b32e377863ae10bdbee336bed2d23a588c6ee7698b9b88ae35f3
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:4b0bb0eb6321886a8d62a7b65e13505f6362fa1a30824f8764b37901f47cf4c4 AS r27
# 0.16.28 index sha256:b45ee55c67cc63c14e975878ea4f72e9d6b6d919f2f5a81bc349d214f227c3e6
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:ee7cc291cad0be91991fc85d0fc0a5bb0d0f352453c8049dbb27004d85139d5c AS r28
# 0.16.29 index sha256:f38919324cdb4de2e2344afc4ce65380929a6272f44245fc02a246297e362571
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:ca4eb41788e8d4dac2190ff078e6b129f993311837f77092e19e983d10ee5152 AS r29
# 0.16.30 index sha256:ef322fad95ba1514796b4eacde113b97a5981678a1a9a35623d693208ac7b280
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:658436dbe19966f651aae0ced6a8e1b6ca715691379465570220dd58f851d9f8 AS r30
# 0.16.31 index sha256:f02c2c270d50225e28090a7be67af3af301dd4309b8ade448c9731f7250cd77e
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:ba4e85b89199f91e514be7f49093a95d3d30518cf9e57d0f76ae052003ae9f20 AS r31
# 0.16.32 index sha256:309323597bc202c7ef603ab3ce14f77aeb6fd7d6bde156348c6aae08be8fd69d
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:924bd693e97680c4f55b67e426f9a4330a1468e0bbd70265f13b978bbdf8094e AS r32
FROM r25
COPY --from=r25 /tigerbeetle /opt/tigerbeetle/0.16.25/tigerbeetle
COPY --from=r26 /tigerbeetle /opt/tigerbeetle/0.16.26/tigerbeetle
COPY --from=r27 /tigerbeetle /opt/tigerbeetle/0.16.27/tigerbeetle
COPY --from=r28 /tigerbeetle /opt/tigerbeetle/0.16.28/tigerbeetle
COPY --from=r29 /tigerbeetle /opt/tigerbeetle/0.16.29/tigerbeetle
COPY --from=r30 /tigerbeetle /opt/tigerbeetle/0.16.30/tigerbeetle
COPY --from=r31 /tigerbeetle /opt/tigerbeetle/0.16.31/tigerbeetle
COPY --from=r32 /tigerbeetle /opt/tigerbeetle/0.16.32/tigerbeetle
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
