{
  description = "Seedbox simulator: deterministic multi-VM world on KVM";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      shell = system:
        let
          pkgs = import nixpkgs { inherit system; };
          filesystemTools = (import ./nix/runtime.nix { inherit pkgs; }).filesystemTools;
          tcgQemu = import ./nix/tcg.nix { inherit pkgs; };
          tcgLinux = import ./nix/tcg-linux.nix { inherit pkgs; };
          linuxEnvironment = {
            SEEDBOX_STATIC_CC = "${pkgs.pkgsStatic.stdenv.cc}/bin/${pkgs.pkgsStatic.stdenv.cc.targetPrefix}cc";
            SEEDBOX_IP = "${pkgs.iproute2}/bin/ip";
            SEEDBOX_TC = "${pkgs.iproute2}/bin/tc";
            # Only the low-level guest/build-initramfs.sh fixture uses BusyBox.
            SEEDBOX_BUSYBOX = "${pkgs.pkgsStatic.busybox}/bin/busybox";
            SEEDBOX_MKE2FS = "${filesystemTools.bin}/bin/mke2fs";
            SEEDBOX_E2FSPROGS_SOURCE = "${pkgs.pkgsStatic.e2fsprogs.src}";
            SEEDBOX_MUSL_SOURCE = "${pkgs.pkgsStatic.stdenv.cc.libc.src}";
          };
          kernelTools = with pkgs; [
            stdenv.cc pkgsStatic.stdenv.cc binutils curl python3 gnumake
            bc bison flex perl git xz util-linux
          ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isx86_64
            (with pkgs; [ pkg-config elfutils elfutils.dev openssl.dev ]);
        in {
          default = pkgs.mkShell ({
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            SEEDBOX_TCG_LINUX = "${tcgLinux}";
            SEEDBOX_TCG_QEMU = "${tcgQemu}/bin/qemu-system-riscv64";
            SEEDBOX_TCG_FIRMWARE = "${tcgQemu}/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin";
            packages = with pkgs; [
              rustc cargo rustfmt clippy pkg-config stdenv.cc git
              cpio gzip curl jq python3 gnumake bc bison flex perl
              docker-client podman
              tcgQemu
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              elfutils elfutils.dev openssl.dev pkgsStatic.stdenv.cc iproute2 go util-linux
            ];
          } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux linuxEnvironment);
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Asset builds need neither Cargo/Rust nor a nested container engine.
          assets = pkgs.mkShell ({ packages = kernelTools; } // linuxEnvironment);
        };
    in {
      devShells = nixpkgs.lib.genAttrs systems shell;
      packages = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          machinePv = rec {
            arm = pkgs.stdenv.hostPlatform.isAarch64;
            kernel = import ./crates/seedbox-machine/pv-kernel.nix { inherit pkgs; };
            progress = import ./crates/seedbox-machine/progress.nix { inherit pkgs; };
            guest = (import ./crates/seedbox-machine/guest.nix {
              inherit pkgs;
              withFixture = true;
            }).${if arm then "aarch64" else "x86_64"};
            assets = import ./crates/seedbox-machine/guest-assets.nix {
              inherit pkgs progress;
              kernel = "${kernel}/kernel";
              init = "${guest}/bin/init";
              commandLine = if arm
                then "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init nokaslr nosmp maxcpus=1 random.trust_cpu=off random.trust_bootloader=on mitigations=off panic=-1 loglevel=4"
                else "console=ttyS0 rdinit=/init random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4";
            };
            testFixtures = import ./crates/seedbox-machine/pv-test-fixtures.nix { inherit pkgs guest; };
          };
        in {
          networkTools = pkgs.iproute2;
          tcgQemu = import ./nix/tcg.nix { inherit pkgs; };
          tcgLinux = import ./nix/tcg-linux.nix { inherit pkgs; };
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Name outputs explicitly so selecting helpers never imports the kernel recipe.
          inherit (import ./nix/runtime.nix { inherit pkgs; }) runtimeAssets runtimeArchive runtimeSources;
          inherit (import ./nix/kernels.nix { inherit pkgs; }) guestKernel guestKernelArchive guestKernelSources;
          tcgKernel = import ./nix/tcg-kernel.nix { inherit pkgs; };
        } // pkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.isAarch64 || pkgs.stdenv.hostPlatform.isx86_64) {
          # seedbox-machine's own native PV guest inputs, for this host's architecture.
          machinePvKernel = machinePv.kernel;
          machinePvProgress = machinePv.progress;
          machinePvGuest = machinePv.guest;
          machinePvAssets = machinePv.assets;
          machinePvTestFixtures = machinePv.testFixtures;
        }
        );
    };
}
