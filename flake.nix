{
  description = "Seedbox simulator: deterministic multi-VM world on KVM";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  outputs = { nixpkgs, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      # Test-only SDK fixture uses the same complete wheel lock as the guest.
      # Nothing installs into the host interpreter or fetches during tests.
      minioPythonFixture = pkgs:
        let
          pins = (builtins.fromJSON (builtins.readFile ./examples/minio/inputs.lock.json)).wheels;
          wheels = map (pin: pkgs.fetchurl {
            inherit (pin) url sha256;
            name = pin.filename;
          }) pins;
        in pkgs.runCommand "seedbox-minio-python-fixture" {
          nativeBuildInputs = [ pkgs.python3 ];
        } ''
          mkdir -p "$out"
          for wheel in ${pkgs.lib.escapeShellArgs (map toString wheels)}; do
            python3 -m zipfile -e "$wheel" "$out"
          done
        '';
      shell = system:
        let
          pkgs = import nixpkgs { inherit system; };
          filesystemTools = (import ./nix/runtime.nix { inherit pkgs; }).filesystemTools;
          tcgQemu = import ./nix/tcg.nix { inherit pkgs; };
          tcgLinux = import ./nix/tcg-linux.nix { inherit pkgs; };
          checker = import ./nix/seedbox-check.nix { inherit pkgs; };
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
            # Provisioned before invocation: wrappers never compile or download.
            SEEDBOX_CHECK = "${checker}/bin/seedbox-check";
            SEEDBOX_MINIO_PYTHONPATH = "${minioPythonFixture pkgs}";
            SEEDBOX_TCG_LINUX = "${tcgLinux}";
            SEEDBOX_TCG_QEMU = "${tcgQemu}/bin/qemu-system-x86_64";
            SEEDBOX_TCG_FIRMWARE = "${tcgQemu}/share/qemu/qboot.rom";
            packages = with pkgs; [
              rustc cargo rustfmt clippy sccache pkg-config stdenv.cc git
              cpio gzip curl jq python3 gnumake bc bison flex perl
              docker-client podman
              tcgQemu checker libarchive
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              elfutils elfutils.dev openssl.dev pkgsStatic.stdenv.cc iproute2 go util-linux patchelf
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
          machineTcg = rec {
            qemu = import ./nix/tcg.nix { inherit pkgs; };
            kernel = import ./nix/tcg-kernel.nix { inherit pkgs; };
            guest = (import ./crates/seedbox-machine/guest.nix {
              inherit pkgs;
              withFixture = true;
            }).x86_64;
            assets = import ./crates/seedbox-machine/guest-assets.nix {
              inherit pkgs;
              kernel = "${kernel}/kernel";
              init = "${guest}/bin/init";
              # The emulated TSC under icount counts exactly 1 GHz, the guest
              # counter's nanosecond units; this states the virtual hardware
              # frequency instead of calibrating it.
              # The initramfs root uses guest RAM directly, without tmpfs's
              # separate quota while the boot archive is still reserved.
              commandLine = "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init rootfstype=ramfs nokaslr panic=-1 tsc_early_khz=1000000";
              # qboot loads the kernel and the ACPI tables from fw_cfg itself,
              # so with option ROMs disabled only owned firmware runs. (The
              # microvm ACPI default, bios-microvm.bin, needs QEMU's
              # linuxboot_dma.bin option ROM to boot a kernel.)
              firmware = "${qemu}/share/qemu/qboot.rom";
            };
            testFixtures = import ./crates/seedbox-machine/tcg-test-fixtures.nix { inherit pkgs guest; };
          };
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
              # Avoid a second, smaller tmpfs quota during initramfs unpacking;
              # the VM's configured physical RAM remains the memory limit.
              commandLine = if arm
                then "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init rootfstype=ramfs nokaslr nosmp maxcpus=1 random.trust_cpu=off random.trust_bootloader=on mitigations=off panic=-1 loglevel=4"
                else "console=ttyS0 rdinit=/init rootfstype=ramfs random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4";
            };
            testFixtures = import ./crates/seedbox-machine/pv-test-fixtures.nix { inherit pkgs guest; };
          };
        in {
          seedboxCheck = import ./nix/seedbox-check.nix { inherit pkgs; };
          minioPythonFixture = minioPythonFixture pkgs;
          networkTools = pkgs.iproute2;
          tcgQemu = import ./nix/tcg.nix { inherit pkgs; };
          tcgLinux = import ./nix/tcg-linux.nix { inherit pkgs; };
        } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # Compile only: ARM native cfg with the project-managed target stdlib.
          # This does not change the host dev shell or build/run guest inputs.
          machinePvAarch64Check = pkgs.pkgsCross.aarch64-multiplatform.rustPlatform.buildRustPackage {
            pname = "seedbox-machine-pv-aarch64-check";
            version = "0.1.0";
            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            doCheck = false;
            buildPhase = ''
              runHook preBuild
              cargo check -p seedbox-machine-pv --all-features --target aarch64-unknown-linux-gnu
              runHook postBuild
            '';
            installPhase = ''
              mkdir -p "$out"
              printf '%s\n' 'aarch64-linux cargo check only; no native execution' > "$out/check-kind"
            '';
          };
          # Name outputs explicitly so selecting helpers never imports the kernel recipe.
          inherit (import ./nix/runtime.nix { inherit pkgs; }) runtimeAssets runtimeArchive runtimeSources;
          inherit (import ./nix/kernels.nix { inherit pkgs; }) guestKernel guestKernelArchive guestKernelSources;
          tcgKernel = import ./nix/tcg-kernel.nix { inherit pkgs; };
          machineTcgAssets = machineTcg.assets;
          machineTcgTestFixtures = machineTcg.testFixtures;
          # These executables run INSIDE the configured Linux guest. TCG always
          # needs x86-64, including when its QEMU host is AArch64.
          machineGuestFormatter = (import ./nix/runtime.nix { inherit pkgs; }).filesystemTools.bin;
          machineGuestFormatterX86 = (import ./nix/runtime.nix {
            pkgs = if pkgs.stdenv.hostPlatform.isx86_64 then pkgs else pkgs.pkgsCross.gnu64;
          }).filesystemTools.bin;
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
