# Import with native Linux packages for the guest architecture (aarch64 or
# x86_64), or aarch64 pkgsCross on Linux. The recipe follows the target platform.
{ pkgs }:
let
  source = builtins.fromJSON (builtins.readFile ./guest/kernel/source.json);
  archive = pkgs.fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${source.version}.tar.xz";
    sha256 = source.sha256;
  };
  prefix = pkgs.stdenv.cc.targetPrefix;
  platform = pkgs.stdenv.hostPlatform;
  inputs =
    if platform.isAarch64 then ./guest/kernel/arm64
    else if platform.isx86_64 then ./guest/kernel/x86_64
    else throw "seedbox-machine PV kernel supports aarch64 and x86_64 guests";
in pkgs.runCommand "seedbox-machine-pv-kernel-${source.version}" {
  nativeBuildInputs = [ pkgs.stdenv.cc ] ++ (with pkgs.buildPackages; [
    python3 gnumake bc bison flex perl xz util-linux
  ]) ++ pkgs.lib.optionals platform.isx86_64 (with pkgs.buildPackages; [
    # objtool links libelf on x86.
    pkg-config elfutils.dev openssl.dev
  ]);
  CROSS_COMPILE = prefix;
  HOST_CC = "${pkgs.buildPackages.stdenv.cc}/bin/cc";
  HOST_CXX = "${pkgs.buildPackages.stdenv.cc}/bin/c++";
  TARGET_OBJDUMP = "${pkgs.stdenv.cc.bintools}/bin/${prefix}objdump";
} ''
  bash ${inputs}/build.sh ${archive} ${inputs} "$out"
''
