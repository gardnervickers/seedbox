# Private, static Linux guest helpers. Import with the project's pinned nixpkgs:
#   import ./guest.nix { pkgs = import nixpkgs { system = "aarch64-linux"; }; }
# (or x86_64-linux for the x86-64 PV guest).
# Build on Linux (including the project's persistent Nix container on macOS).
# Each result provides bin/init and lib/libseedbox-guest.a. Its mkInitramfs
# passthru accepts a target-architecture static /application executable.
{ pkgs, withInit ? true, withFixture ? false }:
let
  inherit (pkgs) lib;
  sources = lib.fileset.toSource {
    root = ./src/pv;
    fileset = lib.fileset.fileFilter (file: file.hasExt "c" || file.hasExt "h") ./src/pv;
  };
  make = target: triple: machine:
    let
      helper = pkgs.stdenvNoCC.mkDerivation {
        pname = "seedbox-machine-guest-${target}";
        version = "0.1.0";
        src = sources;
        nativeBuildInputs = [ pkgs.zig pkgs.binutils ];
        dontConfigure = true;
        dontStrip = true;
        buildPhase = ''
          runHook preBuild
          export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
          export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -c guest_supervisor.c -o guest_supervisor.o
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -c guest_service.c -o guest_service.o
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -c guest_oracle.c -o guest_oracle.o
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -c guest_properties.c -o guest_properties.o
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -c guest_application.c -o guest_application.o
          zig ar rcs libseedbox-guest.a guest_supervisor.o guest_service.o guest_oracle.o guest_properties.o guest_application.o
        '' + lib.optionalString withInit ''
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -static \
            guest_service_init.c libseedbox-guest.a -o init
          # Check the linked target and reject accidental dynamic libc inputs.
          bash ${./guest-static-elf.sh} init '${machine}'
        '' + lib.optionalString withFixture ''
          zig cc -target ${triple} -std=c11 -O2 -Wall -Wextra -Werror -static \
            guest_service_fixture.c -o application
          bash ${./guest-static-elf.sh} application '${machine}'
        '' + ''
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          mkdir -p "$out/lib" "$out/include"
          cp libseedbox-guest.a "$out/lib/"
          cp guest_supervisor.h guest_service.h guest_oracle.h guest_properties.h guest_application.h "$out/include/"
        '' + lib.optionalString withInit ''
          mkdir -p "$out/bin"
          cp init "$out/bin/init"
        '' + lib.optionalString withFixture ''
          mkdir -p "$out/bin"
          cp application "$out/bin/application"
        '' + ''
          runHook postInstall
        '';
        passthru.mkInitramfs = { application, base ? null }:
          assert withInit;
          pkgs.runCommand "seedbox-machine-initramfs-${target}" {
            nativeBuildInputs = [ pkgs.python3 ];
          } ''
            python3 ${./guest-initramfs.py} \
              --init ${helper}/bin/init --application ${application} \
              ${lib.optionalString (base != null) "--base ${base}"} --output "$out"
          '';
      };
    in helper;
in {
  aarch64 = make "aarch64" "aarch64-linux-musl" "AArch64";
  x86_64 = make "x86_64" "x86_64-linux-musl" "X86-64";
}
