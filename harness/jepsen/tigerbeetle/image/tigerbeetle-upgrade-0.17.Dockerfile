# TigerBeetle multiversion releases 0.17.6 to 0.17.9 for Seedbox production
# and upgrade scenarios on the current release line: the official image of
# 0.17.6 (its /tigerbeetle) plus every release's /tigerbeetle under
# /opt/tigerbeetle/RELEASE/, each the linux/arm64 manifest of
# ghcr.io/tigerbeetle/tigerbeetle:RELEASE pinned by digest (image index digests
# in the comments). The tb-node agent copies the starting release onto the
# data volume and `install RELEASE` replaces it there; a production replica
# polls the binary and upgrades itself. The build context is the repository
# root, restricted by the .dockerignore beside this file.
# 0.17.6 index sha256:2d4c789f3492ac674094c5eb10b67ee356fd490246b31928cb3a6aa62f5e2375
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:0c6a276b023ce33379b54b22a6852dfd6e8556128a5990b9f2af3103451fffd8 AS r6
# 0.17.7 index sha256:d431d365a24d0d626f699169803812e08bb95fc19677ce032b6775150654aae6
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:591de689186c979a45ecf4e46d82caabb04149402ed73eb261af2490e0ad82ed AS r7
# 0.17.8 index sha256:b140e2285fcc1ccd6a7a38a1e31ef13f98c3b511b29acc0a935d597546aa96ff
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:872019ac9955e3e53bd0b309fd786c4017a18c6fd8f142f09584446fba9d4830 AS r8
# 0.17.9 index sha256:48f623f9c1e9b6cc44d77ca93634595ae99cce3246ded418763eb1a62eee45e9
FROM ghcr.io/tigerbeetle/tigerbeetle@sha256:40c3fa149b98407257b7533ff69ca5dc34399be44d192ead48fabdafca4f4765 AS r9
FROM r6
COPY --from=r6 /tigerbeetle /opt/tigerbeetle/0.17.6/tigerbeetle
COPY --from=r7 /tigerbeetle /opt/tigerbeetle/0.17.7/tigerbeetle
COPY --from=r8 /tigerbeetle /opt/tigerbeetle/0.17.8/tigerbeetle
COPY --from=r9 /tigerbeetle /opt/tigerbeetle/0.17.9/tigerbeetle
COPY build/jepsen/arm64/seedbox-jepsen /jepsen/seedbox-jepsen
ENTRYPOINT ["/jepsen/seedbox-jepsen", "tb-node"]
