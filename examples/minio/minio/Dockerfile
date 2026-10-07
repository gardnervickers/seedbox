FROM scratch
COPY --chmod=0755 minio /minio
COPY rootfs/ /
# The embedded mc package resolves its default config directory during init,
# before MC_CONFIG_DIR is parsed; HOME avoids go-homedir's getent fallback.
ENV HOME=/root MC_CONFIG_DIR=/root/.mc
ENTRYPOINT ["/minio"]
