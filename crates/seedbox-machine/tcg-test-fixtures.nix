# Static x86-64 Linux inputs owned by seedbox-machine's native TCG tests.
# bin/prepared-fixture, bin/driver-simulation-control, bin/clock-faults and
# bin/kernel-terminal, bin/network-probe, service-initrd and
# control-initrd correspond to the explicit SEEDBOX_TCG_* test inputs.
# bin/disk-probe and disk/ext4.img come from the shared disk fixtures.
{ pkgs, guest }:
let
  sources = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./src/pv/guest_prepared_fixture.c
      ./tests/fixtures/simulation-control.c
      ./tests/fixtures/driver-simulation-control.c
      ./tests/fixtures/clock-faults.c
      ./tests/fixtures/kernel-terminal.c
      ./tests/fixtures/network-probe.c
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
      build() { zig cc -target x86_64-linux-musl -O2 -static -std=c11 "$1" -o "$2"; }
      build src/pv/guest_prepared_fixture.c prepared-fixture
      build tests/fixtures/simulation-control.c simulation-control
      build tests/fixtures/driver-simulation-control.c driver-simulation-control
      build tests/fixtures/clock-faults.c clock-faults
      build tests/fixtures/kernel-terminal.c kernel-terminal
      build tests/fixtures/network-probe.c network-probe
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp prepared-fixture simulation-control driver-simulation-control clock-faults kernel-terminal network-probe "$out/bin/"
    '';
  };
  service = guest.mkInitramfs { application = "${guest}/bin/application"; };
  control = guest.mkInitramfs { application = "${programs}/bin/simulation-control"; };
  disk = import ./disk-test-fixtures.nix { inherit pkgs; triple = "x86_64-linux-musl"; };
in pkgs.runCommand "seedbox-machine-tcg-test-fixtures" { } ''
  mkdir -p "$out/bin"
  ln -s ${programs}/bin/* ${disk}/bin/* "$out/bin/"
  ln -s ${disk}/disk "$out/disk"
  ln -s ${service} "$out/service-initrd"
  ln -s ${control} "$out/control-initrd"
''
