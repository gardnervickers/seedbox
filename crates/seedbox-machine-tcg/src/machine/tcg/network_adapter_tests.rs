//! Native checks of the TCG NIC adapter at the Machine boundary, with the
//! shared `network-probe` server on real Linux: a zero-budget receive wakes a
//! blocked machine without executing or emitting anything until a positive
//! budget; valid frames for a machine without a NIC, or for an ended boot, are
//! dropped without error; frames admitted ahead of a Crash or Reboot in
//! the same inbox never reach the next boot; and a frame the NIC drops before
//! its driver is ready leaves a blocked machine exactly as it was.
use super::{Factory, TcgMachine};
use crate::{
    clock::{Clock, SimulationClock},
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Input, Outbox, Output, Request, Response},
    rng::SeededRng,
};
use seedbox_machine::test_support::network_scenario::{definition, ipv4, mac, udp_frame, PORT};
use std::{fs, path::PathBuf};

const QUANTUM: u64 = 10_000_000;
/// Simulated bound for the server to boot and report ready.
const BOOT_LIMIT: u64 = 60_000_000_000;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}

/// The probe server at 10.0.0.1, optionally without its NIC.
fn server(network: bool) -> TcgMachine {
    let probe = fs::read(input("SEEDBOX_TCG_NETWORK_APPLICATION")).unwrap();
    let mut definition = definition("server", &probe, &["server", PORT], 0, 128 << 20);
    if !network {
        definition.network = None;
    }
    Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        crate::machine::TEST_EPOCH_SECONDS,
    )
    .unwrap()
    .create(&definition)
    .unwrap()
}

/// A UDP datagram to the server's echo port from the client's addresses.
fn datagram() -> Vec<u8> {
    udp_frame(
        (mac(1), ipv4(1), 40000),
        (mac(0), ipv4(0), PORT.parse().unwrap()),
        b"adapter",
    )
}

struct Guest {
    machine: TcgMachine,
    clock: SimulationClock,
    rng: SeededRng,
    output: String,
}

impl Guest {
    fn new(machine: TcgMachine) -> Self {
        Self {
            machine,
            clock: SimulationClock::new(0),
            rng: SeededRng::new(5),
            output: String::new(),
        }
    }
    /// One run to `upto` with `requests` due now; returns the stop and every
    /// output except the boot console.
    fn run(&mut self, requests: Vec<Request>, upto: u64) -> (StopReason, Vec<Output>) {
        let now = at(self.clock.now());
        let mut inbox = requests
            .into_iter()
            .map(|payload| Input { at: now, payload })
            .collect();
        let mut staged = Vec::new();
        let stop = self
            .machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &self.clock,
                    rng: &mut self.rng,
                    execution: Default::default(),
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut staged, now, at(upto)),
                },
                at(upto),
            )
            .unwrap();
        assert!(inbox.is_empty(), "every due request is accepted");
        let staged: Vec<Output> = staged
            .into_iter()
            .filter(|output| !matches!(output.payload, Response::BootConsole { .. }))
            .collect();
        for output in &staged {
            if let Response::ApplicationOutput { bytes, .. } = &output.payload {
                self.output.push_str(&String::from_utf8_lossy(bytes));
            }
        }
        (stop, staged)
    }
    /// Run until the server reports ready, then until it is blocked idle;
    /// returns how many frames it transmitted on the way.
    fn until_ready_and_blocked(&mut self) -> usize {
        let limit = self.clock.now() + BOOT_LIMIT;
        let mut frames = 0;
        loop {
            let now = self.clock.now();
            assert!(now < limit, "no ready, blocked server: {:?}", self.output);
            let (stop, outputs) = self.run(Vec::new(), now + QUANTUM);
            frames += transmitted(&outputs);
            match stop {
                StopReason::BoundaryReached => self.clock.set(now + QUANTUM),
                StopReason::Blocked { reached, deadline } => {
                    if self.output.contains("NETPROBE ready") {
                        self.clock.set(reached.as_nanos());
                        return frames;
                    }
                    let deadline = deadline.expect("blocked forever before ready");
                    self.clock.set(deadline.as_nanos());
                }
                stop => panic!("{stop:?}: {:?}", self.output),
            }
        }
    }
}

fn transmitted(outputs: &[Output]) -> usize {
    outputs
        .iter()
        .filter(|output| matches!(output.payload, Response::TransmitFrame { .. }))
        .count()
}

#[test]
#[ignore = "requires patched QEMU with the NIC device, TCG assets and network fixtures"]
fn tcg_zero_budget_receive_wakes_without_executing_until_a_positive_budget() {
    let mut guest = Guest::new(server(true));
    guest.until_ready_and_blocked();
    let now = guest.clock.now();
    let registers = guest.machine.process().unwrap().registers().unwrap();
    // Without a frame, a zero-budget run leaves the idle server blocked.
    let (stop, outputs) = guest.run(Vec::new(), now);
    assert!(matches!(stop, StopReason::Blocked { .. }), "{stop:?}");
    assert!(outputs.is_empty());
    // The frame's interrupt makes it runnable, but nothing executes yet.
    let frame = Request::ReceiveFrame { bytes: datagram() };
    let (stop, outputs) = guest.run(vec![frame], now);
    assert_eq!(stop, StopReason::BoundaryReached);
    assert!(outputs.is_empty(), "{outputs:?}");
    assert_eq!(
        guest.machine.process().unwrap().registers().unwrap(),
        registers
    );
    // Repeated zero-budget calls still execute nothing.
    let (stop, outputs) = guest.run(Vec::new(), now);
    assert_eq!(stop, StopReason::BoundaryReached);
    assert!(outputs.is_empty());
    assert_eq!(
        guest.machine.process().unwrap().registers().unwrap(),
        registers
    );
    // A positive budget lets the server answer: it resolves the client
    // (ARP) or echoes, transmitting after `now`.
    let (_, outputs) = guest.run(Vec::new(), now + QUANTUM);
    assert!(transmitted(&outputs) > 0, "{outputs:?}");
    assert!(outputs.iter().all(|output| output.at > at(now)));
}

#[test]
#[ignore = "requires patched QEMU with the NIC device, TCG assets and network fixtures"]
fn tcg_valid_frames_without_a_nic_or_after_the_boot_ends_are_dropped() {
    // Without a NIC the frame is taken and dropped before guest execution.
    let mut guest = Guest::new(server(false));
    let (stop, outputs) = guest.run(vec![Request::ReceiveFrame { bytes: datagram() }], 0);
    assert_eq!(stop, StopReason::BoundaryReached);
    assert!(outputs.is_empty());
    // With a NIC, a crashed boot takes and drops frames that follow.
    let mut guest = Guest::new(server(true));
    guest.until_ready_and_blocked();
    let now = guest.clock.now();
    let (stop, outputs) = guest.run(
        vec![Request::Crash, Request::ReceiveFrame { bytes: datagram() }],
        now,
    );
    let crashed = StopReason::Exited {
        reached: at(now),
        reason: ExitReason::Crashed,
    };
    assert_eq!(stop, crashed);
    assert!(outputs.is_empty());
    // Later frames for the ended boot are dropped as well.
    let (stop, outputs) = guest.run(vec![Request::ReceiveFrame { bytes: datagram() }], now);
    assert_eq!(stop, crashed);
    assert!(outputs.is_empty());
}

#[test]
#[ignore = "requires patched QEMU with the NIC device, TCG assets and network fixtures"]
fn tcg_frames_before_crash_or_reboot_never_reach_the_next_boot() {
    for end in [Request::Crash, Request::Reboot] {
        let mut guest = Guest::new(server(true));
        guest.until_ready_and_blocked();
        let now = guest.clock.now();
        // A frame for the live boot, then the boot's end and a fresh boot
        // in the same inbox. The ended boot never runs again, and the new
        // boot's NIC is not ready, so no frame reaches either guest.
        let crash = matches!(end, Request::Crash);
        let mut requests = vec![Request::ReceiveFrame { bytes: datagram() }, end];
        if crash {
            requests.push(Request::Reboot);
        }
        requests.push(Request::ReceiveFrame { bytes: datagram() });
        let (stop, outputs) = guest.run(requests, now);
        assert_eq!(stop, StopReason::BoundaryReached);
        assert!(outputs.is_empty());
        // The new boot starts over and, across its whole boot to ready and
        // one more quantum, transmits nothing in answer to those frames.
        guest.output.clear();
        let mut frames = guest.until_ready_and_blocked();
        assert_eq!(guest.output, "NETPROBE ready port=7000\n");
        let now = guest.clock.now();
        let (stop, outputs) = guest.run(Vec::new(), now + QUANTUM);
        assert!(matches!(stop, StopReason::Blocked { .. }), "{stop:?}");
        frames += transmitted(&outputs);
        assert_eq!(frames, 0, "the new boot transmitted");
    }
}

#[test]
#[ignore = "requires patched QEMU with the NIC device, TCG assets and network fixtures"]
fn tcg_frame_dropped_before_driver_ready_leaves_a_blocked_machine_exactly_blocked() {
    use super::raw_program_data::{Asm, STUB};
    use super::{Boot, Manifest};
    // A raw program with an attached NIC it never drives: it arms the LAPIC
    // timer (deadline at program instruction 1003) and waits with interrupts
    // enabled (see elapsed_tests.rs).
    let image = Asm::default().lapic_timer(998).sti().hlt().image();
    let after = |n: u64| STUB + n;
    let machine = TcgMachine::new_raw(
        &input("SEEDBOX_TCG_QEMU"),
        Manifest {
            boot: Boot::Program(image.into()),
            memory_mib: 16,
            disk: None,
            network: Some(mac(0)),
        },
    )
    .unwrap();
    let mut guest = Guest::new(machine);
    let blocked = StopReason::Blocked {
        reached: at(after(6)),
        deadline: Some(at(after(1003))),
    };
    let (stop, outputs) = guest.run(Vec::new(), after(500));
    assert_eq!(stop, blocked);
    assert!(outputs.is_empty());
    guest.clock.set(after(6));
    let registers = guest.machine.process().unwrap().registers().unwrap();
    // The frame reaches QEMU's NIC, which drops it (no driver); the CPU
    // stays halted, so the machine reports the identical blocked state.
    let (stop, outputs) = guest.run(vec![Request::ReceiveFrame { bytes: datagram() }], after(6));
    assert_eq!(stop, blocked);
    assert!(outputs.is_empty());
    assert_eq!(
        guest.machine.process().unwrap().registers().unwrap(),
        registers
    );
}
