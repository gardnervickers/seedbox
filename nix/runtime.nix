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
  inputs = builtins.fromJSON (builtins.readFile ../tools/runtime/inputs.json);
  runtimeSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions (map (name: ../. + "/${name}") inputs.common);
  };
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
  runtimeArchive = pkgs.runCommand "seedbox-runtime-${pkgs.stdenv.hostPlatform.parsed.cpu.name}.tar.gz" {
    nativeBuildInputs = [ pkgs.python3 ];
  } ''
    python3 ${../tools/runtime/package.py} ${runtimeAssets} ${preparationImage} "$out"
  '';
  sourceDirectory = pkgs.runCommand "seedbox-runtime-source" { } ''
    mkdir -p "$out/upstream"
    cp -R ${runtimeSource}/. "$out/"
    cp ${pkgs.pkgsStatic.busybox.src} "$out/upstream/busybox.source"
    cp ${pkgs.pkgsStatic.e2fsprogs.src} "$out/upstream/e2fsprogs.source"
    cp ${pkgs.pkgsStatic.stdenv.cc.libc.src} "$out/upstream/musl.source"
    cat > "$out/flake.nix" <<'EOF'
    {
      inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
      outputs = { nixpkgs, ... }: {
        packages = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ]
          (system: import ./nix/runtime.nix { pkgs = import nixpkgs { inherit system; }; });
      };
    }
    EOF
  '';
  runtimeSources = pkgs.runCommand "seedbox-runtime-source.tar.gz" {
    nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ];
  } ''
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
      --mode='u=rwX,go=rX' -cf - -C ${sourceDirectory} . | gzip -n > "$out"
  '';
in {
  inherit runtimeAssets preparationImage runtimeArchive runtimeSources;
}
