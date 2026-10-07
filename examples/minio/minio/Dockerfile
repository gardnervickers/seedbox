FROM scratch
COPY --chmod=0755 minio /minio
COPY rootfs/ /
ENTRYPOINT ["/minio"]
