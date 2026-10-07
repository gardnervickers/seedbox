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

This is RAM-only crash and reboot. Nothing persists across a crash, so the
new incarnation starts from its initramfs. Retained disks (#58, #59) will
add the one thing that survives: the crash path is where the engine decides
what the next incarnation inherits, and today that is only the logs.

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
