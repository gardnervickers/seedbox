//! Rule C3 on a native TCG machine: a stop that delivers nothing leaves no
//! trace. The TCG Linux fixture boots and runs two shell loops that fork,
//! read the clock and sleep; the run is driven once with the machine's own
//! stops and host barriers every half millisecond, and again with the same
//! barriers and runs that end early at seeded times, a few microseconds
//! apart. At every barrier the native witness and the console must agree, and
//! so must the full state evidence (registers, devices, timers, RAM) at every
//! `STATE`th barrier and the last. It mirrors `pv::stop_tests` in the PV crate.
use super::tests::lifecycle_initramfs;
use super::*;
use crate::{
    clock::{Clock, SimulationClock},
    rng::{RngStreamExt, SeededRng},
};
use seedbox_machine::execution::ExecutionConfig;
use std::collections::VecDeque;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
const MS: u64 = 1_000_000;
/// Host barriers, shared by every schedule.
const BARRIER: u64 = MS / 2;
/// Full state evidence is taken at every this many barriers (and the last):
/// hashing guest RAM at every barrier would dominate the test.
const STATE: usize = 40;
/// The longest run of a split schedule.
const SPLIT: u64 = 50_000;
/// Simulated time after both loops are done, so the idle guest is compared too.
const AFTER: u64 = 3 * MS;
/// Bound on the simulated length of boot and the workload.
const LIMIT: u64 = 20_000 * MS;

/// Replaces the fixture init's "booted" line: two loops that fold the clock
/// into a value they print, with sleeps of different lengths between reads.
/// A leading 1 keeps the uptime digits out of octal.
const WORKLOAD: &str = r#"work() {
    h=$2
    i=0
    while [ $i -lt 120 ]; do
        read up idle </proc/uptime
        h=$(( (h * 131 + 1${up%.*}${up#*.} + i) % 1000000007 ))
        /bin/busybox sleep 0.00$3
        i=$((i + 1))
        if [ $((i % 40)) -eq 0 ]; then echo "C3_$1 $i $h"; fi
    done
    echo "C3_$1_DONE $h"
}
work CHILD 7 7 &
work PARENT 3 9 &
wait
echo 'seedbox: Linux userspace booted'"#;

fn input(key: &str) -> PathBuf {
    std::env::var_os(key)
        .unwrap_or_else(|| panic!("{key}"))
        .into()
}

fn machine() -> TcgMachine {
    let linux = input("SEEDBOX_TCG_LINUX");
    let mut machine = TcgMachine::new_raw(
        &input("SEEDBOX_TCG_QEMU"),
        Manifest {
            memory_mib: 128,
            disk: None,
            network: None,
            boot: Boot::Linux {
                kernel: fs::read(linux.join("kernel")).unwrap().into(),
                initramfs: lifecycle_initramfs(
                    &fs::read(linux.join("initramfs")).unwrap(),
                    WORKLOAD,
                )
                .into(),
                firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap().into(),
                command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr"
                    .into(),
                epoch_seconds: 0,
            },
        },
    )
    .unwrap();
    machine
        .configure_policy(
            "m",
            &BackendPolicy {
                seed: 42,
                timer_jitter_ns: 17,
                stop_tolerance_ns: 0,
                torn_writes: false,
                full_state: true,
                seeded_pv_speed: false,
                boot_end_evidence: false,
                controller: None,
            },
        )
        .unwrap();
    machine
}

fn run(
    machine: &mut TcgMachine,
    clock: &SimulationClock,
    rng: &mut SeededRng,
    upto: u64,
) -> StopReason {
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    machine
        .run(
            &mut RunContext {
                worker: &mut (),
                clock,
                rng,
                execution: ExecutionConfig::default(),
                inbox: seedbox_machine::pv::Inbox::new(&mut inbox, at(clock.now())),
                outbox: seedbox_machine::pv::Outbox::new(&mut outputs, at(clock.now()), at(upto)),
            },
            at(upto),
        )
        .unwrap()
}

/// What the machine shows at one barrier.
#[derive(PartialEq, Eq)]
struct Look {
    at: u64,
    console: Vec<u8>,
    witness: Option<seedbox_machine::machine::ExecutionEvidence>,
    state: Option<StateEvidence>,
}

/// Where two looks differ, in terms a failure can be read by: the state
/// evidence parts and the guest physical pages that differ.
fn differences(own: &Look, other: &Look) -> String {
    let mut found = Vec::new();
    if own.console != other.console {
        found.push(format!(
            "console:\n  own:   {}\n  other: {}",
            String::from_utf8_lossy(&own.console),
            String::from_utf8_lossy(&other.console)
        ));
    }
    if own.witness != other.witness {
        found.push(format!(
            "witness: own {:?}, other {:?}",
            own.witness, other.witness
        ));
    }
    if let (Some(a), Some(b)) = (&own.state, &other.state) {
        for ((name, x), (_, y)) in a.parts.iter().zip(&b.parts) {
            if x != y {
                found.push(format!("state part {name}"));
            }
        }
        let mut pages = Vec::new();
        let mut index = 0;
        for region in &a.regions {
            for page in 0..region.length / 4096 {
                let (x, y) = (
                    a.page_hashes.get(index * 8..index * 8 + 8),
                    b.page_hashes.get(index * 8..index * 8 + 8),
                );
                if x != y {
                    pages.push(format!("{:#x}", region.base + page * 4096));
                }
                index += 1;
            }
        }
        if !pages.is_empty() {
            found.push(format!("{} pages: {}", pages.len(), pages.join(" ")));
        }
    }
    found.join("\n")
}

/// Drive a machine through `barriers` (or, without them, until both loops
/// are done and `AFTER` more), looking at each. A blocked machine sleeps
/// until its deadline, as the Driver wakes it. With a `split` seed, most runs
/// end instead at a time drawn in (now, next barrier], at most `SPLIT` after
/// now. Returns the looks and how many runs the machine took.
fn drive(barriers: Option<&[u64]>, split: Option<u64>) -> (Vec<Look>, usize) {
    let mut machine = machine();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(7);
    let mut stops = split.map(SeededRng::new);
    let guard = seedbox_test_time::HangGuard::new(
        "TCG stop-invariance guest",
        std::time::Duration::from_secs(3600),
    );
    let mut looks = Vec::new();
    let mut runs = 0;
    let mut blocked: Option<Option<u64>> = None;
    let mut done_at = None;
    for index in 1.. {
        let (target, last) = match barriers {
            Some(barriers) => match barriers.get(index - 1) {
                Some(&target) => (target, index == barriers.len()),
                None => break,
            },
            None => (index as u64 * BARRIER, false),
        };
        if barriers.is_none() && done_at.is_some_and(|done| target > done + AFTER) {
            break;
        }
        assert!(
            target <= LIMIT,
            "guest never finished: {}",
            String::from_utf8_lossy(&machine.console().unwrap())
        );
        loop {
            guard.remaining().expect("TCG stop-invariance hang");
            if let Some(deadline) = blocked {
                match deadline {
                    Some(deadline) if deadline <= target => {
                        clock.set(deadline);
                        blocked = None;
                    }
                    _ => {
                        clock.set(target);
                        break;
                    }
                }
            }
            let now = clock.now();
            let split = now < target
                && stops
                    .as_mut()
                    .is_some_and(|stops| stops.next_u32() % 8 != 0);
            let upto = match &mut stops {
                Some(stops) if split => now + 1 + stops.next_u64() % (target - now).min(SPLIT),
                _ => target,
            };
            runs += 1;
            match run(&mut machine, &clock, &mut rng, upto) {
                StopReason::BoundaryReached => {
                    clock.set(upto);
                    if upto == target {
                        break;
                    }
                }
                StopReason::Blocked { deadline, .. } => {
                    blocked = Some(deadline.map(|deadline| deadline.as_nanos()));
                }
                other => panic!(
                    "unexpected stop {other:?}: {}",
                    String::from_utf8_lossy(&machine.console().unwrap())
                ),
            }
        }
        let console = machine.console().unwrap();
        let text = String::from_utf8_lossy(&console);
        assert!(!text.contains("Kernel panic"), "{text}");
        if done_at.is_none() && text.contains("C3_PARENT_DONE") && text.contains("C3_CHILD_DONE") {
            done_at = Some(target);
        }
        // The own schedule takes state at every STATE-th barrier; its last
        // barrier is only known when it ends, so it takes state there too
        // when it finds itself done, and a split schedule at its last.
        let finishing =
            barriers.is_none() && done_at.is_some_and(|done| target + BARRIER > done + AFTER);
        let state = if index % STATE == 0 || last || finishing {
            machine.state_evidence().unwrap()
        } else {
            None
        };
        looks.push(Look {
            at: target,
            console,
            witness: machine.execution_evidence().unwrap(),
            state,
        });
    }
    (looks, runs)
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU), SEEDBOX_TCG_LINUX and SEEDBOX_TCG_FIRMWARE"]
fn native_stops_that_deliver_nothing_leave_no_trace() {
    let (own, own_runs) = drive(None, None);
    let text = String::from_utf8_lossy(&own.last().unwrap().console).into_owned();
    assert!(
        text.contains("C3_PARENT_DONE") && text.contains("C3_CHILD_DONE"),
        "{text}"
    );
    assert!(
        own.iter().all(|look| look.witness.is_some()),
        "policy witness"
    );
    assert!(
        own.last().is_some_and(|look| look.state.is_some()),
        "full state evidence"
    );
    let barriers: Vec<u64> = own.iter().map(|look| look.at).collect();
    for seed in [1, 2, 0x5eed] {
        let (split, runs) = drive(Some(&barriers), Some(seed));
        assert!(
            runs > own_runs,
            "seed {seed} split runs: {runs} against {own_runs}"
        );
        for (a, b) in own.iter().zip(&split) {
            assert!(
                a == b,
                "seed {seed}: first difference at {} ns ({runs} runs against {own_runs}):\n{}",
                a.at,
                differences(a, b)
            );
        }
    }
}
