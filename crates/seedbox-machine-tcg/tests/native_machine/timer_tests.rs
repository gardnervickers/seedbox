//! The TCG kernel takes its LAPIC timer rate from `lapic_timer_khz` on the
//! command line instead of calibrating it at boot. Time the timer inside the
//! guest against the TSC, which counts guest nanoseconds: a rate stated too
//! low fires the timer early, and one stated too high fires it late.
use seedbox_machine::{
    clock::Clock,
    pv::{Inbox, Outbox, Response, Stream},
    test_support::{
        kernel_terminal_tests::{definition, BOOT_LIMIT, HOST_LIMIT, QUANTUM},
        Machine, RunContext, SeededRng, SimulationClock, StopReason, Timestamp,
    },
};
use seedbox_test_time::HangGuard;
use std::{collections::VecDeque, fs, path::PathBuf};

fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

#[test]
#[ignore = "requires native TCG QEMU, rebuilt assets and clock-faults fixture"]
fn tcg_guest_lapic_timer_ticks_at_hz() {
    let binary = fs::read(input("SEEDBOX_TCG_CLOCK_APPLICATION")).unwrap();
    let mut factory = super::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        super::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let mut machine = factory
        .create(&definition(&binary, "ticks", "ticks", 256 << 20))
        .unwrap();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(1);
    let guard = HangGuard::new("native TCG tick fixture", HOST_LIMIT);
    let mut stdout = Vec::new();
    let code = 'run: loop {
        guard
            .remaining()
            .expect("TCG tick fixture infrastructure hang");
        assert!(clock.now() < BOOT_LIMIT, "fixture did not finish");
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + QUANTUM);
        let (mut inbox, mut outputs) = (VecDeque::new(), Vec::new());
        let stop = machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    execution: Default::default(),
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut outputs, now, upto),
                },
                upto,
            )
            .unwrap();
        for output in outputs {
            match output.payload {
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes,
                } => stdout.extend(bytes),
                Response::ApplicationOutcome { code } => break 'run code,
                _ => {}
            }
        }
        match stop {
            StopReason::BoundaryReached => clock.set(upto.as_nanos()),
            StopReason::Blocked { deadline, .. } => clock.set(
                deadline
                    .expect("fixture blocked without a timer")
                    .as_nanos(),
            ),
            StopReason::Exited { reason, .. } => panic!("guest exited: {reason:?}"),
        }
    };
    let stdout = String::from_utf8_lossy(&stdout);
    assert_eq!(code, 0, "{stdout}");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("TICKS "))
        .unwrap_or_else(|| panic!("no TICKS line: {stdout}"));
    let field = |name: &str| {
        line.split(' ')
            .find_map(|field| field.strip_prefix(name))
            .unwrap_or_else(|| panic!("{name} in {line}"))
    };
    let number = |name: &str| -> i64 {
        field(name)
            .parse()
            .unwrap_or_else(|_| panic!("{name} in {line}"))
    };
    // 1000 ticks after its initcalls, before the busy second ends, the kernel
    // swaps tsc-early for tsc and starts high-resolution timers. A wrong rate
    // can delay that, or make its watchdog replace the TSC with jiffies.
    assert_eq!(field("source="), "tsc", "{line}");
    // HZ is 1000: a busy CPU takes one tick per guest millisecond, and more
    // interrupts if the timer fires early.
    let ticks = number("loc=");
    assert!(
        (990..=1010).contains(&ticks),
        "{ticks} local timer interrupts in one busy guest second"
    );
    // A correct rate wakes the fixture about 10 µs after expiry. One stated
    // 0.1% too high fires the 100 ms timer about 100 µs late.
    let late = number("late=");
    assert!(
        (0..50_000).contains(&late),
        "100 ms timer woke {late} ns late"
    );
}
