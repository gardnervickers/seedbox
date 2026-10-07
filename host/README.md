# Host kernel patches

These patches are for the simulation host's kernel, not the guest's. They are
meant to be performance only: a run's results are the same with or without them.

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
exports `perf_get_event` from the core kernel, so KVM can confirm the fd really
is a perf event (`f_op == &perf_fops`) rather than trusting its name. That
means a full kernel build, not just the KVM modules.

- NixOS: add it to `boot.kernelPatches`
  (`{ name = "seedbox-kvm-yield-filter"; patch = ./host/kvm-yield-filter.patch; }`)
  and reboot into the new generation.
- Elsewhere: apply it with `patch -p1` to the source of the running kernel's
  version and config, then build and install the kernel as usual.

The ioctl number (0xf0) is out of tree. `version` must be 1 or the ioctl fails
with `EPROTO`, but a future upstream ioctl with the same encoding would still
be mistaken for it, so rebase deliberately.

An independent review applied it to v6.18.54 and built `x86.o` and `svm.o`
warning-free (before the `perf_get_event` change). It has not been linked,
loaded or run.

### Using and qualifying it

`seedbox run --yield-filter` and `seedbox replay --yield-filter` turn it on
(`SEEDBOX_YIELD_FILTER=on` for the engine). On an unpatched host the run fails
at start with "host KVM has no yield filter". `vmm.json` reports
`yields_filtered` per machine, and `yields` counts only the yields that still
reached the VMM.

The VMM change itself makes a new engine (any `crates/seedbox-vmm` change
does), so this is a performance-only engine change, not a performance setting
on the accepted engine. Qualify it across engines: replay runs recorded on the
accepted engine with `--vmm <new seedbox-vmm> --allow-engine-change`, once
without and once with `--yield-filter`. All must report `MATCH` (world
witness, per-machine witnesses and, with `--full-state`, digests), and the
filtered replays must show `yields_filtered` in the millions and lower host
time.

Trap: a plain `seedbox replay RUN --yield-filter` uses the run's retained old
engine, which ignores `SEEDBOX_YIELD_FILTER` and reports a vacuous `MATCH`.
`yields_filtered` is then missing or null.
