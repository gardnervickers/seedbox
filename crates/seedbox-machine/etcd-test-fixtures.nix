# Guest programs of the etcd acceptance test (tests/etcd_acceptance.rs); the
# factories never read them. `triple` is the guest's zig target, such as
# x86_64-linux-musl. The etcd binary and its ext4 image
# are separate inputs.
#   bin/etcd-node     mount wrapper (tests/fixtures/etcd-node.c)
#   bin/etcd-client   v3 JSON gateway oracle (tests/fixtures/etcd-client.c)
{ pkgs, triple }:
let
  sources = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./tests/fixtures/etcd-node.c
      ./tests/fixtures/etcd-client.c
    ];
  };
in pkgs.stdenvNoCC.mkDerivation {
  pname = "seedbox-machine-etcd-test-fixtures";
  version = "0.1.0";
  src = sources;
  nativeBuildInputs = [ pkgs.zig ];
  dontConfigure = true;
  dontStrip = true;
  buildPhase = ''
    export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-global"
    export ZIG_LOCAL_CACHE_DIR="$TMPDIR/zig-local"
    for program in etcd-node etcd-client; do
      zig cc -target ${triple} -O2 -static -std=c11 -Wall -Werror \
        tests/fixtures/$program.c -o $program
    done
  '';
  installPhase = ''
    mkdir -p "$out/bin"
    cp etcd-node etcd-client "$out/bin/"
  '';
}
