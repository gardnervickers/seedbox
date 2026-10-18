{ pkgs }:

let
  # Released upstream binaries keep this example usable from macOS without a
  # Linux builder. These are build inputs; runtime never downloads guest files.
  base = "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64";
  boot = pkgs.fetchurl {
    url = "${base}/alpine-netboot-3.24.2-x86_64.tar.gz";
    sha256 = "bbdd643360fac384a7933d8bebc7132eb720002e58d1321edcde4a5a3c3b83c6";
  };
  root = pkgs.fetchurl {
    url = "${base}/alpine-minirootfs-3.24.2-x86_64.tar.gz";
    sha256 = "c5ca053cfe1d85c5b96dff8b9bc57045f7f184a30ffb6b65776409ca90388677";
  };
in pkgs.runCommand "seedbox-tcg-linux" {
  nativeBuildInputs = [ pkgs.cpio pkgs.gnutar pkgs.gzip pkgs.squashfsTools ];
} ''
  mkdir -p "$out" boot root
  # The virt kernel is a bzImage with virtio-mmio and its console built in.
  tar -xzf ${boot} -C boot boot/vmlinuz-virt boot/modloop-virt
  cp boot/boot/vmlinuz-virt "$out/kernel"
  tar -xzf ${root} -C root
  # The example init asks modprobe for built-in drivers too; keep the module
  # indexes (including modules.builtin) rather than a mounted modloop.
  unsquashfs -no-progress -d modules boot/boot/modloop-virt 'modules/*/modules.*'
  mkdir -p root/lib/modules
  cp -R modules/modules/. root/lib/modules/
  cp ${../crates/seedbox-driver/examples/tcg-init} root/init
  chmod +x root/init
  cd root
  find . -print | LC_ALL=C sort | cpio --quiet -o -H newc --owner=0:0 > "$out/initramfs"
''
