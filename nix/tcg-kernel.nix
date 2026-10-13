{ pkgs }:
let
  source = builtins.fromJSON (builtins.readFile ../crates/seedbox-machine/guest/kernel/source.json);
  archive = pkgs.fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${source.version}.tar.xz";
    sha256 = source.sha256;
  };
in pkgs.runCommand "seedbox-tcg-kernel-riscv64-${source.version}" {
  HOST_CC = "${pkgs.stdenv.cc}/bin/cc";
  HOST_CXX = "${pkgs.stdenv.cc}/bin/c++";
  nativeBuildInputs = with pkgs; [
    llvmPackages.clang-unwrapped llvmPackages.lld llvmPackages.llvm
    gnumake bc bison flex perl python3 pkg-config openssl elfutils xz
  ];
} ''
  bash ${../crates/seedbox-machine/guest/kernel/build.sh} \
    ${archive} ${../crates/seedbox-machine/guest/kernel/riscv64.config} "$out"
''
