# Connection selector AMD validation

This is a finite new-engine validation procedure, not a qualification result.
Run on an otherwise idle qualified AMD Zen Linux KVM host, in the project dev
shell, with the existing qualified kernel, static BusyBox and guest init inputs
available. Do not change kernel pins or release admission constants. Root run/replay
owns CPU/SMT leasing and MSR setup/restoration. Keep every failed attempt; do not
retry a failed seed with different parameters and call it a pass.

Build and portable checks (all must exit zero):

```sh
cargo fmt -p seedbox --check
cargo clippy -p seedbox --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release --locked
cargo build-vmm
cargo test-vmm
bash harness/acceptance.sh portable
```

The fixture uses three Python guests and explicit cooperative phase commands.
Two concurrent TCP flows connect client ports 4000/4001 to server port 8000. The
scenario selects only server-to-client port 4000. That return path must time out,
port 4001 must complete, and an abortive close/reopen of port 4000 must complete
with a new incarnation while the old selector stays armed. Stale admission must
fail, heal must report `dropped > 0`, and no active handles may remain. These are
guest-owned checks. This fixture makes no HTTP accepted-write claim.

Run seed 61 once with full state, then replay the retained run normally and
with undo disabled. Set `RUN61` to the literal run directory printed by `PASS:`.
The run must exit 0 with PASS; both replays must exit 0 with MATCH, including
full machine state comparisons.

```sh
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/connections.yaml --seed 61 --full-state
RUN61=/absolute/path/from/PASS
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN61"
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN61" --undo 0
```

No-selector control: use the #60 plain world on main and this branch, with the
same retained guest/kernel inputs, seed and runtime parameters. Record both
commit IDs and engine hashes. Require matching world and per-machine execution
witnesses and guest output; compare network witnesses when both engines report
them. Compare separate executions: a cross-engine
replay is not this control. This checks that passive connection tracking leaves
ordinary execution unchanged.

Seed 62, undo 16, margin 32768 and the observation comparison are outside this
focused selector check; broader engine qualification remains with the smoke tier.

The portable packet tests separately cover SYN/data retransmission, simultaneous
open, FIN half-close, RST, changed ISN, options, malformed/truncated/fragmented and
non-TCP traffic, missing/evicted incarnations, bounded pages, protected selections
under connection churn and capacity exhaustion, overlapping handles, partition
precedence, RNG positions, snapshot restoration and send-time retention.
They also compare passive classification against legacy link decisions, logs and
network witness bytes. The finite guest runs above do not establish support for
IPv6, fragments, HTTP-object selection or every TCP lifecycle on real guests.
