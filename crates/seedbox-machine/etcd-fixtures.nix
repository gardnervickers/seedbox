# Real etcd for guest acceptance: the pinned nixpkgs etcd release, built for
# the guest platform of `pkgs` (x86-64 for TCG and x86 PV, or AArch64 PV), plus an
# empty ext4 data image. Test inputs only; factories never read them.
#   bin/etcd, bin/etcdctl, bin/etcdutl   static Linux executables: CGO off, as
#                                        nixpkgs builds them, and Go's internal
#                                        linker
#   disk/ext4-256m.img                   256 MiB empty ext4 filesystem with a
#                                        fixed filesystem identity
{ pkgs }:
let
  build = pkgs.buildPackages;
  # Go's internal linker keeps the result static whether the build is native
  # or cross, even with CGO off.
  static = component: component.overrideAttrs (old: {
    ldflags = old.ldflags ++ [ "-linkmode=internal" ];
  });
  tools = {
    etcd = static pkgs.etcd.passthru.deps.etcdserver;
    etcdctl = static pkgs.etcd.passthru.deps.etcdctl;
    etcdutl = static pkgs.etcd.passthru.deps.etcdutl;
  };
  machine = {
    x86_64 = "X86-64";
    aarch64 = "AArch64";
  }.${pkgs.stdenv.hostPlatform.parsed.cpu.name};
  # Room for etcd's 64 MiB WAL segments and snapshots; the 16 MiB disk
  # fixture is too small.
  disk = build.runCommand "seedbox-machine-etcd-disk" {
    nativeBuildInputs = [ build.e2fsprogs ];
  } ''
    mkdir -p "$out/disk"
    # Fixed identity, time and full initialization, as in disk-test-fixtures.nix.
    E2FSPROGS_FAKE_TIME=1 mke2fs -q -F -t ext4 -b 4096 \
      -U 5eedb0c5-0000-4000-8000-000000000011 \
      -E lazy_itable_init=0,lazy_journal_init=0,nodiscard,root_owner=0:0,hash_seed=5eedb0c5-0000-4000-8000-000000000012 \
      "$out/disk/ext4-256m.img" 256M
  '';
in build.runCommand "seedbox-machine-etcd-fixtures-${pkgs.etcd.version}-${pkgs.stdenv.hostPlatform.parsed.cpu.name}" {
  # Unprefixed build-machine readelf; it reads any guest architecture.
  nativeBuildInputs = [ pkgs.pkgsBuildBuild.binutils-unwrapped ];
  passthru = { inherit tools disk; };
} (''
  mkdir -p "$out/bin"
'' + pkgs.lib.concatStrings (pkgs.lib.mapAttrsToList (tool: component: ''
  cp ${component}/bin/${tool} "$out/bin/${tool}"
'') tools) + ''
  for tool in etcd etcdctl etcdutl; do
    file="$out/bin/$tool"
    # The guest architecture, and no interpreter or shared libraries. (A
    # negated pipeline would not stop the build, so failures are explicit.)
    readelf -h "$file" | grep -q "Machine:.*${machine}" || { echo "$tool: wrong machine"; exit 1; }
    readelf -h "$file" | grep -q "Class:.*ELF64" || { echo "$tool: not ELF64"; exit 1; }
    if readelf -lW "$file" | grep -q INTERP; then echo "$tool: has an interpreter"; exit 1; fi
    if readelf -dW "$file" 2>/dev/null | grep -q NEEDED; then echo "$tool: needs shared libraries"; exit 1; fi
  done
  ln -s ${disk}/disk "$out/disk"
'')
