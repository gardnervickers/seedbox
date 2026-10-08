{ pkgs }:
let
  inherit (pkgs) lib;
  inputs = builtins.fromJSON (builtins.readFile ../kernel/inputs.json);
  source = builtins.fromJSON (builtins.readFile ../kernel/source.json);
  kernelSource = pkgs.fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${source.version}.tar.xz";
    sha256 = source.sha256;
  };
  architecture = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
  sourceTree = names: lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions (map (name: ../. + "/${name}") names);
  };
  nativeSource = sourceTree (inputs.common ++ inputs.${architecture});
  allSource = sourceTree (lib.unique (lib.concatLists (builtins.attrValues inputs)));
  archive = name: directory: pkgs.runCommand name {
    nativeBuildInputs = [ pkgs.gnutar pkgs.gzip ];
  } ''
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
      --mode='u=rwX,go=rX' -cf - -C ${directory} . | gzip -n > "$out"
  '';
  guestKernel = pkgs.runCommand "seedbox-kernel-${architecture}" {
    nativeBuildInputs = with pkgs; [
      stdenv.cc binutils curl python3 gnumake bc bison flex perl xz util-linux
    ] ++ lib.optionals pkgs.stdenv.hostPlatform.isx86_64
      (with pkgs; [ pkg-config elfutils elfutils.dev openssl.dev ]);
    SEEDBOX_KERNEL_SOURCE = kernelSource;
    BUILD_JOBS = "4";
  } ''
    cp -R ${nativeSource} source
    chmod -R u+w source
    cd source
    ${if pkgs.stdenv.hostPlatform.isAarch64 then ''
      SEEDBOX_ARM64_BUILD="$TMPDIR/kernel" bash kernel-arm64/build.sh --application --kernel-only "$out"
    '' else ''
      SEEDBOX_WORK="$TMPDIR/kernel" SEEDBOX_ARTIFACTS="$out" bash kernel/build.sh
    ''}
    mkdir -p "$out/licenses"
    tar -xOf ${kernelSource} linux-${source.version}/COPYING > "$out/licenses/COPYING"
    tar -xOf ${kernelSource} linux-${source.version}/LICENSES/preferred/GPL-2.0 > "$out/licenses/GPL-2.0"
    rm -f "$out/build-directory.txt" "$out/SHA256SUMS"
  '';
  sourceDirectory = pkgs.runCommand "seedbox-kernel-source" { } ''
    mkdir -p "$out"
    cp -R ${allSource}/. "$out/"
    cp ${kernelSource} "$out/linux-${source.version}.tar.xz"
    # A standalone source flake keeps CLI and helper sources out of the recipe.
    cat > "$out/flake.nix" <<'EOF'
    {
      inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
      outputs = { nixpkgs, ... }: {
        packages = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ]
          (system: import ./nix/kernels.nix { pkgs = import nixpkgs { inherit system; }; });
      };
    }
    EOF
  '';
in {
  inherit guestKernel;
  guestKernelArchive = archive "seedbox-kernel-${architecture}.tar.gz" guestKernel;
  guestKernelSources = archive "seedbox-kernel-source.tar.gz" sourceDirectory;
}
