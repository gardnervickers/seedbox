# Dynamic network fault validation

This completes the remaining network mechanisms of #173 through the existing
runtime `shape`/`heal` control path. The [protocol](../python/PROTOCOL.md#dynamic-network-impairments)
defines all parameters, layering, evidence and supported reset traffic.

## RNG audit and compatibility

The pre-change network streams are `link-loss/FROM/TO` and `link/FROM/TO`
(jitter), derived by `rng::Rng` from the seed and versioned domain. For each
non-cut directed candidate, loss draws first and jitter second, once each,
including selected connection loss and bounded-queue tail drops. A cut draws
neither. Broadcast candidates follow ascending machine index. Bandwidth,
fault-handle administration and connection classification have no RNG stream.
Checkpoint branches prefix domains with their full branch identity.

These domains and draw sites remain unchanged. The five new fault streams are
separate per direction and draw only while their fault is active. Off, masked,
healed or cut layers draw nothing. New state contributes no witness bytes until
a new impairment is used. Existing golden link witnesses and policy tests guard
the portable feature-off path. No CLI flags, output/exit contracts, admission
pins, disk policies or backend-specific adapter behavior are changed.

## Portable checks

Run inside the project development shell:

```sh
cargo fmt -p seedbox --check
cargo clippy -p seedbox --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash harness/acceptance.sh portable
```

Focused tests cover inactive draws and legacy order, independent streams,
directionality, exact bursts and non-reservation of clocks, overtaking,
duplication deadlines, checksummed resets and unsupported frames, dynamic
admission and healing evidence, strict parsing, transcript retention,
layer masking/healing, snapshot/reseed state and worker-count independence.

## Linux KVM checks

The VMM is Linux x86-64 only. Run the portable gate before hardware checks, then:

```sh
cargo build --release --locked
cargo build-vmm
cargo test-vmm --features kvm-tests
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/network-faults.yaml --cpu 14 --seed 173 --full-state
RUN=/absolute/path/printed/by/PASS
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu 14
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu 14 --undo 0
```

The guest scenario requires receipt overtaking, exact duplicate receipts,
consecutive losses, a measured spike, an actual `ECONNRESET`, positive injection
counts, successful healed UDP exchanges and a TCP reconnect. Both replays must
exit zero with `MATCH`. A second seed should also pass and replay; same-seed
replay compares retained inputs, rather than replaying across different engines.

For the feature-off control, build the base and changed engines from separate
source trees. Run `harness/worlds/plain.json` and `shaped-tol-short.json` with
identical retained link-probe/kernel inputs, seed and parameters, and compare
world/network/machine witnesses, RAM digests and network counters. Fresh runs
must agree with the base, and same-engine replay must match. The existing
`harness/fixtures/links` provides the guest workload; do not add a host scenario
runner. Use isolated host worktrees and preserve failures as evidence.

Final merge-train AMD and Linux reproducibility checks still apply to the train
head; finite feature checks do not replace them.
