# Host kernel patches

These patches are for the simulation host's kernel, not the guest's. They are
performance only: a run's results are the same with or without them.

## `kvm-yield-filter.patch`: in-kernel yield filter (#13)

While a soft stop is pending, the guest's syscall hook writes to port 0x52c on
every syscall. The VMM treats a write before the soft target as a transparent
yield: it does nothing and resumes the guest. Every one of those writes still
costs a full exit to userspace, which with VMSCAPE and TSA is several
microseconds. Multi-VM worlds take tens of millions of them per 60 simulated
seconds.

With the patch, KVM decides in the kernel. The VMM opens the filter with
`KVM_SEEDBOX_YIELD_FILTER` (a vCPU ioctl), passing its guest-only branch
counter's perf fd and the address of a two-word struct. Before each
`KVM_RUN`, it writes the target branch count there:
`yield_target(count, time, soft)` in `crates/seedbox-vmm/src/boundary.rs`, or
0 while stepping or while the window is closed. A 4-byte write to port 0x52c
at a count below the target is skipped in `kvm_fast_pio_out` and the guest
re-enters at once. Every other write, and every failure (the counter not
active on this CPU, or a fault reading the struct), exits to userspace as
before.

The kernel reads the same counter the VMM reads, at an exit, where the count
is exact. A yield is consumed only where the VMM would have made exactly the
same decision, so the guest, witnesses and digests don't change.

### Building

The patch is against Linux 6.18.54 (`gregkh/linux` tag `v6.18.54`). It
changes `struct kvm_vcpu_arch`, so rebuild **both** `kvm.ko` and `kvm-amd.ko`
from a tree configured exactly like the running kernel. It needs no core kernel
symbols beyond what KVM already uses.

- NixOS: add it to `boot.kernelPatches`
  (`{ name = "seedbox-kvm-yield-filter"; patch = ./host/kvm-yield-filter.patch; }`)
  and reboot into the new generation.
- Elsewhere: apply it to the matching source with `patch -p1`, build with
  `make M=arch/x86/kvm modules`, then with no VMs running,
  `modprobe -r kvm_amd kvm` and `insmod` the two new modules.

It has not been compiled or run yet. The first build on the host is the first
test.

### Using and qualifying it

`seedbox run --yield-filter` and `seedbox replay --yield-filter` turn it on
(`SEEDBOX_YIELD_FILTER=on` for the engine). On an unpatched host the run fails
at start with "host KVM has no yield filter". `vmm.json` reports
`yields_filtered` per machine, and `yields` counts only the yields that still
reached the VMM.

Qualification is the same as for undo: run a few seeds without the flag, then
`seedbox replay RUN --yield-filter` each one. Every replay must report `MATCH`
(world witness, per-machine witnesses and, with `--full-state`, digests), with
`yields_filtered` in the millions and lower host time.
