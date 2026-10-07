# Experimental ARM64 PV backends for HVF and KVM

Adds an isolated ARM64 machine with a patched Linux PV clock, deterministic
interrupt rendezvous, seeded entropy, full RAM witnesses, and native Mac HVF
and Linux KVM adapters. Production x86 paths remain unchanged.

Native HVF and nested KVM repeat/noise acceptance pass after rerunning both
suites. The KVM setup regression is fixed: Linux 6.8 caps `NR_IRQS` at 1023
with 32-ID alignment, so KVM uses the closest valid total, 992 (960 SPIs,
ITLinesNumber=30). HVF remains at 1024 advertised IDs (988 usable SPIs,
ITLinesNumber=31); its two witnesses are unchanged. All 2,291 exits plus console
bytes match across backends for both seeds. Full cross-backend RAM equality and
the remaining GIC count difference stay open. Optional `--ram-pages FILE`
diagnostics locate differing 4 KiB pages without changing the witness.

## Try it on your Mac

Requires an M-series Mac, macOS 15+, Xcode Command Line Tools, and Nix with
flakes enabled. This runs the experimental single-CPU PV fixture. The Linux VM
only builds its inputs; HVF executes the fixture directly on the Mac. Allow
about 24 GiB of VM disk and several minutes for the kernel build.

```sh
git clone --branch astra/arm64-hvf https://github.com/gardnervickers/seedbox.git
cd seedbox
nix develop
cargo build-arm64
bash crates/seedbox-arm64/codesign.sh

# Enter a temporary shell with Lima. Stay in the checkout root.
nix shell nixpkgs#lima -c bash
export LIMA_HOME="$(mktemp -d /tmp/sb-lima.XXXXXX)"
cat > "$LIMA_HOME/seedbox-build.yaml" <<'YAML'
vmType: vz
arch: aarch64
cpus: 4
memory: 8GiB
disk: 24GiB
images:
  - location: https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img
    arch: aarch64
mounts: []
containerd:
  system: false
  user: false
YAML
limactl start --name=seedbox-build --tty=false "$LIMA_HOME/seedbox-build.yaml"
limactl copy -r kernel-arm64 seedbox-build:/tmp/kernel-arm64
limactl shell --workdir /tmp seedbox-build sudo apt-get update
limactl shell --workdir /tmp seedbox-build sudo apt-get install -y \
  build-essential musl-tools flex bison bc libssl-dev libelf-dev curl xz-utils python3
limactl shell --workdir /tmp seedbox-build env BUILD_JOBS=4 \
  bash /tmp/kernel-arm64/build.sh /tmp/seedbox-arm64-inputs
mkdir -p artifacts/arm64-mac
limactl copy -r seedbox-build:/tmp/seedbox-arm64-inputs/. artifacts/arm64-mac/
(cd artifacts/arm64-mac && shasum -a 256 -c SHA256SUMS)
# Preserve the inputs on the Mac, then remove only this task's VM.
limactl stop seedbox-build
limactl delete --force seedbox-build
exit  # leave the Lima shell; return to nix develop in the checkout

for repeat in 1 2; do
  crates/seedbox-arm64/target/release/seedbox-arm64 run \
    --backend hvf --kernel artifacts/arm64-mac/Image \
    --initrd artifacts/arm64-mac/init.cpio --seed 1
done
```

Both runs print this stdout line; guest console and timing go to stderr:

```text
PASS: b3de4bbdaa7a747b54d3a940f02c4281bd3f26fe8574454c6cb7f06a3658a2a7
```

That is the current tested HVF witness with Image SHA-256
`7ea8fa2858ef33d05da559e9de5e5a9329c6d397e5b03fd7f4fcd3ebb75d9ff1`
and init.cpio SHA-256
`9b952991625d7603e2db03e5ccdb87f78f1f7a16e6c664062505b7ddc75d8649`.
These inputs were built with Ubuntu GCC 13.3.0 and binutils 2.42. Updated build
tools or a different Mac CPU feature profile can change the witness; repeated
runs with identical inputs on the same supported host must still match. Expected
fixture output includes `SEEDBOX_INIT_PASS`, 2,291 exits, and 1,010,051,041 virtual
ns. Sign again after any Cargo command that relinks the executable.

The configuration follows [Lima's VZ documentation](https://lima-vm.io/docs/config/vmtype/)
and uses Ubuntu's published arm64 cloud image. Nested virtualization is not
needed for this build-only VM. KVM execution is a separate operator step.
