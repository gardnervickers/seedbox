{ pkgs }:

let
  # Released upstream binaries keep this example usable from macOS without a
  # Linux builder. These are build inputs; runtime never downloads guest files.
  base = "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/riscv64";
  boot = pkgs.fetchurl {
    url = "${base}/alpine-uboot-3.24.2-riscv64.tar.gz";
    sha256 = "036e2694fa4f90d94dbe1a74ef1420be163eff6f3c72d194eb8b14f9dac932d6";
  };
  root = pkgs.fetchurl {
    url = "${base}/alpine-minirootfs-3.24.2-riscv64.tar.gz";
    sha256 = "57132e6e4f3a4ba9ffdf24e485513ce507e45471e7fd565aeed91c055cd63f7b";
  };
in pkgs.runCommand "seedbox-tcg-linux" {
  nativeBuildInputs = [ pkgs.cpio pkgs.gnutar pkgs.gzip ];
} ''
  mkdir -p "$out" boot root
  tar -xzf ${boot} -C boot ./boot/vmlinuz-lts
  gzip -dc boot/boot/vmlinuz-lts > "$out/kernel"
  tar -xzf ${root} -C root
  cp ${../crates/seedbox-machine/examples/tcg-init} root/init
  chmod +x root/init
  cd root
  find . -print | LC_ALL=C sort | cpio --quiet -o -H newc --owner=0:0 > "$out/initramfs"
''
