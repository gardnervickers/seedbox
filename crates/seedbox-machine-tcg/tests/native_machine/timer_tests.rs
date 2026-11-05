//! The TCG kernel takes its LAPIC timer rate from `lapic_timer_khz` on the
//! command line instead of calibrating it at boot. A wrong rate would fire the
//! tick early or late, so count the tick inside the guest.
use seedbox_machine::{
    clock::Clock,
    pv::{Inbox, Outbox, Response, Stream},
    test_support::{
        kernel_terminal_tests::definition, Machine, RunContext, SeededRng, SimulationClock,
        StopReason, Timestamp,
    },
};
use seedbox_test_time::HangGuard;
use std::{collections::VecDeque, fs, path::PathBuf, time::Duration};

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
    let guard = HangGuard::new("native TCG tick fixture", Duration::from_secs(600));
    let mut stdout = Vec::new();
    let code = 'run: loop {
        guard
            .remaining()
            .expect("TCG tick fixture infrastructure hang");
        assert!(
            clock.now() < 60_000_000_000,
            "fixture did not finish within 60 guest seconds"
        );
        let now = Timestamp::from_nanos(clock.now());
        let upto = Timestamp::from_nanos(clock.now() + 10_000_000);
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
    let field = |name: &str| -> u64 {
        line.split(' ')
            .find_map(|field| field.strip_prefix(name))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{name} in {line}"))
    };
    let (ticks, elapsed) = (field("loc="), field("mono="));
    // HZ is 1000: a busy CPU takes one tick per guest millisecond.
    let expected = elapsed / 1_000_000;
    assert!(
        ticks.abs_diff(expected) <= expected / 100,
        "{ticks} local timer interrupts in {elapsed} guest ns"
    );
}
