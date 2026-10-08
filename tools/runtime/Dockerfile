# Linux is the build environment; Nix owns guest and filesystem-tool artifacts.
FROM docker.io/nixos/nix:2.28.5 AS assets
WORKDIR /src
ENV NIX_CONFIG="experimental-features = nix-command flakes"
RUN printf '\nfilter-syscalls = false\n' >> /etc/nix/nix.conf
COPY flake.nix flake.lock ./
COPY nix ./nix
COPY kernel/source.json ./kernel/source.json
COPY tools/runtime/filesystem.py ./tools/runtime/filesystem.py
# Tool dependencies remain cached when guest sources change.
RUN nix --max-jobs 1 --cores 2 build .#preparationImage --out-link /preparation-image
COPY guest ./guest
COPY kernel ./kernel
COPY kernel-arm64 ./kernel-arm64
COPY tools/runtime/build-assets.sh ./tools/runtime/build-assets.sh
COPY tools/linux-runtime/package.py ./tools/linux-runtime/package.py
RUN nix --max-jobs 1 --cores 2 build .#runtimeAssets --out-link /guest-assets \
 && mkdir /assets && cp -L /guest-assets/* /assets/ \
 && cp /preparation-image /assets/tools-image.tar
