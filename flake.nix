{
  description = "Seedbox simulator: deterministic multi-VM world on KVM";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      shell = system:
        let
          pkgs = import nixpkgs { inherit system; };
          linuxEnvironment = {
            SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/${pkgs.pkgsStatic.stdenv.cc.targetPrefix}cc";
            SEEDBOX_IP = "${pkgs.iproute2}/bin/ip";
            SEEDBOX_TC = "${pkgs.iproute2}/bin/tc";
            SEEDBOX_BUSYBOX = "${pkgs.pkgsStatic.busybox}/bin/busybox";
            SEEDBOX_MKE2FS = "${pkgs.pkgsStatic.e2fsprogs.bin}/bin/mke2fs";
            SEEDBOX_BUSYBOX_SOURCE = "${pkgs.pkgsStatic.busybox.src}";
            SEEDBOX_E2FSPROGS_SOURCE = "${pkgs.pkgsStatic.e2fsprogs.src}";
            SEEDBOX_MUSL_SOURCE = "${pkgs.pkgsStatic.stdenv.cc.libc.src}";
          };
          kernelTools = with pkgs; [
            stdenv.cc pkgsStatic.stdenv.cc binutils curl python3 gnumake
            bc bison flex perl git xz util-linux
          ];
        in {
          default = pkgs.mkShell ({
            packages = with pkgs; [
              rustc cargo rustfmt clippy pkg-config stdenv.cc git
              cpio gzip curl jq python3 gnumake bc bison flex perl
              docker-client podman
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              elfutils elfutils.dev openssl.dev pkgsStatic.stdenv.cc iproute2 go
              skopeo umoci
            ];
          } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux linuxEnvironment);
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Asset builds need neither Cargo/Rust nor a nested container engine.
          assets = pkgs.mkShell ({ packages = kernelTools; } // linuxEnvironment);
        };
    in {
      devShells = nixpkgs.lib.genAttrs systems shell;
      packages = nixpkgs.lib.genAttrs systems (system:
        let pkgs = import nixpkgs { inherit system; }; in {
          networkTools = pkgs.iproute2;
          preparationTools = pkgs.buildEnv {
            name = "seedbox-preparation-tools";
            paths = with pkgs; [ skopeo umoci python3 gnutar coreutils ];
          };
        });
    };
}
