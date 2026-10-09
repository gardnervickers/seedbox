FROM docker.io/library/node:22-bookworm-slim AS loader
FROM quay.io/coreos/etcd:v3.5.2
# Supply the launcher dependencies without changing the existing etcd executable.
COPY --from=loader /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libc.so.6
COPY --from=loader /lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
COPY sdk /opt/dynamorio
COPY libbudget.so /experiment/libbudget.so
