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

Run exactly seeds 61 and 62, each once with full state, then the four replays
below for each retained run. Set `RUN61`/`RUN62` to the literal run-directory
printed by the corresponding `PASS:` line. Every run must exit 0 with PASS;
every replay must exit 0 with MATCH, including full machine state comparisons.

```sh
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/connections.yaml --seed 61 --full-state
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/connections.yaml --seed 62 --full-state
RUN61=/absolute/path/from/first/PASS
RUN62=/absolute/path/from/second/PASS
for run in "$RUN61" "$RUN62"; do
  sudo -E env PATH="$PATH" target/release/seedbox replay "$run"
  sudo -E env PATH="$PATH" target/release/seedbox replay "$run" --undo 0
  sudo -E env PATH="$PATH" target/release/seedbox replay "$run" --undo 16
  sudo -E env PATH="$PATH" target/release/seedbox replay "$run" --undo 0 --margin 32768
done
```

Observation control: run seed 61 once without full-state observation, using the
same checkout and cached registry image closure. Set `PLAIN61` to its PASS path.
The comparison below must print `observation: MATCH`: it checks exact network,
selection/control history, per-machine exit witnesses, guest output bytes and
retained runtime inputs. World witnesses differ by definition when full-state
RAM hashes are included, so compare the underlying exit/network witnesses.
Host runtime, PMU recovery statistics and capture metadata are not semantic.

```sh
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/connections.yaml --seed 61
PLAIN61=/absolute/path/from/plain/PASS
sudo -E env PATH="$PATH" python3 - "$RUN61" "$PLAIN61" <<'PY'
import json
import pathlib
import sys
left, right = [pathlib.Path(p) / 'output' for p in sys.argv[1:]]
a, b = [json.loads((p / 'vmm.json').read_text()) for p in (left, right)]
for key in ('seed', 'outcome', 'network_witness', 'network_counters',
            'frames_delivered', 'frames_dropped', 'faults', 'fault_error',
            'control_history', 'control_latency_ns', 'management_readiness'):
    assert a[key] == b[key], key
for x, y in zip(a['machines'], b['machines'], strict=True):
    for key in ('name', 'state', 'guest_exit_code', 'guest_init_error',
                'time_ns', 'branches', 'witness', 'pending_io', 'net'):
        assert x[key] == y[key], (x['name'], key)
    for suffix in ('console', 'mgmt'):
        filename = x['name'] + '.' + suffix
        assert (left / 'machines' / filename).read_bytes() == (right / 'machines' / filename).read_bytes(), filename
wa, wb = [json.loads((p / 'world.json').read_text()) for p in (left, right)]
assert wa == wb, 'runtime input closure differs; observation comparison invalid'
assert any(f.get('selection_counts', {}).get('dropped', 0) > 0 for f in a['faults'])
print('observation: MATCH')
PY
```

The portable packet tests separately cover SYN/data retransmission, simultaneous
open, FIN half-close, RST, changed ISN, options, malformed/truncated/fragmented and
non-TCP traffic, missing/evicted incarnations, bounded pages, overlapping handles,
partition precedence, RNG positions, snapshot restoration and send-time retention.
They also compare passive classification against legacy link decisions, logs and
network witness bytes. The finite guest runs above do not establish support for
IPv6, fragments, HTTP-object selection or every TCP lifecycle on real guests.
