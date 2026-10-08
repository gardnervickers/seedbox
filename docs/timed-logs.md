# Committed-time logs (#63)

This changes the development engine. Frozen engine bytes, kernel pins, admission
policy and witness construction are unchanged. Mac portable checks cannot qualify
this engine; the AMD work below is still pending.

The existing commit gate rejects overshot exits or restores the held checkpoint
before applying output effects. `Machine::capture_output` observes accepted UART
writes (including port 0xe9) and management transport output immediately after
raw bytes are appended. The error-return path also indexes an accepted partial
management write. It never samples host time, changes stop placement, adds a
witness record, or changes an application's bytes. Metadata sink failures stop
indexing without turning successful guest execution into an error. Raw-output
limits and I/O errors retain their existing behavior.

Each stream has a sidecar `machines/NAME.console.chunks` or
`machines/NAME.mgmt.chunks`. Its eight-byte magic/version is `SBLOG001`, followed
by 40-byte records: five little-endian u64 values `(time_ns, incarnation, order,
offset, length)`. Incarnation fits u32; offsets address the unchanged raw stream.
A record covers consecutive bytes from one machine/incarnation/stream within a
world slice, not an application line. Contiguous byte writes extend the open
record until another stream writes, the incarnation changes, or `run_until`
returns (including errors). The timestamp is the **first byte's commit time**;
later bytes in the same chunk can have later times. Time filters select whole
chunks by that first time, so selected bytes may extend beyond an exclusive
upper bound. Per-machine order increases for each new burst across both streams
and survives reboot. Forks restore both metadata and ordering to the snapshot. Equal times across machines sort by
machine name; this is a deterministic display tie-break, not host dispatch order
or a claim that concurrent machines have a physical total order.

The hard cap is 65,536 records per stream (2,621,448 bytes including header).
The host adapter divides a 1,048,576-record budget among all streams (40 MiB of
records total). Low-level `--log-chunk-limit` can reduce capture to zero for
observation controls; it is not a semantic parameter. Raw bytes continue after
the metadata cap; the last allowed open record can still grow within its slice.
One MiB emitted through one-byte exits over 256 slices of 4 KiB each uses 256
records (10,248 bytes including header), rather than exhausting the cap at 64 KiB.
Alternating streams or one-byte slices can still exhaust the bound.

Records are serialized only after closing, through a buffered writer flushed at
every slice return and normal drop. There is no per-byte index write syscall.
Cancellation retains the flushed committed prefix without requiring a terminal
report. Abrupt termination may leave an unindexed raw suffix from the current
slice or buffered records. A killed write can leave a partial record, which the
reader rejects. A complete record prefix shorter than the raw stream reports
the uncovered byte interval;
it cannot distinguish capture limits, abrupt interruption, or sink failure.

`logs::Reader` validates the manifest, index sizes/version, contiguous byte
ranges, monotonic times/incarnations/order and cross-stream ordering before
filtering. It verifies retained file checksums, refuses symlinks, and reads only
manifest-declared files. Whole-file corruption/truncation fails checksum checks;
a declared partial record fails structural validation too. Missing indexes or
raw files and unindexed tails remain explicit even under filters. The reader
bounds total metadata to 1,048,576 records and copies payloads using fixed-size
buffers. Raw stdout/stderr from the host VMM have no simulated-time attribution.

Retention and bundles use the existing generic retained-log map and limits;
there is no special exception that includes an omitted sidecar or raw stream.
Portable orchestration tests cover success, error, cancellation and omission;
bundle tests delete original output before importing and reading the index.

## Portable retained demo

`crates/seedbox/tests/support/logs_fixture.rs` retains console bytes
`00 ff 41 0a 42` and management byte `fe`. It is an explicitly synthetic format
fixture, not engine proof. `seedbox logs RUN --since-ns 10 --until-ns 20 --json`
selects three chunks, in this order:

```text
machine   incarnation  stream   time_ns  order  offset  length  bytes_hex
workload  0            console  10       0      0       2       00ff
workload  0            mgmt     10       1      0       1       fe
workload  0            console  10       2      2       2       410a
```

The final console byte `42` is committed at time 20 in incarnation 1 and is
excluded by the exclusive upper bound. The default read includes it. These
values are asserted through the public CLI in `tests/cli.rs` and through the
library's raw byte API in `tests/logs.rs`. Without `--json`, stdout contains the
selected bytes themselves: the unfiltered fixture yields `00 ff fe 41 0a 42`,
including the binary bytes and without an added trailing newline. Status and
omissions are lowercase stderr diagnostics. JSON retains the chunk metadata and
`bytes_hex`; neither mode parses application output.

Portable tests also exercise the real index producer with one MiB of byte exits,
verify first-byte filtering and exact byte recovery through the reader, and cover
stream switches, incarnation/slice boundaries, the hard cap, sink failure,
snapshot reopening and flushing the last record on drop.

## AMD KVM acceptance still required

Use the existing qualified AMD host, the accepted guest kernel, a free CPU with
its SMT siblings reserved, and a fresh output directory. Do not change release
pins to run this development build. In the project development shell, set
`CPU` to the selected core and `KERNEL` to the accepted bzImage path. Run:

```sh
cargo build-vmm
cargo test-vmm
cargo build --release --locked
sudo -E env PATH="$PATH" target/release/seedbox doctor --cpu "$CPU" --kernel "$KERNEL" --allow-dev-engine
bash harness/check.sh --require counter --cpu "$CPU"
```

Expect both builds and non-ignored VMM tests to pass, doctor to report VALID with
the unqualified dev marker, and the native supervisor to pass all compiled
`machine::timed_log_tests` fixtures. It holds the shared CPU/SMT leases and MSR
workaround through child cleanup, then verifies restoration. The real guest
fixture writes binary bytes to COM1/debug and COM2 at equal time, then writes
after 1,000 branches. It compares raw bytes, terminal times, exit witnesses and
full state with metadata caps 0/2/65536 and undo 0/16/256, including an exact stop
at branch 500 and extra mid-run full-state observation. A reboot fixture checks
that old bytes/order survive and new bytes carry incarnation 1. The rejection test
injects rollback/overshoot decisions at a real KVM exit: it checks the actual
commit gate but does **not** establish that natural PMU skid occurred.

Next use public run/replay for real application and natural PMU evidence:

```sh
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/race.yaml --seed 1 --cpu "$CPU" --kernel "$KERNEL" --allow-dev-engine --state /tmp/logs63-race --full-state
# Set RUN to the exact directory printed by the preceding command.
target/release/seedbox logs "$RUN" --json > /tmp/logs63-all.json
target/release/seedbox logs "$RUN" --machine n1 --stream console --since-ns 0 --until-ns 1000000000 --json > /tmp/logs63-first-second.json
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --state /tmp/logs63-race
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --undo 16 --state /tmp/logs63-race
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --undo 0 --margin 32768 --state /tmp/logs63-race
```

Expect PASS and all replays MATCH, including full-state equality. Run `logs
REPLAY --json` on each printed replay directory and require byte-for-byte equality
with `/tmp/logs63-all.json`; the filtered file must contain only n1 console
chunks in [0, 1000000000). Inspect each `output/vmm.json`: retain actual positive
rollback counts from at least one undo-enabled execution, and verify no duplicate
or speculative chunk bytes. A campaign with zero actual rollbacks leaves natural
rollback acceptance **unverified**; do not relabel injected decisions as that
proof or retry a failed execution invisibly. Preserve every failed attempt.

Exercise one explicit low-margin no-undo control, retaining its outcome without
retry:

```sh
sudo -E env PATH="$PATH" target/release/seedbox replay "$RUN" --cpu "$CPU" --allow-dev-engine --undo 0 --margin 1 --state /tmp/logs63-race
# Set LOW_MARGIN_RUN to its printed directory.
target/release/seedbox logs "$LOW_MARGIN_RUN" --json > /tmp/logs63-low-margin.json
```

An actual PMU overshoot must remain ERROR with its diagnostic and a committed
prefix index containing no rejected output. A successful execution must MATCH;
that result leaves natural overshoot rejection unexercised. Do not require a
particular host skid or alter the witness to obtain either outcome.

For a comparison with the unchanged source engine, build the starting commit
separately, keeping all frozen artifacts intact:

```sh
BASELINE=$(mktemp -d /tmp/logs63-baseline.XXXXXX)
git archive 82f1adae51f96ec4fa44a5e9b338ce79b54f6231 | tar -x -C "$BASELINE"
(cd "$BASELINE" && cargo build-vmm)
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/race.yaml --seed 1 --cpu "$CPU" --kernel "$KERNEL" --vmm "$BASELINE/crates/seedbox-vmm/target/release/seedbox-vmm" --allow-dev-engine --state /tmp/logs63-before --full-state
# Set OLD_RUN to its printed directory.
sudo -E env PATH="$PATH" target/release/seedbox replay "$OLD_RUN" --state /tmp/logs63-before --vmm "$PWD/crates/seedbox-vmm/target/release/seedbox-vmm" --allow-engine-change --allow-dev-engine --cpu "$CPU"
```

Expect PASS then MATCH for the same prepared inputs, semantic result, exit and
full-state witnesses. The engine hash changes as expected. Both source builds
are development artifacts under the unchanged release policy. No missing old
index may be reconstructed from raw text.

For real cancellation, run the longer existing workload:

```sh
sudo -E env PATH="$PATH" target/release/seedbox run harness/acceptance/workloads/kill-3.5.3.yaml --seed 4000 --cpu "$CPU" --kernel "$KERNEL" --allow-dev-engine --state /tmp/logs63-cancel
# Send Ctrl-C after guest output is present; set CANCEL_RUN to its printed directory.
target/release/seedbox logs "$CANCEL_RUN" --json > /tmp/logs63-cancel.json
target/release/seedbox export "$CANCEL_RUN" --state /tmp/logs63-cancel -o /tmp/logs63-cancel.tar
target/release/seedbox import /tmp/logs63-cancel.tar --state /tmp/logs63-import
# Set IMPORTED_RUN to the imported directory printed above.
target/release/seedbox logs "$IMPORTED_RUN" --json > /tmp/logs63-import.json
cmp /tmp/logs63-cancel.json /tmp/logs63-import.json
```

Expect INCOMPLETE/exit 3 and restored leases/MSRs after cancellation. Valid indexes
must expose only committed prefix bytes with explicit gaps; a partial trailing
record must fail with exit 2, never silently repair itself. If a partial record
was captured, require the same rejection after import instead of `cmp` of JSON.
Retain a successful full/filtered text or JSON demo from the actual guest runs.
Cancellation while writing and natural overshoot outcomes depend on host timing;
record what actually occurred and leave any unexercised case unverified.
