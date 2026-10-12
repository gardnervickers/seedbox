{ pkgs }:

# Keep the reference backend on QEMU's normal system-emulation path. The patch
# adds a bounded execution command; guest CPU, MMU, and devices stay upstream.
(pkgs.qemu.override {
  minimal = true;
  hostCpuTargets = [ "riscv64-softmmu" ];
  enableBlobs = true; # OpenSBI firmware for Linux boot.
}).overrideAttrs (old: {
  pname = "seedbox-qemu";
  patches = (old.patches or [ ]) ++ [
    ../crates/seedbox-machine/qemu/bounded-run.patch
  ];
  postInstall = (old.postInstall or "") + ''
    test -x "$out/bin/qemu-system-riscv64"
  '';
})
