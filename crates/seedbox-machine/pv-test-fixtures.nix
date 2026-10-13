# Static native PV test inputs for the guest architecture of `pkgs`. These are
# test fixtures only; the factory never reads them. Each output path matches one
# SEEDBOX_PV_* test input:
#   bin/cpu-loop                     SEEDBOX_PV_PROGRESS_FIXTURE
#   bin/prepared-fixture             SEEDBOX_PV_PREPARED_FIXTURE
#   bin/driver-simulation-control    SEEDBOX_PV_DRIVER_CONTROL_APPLICATION
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
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp cpu-loop prepared-fixture simulation-control driver-simulation-control "$out/bin/"
    '';
  };
  service = guest.mkInitramfs { application = "${guest}/bin/application"; };
  control = guest.mkInitramfs { application = "${programs}/bin/simulation-control"; };
in pkgs.runCommand "seedbox-machine-pv-test-fixtures" { } ''
  mkdir -p "$out"
  ln -s ${programs}/bin "$out/bin"
  ln -s ${service} "$out/service-initrd"
  ln -s ${control} "$out/control-initrd"
''
