{
  description = "Shared Seedbox research environment";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "x86_64-linux" ];
      shells = system:
        let
          pkgs = import nixpkgs { inherit system; };
          common = with pkgs; [
            rustc cargo rustfmt clippy pkg-config stdenv.cc clang lld nasm
            qemu cpio curl jq git coreutils gnutar gzip gnumake bc bison flex
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
            strace util-linux elfutils elfutils.dev openssl.dev pkgsStatic.stdenv.cc
          ];
        in {
          default = pkgs.mkShell ({
            packages = common;
          } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            SEEDBOX_LINUX_SOURCE = pkgs.linux_6_12.src;
            SEEDBOX_LINUX_VERSION = pkgs.linux_6_12.version;
            SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/cc";
          });
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          workloads = pkgs.mkShell {
            packages = common ++ (with pkgs; [ redis etcd postgresql go python3 ]);
          };
        };
    in { devShells = nixpkgs.lib.genAttrs systems shells; };
}
