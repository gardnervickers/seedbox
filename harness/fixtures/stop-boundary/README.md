# Focused clock and syscall fixture

Build on Linux in the repository Nix shell:

```sh
mkdir -p artifacts/stop-boundary
"$SEEDBOX_STATIC_CC" -O2 -static -Wall -Wextra -Werror guest/stop-boundary.c -o artifacts/stop-boundary/stop-boundary
cp harness/fixtures/stop-boundary/Dockerfile artifacts/stop-boundary/Dockerfile
```

Run `harness/fixtures/stop-boundary/compose.yaml` through the public CLI on an allocated Linux KVM host. Use the same retained prepared execution inputs for performance-setting replays. Before comparing separately prepared exits and full-state observation runs, verify their kernel, guest, image/rootfs and other execution-input hashes and semantic parameters are identical except witness mode. An image build that changes this closure leaves the observation comparison unresolved.

The probe checks monotonic clock reads interspersed with busy loops and syscalls. It does not prove platform qualification. The explicit ignored KVM test in `machine.rs` supplies the complementary pending-I/O, timer-ordering and completion evidence under an exclusive CPU/SMT lease.
