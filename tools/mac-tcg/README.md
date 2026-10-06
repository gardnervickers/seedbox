# Experimental local Mac TCG runner

This tool boots real x86-64 Linux on an Apple Silicon Mac through QEMU TCG.
`run.py` probes controlled clocks and entropy; `service.py` runs the existing
etcd namespace fixture with packet controls, partition/heal and process
stop/restart. It is a standalone experiment for
[issue #8](https://github.com/gardnervickers/seedbox/issues/8), outside the
production `seedbox run` / `replay` path.

The [acceptance record](../../docs/acceptance.md#local-mac-tcg-feasibility)
summarizes the historical results and witness limitations. Exact measured
sources, failed attempts, manifests and raw captures remain in the
[historical experiment](https://github.com/gardnervickers/seedbox/tree/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/mac-tcg).
The aggregate-report scripts for that campaign are preserved there too.
`testdata/` keeps one captured run used by the evidence-tampering regression
tests; its presence does not qualify a new execution.

## Portable tests

From this directory, use its own pinned Nix shell:

```sh
nix develop --command python3 -m unittest discover -p 'test_*.py' -v
```

The tests exercise classification, archive construction, adapted guest source,
witness decoding, signal cleanup, budget accounting and evidence validation.
They do not start QEMU or qualify the current checkout's guest execution.
The fixture adapter reads `guest/` at the repository root and rejects source
or cached-image drift.

## Cached-input execution

Initial artifact preparation is still manual. A clean checkout does not
include the kernel, initramfs, networking modules or service binaries. The
measured profile pins QEMU 11.1.1, stock Linux 6.18.54, one vCPU, fixed RTC,
no external network or disks, and seeded entropy before application startup.
Do not substitute the KVM-patched guest kernel; QEMU does not implement its
private PV ABI. The pinned flake supplies QEMU, Python, Zig and Clang.

Both runners require `build/bzImage` with SHA256
`66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc`.
The service runner additionally requires:

- `build/single-merged.cpio`, SHA256
  `4a926d378fb651676a1c4f208fa0be0c13a098f5d94bffcbde1d6f55b1f61220`.
  Its init, network and member scripts must match the current `guest/` sources.
- `build/modules/{veth,act_gact,sch_ingress,sch_netem,cls_u32,cls_matchall}.ko.xz`
  from the pinned Linux 6.18.54 module build. The runner retains their hashes.
- For `--etcd-version 3.5.3`, `build/etcd-3.5.3`, SHA256
  `79e672f61c4ae4197cee976fc4e6ab94cf42b40124a42e1c3774dc34c8d01c5e`.

The historical report records exact artifact origins and preparation details.
After these inputs are cached, commands start from this directory:

```sh
mkdir -p build
nix develop --command bash build-guest.sh
nix develop --command python3 run.py \
  --kernel build/bzImage --output runs/new-probe
nix develop --command python3 service.py --mode fault --shift 0 --witness \
  --seeds 1009 1009 --timeout 60 --output runs/new-fault
```

Service modes are `controls`, `healthy`, `fault` and `negative`; the last plants
a missing acknowledged write. Use a fresh output directory for every batch.
The service runner uses an exclusive batch lock and a persistent
`build/service-budget.json` with a 600-second aggregate QEMU budget. Preserve
existing receipts and reconcile a pending interrupted attempt before running
again. Do not reset a spent budget to hide attempts; a further campaign needs
its own explicitly agreed budget. The original campaign used 573.504 seconds.

The optional witness plugin currently uses macOS CommonCrypto and QEMU plugin
API 7. Linux portability of that plugin is unimplemented. It hashes selected
instruction boundaries and architectural discontinuities through explicit
guest completion, not all instructions, registers, RAM or device state.
Its schema and hashes are separate from KVM witnesses.

## Remaining integration

Production integration must use the existing preparation, content-store and
execution interfaces, declare the backend/topology/time/capability profile,
and retain inputs and fault acknowledgments for replay. Automated artifact
preparation and a clean-checkout execution remain unqualified. A shared kernel
does not provide independent-machine clocks, reboots or durable-storage
failures. Two-machine causal coordination, complete snapshots and campaign
effectiveness remain open in issues #8, #10 and #15.
