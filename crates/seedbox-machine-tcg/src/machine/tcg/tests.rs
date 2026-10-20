use super::raw_program_data::{
    counting_loop, storing_loop, Asm, Reg, RBX, SCRATCH, SHUTDOWN, STUB, UNTIMED,
};
use super::*;
use crate::clock::Clock;
use crate::rng::SeededRng;
use std::sync::Arc;
type TcgMachine = crate::machine::test_environment::Environment<super::TcgMachine>;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fs, time::Duration};

const QEMU: &str = "qemu-system-x86_64";

// The portable Linux idle test requires userspace within 2 guest seconds.
// Each phase below adds its authored sleeps to that kernel-work allowance.
// Host speed does not decide success: Process::execute already has a test-only
// per-run watchdog to kill a QEMU that never returns (qemu::TEST_RUN_WATCHDOG).
const LINUX_KERNEL_WORK_NS: u64 = 2_000_000_000;
struct LinuxProgress {
    deadline: Timestamp,
    previous_start: Option<Timestamp>,
}
impl LinuxProgress {
    fn new(start: Timestamp, authored_sleep_ns: u64) -> Self {
        Self {
            deadline: at(start.as_nanos() + LINUX_KERNEL_WORK_NS + authored_sleep_ns),
            previous_start: None,
        }
    }
    fn next(&mut self, now: Timestamp, quantum: u64) -> Result<Timestamp, &'static str> {
        if now >= self.deadline {
            return Err("Linux fixture exhausted its guest-time progress budget");
        }
        if self.previous_start.is_some_and(|previous| now <= previous) {
            return Err("Linux fixture made no guest-time progress");
        }
        self.previous_start = Some(now);
        Ok(at(now.as_nanos() + quantum).min(self.deadline))
    }
    fn timer(&self, deadline: Timestamp) -> Timestamp {
        assert!(
            deadline <= self.deadline,
            "Linux timer exceeds fixture guest-time budget"
        );
        deadline
    }
}

#[test]
fn linux_progress_bounds_stalled_and_nonterminating_guests_without_host_time() {
    let mut stalled = LinuxProgress::new(at(0), 0);
    assert_eq!(stalled.next(at(0), 1_000_000), Ok(at(1_000_000)));
    assert_eq!(
        stalled.next(at(0), 1_000_000),
        Err("Linux fixture made no guest-time progress")
    );
    for (sleep, quantum) in [
        (30_000_000, 1_000_000),
        (100_000_000, 1_000_000),
        (50_000_000, 100_000),
    ] {
        let mut spinning = LinuxProgress::new(at(17), sleep);
        let mut now = at(17);
        while let Ok(limit) = spinning.next(now, quantum) {
            now = limit;
        }
        assert_eq!(now, at(17 + LINUX_KERNEL_WORK_NS + sleep));
        assert_eq!(
            spinning.next(now, quantum),
            Err("Linux fixture exhausted its guest-time progress budget")
        );
    }
}
/// LAPIC count whose deadline is 1003 program instructions in: the count
/// store completes at program instruction 4 and fires `count + 1` later.
const TIMER_COUNT: u32 = 998;

fn manifest(image: Vec<u8>) -> Manifest {
    Manifest {
        boot: Boot::Program(image.into()),
        memory_mib: 16,
        disk: None,
        network: None,
    }
}

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn clock(nanos: u64) -> Arc<TestClock> {
    Arc::new(TestClock(AtomicU64::new(nanos)))
}

fn start(image: Vec<u8>) -> TcgMachine {
    TcgMachine::new(
        Path::new(QEMU),
        manifest(image),
        SeededRng::new(0),
        clock(0),
    )
    .expect("QEMU fixture startup; run tests inside nix develop")
}

fn at(nanos: u64) -> Timestamp {
    Timestamp::from_nanos(nanos)
}

/// The time at which program instruction `n` completes at factor 1000.
fn after(n: u64) -> u64 {
    STUB + n
}

fn registers(machine: &mut TcgMachine) -> [u64; 17] {
    machine.process.as_mut().unwrap().registers().unwrap()
}

#[test]
fn stub_enters_the_program_at_its_first_instruction() {
    let mut machine = start(counting_loop());
    assert_eq!(
        machine.run(&mut (), at(STUB)).unwrap(),
        StopReason::BoundaryReached
    );
    let stopped = registers(&mut machine);
    assert_eq!(
        stopped[super::raw_program_data::RIP],
        super::raw_program_data::PROGRAM
    );
    assert_eq!(stopped[RBX], 0);
    machine.run(&mut (), at(after(1))).unwrap();
    assert_eq!(registers(&mut machine)[RBX], 1);
}

#[test]
fn execution_bounds_and_snapshot_branches_preserve_registers_and_ram() {
    // mov edi, SCRATCH; then repeat: inc ebx; mov [edi], ebx; jmp.
    let code = storing_loop();
    let mut machine = start(code.clone());
    assert_eq!(
        machine.run(&mut (), at(after(8))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine)[RBX], 3);
    let snapshot = machine.snapshot().unwrap();
    let mut fresh = start(code.clone());
    fresh.run(&mut (), at(after(20))).unwrap();
    machine.run(&mut (), at(after(20))).unwrap();
    assert_eq!(registers(&mut machine), registers(&mut fresh));
    // Capture a second time after continuing, then destroy the source process
    // and all its temporary files before restoring the first snapshot twice.
    let _later = machine.snapshot().unwrap();
    drop(machine);
    let mut branch = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock(0)).unwrap();
    assert_eq!(registers(&mut branch)[RBX], 3);
    // Read eight bytes of the scratch counter; its upper half stays zero.
    let memory = branch
        .process
        .as_mut()
        .unwrap()
        .memory(SCRATCH.into(), 8)
        .unwrap();
    assert_eq!(memory, 2u64.to_le_bytes());
    branch.run(&mut (), at(after(20))).unwrap();
    assert_eq!(registers(&mut branch), registers(&mut fresh));
    let mut second = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock(0)).unwrap();
    second.run(&mut (), at(after(21))).unwrap();
    assert_ne!(registers(&mut second), registers(&mut branch));
}

#[test]
fn execution_does_not_advance_shared_clock_and_adopts_elapsed_time() {
    let clock = clock(0);
    let mut machine = TcgMachine::new(
        Path::new(QEMU),
        manifest(counting_loop()),
        SeededRng::new(0), // inc ebx; jmp back.
        clock.clone(),
    )
    .unwrap();
    machine.run(&mut (), at(after(10))).unwrap();
    assert_eq!(clock.now(), 0);
    assert_eq!(registers(&mut machine)[RBX], 5);
    clock.0.store(after(11), Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(after(11))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine)[RBX], 5);
    machine.run(&mut (), at(after(20))).unwrap();
    assert_eq!(registers(&mut machine)[RBX], 10);
}

#[test]
fn shutdown_stops_before_following_instructions_even_at_budget_boundary() {
    let image = Asm::default()
        .bytes(&SHUTDOWN)
        .mov_imm(Reg::Ebx, 42) // Must never execute after shutdown.
        .spin() // Spin if shutdown failed to stop execution.
        .image();
    for limit in [after(1), 100_000] {
        let mut machine = start(image.clone());
        assert_eq!(
            machine.run(&mut (), at(limit)).unwrap(),
            StopReason::Exited {
                reached: at(after(1)),
                reason: ExitReason::Shutdown,
            }
        );
        assert_eq!(registers(&mut machine)[RBX], 0);
    }
}

#[test]
fn runaway_execution_outside_ram_still_stops_at_its_budget() {
    // 16 MiB of RAM: 3 GiB has neither RAM nor a device. A guest that jumps
    // there keeps executing whatever the bus returns; each bounded run must
    // still end at its budget rather than run without limit. QEMU translates
    // such code one instruction at a time (about 3 us each on the host), so
    // budgets stay small.
    let image = Asm::default()
        .mov_imm(Reg::Ecx, 0xC000_0000)
        .jmp_reg(Reg::Ecx)
        .image();
    let mut machine = start(image);
    for end in [after(2), after(3), after(1_000), after(100_000)] {
        assert_eq!(
            machine.run(&mut (), at(end)).unwrap(),
            StopReason::BoundaryReached,
            "end {end}"
        );
    }
    let rip = registers(&mut machine)[super::raw_program_data::RIP];
    assert!(rip >= 0xC000_0000, "{rip:#x}");
}

#[test]
fn idle_with_no_timer_stays_blocked_even_after_time_advances() {
    // Stop the board timers, then halt with interrupts disabled.
    let image = Asm::default().untimed().hlt().image();
    let idle = after(UNTIMED + 1);
    let blocked = StopReason::Blocked {
        reached: at(idle),
        deadline: None,
    };
    let mut machine = start(image);
    assert_eq!(machine.run(&mut (), at(idle + 49)).unwrap(), blocked);
    assert_eq!(machine.run(&mut (), at(idle + 49)).unwrap(), blocked);
    let snapshot = machine.snapshot().unwrap();
    drop(machine);
    let clock = clock(idle);
    let mut machine = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock.clone()).unwrap();
    assert_eq!(machine.run(&mut (), at(idle + 49)).unwrap(), blocked);
    clock.0.store(idle + 29, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(idle + 49)).unwrap(),
        StopReason::Blocked {
            reached: at(idle + 29),
            deadline: None
        }
    );
}

#[test]
fn idle_reports_timer_and_restored_guest_resumes_at_its_deadline() {
    // The count store completes at program instruction 4, so the LAPIC
    // deadline is 1003. HLT wakes through the EOI handler (two
    // instructions); the power-off store is the third after the wakeup.
    let image = Asm::default()
        .lapic_timer(TIMER_COUNT)
        .sti()
        .hlt()
        .bytes(&SHUTDOWN)
        .image();
    let mut machine = start(image);
    let blocked = StopReason::Blocked {
        reached: at(after(6)),
        deadline: Some(at(after(1003))),
    };
    assert_eq!(machine.run(&mut (), at(after(500))).unwrap(), blocked);
    let snapshot = machine.snapshot().unwrap();
    drop(machine);
    let resumed_clock = clock(after(6));
    let mut machine =
        TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, resumed_clock.clone()).unwrap();
    assert_eq!(machine.run(&mut (), at(after(500))).unwrap(), blocked);
    resumed_clock.0.store(after(1004), Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(after(1050))).unwrap(),
        StopReason::Exited {
            reached: at(after(1004) + 3),
            reason: ExitReason::Shutdown,
        }
    );
    // A second restored branch delivers the timer exactly at its deadline.
    let mut machine =
        TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock(after(1003))).unwrap();
    assert_eq!(
        machine.run(&mut (), at(after(1050))).unwrap(),
        StopReason::Exited {
            reached: at(after(1003) + 3),
            reason: ExitReason::Shutdown
        }
    );
    let exited = machine.snapshot().unwrap();
    let mut restored = TcgMachine::from_snapshot(Path::new(QEMU), &exited, clock(0)).unwrap();
    assert!(restored.run(&mut (), at(2000)).is_err());
}

#[test]
fn large_budget_matches_partitioned_execution_and_initial_snapshot() {
    let mut machine = start(counting_loop()); // inc ebx; jmp back.
    let initial = machine.snapshot().unwrap();
    machine.run(&mut (), at(after(1_000_001))).unwrap();
    assert_eq!(registers(&mut machine)[RBX], 500_001);
    let mut partitioned = TcgMachine::from_snapshot(Path::new(QEMU), &initial, clock(0)).unwrap();
    for boundary in [1, 17, 256, 65_537, 1_000_001] {
        assert_eq!(
            partitioned.run(&mut (), at(after(boundary))).unwrap(),
            StopReason::BoundaryReached
        );
    }
    assert_eq!(registers(&mut machine), registers(&mut partitioned));
}

#[test]
fn crash_terminates_process_without_changing_saved_state() {
    let mut machine = start(counting_loop()); // inc ebx; jmp back.
    machine.run(&mut (), at(after(4))).unwrap();
    let snapshot = machine.snapshot().unwrap();
    machine
        .request(seedbox_machine::pv::Request::Crash)
        .unwrap();
    assert!(machine.process.is_none());
    assert!(machine.run(&mut (), at(after(6))).is_err());
    assert!(matches!(
        machine
            .request(seedbox_machine::pv::Request::Crash)
            .unwrap(),
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    let crashed = machine.snapshot().unwrap();
    let mut dead = TcgMachine::from_snapshot(Path::new(QEMU), &crashed, clock(0)).unwrap();
    assert!(dead.run(&mut (), at(after(6))).is_err());
    let mut alive = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock(0)).unwrap();
    alive.run(&mut (), at(after(6))).unwrap();
    assert_eq!(registers(&mut alive)[RBX], 3);
}

#[test]
fn invalid_requests_and_execution_failures_are_distinct() {
    assert!(TcgMachine::new(
        Path::new("/missing/qemu"),
        manifest(Asm::default().spin().image()),
        SeededRng::new(0),
        clock(0)
    )
    .err()
    .unwrap()
    .to_string()
    .contains("/missing/qemu"));
    let mut machine = start(counting_loop()); // inc ebx; jmp back.
    machine.run(&mut (), at(after(2))).unwrap();
    assert!(machine.run(&mut (), at(after(1))).is_err());
    assert_eq!(
        machine.run(&mut (), at(after(2))).unwrap(),
        StopReason::BoundaryReached
    );
    // A dead emulator is a host execution error, never a simulated guest crash.
    machine.process.as_mut().unwrap().terminate().unwrap();
    assert!(machine.run(&mut (), at(after(3))).is_err());
    assert!(machine.snapshot().is_err());
}

#[test]
fn deferred_startup_failure_is_a_run_error_and_invalidates_the_machine() {
    let machine = super::TcgMachine::new_raw(
        Path::new("/missing/qemu"),
        manifest(Asm::default().spin().image()),
    )
    .unwrap();
    crate::machine::test_environment::check_deferred_startup_failure(machine, "/missing/qemu");
}

#[test]
fn slow_bounded_run_outside_ram_ends_with_its_result_not_a_transport_timeout() {
    // 16 MiB of RAM: 3 GiB has neither RAM nor a device. Preserve real
    // outside-RAM execution/result coverage without assuming host throughput.
    // The controlled-peer transport test proves the run wait removes its read
    // deadline before the peer is allowed to return, independent of wall speed.
    let image = Asm::default()
        .mov_imm(Reg::Ecx, 0xC000_0000)
        .jmp_reg(Reg::Ecx)
        .image();
    let mut machine = start(image);
    assert_eq!(
        machine.run(&mut (), at(after(2))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(
        machine.run(&mut (), at(after(300_000))).unwrap(),
        StopReason::BoundaryReached
    );
    let rip = registers(&mut machine)[super::raw_program_data::RIP];
    assert!(rip >= 0xC000_0000, "{rip:#x}");
}

#[test]
fn test_run_watchdog_kills_a_stuck_qemu_and_fails_the_run() {
    let mut machine = start(counting_loop());
    assert_eq!(
        machine.run(&mut (), at(after(2))).unwrap(),
        StopReason::BoundaryReached
    );
    let process = machine.process.as_mut().unwrap();
    process.set_run_watchdog(Duration::from_millis(200));
    let pid = process.pid() as libc::pid_t;
    // A stopped QEMU is alive but never answers: exactly a livelocked run as
    // seen from the adapter, which waits for a run without a read timeout.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    // Wait for the kernel's explicit stop acknowledgement, not a scheduling
    // delay. The watchdog's200ms is a hang guard for this deliberately stuck
    // child, never a test of how quickly the host must complete cleanup.
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) },
        pid
    );
    assert!(libc::WIFSTOPPED(status));
    assert_eq!(libc::WSTOPSIG(status), libc::SIGSTOP);
    let error = machine
        .run(&mut (), at(after(1_000)))
        .unwrap_err()
        .to_string();
    assert!(error.contains("test run watchdog"), "{error}");
    // QEMU was killed and reaped, not left stopped.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert!(machine.snapshot().is_err(), "the failed machine is invalid");
}

// Replace one newc member in the Linux fixture without requiring host cpio.
fn lifecycle_tx_initramfs(input: &[u8]) -> Vec<u8> {
    // A simulated 10ms wait allows Linux's asynchronous TTY worker to drain
    // each short UART marker before the next device is written.
    lifecycle_initramfs(
        input,
        "while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done\nexec 9<>/dev/vport0p1\nprintf console-A >/dev/ttyS0\n/bin/busybox sleep 0.01\nprintf fra >&9\nprintf console-B >/dev/ttyS0\n/bin/busybox sleep 0.01\nprintf 'gment\\nsecond\\n' >&9\nprintf console-C >/dev/ttyS0\n/bin/busybox sleep 0.01\necho 'seedbox: Linux userspace booted'",
    )
}
fn lifecycle_initramfs(input: &[u8], injection: &str) -> Vec<u8> {
    transform_initramfs(input, |init| {
        init.replace("echo 'seedbox: Linux userspace booted'", injection)
    })
}
fn transform_initramfs(input: &[u8], transform: impl Fn(&str) -> String) -> Vec<u8> {
    let mut output = Vec::new();
    let mut at = 0;
    while at + 110 <= input.len() {
        let mut header = input[at..at + 110].to_vec();
        assert_eq!(&header[..6], b"070701");
        let hex = |offset| {
            usize::from_str_radix(
                std::str::from_utf8(&header[offset..offset + 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let size = hex(54);
        let namesize = hex(94);
        let name_end = at + 110 + namesize;
        let data_at = (name_end + 3) & !3;
        let end = (data_at + size + 3) & !3;
        let name = &input[at + 110..name_end - 1];
        let bytes = if name == b"init" || name == b"./init" {
            transform(std::str::from_utf8(&input[data_at..data_at + size]).unwrap()).into_bytes()
        } else {
            input[data_at..data_at + size].to_vec()
        };
        header[54..62].copy_from_slice(format!("{:08x}", bytes.len()).as_bytes());
        output.extend(header);
        output.extend_from_slice(&input[at + 110..data_at]);
        output.extend(bytes);
        while output.len() % 4 != 0 {
            output.push(0);
        }
        at = end;
        if name == b"TRAILER!!!" {
            break;
        }
    }
    output
}
#[test]
#[ignore = "requires TCG Linux fixture"]
fn linux_lifecycle_tx_retains_fragments_and_snapshot_drain_position() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    let manifest = Manifest {
        memory_mib: 256,
        disk: None,
        network: None,
        boot: Boot::Linux {
            kernel: fs::read(assets.join("kernel")).unwrap().into(),
            initramfs: lifecycle_tx_initramfs(&fs::read(assets.join("initramfs")).unwrap()).into(),
            firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap())
                .unwrap()
                .into(),
            command_line:
                "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr seedbox_sleep=100"
                    .into(),
            epoch_seconds: 0,
        },
    };
    let mut source = Process::start(&qemu, &manifest, None, 3, None).unwrap();
    let mut now = at(0);
    // Three authored 10ms sleeps separate the console/lifecycle writes.
    let mut progress = LinuxProgress::new(now, 3 * 10_000_000);
    loop {
        let upto = progress.next(now, 1_000_000).unwrap();
        let result = source.execute(now, upto, Default::default()).unwrap();
        now = timestamp(&result, "time").unwrap();
        if String::from_utf8_lossy(&source.console().unwrap())
            .contains("seedbox: Linux userspace booted")
        {
            break;
        }
        if result["reason"] == "blocked" {
            now = progress.timer(timestamp(&result, "deadline").unwrap());
            source.advance(now).unwrap();
        }
    }
    use seedbox_machine::pv::transport::GuestWrite;
    let pending = source.snapshot().unwrap();
    let writes = source.drain_guest().unwrap();
    assert!(writes.windows(2).all(|w| w[0].time() <= w[1].time()));
    let lifecycle: Vec<_> = writes
        .iter()
        .enumerate()
        .filter_map(|(i, w)| match w {
            GuestWrite::Lifecycle(write) => Some((i, write)),
            GuestWrite::Console(_) | GuestWrite::Frame(_) => None,
        })
        .collect();
    assert_eq!(
        lifecycle
            .iter()
            .flat_map(|(_, w)| w.bytes.iter().copied())
            .collect::<Vec<_>>(),
        b"fragment\nsecond\n"
    );
    let console = |range: std::ops::Range<usize>| -> String {
        String::from_utf8_lossy(
            &writes[range]
                .iter()
                .flat_map(|w| match w {
                    GuestWrite::Console(w) => w.bytes.clone(),
                    GuestWrite::Lifecycle(_) | GuestWrite::Frame(_) => vec![],
                })
                .collect::<Vec<_>>(),
        )
        .into_owned()
    };
    assert_eq!(
        console(0..writes.len()).as_bytes(),
        source.console().unwrap()
    );
    let first = lifecycle.first().unwrap().0;
    let last = lifecycle.last().unwrap().0;
    assert!(console(0..first).contains("console-A"));
    assert!(console(first..last).contains("console-B"));
    assert!(console(last..writes.len()).contains("console-C"));
    let mut restored = Process::start(&qemu, &manifest, Some(&pending), 3, None).unwrap();
    assert_eq!(restored.drain_guest().unwrap(), writes);
    assert!(restored.drain_guest().unwrap().is_empty());
    let drained = restored.snapshot().unwrap();
    let mut restored = Process::start(&qemu, &manifest, Some(&drained), 3, None).unwrap();
    assert!(restored.drain_guest().unwrap().is_empty());
}

#[test]
#[ignore = "requires TCG Linux fixture"]
fn lifecycle_rx_backpressure_and_pending_suffix_survive_qemu_migration() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    let manifest = Manifest {
        memory_mib: 256,
        disk: None,
        network: None,
        boot: Boot::Linux {
            kernel: fs::read(assets.join("kernel")).unwrap().into(),
            initramfs: lifecycle_initramfs(
                &fs::read(assets.join("initramfs")).unwrap(),
                "while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done\nexec 9<>/dev/vport0p1\necho RX_OPEN\n/bin/busybox sleep 0.1\n/bin/busybox dd bs=700013 count=1 iflag=fullblock <&9 >&9 2>/dev/null\necho RX_DONE\n/bin/busybox sleep 100",
            ).into(),
            firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap()).unwrap().into(),
            command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr seedbox_sleep=100"
                .into(),
            epoch_seconds: 0,
        },
    };
    let bytes: Vec<_> = (0..700013).map(|i| (i % 251) as u8).collect();
    let mut source = Process::start(&qemu, &manifest, None, 3, None).unwrap();
    source.input(&bytes[..25000]).unwrap();
    source.input(&bytes[25000..]).unwrap();
    let mut now = at(0);
    let mut progress = LinuxProgress::new(now, 0);
    loop {
        let upto = progress.next(now, 1_000_000).unwrap();
        let result = source.execute(now, upto, Default::default()).unwrap();
        now = timestamp(&result, "time").unwrap();
        if String::from_utf8_lossy(&source.console().unwrap()).contains("RX_OPEN") {
            break;
        }
        if result["reason"] == "blocked" {
            now = progress.timer(timestamp(&result, "deadline").unwrap());
            source.advance(now).unwrap();
        }
    }
    let pending = source.snapshot().unwrap();
    let mut restored = Process::start(&qemu, &manifest, Some(&pending), 3, None).unwrap();
    assert!(restored.drain_lifecycle().unwrap().is_empty());
    let mut observed = Vec::new();
    // The restored receiver waits 100ms, then drains the fixed 700013-byte
    // payload. Give that finite kernel copy the same 2s kernel-work allowance.
    let mut progress = LinuxProgress::new(now, 100_000_000);
    loop {
        let upto = progress.next(now, 1_000_000).unwrap();
        let result = restored.execute(now, upto, Default::default()).unwrap();
        now = timestamp(&result, "time").unwrap();
        for write in restored.drain_lifecycle().unwrap() {
            observed.extend(write.bytes);
        }
        if String::from_utf8_lossy(&restored.console().unwrap()).contains("RX_DONE") {
            break;
        }
        if result["reason"] == "blocked" {
            now = progress.timer(timestamp(&result, "deadline").unwrap());
            restored.advance(now).unwrap();
        }
    }
    assert_eq!(observed, bytes);
    assert!(restored.drain_lifecycle().unwrap().is_empty());
}

#[test]
#[ignore = "requires patched QEMU and TCG Linux inputs"]
fn application_outcome_remains_distinct_from_later_shutdown_across_snapshot() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    fn advance(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<seedbox_machine::pv::Output>,
        progress: &mut LinuxProgress,
    ) -> StopReason {
        if let State::Blocked {
            deadline: Some(deadline),
        } = machine.state
        {
            clock
                .0
                .store(progress.timer(deadline).as_nanos(), Ordering::Relaxed);
        }
        let end = progress
            .next(at(machine.reached.as_nanos().max(clock.now())), 100_000)
            .unwrap()
            .as_nanos();
        machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock,
                    rng,
                    inbox: seedbox_machine::pv::Inbox::new(
                        &mut Default::default(),
                        at(clock.now()),
                    ),
                    outbox: seedbox_machine::pv::Outbox::new(outputs, at(clock.now()), at(end)),
                },
                at(end),
            )
            .unwrap()
    }
    fn finish(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<seedbox_machine::pv::Output>,
    ) -> StopReason {
        // PID1 delays poweroff by 50ms after reporting the application exit.
        let mut progress =
            LinuxProgress::new(at(machine.reached.as_nanos().max(clock.now())), 50_000_000);
        loop {
            let stop = advance(machine, clock, rng, outputs, &mut progress);
            if matches!(stop, StopReason::Exited { .. }) {
                return stop;
            }
        }
    }
    fn run_at(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<seedbox_machine::pv::Output>,
    ) -> StopReason {
        clock.0.store(
            clock.now().max(machine.reached.as_nanos()),
            Ordering::Relaxed,
        );
        let now = at(clock.now());
        machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock,
                    rng,
                    inbox: seedbox_machine::pv::Inbox::new(&mut Default::default(), now),
                    outbox: seedbox_machine::pv::Outbox::new(outputs, now, now),
                },
                now,
            )
            .unwrap()
    }
    for code in [0, 23] {
        // This child defines the fixture's lifetime. Its actual wait status is
        // reported first; PID1 deliberately keeps Linux alive before poweroff.
        let script = format!(
            r#"#!/bin/sh
set -eu
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
exec </dev/console >/dev/console 2>&1
/bin/busybox modprobe virtio_mmio
/bin/busybox modprobe virtio_console
while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done
exec 9<>/dev/vport0p1
printf 'SBX_READY1\n' >&9
set +e
/bin/busybox sh -c 'exit {code}'
status=$?
set -e
printf 'SBX_EXIT1 status=%s\n' "$status" >&9
/bin/busybox sleep 0.05
/bin/busybox poweroff -f
"#
        );
        let manifest = Manifest {
            memory_mib: 256,
            disk: None,
            network: None,
            boot: Boot::Linux {
                kernel: fs::read(assets.join("kernel")).unwrap().into(),
                initramfs: transform_initramfs(
                    &fs::read(assets.join("initramfs")).unwrap(),
                    |_| script.clone(),
                )
                .into(),
                firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap())
                    .unwrap()
                    .into(),
                command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr"
                    .into(),
                epoch_seconds: 0,
            },
        };
        let clock = clock(0);
        let mut rng = SeededRng::new(7);
        let mut outputs: Vec<seedbox_machine::pv::Output> = Vec::new();
        let mut source = super::TcgMachine::new_raw(&qemu, manifest).unwrap();
        let mut progress = LinuxProgress::new(at(0), 0);
        while !outputs.iter().any(|output| {
            matches!(
                output.payload,
                seedbox_machine::pv::Response::ApplicationOutcome { .. }
            )
        }) {
            assert!(!matches!(
                advance(&mut source, &clock, &mut rng, &mut outputs, &mut progress),
                StopReason::Exited { .. }
            ));
        }
        assert!(outputs.iter().any(|output| output.payload
            == seedbox_machine::pv::Response::ApplicationOutcome { code: code as u8 }));
        let pending_rng = rng.clone();
        assert!(
            !matches!(source.state, State::Exited(_)),
            "application outcome must leave Linux running"
        );
        let pending = source.snapshot().unwrap();
        let expected = finish(&mut source, &clock, &mut rng, &mut outputs);
        assert!(matches!(
            expected,
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                ..
            }
        ));
        for _ in 0..2 {
            let branch_clock = super::tests::clock(pending.reached.as_nanos());
            let mut branch = super::TcgMachine::from_snapshot(&qemu, &pending).unwrap();
            let mut branch_rng = pending_rng.clone();
            let mut branch_outputs = Vec::new();
            assert_eq!(
                finish(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                expected
            );
            assert_eq!(
                run_at(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                expected
            );
            let adopted = branch.reached.as_nanos() + 17;
            branch_clock.0.store(adopted, Ordering::Relaxed);
            assert_eq!(
                run_at(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                StopReason::Exited {
                    reached: at(adopted),
                    reason: ExitReason::Shutdown
                }
            );
            let exited = branch.snapshot().unwrap();
            let mut restored = super::TcgMachine::from_snapshot(&qemu, &exited).unwrap();
            assert_eq!(
                run_at(
                    &mut restored,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                StopReason::Exited {
                    reached: at(adopted),
                    reason: ExitReason::Shutdown
                }
            );
        }
        let crash_clock = super::tests::clock(pending.reached.as_nanos());
        let mut crashed = super::TcgMachine::from_snapshot(&qemu, &pending).unwrap();
        assert!(matches!(
            crate::machine::test_environment::run_requests(
                &mut crashed,
                crash_clock.as_ref(),
                &mut pending_rng.clone(),
                [seedbox_machine::pv::Request::Crash],
                at(crash_clock.now())
            )
            .unwrap(),
            StopReason::Exited {
                reason: ExitReason::Crashed,
                ..
            }
        ));
    }
}

// Constructor checks remain portable: process creation is deferred until run.
/// The smallest kernel and firmware the x86 boot-input checks accept.
fn bzimage() -> Vec<u8> {
    seedbox_machine::test_support::assets::bzimage_header()
}
fn bios_image() -> Vec<u8> {
    vec![3; 64 * 1024]
}
fn common_inputs() -> (
    seedbox_machine::application::GuestBootstrap,
    seedbox_machine::definition::MachineDefinition,
) {
    use seedbox_machine::application::{GuestBootstrap, Kernel};
    use seedbox_machine::definition::*;
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &[0_u8][..])
        .unwrap();
    (
        GuestBootstrap {
            kernel: Kernel {
                image: bzimage(),
                command_line: "custom=verbatim".into(),
            },
            init: vec![2],
            base_initramfs: Vec::new(),
            epoch_seconds: 0,
        },
        MachineDefinition {
            name: "portable".into(),
            resources: Resources {
                memory_bytes: 128 << 20,
            },
            application: PreparedApplication {
                filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
                launch: Launch {
                    argv: vec!["/application".into()],
                    environment: Default::default(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
            },
            disk: None,
            network: None,
        },
    )
}

#[test]
fn common_constructor_preserves_inputs_and_reuses_prepared_application() {
    let (bootstrap, definition) = common_inputs();
    let original = definition.application.clone();
    let first = super::TcgMachine::new(
        Path::new("/missing/qemu"),
        bios_image(),
        &bootstrap,
        definition.clone(),
    )
    .unwrap();
    let second = super::TcgMachine::new(
        Path::new("/missing/qemu"),
        bios_image(),
        &bootstrap,
        definition.clone(),
    )
    .unwrap();
    assert_eq!(definition.application, original);
    assert_eq!(first.manifest.memory_mib, 128);
    match (&first.manifest.boot, &second.manifest.boot) {
        (
            Boot::Linux {
                kernel,
                initramfs,
                command_line,
                ..
            },
            Boot::Linux {
                initramfs: again, ..
            },
        ) => {
            assert_eq!(kernel.as_ref(), bzimage());
            assert_eq!(command_line, "custom=verbatim");
            assert_eq!(initramfs, again);
        }
        _ => unreachable!(),
    }
}

#[test]
fn common_constructor_rejects_initramfs_larger_than_guest_memory() {
    let (mut bootstrap, mut definition) = common_inputs();
    definition.resources.memory_bytes = 1 << 20;
    // The packed archive includes the helper as well as the prepared filesystem.
    bootstrap.init = vec![2; 1 << 20];
    let packed =
        seedbox_machine::application::boot_archive(&definition.application, &bootstrap).unwrap();
    assert!(packed.len() > definition.resources.memory_bytes as usize);
    let error = super::TcgMachine::new(
        Path::new("/missing/qemu"),
        bios_image(),
        &bootstrap,
        definition,
    )
    .err()
    .expect("oversized initramfs must fail before starting QEMU");
    assert_eq!(
        error.to_string(),
        format!(
            "TCG initramfs ({} bytes) must fit in guest RAM (1048576 bytes)",
            packed.len()
        )
    );
}

#[test]
fn tcg_initramfs_memory_bound_is_inclusive() {
    for length in [(1 << 20) - 1, 1 << 20, (1 << 20) + 1] {
        let result = super::TcgMachine::new_raw(
            Path::new("/missing/qemu"),
            Manifest {
                memory_mib: 1,
                disk: None,
                network: None,
                boot: Boot::Linux {
                    kernel: bzimage().into(),
                    initramfs: vec![0; length].into(),
                    firmware: bios_image().into(),
                    command_line: String::new(),
                    epoch_seconds: 0,
                },
            },
        );
        assert_eq!(result.is_ok(), length <= 1 << 20);
    }
}

#[test]
fn tcg_snapshots_and_restores_share_owned_boot_blobs() {
    fn assert_shared(left: &Manifest, right: &Manifest) {
        let (
            Boot::Linux {
                kernel: lk,
                initramfs: li,
                firmware: lf,
                ..
            },
            Boot::Linux {
                kernel: rk,
                initramfs: ri,
                firmware: rf,
                ..
            },
        ) = (&left.boot, &right.boot)
        else {
            panic!("expected Linux boot inputs");
        };
        assert!(Arc::ptr_eq(lk, rk));
        assert!(Arc::ptr_eq(li, ri));
        assert!(Arc::ptr_eq(lf, rf));
    }
    let (bootstrap, definition) = common_inputs();
    let qemu = Path::new("/missing/qemu");
    let mut machine = super::TcgMachine::new(qemu, bios_image(), &bootstrap, definition).unwrap();
    let snapshot = machine.snapshot().unwrap();
    let copy = snapshot.clone();
    assert_shared(&machine.manifest, &snapshot.manifest);
    assert_shared(&snapshot.manifest, &copy.manifest);
    drop(machine);
    drop(bootstrap);
    drop(snapshot);
    // The retained snapshot owns the bytes, not a source file or live machine.
    let restored = super::TcgMachine::from_snapshot(qemu, &copy).unwrap();
    assert_shared(&copy.manifest, &restored.manifest);
    match &restored.manifest.boot {
        Boot::Linux {
            kernel,
            initramfs,
            firmware,
            ..
        } => {
            assert_eq!(kernel.as_ref(), bzimage());
            assert!(!initramfs.is_empty());
            assert_eq!(firmware.as_ref(), bios_image());
        }
        _ => unreachable!(),
    }
}

#[test]
fn common_constructor_checks_resource_and_kernel_argument_bounds() {
    let (mut bootstrap, definition) = common_inputs();
    for memory_bytes in [0, 1, (u64::from(u32::MAX) + 1) << 20] {
        let mut invalid = definition.clone();
        invalid.resources.memory_bytes = memory_bytes;
        assert!(super::TcgMachine::new(
            Path::new("/missing/qemu"),
            bios_image(),
            &bootstrap,
            invalid
        )
        .is_err());
    }
    for accepted in ["a".repeat(2047), format!("{}a", "é".repeat(1023))] {
        bootstrap.kernel.command_line = accepted;
        assert!(super::TcgMachine::new(
            Path::new("/missing/qemu"),
            bios_image(),
            &bootstrap,
            definition.clone()
        )
        .is_ok());
    }
    for rejected in ["a".repeat(2048), "é".repeat(1024), "a\0b".into()] {
        bootstrap.kernel.command_line = rejected;
        assert!(super::TcgMachine::new(
            Path::new("/missing/qemu"),
            bios_image(),
            &bootstrap,
            definition.clone()
        )
        .is_err());
    }
}
