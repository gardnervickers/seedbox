# Guest assets and toolchains stay in this stage; no Seedbox/Rust is compiled.
FROM nixos/nix:2.28.5 AS assets
WORKDIR /src
ENV NIX_CONFIG="experimental-features = nix-command flakes"
RUN printf '\nfilter-syscalls = false\n' >> /etc/nix/nix.conf
# Keep dependency downloads/builds cached when guest sources change.
COPY flake.nix flake.lock ./
RUN nix --max-jobs 1 --cores 2 build .#preparationTools --out-link /preparation-tools
RUN nix --max-jobs 1 --cores 2 develop .#assets --command true
COPY guest ./guest
COPY kernel ./kernel
COPY kernel-arm64 ./kernel-arm64
COPY tools/runtime/build-assets.sh ./tools/runtime/build-assets.sh
COPY tools/linux-runtime/package.py ./tools/linux-runtime/package.py
RUN nix --max-jobs 1 --cores 2 develop .#assets --command bash tools/runtime/build-assets.sh /assets && rm -rf /tmp/kernel

# Only filesystem tools and runtime libraries: no Nix, compiler or second CLI.
FROM scratch
ENV PATH="/tools/bin" HOME="/tmp"
COPY --from=assets /tools-rootfs/ /
COPY tools/runtime/filesystem.py /filesystem.py
ENTRYPOINT ["/tools/bin/sleep", "infinity"]
