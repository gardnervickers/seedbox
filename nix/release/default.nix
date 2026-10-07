# Draft artifact recipe. Evaluation is safe; building requires a separate gate.
# Preserve the two Cargo workspaces, VMM release profile and build-vmm alias.
{ pkgs, sourceRevision, nixpkgsRevision }:
assert builtins.match "[0-9a-f]{40}" sourceRevision != null;
let
  inherit (pkgs) lib;
  source = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../.cargo ../../Cargo.toml ../../Cargo.lock ../../flake.lock
      ../../crates/seedbox/Cargo.toml ../../crates/seedbox/build.rs ../../crates/seedbox/README.md
      ../../crates/seedbox/src ../../crates/seedbox/templates
      ../../crates/seedbox-control/Cargo.toml ../../crates/seedbox-control/src
      ../../crates/seedbox-vmm/Cargo.toml ../../crates/seedbox-vmm/Cargo.lock
      ../../crates/seedbox-vmm/src
      ../../guest/compose-init.c ../../guest/control.h ../../kernel
      ../../examples/etcd ../../examples/redis ../../tools/release ../../nix/release
    ];
  };
  # A fixed source name gives /build/seedbox, including env!(CARGO_MANIFEST_DIR).
  src = builtins.path { path = source; name = "seedbox"; };
  common = {
    inherit src;
    version = (builtins.fromTOML (builtins.readFile ../../Cargo.toml)).workspace.package.version;
    sourceRoot = "seedbox";
    auditable = false;
    dontStrip = true;
    dontFixup = true;
    doCheck = false; # Source tests are separate; no implicit guest execution.
    RUSTFLAGS = "--remap-path-prefix=/build=/seedbox-build";
    env = {
      SOURCE_DATE_EPOCH = "0";
      CARGO_HOME = "/build/cargo-home";
      HOME = "/build/home";
      TMPDIR = "/build/tmp";
      CARGO_NET_OFFLINE = "true";
    };
    preConfigure = ''
      test "$PWD" = /build/seedbox
      mkdir -p "$CARGO_HOME" "$HOME" "$TMPDIR"
    '';
    meta.platforms = [ "x86_64-linux" ];
  };
  cli = pkgs.rustPlatform.buildRustPackage (common // {
    pname = "seedbox-cli-draft";
    cargoLock.lockFile = ../../Cargo.lock;
    nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc pkgs.binutils ];
    env = common.env // {
      SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/x86_64-unknown-linux-musl-cc";
    };
    buildPhase = ''
      runHook preBuild
      cargo build --release --locked --offline -j 2
      runHook postBuild
    '';
    installPhase = ''
      mkdir -p "$out/bin" "$out/libexec"
      cp target/release/seedbox "$out/bin/seedbox"
      mapfile -t init < <(find target/release/build -path '*/out/compose-init' -type f | sort)
      test "''${#init[@]}" = 1
      # Fail if the embedding compiler silently fell back to no supervisor.
      ! readelf -l "''${init[0]}" | grep -q INTERP
      cp "''${init[0]}" "$out/libexec/compose-init"
    '';
  });
  vmm = pkgs.rustPlatform.buildRustPackage (common // {
    pname = "seedbox-vmm-draft";
    cargoLock.lockFile = ../../crates/seedbox-vmm/Cargo.lock;
    cargoRoot = "crates/seedbox-vmm"; # Vendor hook checks the standalone lock.
    buildPhase = ''
      runHook preBuild
      cargo build-vmm --offline -j 2
      runHook postBuild
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp crates/seedbox-vmm/target/release/seedbox-vmm "$out/bin/seedbox-vmm"
    '';
  });
  kernelVersion = "6.12.112";
  kernelHash = "164dc9d1f6c93c61a15e1f071c48379b467f2b17c469cce7223471968208ed03";
  tarball = pkgs.fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${kernelVersion}.tar.xz";
    sha256 = kernelHash;
  };
  kernel = pkgs.stdenv.mkDerivation {
    pname = "seedbox-kernel-draft";
    version = kernelVersion;
    inherit src;
    sourceRoot = "seedbox";
    nativeBuildInputs = with pkgs; [ bash python3 gnumake bc bison flex perl pkg-config ];
    buildInputs = [ pkgs.elfutils pkgs.openssl ];
    dontFixup = true;
    env = {
      ARCH = "x86";
      SOURCE_DATE_EPOCH = "0";
      KBUILD_BUILD_VERSION = "1";
      LOCALVERSION = "";
      KCFLAGS = "-ffile-prefix-map=/build=/seedbox-build";
      KAFLAGS = "-ffile-prefix-map=/build=/seedbox-build";
      HOSTCFLAGS = "-O2 -ffile-prefix-map=/build=/seedbox-build";
      HOSTCXXFLAGS = "-O2 -ffile-prefix-map=/build=/seedbox-build";
    };
    buildPhase = ''
      test "$PWD" = /build/seedbox
      export HOME=/build/home TMPDIR=/build/tmp
      export SEEDBOX_WORK=/build/kernel-work SEEDBOX_KBUILD=/build/kernel-build
      export SEEDBOX_ARTIFACTS=/build/kernel-artifacts BUILD_JOBS=2
      export CC=cc HOSTCC=cc HOSTCXX=c++ LD=ld AR=ar NM=nm OBJCOPY=objcopy OBJDUMP=objdump
      mkdir -p "$HOME" "$TMPDIR" "$SEEDBOX_WORK/src"
      # Fresh Nix sandbox: no reused kernel-out, network fetch or source checkout.
      test ! -e "$SEEDBOX_KBUILD"
      cp ${tarball} "$SEEDBOX_WORK/src/linux-${kernelVersion}.tar.xz"
      # Linux scripts/config uses /usr/bin/env, absent from the Nix sandbox.
      # Adjust only its three calls in the shared builder; other sources stay raw.
      substituteInPlace kernel/build.sh \
        --replace-fail '"$src/scripts/config"' '"${pkgs.bash}/bin/bash" "$src/scripts/config"'
      ${pkgs.bash}/bin/bash kernel/build.sh
    '';
    installPhase = ''
      mkdir -p "$out/share/seedbox"
      cp /build/kernel-artifacts/{bzImage,kernel.config} "$out/share/seedbox/"
    '';
    meta.platforms = [ "x86_64-linux" ];
  };
  busybox = pkgs.pkgsStatic.busybox;
  # Canonicalize registration order as well as tar traversal. Nix closureInfo
  # owns the graph/format; retain its fields with sorted paths and references.
  closure = (pkgs.closureInfo { rootPaths = [ cli vmm kernel busybox ]; }).overrideAttrs {
    buildCommand = ''
      out="''${outputs[out]}"
      mkdir "$out"
      jq -r '.closure | map(.narSize) | add' < "$NIX_ATTRS_JSON_FILE" > "$out/total-nar-size"
      jq -r '.closure | sort_by(.path) | map(.references |= sort) | map([.path, .narHash, .narSize, "", (.references | length)] + .references) | add | map("\(.)\n") | add' < "$NIX_ATTRS_JSON_FILE" | head -n -1 > "$out/registration"
      jq -r '.closure | sort_by(.path) | .[].path' < "$NIX_ATTRS_JSON_FILE" > "$out/store-paths"
    '';
  };
  provenance = pkgs.writeText "seedbox-release-provenance.json" (builtins.toJSON {
    format = 1;
    status = "UNQUALIFIED_DRAFT";
    platform = "x86_64-linux-gnu-nix-amd-zen";
    inherit sourceRevision nixpkgsRevision kernelVersion kernelHash;
    source = toString src;
    flakeLockSha256 = builtins.hashFile "sha256" ../../flake.lock;
    cargoLockSha256 = builtins.hashFile "sha256" ../../Cargo.lock;
    vmmCargoLockSha256 = builtins.hashFile "sha256" ../../crates/seedbox-vmm/Cargo.lock;
    recipes = { cli = "cargo build --release --locked --offline"; vmm = "cargo build-vmm --offline"; kernel = "pinned bash kernel/build.sh; scripts/config via pinned bash"; };
    toolchain = {
      rustc = toString pkgs.rustc; cargo = toString pkgs.cargo;
      cc = toString pkgs.stdenv.cc; staticCc = toString pkgs.pkgsStatic.stdenv.cc;
      bash = toString pkgs.bash; binutils = toString pkgs.binutils; busybox = toString busybox;
    };
    inputs = {
      sourceRoot = "/build/seedbox"; rustRemap = common.RUSTFLAGS;
      sourceDateEpoch = 0; kernelBuildVersion = 1; localVersion = "";
      kernelBuildUser = "seedbox"; kernelBuildHost = "seedbox";
      kernelBuildTimestamp = "Thu Jan 1 00:00:00 UTC 1970";
      nativeTarget = "x86_64-unknown-linux-gnu";
      bootstrapTarget = "x86_64-unknown-linux-musl";
      bootstrapFlags = [ "-O2" "-static" "-Wall" "-Wextra" "-Werror" ];
      kernelConfigurationHelperShell = "${pkgs.bash}/bin/bash";
      kernelPathMap = "-ffile-prefix-map=/build=/seedbox-build";
      cargoProfile = "release (unaltered)"; strip = false; cargoAuditable = false; buildJobs = 2;
    };
  });
in pkgs.runCommand "seedbox-release-draft" {
  nativeBuildInputs = [ pkgs.python3 ];
  passthru = { inherit cli vmm kernel closure provenance; };
  meta.platforms = [ "x86_64-linux" ];
} ''
  umask 022
  mkdir -p payload/{bin,libexec,share/seedbox} "$out"
  cp ${cli}/bin/seedbox ${vmm}/bin/seedbox-vmm payload/bin/
  cp ${cli}/libexec/compose-init ${busybox}/bin/busybox payload/libexec/
  cp ${kernel}/share/seedbox/{bzImage,kernel.config} payload/share/seedbox/
  cp ${provenance} payload/provenance.json
  cp ${closure}/registration payload/nix-registration
  cp ${closure}/store-paths payload/runtime-store-paths
  python3 ${../../tools/release/package.py} --payload payload \
    --store-paths ${closure}/store-paths --output "$out"
''
