{
  description = "Seedbox simulator: deterministic multi-VM world on KVM";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "x86_64-linux" ];
      shell = system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell ({
            packages = with pkgs; [
              rustc cargo rustfmt clippy pkg-config stdenv.cc
              cpio gzip curl jq python3 gnumake bc bison flex perl
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              elfutils elfutils.dev openssl.dev pkgsStatic.stdenv.cc iproute2 go
              skopeo umoci podman
            ];
          } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/x86_64-unknown-linux-musl-cc";
            SEEDBOX_IP = "${pkgs.iproute2}/bin/ip";
            SEEDBOX_TC = "${pkgs.iproute2}/bin/tc";
            SEEDBOX_BUSYBOX = "${pkgs.pkgsStatic.busybox}/bin/busybox";
            SEEDBOX_MKE2FS = "${pkgs.pkgsStatic.e2fsprogs.bin}/bin/mke2fs";
          });
        };
    in {
      devShells = nixpkgs.lib.genAttrs systems shell;
      packages = nixpkgs.lib.genAttrs systems (system: { networkTools = (import nixpkgs { inherit system; }).iproute2; });
    };
}
