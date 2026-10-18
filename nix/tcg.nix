{ pkgs }:

# Keep the reference backend on QEMU's normal system-emulation path. The
# patches add a bounded execution command and Seedbox's private devices; guest
# CPU, MMU and the remaining devices stay upstream.
let
  machine = ../crates/seedbox-machine;
  tcg = ../crates/seedbox-machine-tcg;
  # Seedbox's Rust disk store as a static library for the seedbox-blk device:
  # the machine crate's own disk sources, compiled with the project's rustc
  # and workspace edition. panic=abort keeps unwinding out of C.
  disk = pkgs.runCommand "seedbox-disk-store" {
    nativeBuildInputs = [ pkgs.rustc ];
    src = pkgs.lib.fileset.toSource {
      root = ../crates;
      fileset = pkgs.lib.fileset.unions [
        (tcg + "/qemu/seedbox_disk.rs")
        (machine + "/src/machine/disk.rs")
        (machine + "/src/machine/disk/request.rs")
        (machine + "/src/machine/disk/codec.rs")
        (machine + "/src/machine/disk/abi.rs")
      ];
    };
  } ''
    mkdir -p $out/lib
    rustc --edition 2021 --crate-type staticlib --crate-name seedbox_disk \
      -C panic=abort -C opt-level=2 --cfg seedbox_disk_abi \
      --print native-static-libs -o $out/lib/libseedbox_disk.a \
      $src/seedbox-machine-tcg/qemu/seedbox_disk.rs 2> notes
    # The host's own system libraries for Rust std (Linux and Darwin differ).
    sed -n 's/^note: native-static-libs: //p' notes > $out/lib/seedbox_disk.link
    test -s $out/lib/seedbox_disk.link
  '';
in
(pkgs.qemu.override {
  minimal = true;
  hostCpuTargets = [ "x86_64-softmmu" ];
  # microvm boots Linux with qboot.rom, passed explicitly: qboot loads the
  # kernel from fw_cfg (and the ACPI tables when acpi=on) with no option ROM.
  # The default SeaBIOS bios-microvm.bin needs linuxboot_dma.bin, which
  # x-option-roms=off withholds so Seedbox owns all boot firmware.
  enableBlobs = true;
}).overrideAttrs (old: {
  pname = "seedbox-qemu";
  patches = (old.patches or [ ]) ++ [
    (tcg + "/qemu/bounded-run.patch")
    (tcg + "/qemu/seedbox-disk.patch")
    (tcg + "/qemu/seedbox-net.patch")
    (tcg + "/qemu/seedbox-state-timer.patch")
    (tcg + "/qemu/seedbox-witness.patch")
    (tcg + "/qemu/seedbox-witness-delivery.patch")
  ];
  postPatch = (old.postPatch or "") + ''
    cp ${disk}/lib/libseedbox_disk.a ${disk}/lib/seedbox_disk.link hw/block/
  '';
  # Host-only production-helper tests: no QEMU process or guest is started.
  postBuild = (old.postBuild or "") + ''
    ninja -C build -j"$NIX_BUILD_CORES" tests/unit/test-seedbox-timer-rng tests/unit/test-seedbox-evidence-pages tests/unit/test-seedbox-witness-chain
    build/tests/unit/test-seedbox-timer-rng
    build/tests/unit/test-seedbox-evidence-pages
    build/tests/unit/test-seedbox-witness-chain
  '';
  passthru = (old.passthru or { }) // { seedboxDisk = disk; };
  postInstall = (old.postInstall or "") + ''
    test -x "$out/bin/qemu-system-x86_64"
    test -s "$out/share/qemu/qboot.rom"
  '';
})
