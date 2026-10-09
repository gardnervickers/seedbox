{ pkgs }:
let
  inherit (pkgs) lib;
  inputs = builtins.fromJSON (builtins.readFile ../tools/runtime/inputs.json);
  runtimeSource = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions (map (name: ../. + "/${name}") inputs.common);
  };
  # mke2fs needs UUID/block probing libraries, not util-linux's login tools.
  # Use e2fsprogs' bundled implementations for this static guest helper.
  filesystemTools = (pkgs.pkgsStatic.e2fsprogs.override {
    libuuid = null;
    withFuse = false;
  }).overrideAttrs (old: {
    configureFlags = map (flag:
      if flag == "--disable-libuuid" then "--enable-libuuid"
      else if flag == "--disable-libblkid" then "--enable-libblkid"
      else flag
    ) old.configureFlags;
  });
  progress = import ./progress.nix { inherit pkgs; };
  runtimeAssets = pkgs.runCommand "seedbox-guest-assets" {
    nativeBuildInputs = with pkgs; [ pkgsStatic.stdenv.cc binutils python3 ];
    SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/${pkgs.pkgsStatic.stdenv.cc.targetPrefix}cc";
    SEEDBOX_MKE2FS = "${filesystemTools.bin}/bin/mke2fs";
    SEEDBOX_E2FSPROGS_SOURCE = pkgs.pkgsStatic.e2fsprogs.src;
    SEEDBOX_MUSL_SOURCE = pkgs.pkgsStatic.stdenv.cc.libc.src;
  } ''
    cp -R ${runtimeSource} source
    chmod -R u+w source
    cd source
    bash tools/runtime/build-assets.sh "$out"
    cp ${progress} "$out/progress.tar"
  '';
  runtimeArchive = pkgs.runCommand "seedbox-runtime-${pkgs.stdenv.hostPlatform.parsed.cpu.name}.tar.gz" {
    nativeBuildInputs = [ pkgs.python3 ];
  } ''
    python3 ${../tools/runtime/package.py} ${runtimeAssets} "$out"
  '';
  sourceDirectory = pkgs.runCommand "seedbox-runtime-source" { } ''
    mkdir -p "$out/upstream"
    cp -R ${runtimeSource}/. "$out/"
    cp ${pkgs.pkgsStatic.e2fsprogs.src} "$out/upstream/e2fsprogs.source"
    cp ${pkgs.pkgsStatic.stdenv.cc.libc.src} "$out/upstream/musl.source"
    cp ${progress.sdk} "$out/upstream/dynamorio-sdk.tar.gz"
    cp ${progress.glibcSource} "$out/upstream/glibc.source"
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
  inherit filesystemTools runtimeAssets runtimeArchive runtimeSources;
}
