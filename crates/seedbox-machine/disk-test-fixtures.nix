# Disk test inputs shared by the native PV and TCG suites; the factories never
# read them. `triple` is the guest's zig target, such as x86_64-linux-musl.
#   bin/disk-probe   static guest application (tests/fixtures/disk-probe.c)
#   disk/ext4.img    16 MiB empty ext4 filesystem, identical on every build
{ pkgs, triple }:
let
  sources = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = ./tests/fixtures/disk-probe.c;
  };
in pkgs.stdenvNoCC.mkDerivation {
  pname = "seedbox-machine-disk-test-fixtures";
  version = "0.1.0";
  src = sources;
  nativeBuildInputs = [ pkgs.zig pkgs.e2fsprogs ];
  dontConfigure = true;
  dontStrip = true;
  buildPhase = ''
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
    export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
    zig cc -target ${triple} -O2 -static -std=c11 tests/fixtures/disk-probe.c -o disk-probe
    # Fixed identity, time and full initialization: no lazy background
    # initialization writes in the guest, and the same bytes on every build.
    E2FSPROGS_FAKE_TIME=1 mke2fs -q -F -t ext4 -b 4096 \
      -U 5eedb0c5-0000-4000-8000-000000000001 \
      -E lazy_itable_init=0,lazy_journal_init=0,nodiscard,root_owner=0:0,hash_seed=5eedb0c5-0000-4000-8000-000000000002 \
      ext4.img 16M
  '';
  installPhase = ''
    mkdir -p "$out/bin" "$out/disk"
    cp disk-probe "$out/bin/"
    cp ext4.img "$out/disk/"
  '';
}
