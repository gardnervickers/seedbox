# Low-level engine fixtures

Application acceptance uses the [YAML scenarios](../acceptance/workloads/) with
`seedbox run` / `seedbox replay`. Faults, workloads and verification run in their
guest drivers. Seed sweeps are ordinary CLI loops.

## Removed application paths

| Removed definition | Replacement |
| --- | --- |
| `etcd1.json`, `guest/etcd1.init` | Removed obsolete single-member boot fixture; use the etcd Compose example |
| `cluster.json` | [etcd Compose example](../../examples/etcd/compose.yaml) |
| `kill-3.5.2.json`, `short-kill-3.5.2.json` | [etcd 3.5.2 scenario](../acceptance/workloads/short-kill-3.5.2.yaml) |
| `kill-3.5.3.json` | [etcd 3.5.3 scenario](../acceptance/workloads/kill-3.5.3.yaml) |
| `sanity.json` | [planted-failure control](../acceptance/workloads/sanity.yaml) |
| `race.json`, `guest/race.init` | [race scenario](../acceptance/workloads/race.yaml), using the existing C workload |
| `campaign.sh`, `campaign-result.py` and their tests | `seedbox run` exit status, retained reports, and CLI seed loops |

These replace the application acceptance workflow; they do not preserve the old
serial fault schedule, witnesses or historical failing seeds. The removed files
from this cleanup are available in Git at `c7d7c22`. Historical research records at commit `788fc43` retain their original references
and identities.

## Remaining fixtures and dependencies

| Definition | Why it remains |
| --- | --- |
| `smoke.json`, `net.json` | Raw kernel boot/timer/entropy and virtual NIC checks |
| `cluster-fixed.json`, `single-vm*.json` | Matched multi-VM versus one-VM network-namespace experiments; Compose uses one service per VM |
| `cluster-fork.json` | `cluster-fixed.json` booted once and forked per seed (see below) |
| `cluster-fork-late.json` | The same cluster forked at 1.4 simulated seconds, after etcd has started, with timer and link jitter (see below) |
| `cluster-fork-skew.json` | `cluster-fork.json` released with up to 50 ms of seeded skew, jitter, and a timed partition after the release (see below) |
| `rdrand.json`, `rdrand-raw.json` | RDRAND/RDSEED as a guest sees them: hidden from CPUID and, with `-raw`, executed anyway (see below) |
| `net-pause.json`, `net-crash.json`, `net-crash-fork.json` | `net.json` with machine `b` paused, or crashed and rebooted, while `a` pings it (see below) |
| `disk-nocrash.json`, `disk-crash.json`, `disk-crash-fork.json`, `disk-ext4-crash.json` | One machine with a retained disk: raw-block FLUSH/FUA probe with and without a crash, and an fsync ledger on ext4 across a crash (see below) |

`batch.sh`, `batch.py` and `msr54.py` still run these fixtures with shared CPU/SMT
leases and verified MSR restoration. Runtime lifecycle tests also use their CPU
topology/readback helpers. `guest/build-netns.sh` still needs `build-client.sh`,
`build-initramfs.sh`, `cluster.init`, `single.init`, `network.sh`, `members.sh`,
`netprobe.c` and `sbx.c` for the one-VM/multi-VM comparison. The raw boot/network
fixtures use the same initramfs builder. These dependencies are not replaced by
the application YAML scenarios.

See the [one-VM fixture instructions](../../README.md#one-vm-network-fixture) for
building and running those experiments. Use the [runtime fault API](../../python/README.md)
for new application scenarios.

## Boot once, fork per seed

`cluster-fork.json` is `cluster-fixed.json` with a `boot` object. With `boot`,
the engine boots every machine under `boot.seed`, and each guest stops at its
launch barrier (`sbx barrier` in `cluster.init`, `guest/barrier.h` in the
Compose supervisor) just before it starts its application. When every machine
has arrived, the world is released under the run seed: each machine resumes at
the latest barrier time plus a seeded delay of up to `boot.release_skew_ns`,
reads per-seed entropy and reseeds its kernel CRNG, and timer jitter, link
jitter, faults, control and scenario events start from the run seed. So two
runs with the same `boot` and different seeds share everything up to the
barrier and nothing after it.

`--fork-seeds A,B,...` runs further seeds in the same engine process. The
engine keeps the world at the barrier and restores it before each later seed
instead of booting again; with undo on, only pages written since the barrier
are copied back. Each seed's report and consoles go to `OUT/seed-N/`, and the
JSON on stdout lists them under `seeds`. A restored seed must match a run of
that seed alone, byte for byte: compare `world_witness` and, with `--digest`,
each machine's `state_digest`.

```sh
CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/cluster-fork.json fork-cold "1010" --digest
CPU0=14 SEEDBOX_TIMEOUT=300 harness/batch.sh harness/worlds/cluster-fork.json fork-loop "1009" --digest --fork-seeds 1010,1011
```

With `boot.fork_at_ns` instead, the world forks at a simulated time rather
than at the guests' barrier: every machine parks at its first deterministic
point at or after that time, so boot and application startup are both shared.
No guest cooperation is needed, but nothing reseeds the guest kernels either:
seeds differ only through what the run seed drives after the fork (timer and
link jitter, release skew, faults, control and scenario events). A world with
no jitter and no faults therefore runs identically for every seed after a
time fork, which is why `cluster-fork-late.json` adds jitter. Run it the same
way, with `fork-late-cold` and `fork-late-loop` prefixes.

The `scenario` partitions these fixtures use are older than the runtime fault
API and don't follow its same-time ordering (see
[docs/runtime.md](../../docs/runtime.md)). Each is queued at the sending machine
and turns on or off when that machine's next slice starts, which can be up to
the early-delivery window (half the link latency, at most `stop_tolerance_ns`)
before its `at_ns`. Frames sent in that window already see the new state. New
scenarios should use `faults` or the runtime fault API instead.

Times in a forked world's plan (`faults[].at_ns`, `scenario[].at_ns`,
`kill_restart.start_ns`) must be at or after the release, the fork point plus
`boot.release_skew_ns`; an earlier time is an error, checked before boot for a
time fork and at the release for a barrier fork. A machine released later than
it parked takes the timer interrupts that fell due meanwhile as one late tick.
Management readiness signalled before a time fork carries over to every seed.
`cluster-fork-skew.json` exercises the skew and a partition after the release;
run its seeds alone and in both orders and compare the witnesses:

```sh
for s in 1010 1011; do CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/cluster-fork-skew.json fork-skew-cold "$s" --digest; done
CPU0=14 SEEDBOX_TIMEOUT=300 harness/batch.sh harness/worlds/cluster-fork-skew.json fork-skew-ab "1009" --digest --fork-seeds 1010,1011
CPU0=14 SEEDBOX_TIMEOUT=300 harness/batch.sh harness/worlds/cluster-fork-skew.json fork-skew-ba "1009" --digest --fork-seeds 1011,1010
```

The second command's `seed-1010` report must carry the first command's world
witness. Each seed report's `boot` object records the barrier time, the boot's
host seconds, and for forked seeds the restored page count and restore time.

## RDRAND and RDSEED

The engine hides both from CPUID, but AMD SVM cannot intercept them
([backends.md](../../docs/backends.md#qualification-and-capability-limits)).
Build `rdrand.cpio` from `guest/rdrand.init` with `guest/rdrandprobe.c` at
`bin/rdrandprobe`. `rdrand.json` must print `cpuid: rdrand=0 rdseed=0` and
`not offered` for both, and two runs of one seed must give the same
`world_witness`. `rdrand-raw.json` also executes both instructions without
asking CPUID: `#UD` means the CPU refused them (Intel), and differing values
across runs of one seed mean they reached the hardware (AMD), which only code
that ignores CPUID can do.

## Machine pause, crash and reboot

Four fault actions act on a whole machine at an exact world time. Each is a
`faults` entry like a partition, applied by the engine at `at_ns` with no
guest involvement (`deadline_ns` must equal or follow `at_ns` and is not
otherwise used):

| Action | Allowed from | Effect |
| --- | --- | --- |
| `pause` | running | The vCPU and its devices stop. RAM, registers, device state and any unpaid I/O cost are kept as they are. |
| `resume` | paused | The machine continues at `at_ns`. Timer ticks that fell due while it was paused fire once, late, keeping their phase, and its held frames are delivered before it runs. |
| `crash` | running or paused | The VM is destroyed: RAM, devices and their queues, KVM's I/O state, management commands queued for it and the undo checkpoint. |
| `reboot` | down | A new incarnation boots at `at_ns` from the same kernel, initrd and command line, with entropy and timer jitter drawn from the run seed for that incarnation. It never waits at a launch barrier. |

Any other transition (resuming a running machine, rebooting one that is not
down) fails the run with a fault error, as a duplicate partition does.

Lifecycle actions apply before anything else at their time, in plan order.
A machine never receives early a frame that arrives at or after its next
pause or crash. Other machines keep running and sending throughout. A frame
that arrives while its receiver is paused waits for the resume. Of the frames
sent after the pause began, at most 256 wait; later ones are dropped at the
receiver. Frames already in flight when it paused also wait, uncounted, since
there can only be a lookahead's worth. A frame that arrives while its
receiver is down is dropped, and so is anything the dead incarnation was
holding. Frames still queued for a machine that is down when the run ends
count as neither delivered nor dropped, like any frame in flight at the end. Frames already in flight when the receiver
reboots, and everything sent to it afterwards, reach the new incarnation's
network device like any other frame. The engine sends nothing on a machine's
behalf: peers learn about a crash only from silence, as with a power cut.
When every machine is paused or down with no resume or reboot planned, the
run idles to `duration_ns`.

A crash is not an exit. Crashing the `end_on_exit` machine doesn't end the
run: it ends when a later incarnation of that machine exits, or at
`duration_ns`. In Compose runs that machine is the controller, which can't be
a lifecycle target.

Process faults belong to an incarnation. A crash makes the machine's waiting
`stop` and `restart` faults unavailable, a process fault for a machine that is
down is unavailable at once, and a rebooted machine must report management
readiness again before it is sent anything. Guest time does not restart: a
new incarnation's clock reads world time from its first instruction, and its
uptime counts from its boot.

The report gives each machine's `incarnation` (0 for the first boot),
`booted_at_ns` and `crashed_incarnations`, one summary per incarnation that
crashed, with its exit witness. Its console and management logs continue
across incarnations, and its exit witness covers all of them. A machine's
`state` is `Paused` or `Down` while it is. With `--fork-seeds`, a seed that
crashed a machine leaves no trace in the next: the engine boots a first
incarnation again to restore into.

Without a disk this is RAM-only crash and reboot: the new incarnation starts
from its initramfs. A machine with a `disk` (below) also keeps the disk's
flushed writes.

`net-pause.json` pauses `b` from 2 s to 3.5 s while `a` pings it, and
`net-crash.json` crashes `b` at 2 s and reboots it at 2.5 s. Check that a run
repeats exactly, and that `SEEDBOX_UNDO=0` and a fixed `SEEDBOX_MARGIN` give
the same witnesses. `net-crash-fork.json` adds a time fork, so a crash in one
seed is followed by a restore for the next:

```sh
CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/net-crash.json crash "1009 1009" --digest
CPU0=14 SEEDBOX_UNDO=0 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/net-crash.json crash-undo0 "1009" --digest
for s in 1010 1011; do CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/net-crash-fork.json crash-fork-cold "$s" --digest; done
CPU0=14 SEEDBOX_TIMEOUT=300 harness/batch.sh harness/worlds/net-crash-fork.json crash-fork-loop "1009" --digest --fork-seeds 1010,1011
```

## Retained disk

A machine's `disk` gives it a virtio-blk disk at `/dev/vda`:
`{"size_mib": N}` for a zeroed disk, or `{"image": PATH}` for a raw image
(relative to the config; with `size_mib` too, it must be that size). Its
semantics (`disk.rs`, model `flush-stable/drop-all-unflushed/v1`):

- The device offers a write cache with FLUSH and no native FUA, as virtio 1.2
  defines it, so Linux runs it in writeback mode and emulates FUA with a
  FLUSH after the write. One queue; reads, writes, FLUSH and GET_ID.
- Requests run in ring order when the guest notifies the queue and complete
  before the notify returns, at a deterministic point. No rollback crosses a
  completed request.
- A completed write is volatile; reads see it at once. A completed FLUSH
  makes every write completed before it stable before it is acknowledged.
- By default a crash drops every volatile write and keeps every stable one.
  The next incarnation boots with that disk and a fresh device. Opt-in seeded
  survival is described below. This models a machine crash, not host power loss:
  nothing is written to host storage.
- A fork restore puts the whole disk back, both layers.

Each machine's report has a `disk` object: the model, capacity, the base
image's SHA-256 (null for a zeroed disk), a `digest` of both layers, their sizes and request counts.
The disk's digest feeds the world witness, and with `--digest` it is the
`disk` part of the machine's state digest. Each crashed incarnation reports
`disk_lost_sectors`. With `--trace`, each completed request is a `d` line
whose value is `type << 56 | status << 48 | sector` (type 1 write, 4 flush).

`guest/build-disk.sh artifacts/disk.cpio artifacts/ext4.img` builds the
probes' initramfs and a fixed ext4 image (with the flake's pinned `SEEDBOX_MKE2FS`; a run records the image's hash as `base_sha256`). Disks need
a kernel with `VIRTIO_BLK` and `EXT4_FS` (`kernel/config.options`).

- `disk-nocrash.json`: `guest/blkprobe.c` writes a header and `A` with an
  fsync each, `F` with `O_DSYNC` (FUA), then `B` over `A` and `C` with no
  flush, reads them back, flushes and checks again. Exits 0 on PASS.
- `disk-crash.json`: the same writes, then the machine crashes at 5 s and
  reboots at 5.5 s. The next incarnation must read `A`, `F` and zeros: the
  flushed and FUA writes survive, the unflushed ones do not. The crashed
  incarnation reports 16 lost sectors.
- `disk-ext4-crash.json`: `guest/fsledger.c` appends 50 records with an fsync
  each and 20 without to a file on ext4, then the machine crashes and
  reboots. After journal recovery the file must hold records 1..M in order,
  whole, with 50 <= M <= 70.
- `disk-crash-fork.json`: `disk-crash.json` with a time fork at 0.2 s, before
  the probe writes. Each later seed restores a disk that the previous seed
  wrote, flushed and crashed, so it must match that seed's cold run.

```sh
for w in disk-nocrash disk-crash disk-ext4-crash; do CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/$w.json $w "1009 1009" --digest --trace; done
CPU0=14 SEEDBOX_UNDO=0 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/disk-crash.json disk-crash-undo0 "1009" --digest
for s in 1010 1011; do CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/disk-crash-fork.json disk-fork-cold "$s" --digest; done
CPU0=14 SEEDBOX_TIMEOUT=300 harness/batch.sh harness/worlds/disk-crash-fork.json disk-fork-loop "1009" --digest --fork-seeds 1010,1011
```

For seeded torn writes, set the world's top-level `disk_loss` to
`flush-stable/seeded-latest-sector-512-half/disk-loss-machine-incarnation/v1`.
[`disk-ext4-torn.json`](disk-ext4-torn.json) enables it for the same fsync ledger.
Each latest unflushed 512-byte sector survives with probability 1/2, with its own
machine/incarnation RNG domain. Stable data remains unless overwritten by a
surviving sector; older overwritten volatile versions are not candidates. See
[the full contract](../../docs/runtime.md#disk-crash-policy).

On the qualified Linux host, run the fixture with repeated and distinct seeds:

```sh
CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/disk-ext4-torn.json disk-ext4-torn "1009 1009 1010 1011" --digest --trace
```

Check the recovered ledger retains all 50 acknowledged records and repeated-seed
witnesses match. Inspect the crashed incarnation's `disk_lost_sectors` and recovered
unacknowledged records across seeds. Variation requires unflushed writes to have
reached the device at the crash: data still in Linux's page cache cannot survive,
and a filesystem journal commit can already have flushed it. This fixture's
seed variation and ext4 recovery under the new policy remain hardware acceptance
checks; portable disk-model tests do not establish them. The raw `blkprobe` crash
fixture explicitly expects drop-all loss and is not an oracle for this policy.

## Directed-link policy controls

`harness/run-links.sh` runs six low-level broadcast-probe controls on the AMD
host. `shaped.json` and `plain.json`, and the full probe's C source and init,
retain the experiment inputs from `14aab0c`: L1/L2 repeat the full shaped world;
P1/M1 compare the full plain world on the PR and base engines. Prior train/3
witness prefixes were `ca9a7f6d` and `b66611d2`, respectively; a new train must
verify equality again, since its engine bytes determine identity.

Only U1/U2 use `shaped-tol-short.json`, with default undo (U1) and undo off
(U2), identical world/initrd/seed, and retained 600-second safety timeouts. The
performance target is **U2 below 300 seconds**, with U1 == U2. The previous
1 ms global latency, 200 us tolerance and 4 ms probe gap produced 2,845 U1
precise stops and a 564-second U2 run. Shortening the 2.5 s duration cap alone
would not help: `end_on_exit: a` normally ends execution near 1.2 s.

The short world now raises **every latency by 9 ms**: global 10 ms, a->b baseline
12 ms, old/new shape layers 14/17 ms, and loss layers 10 ms. This preserves
relative delays and keeps every policy above the global scheduling floor. Its
5 ms `stop_tolerance_ns` gives both a 5 ms early world window
(`min(global_latency / 2, tolerance)`) and a 5 ms late timer window. These
are semantic world inputs, applied identically to U1 and U2; undo remains a
performance-only variant. No engine or full L/P fixture bytes change.

All 16 fault IDs, actions, handles, jitter/loss settings and times stay in order
from 0.3 to 1.0 s. Each machine's world `cmdline` sets
`seedbox.link_gap_ns=1000000`; `link-short.init` reads it, retaining its old 4 ms
default when absent. The 1.1 s end and a's 0.1 s reporting grace stay fixed.
This restores an estimated
**340 uncut lossmid sends** over 0.6–0.7 and 0.76–1.0 s, with 500 us jitter and
50% loss (the old 4 ms gap allowed only 85). The 100 ms after the final heal
exceeds the longest shaped delay, 17 ms. The unchanged 2.5 s cap leaves reporting
headroom. Actual send counts and completed reports remain host evidence; a
scheduled candidate count is not a measured guest send count.

### Short-world stop budget

Most precise stops in the old short world came from the **cross-machine
horizon**, not the probe: a slice is capped by the earliest other machine's key
plus global latency (`crates/seedbox-vmm/src/main.rs`), and the probe's busy
polling gives no natural exit before that cap. With a 1 ms horizon and a 200 us
early window, slices ended between sends by stepping. The 10 ms horizon and 5 ms
windows cover several 1 ms send gaps, so the next natural send or device exit
usually ends the slice first. `precise_stops` counts entries into stepping; the
engine's `--trace` does not record why a stepping entry began.

Measured on the AMD host (main b6e9809, PR #127 at 47cfb5e):

| World | U1 precise stops | U1 rollbacks | U1 seconds | U2 seconds |
| --- | --- | --- | --- | --- |
| 1 ms latency, 200 us tolerance, 4 ms probe gap | 2,845 | not recorded | 30 | 564 |
| 10 ms latency, 5 ms tolerance, 1 ms probe gap | 433 | 3 | 33.0 | 118.5 |

U1 and U2 share world witness b0427503f6428b12 and network witness
32838ac9f494e2fb, with loss on c->a and c->b, the c|a cut counted both ways, all
16 faults applied in order and no reorders. `links.py` fails U1 if it records no
rollbacks, so the pair always exercises undo.

Run the pair with `RUNS="U1 U2"` through `harness/run-links.sh` on two free
physical cores with idle SMT siblings. U2 is a fresh same-seed run with undo off,
not a replay of U1.

`harness/links.py` derives full-world sender-stamp window expectations from the
retained world JSON, using cut precedence, highest live shape ID and heal by
handle. Window 45 of a<-c (4.5–4.6 s) is still lossmid: healing the cut at 4.5 s
does not heal lossmid until 4.7 s. A packet's arrival cannot shift its reported
window, since the probe indexes receipts by the sender's stamp.

The prior checker already expected 50% receipts there. Train/3's 207/400 receipts,
1.004 ms minimum and 1.25 ms mean satisfy that plan; #97 alone reproduced the
same tuple (world witness `ca9a7f6d`, network witness `bf975406`). Its 1.64 ms
maximum exceeded the old 1.6 ms cap because sender stamping precedes `sendto`
and receiver timing follows `recv`, including both kernels and concurrent bursts.
Jittered windows now keep a loose 200 us receipt allowance (1.7 ms for window 45).
Count, floor and mean checks remain; fixed-delay receipt tails stay diagnostic.

When a full shaped lane has a `network.jsonl` of routed-frame records, the
analyzer checks it exactly. Each line is a JSON object with machine names `src` and
`dst`, and integer `send_ns` and `arrival_ns`. Include **every queued frame**,
including boot/ARP traffic, in routing order, not receipt order; omit cut/lost
frames, which reserve no FIFO arrival. The bound is
`max(send_ns + latency_ns + jitter_ns, link_last + 1)`, with a lower bound of
`max(send_ns + latency_ns, link_last + 1)`. Thus delay decreases can preserve an
old FIFO tail, but a FIFO extension is exactly one nanosecond. At least one c->a
arrival must be present in routed-time window 45. A present file that is empty,
invalid or out of bounds fails the analyzer. An absent file is a SKIP: the
portable `crates/seedbox/tests/link_worlds.rs` covers the exact bounds instead,
by driving the production `links.rs` and `faults.rs` through `shaped.json` and
checking nominal arrivals in window 45. Synthetic portable tests cover both
inclusive boundaries and one-nanosecond violations; the Rust fixture tests still
exercise production `Faults`/`Links` directly.

**Capture limitation:** this engine's `--trace` emits machine/device exits, not
routed send/arrival pairs. Its network report retains only a witness digest,
not the inputs needed to recover those times. The runner retains the device
traces and copies `network.jsonl` if an external capture supplies it in the run
output. `NETWORK_TRACE_ROOT` can instead point to same-run captures under
`L1/network.jsonl` and `L2/network.jsonl`. Such captures must come from the exact
runs being analyzed; a model-generated trace or a different run is not engine
evidence. No route capture is implemented here, since changing the engine is
outside this follow-up's scope, so on the host L1/L2 report the exact-bound
check as covered by `link_worlds.rs`. The host lanes still check witnesses,
counters, fault application, FIFO receipt order and the receipt-time plan.

Run selected lanes concurrently, one distinct physical core per lane:

```sh
RUNS="L1 U1" CPUS="14 15" SHA=<fixture-commit> MAIN=<base-commit> bash harness/run-links.sh
```

Omit `RUNS` to select L1 L2 P1 M1 U1 U2, and supply six `CPUS` in that order.
Unknown/duplicate lane names, duplicate CPUs and missing cores are errors before
host work. Each lane keeps the existing CPU/SMT lease and MSR restoration in
`batch.py`; SMT siblings must be idle. A subset checks its own lane evidence and
prints SKIP for pair comparisons whose partner was not selected. All children
are joined, including after failure, before analysis and cleanup. Interruption
waits for bounded lane completion before deleting temporary builds. Each
invocation owns fresh temporary build and results directories. Results are kept
under `~/seedbox-link60-results/run.*`; shared results are never removed.
