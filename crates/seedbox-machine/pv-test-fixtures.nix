# Static native PV test inputs for the guest architecture of `pkgs`. These are
# test fixtures only; the factory never reads them. Each output path matches one
# SEEDBOX_PV_* test input:
#   bin/cpu-loop                     SEEDBOX_PV_PROGRESS_FIXTURE
#   bin/prepared-fixture             SEEDBOX_PV_PREPARED_FIXTURE
#   bin/driver-simulation-control    SEEDBOX_PV_DRIVER_CONTROL_APPLICATION
#   bin/clock-faults                 SEEDBOX_PV_CLOCK_APPLICATION
#   bin/kernel-terminal              SEEDBOX_PV_TERMINAL_APPLICATION
#   bin/network-probe                SEEDBOX_PV_NETWORK_APPLICATION
#   bin/disk-probe                   SEEDBOX_PV_DISK_PROBE
#   disk/ext4.img                    SEEDBOX_PV_DISK_IMAGE
#   service-initrd                   SEEDBOX_PV_SERVICE_INITRD
#   control-initrd                   SEEDBOX_PV_CONTROL_INITRD
{ pkgs, guest }:
let
  inherit (pkgs) lib;
  platform = pkgs.stdenv.hostPlatform;
  triple = if platform.isAarch64 then "aarch64-linux-musl" else "x86_64-linux-musl";
  sources = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./guest/progress/fixtures/cpu-loop.c
      ./src/pv/guest_prepared_fixture.c
      ./tests/fixtures/simulation-control.c
      ./tests/fixtures/driver-simulation-control.c
      ./tests/fixtures/clock-faults.c
      ./tests/fixtures/kernel-terminal.c
      ./tests/fixtures/network-probe.c
    ];
  };
  programs = pkgs.stdenvNoCC.mkDerivation {
    pname = "seedbox-machine-pv-test-programs";
    version = "0.1.0";
    src = sources;
    nativeBuildInputs = [ pkgs.zig ];
    dontConfigure = true;
    dontStrip = true;
    buildPhase = ''
      export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
      export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
      build() { zig cc -target ${triple} -O2 -static "$1" -o "$2"; }
      build guest/progress/fixtures/cpu-loop.c cpu-loop
      build src/pv/guest_prepared_fixture.c prepared-fixture
      build tests/fixtures/simulation-control.c simulation-control
      build tests/fixtures/driver-simulation-control.c driver-simulation-control
      build tests/fixtures/clock-faults.c clock-faults
      build tests/fixtures/kernel-terminal.c kernel-terminal
      build tests/fixtures/network-probe.c network-probe
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp cpu-loop prepared-fixture simulation-control driver-simulation-control clock-faults kernel-terminal network-probe "$out/bin/"
    '';
  };
  disk = import ./disk-test-fixtures.nix { inherit pkgs triple; };
  service = guest.mkInitramfs { application = "${guest}/bin/application"; };
  control = guest.mkInitramfs { application = "${programs}/bin/simulation-control"; };
in pkgs.runCommand "seedbox-machine-pv-test-fixtures" { } ''
  mkdir -p "$out/bin"
  ln -s ${programs}/bin/* ${disk}/bin/* "$out/bin/"
  ln -s ${disk}/disk "$out/disk"
  ln -s ${service} "$out/service-initrd"
  ln -s ${control} "$out/control-initrd"
''
