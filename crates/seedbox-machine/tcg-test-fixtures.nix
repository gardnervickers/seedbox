# Static RISC-V Linux inputs owned by seedbox-machine's native TCG tests.
# bin/prepared-fixture and bin/driver-simulation-control, service-initrd and
# control-initrd correspond to the explicit SEEDBOX_TCG_* test inputs.
{ pkgs, guest }:
let
  sources = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./src/pv/guest_prepared_fixture.c
      ./tests/fixtures/simulation-control.c
      ./tests/fixtures/driver-simulation-control.c
    ];
  };
  programs = pkgs.stdenvNoCC.mkDerivation {
    pname = "seedbox-machine-tcg-test-programs";
    version = "0.1.0";
    src = sources;
    nativeBuildInputs = [ pkgs.zig ];
    dontConfigure = true;
    dontStrip = true;
    buildPhase = ''
      export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
      export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
      build() { zig cc -target riscv64-linux-musl -O2 -static -std=c11 "$1" -o "$2"; }
      build src/pv/guest_prepared_fixture.c prepared-fixture
      build tests/fixtures/simulation-control.c simulation-control
      build tests/fixtures/driver-simulation-control.c driver-simulation-control
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp prepared-fixture simulation-control driver-simulation-control "$out/bin/"
    '';
  };
  service = guest.mkInitramfs { application = "${guest}/bin/application"; };
  control = guest.mkInitramfs { application = "${programs}/bin/simulation-control"; };
in pkgs.runCommand "seedbox-machine-tcg-test-fixtures" { } ''
  mkdir -p "$out"
  ln -s ${programs}/bin "$out/bin"
  ln -s ${service} "$out/service-initrd"
  ln -s ${control} "$out/control-initrd"
''
