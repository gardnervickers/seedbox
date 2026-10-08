{ pkgs }:
let
  inherit (pkgs) lib;
  preparationTools = pkgs.buildEnv {
    name = "seedbox-preparation-tools";
    paths = with pkgs; [ skopeo umoci python3Minimal gnutar coreutils ];
    pathsToLink = [ "/bin" ];
  };
  imageRoot = pkgs.runCommand "seedbox-filesystem-root" { } ''
    mkdir -p $out/tmp
    ln -s ${preparationTools} $out/tools
    cp ${../tools/runtime/filesystem.py} $out/filesystem.py
  '';
  runtimeSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../guest ../kernel-arm64/supervisor-io.h ../flake.lock
      ../tools/runtime/build-assets.sh ../tools/linux-runtime/package.py
    ];
  };
in {
  preparationImage = pkgs.dockerTools.buildLayeredImage {
    name = "seedbox-filesystem";
    compressor = "none";
    contents = [ imageRoot ];
    # Store normalization removes directory write bits; set image permissions here.
    extraCommands = "chmod 1777 tmp";
    config = {
      Env = [ "PATH=/tools/bin" "HOME=/tmp" ];
      Entrypoint = [ "/tools/bin/sleep" "infinity" ];
    };
  };
  runtimeAssets = pkgs.runCommand "seedbox-guest-assets" {
    nativeBuildInputs = with pkgs; [ pkgsStatic.stdenv.cc binutils python3 ];
    SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/${pkgs.pkgsStatic.stdenv.cc.targetPrefix}cc";
    SEEDBOX_BUSYBOX = "${pkgs.pkgsStatic.busybox}/bin/busybox";
    SEEDBOX_MKE2FS = "${pkgs.pkgsStatic.e2fsprogs.bin}/bin/mke2fs";
    SEEDBOX_BUSYBOX_SOURCE = pkgs.pkgsStatic.busybox.src;
    SEEDBOX_E2FSPROGS_SOURCE = pkgs.pkgsStatic.e2fsprogs.src;
    SEEDBOX_MUSL_SOURCE = pkgs.pkgsStatic.stdenv.cc.libc.src;
  } ''
    cp -R ${runtimeSource} source
    chmod -R u+w source
    cd source
    bash tools/runtime/build-assets.sh "$out"
  '';
}
