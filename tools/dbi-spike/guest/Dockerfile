FROM docker.io/library/node:22-bookworm-slim
COPY sdk /opt/dynamorio
COPY libbudget.so workload workload.js spinlock /experiment/
ENTRYPOINT ["/opt/dynamorio/bin64/drrun", "-c", "/experiment/libbudget.so", "--"]
CMD ["/experiment/workload", "200000000"]
