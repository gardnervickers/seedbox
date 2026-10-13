# Import with native aarch64 Linux packages or aarch64 pkgsCross on Linux.
{ pkgs }:
let
  source = builtins.fromJSON (builtins.readFile ./guest/kernel/source.json);
  archive = pkgs.fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${source.version}.tar.xz";
    sha256 = source.sha256;
  };
  prefix = pkgs.stdenv.cc.targetPrefix;
in pkgs.runCommand "seedbox-machine-pv-kernel-${source.version}" {
  nativeBuildInputs = [ pkgs.stdenv.cc ] ++ (with pkgs.buildPackages; [
    python3 gnumake bc bison flex perl xz util-linux
  ]);
  CROSS_COMPILE = prefix;
  HOST_CC = "${pkgs.buildPackages.stdenv.cc}/bin/cc";
  HOST_CXX = "${pkgs.buildPackages.stdenv.cc}/bin/c++";
  TARGET_OBJDUMP = "${pkgs.stdenv.cc.bintools}/bin/${prefix}objdump";
} ''
  bash ${./guest/kernel/arm64/build.sh} ${archive} ${./guest/kernel/arm64} "$out"
''
