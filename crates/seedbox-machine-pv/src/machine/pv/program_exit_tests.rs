//! Actual application outcomes leave the kernel and retained launch recipe alive.
use super::*;
use crate::{clock::SimulationClock, rng::SeededRng};
use seedbox_machine::pv::{
    Inbox, Outbox, Output, Request as GuestRequest, Response as GuestResponse,
};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{collections::VecDeque, time::Duration};
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
fn application(code: u32) -> Vec<u8> {
    assert!(code < 65536);
    // exit(code)
    #[cfg(target_arch = "aarch64")]
    let (machine, code): (u16, Vec<u8>) = (
        183,
        [0xd280_0000u32 | (code << 5), 0xd280_0ba8, 0xd400_0001]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect(),
    );
    // MOV EDI,code; MOV EAX,60; SYSCALL.
    #[cfg(target_arch = "x86_64")]
    let (machine, code): (u16, Vec<u8>) = (62, {
        let mut bytes = vec![0xbf];
        bytes.extend(code.to_le_bytes());
        bytes.extend([0xb8, 60, 0, 0, 0, 0x0f, 0x05]);
        bytes
    });
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&machine.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&0x400078u64.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes());
    elf[68..72].copy_from_slice(&5u32.to_le_bytes());
    elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
    elf[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
    let length = (elf.len() + code.len()) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    elf.extend_from_slice(&code);
    elf
}
fn run(
    machine: &mut PVMachine,
    now: u64,
    upto: u64,
    requests: &[GuestRequest],
) -> (StopReason, Vec<Output>) {
    let mut inbox = requests
        .iter()
        .cloned()
        .map(|payload| seedbox_machine::pv::Input {
            at: at(now),
            payload,
        })
        .collect::<VecDeque<_>>();
    let clock = SimulationClock::new(now);
    let mut rng = SeededRng::new(7);
    let mut output = Vec::new();
    let stopped = machine
        .run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: Inbox::new(&mut inbox, at(now)),
                outbox: Outbox::new(&mut output, at(now), at(upto)),
            },
            at(upto),
        )
        .unwrap();
    (stopped, output)
}
fn phase(name: &str, start: u64, deadline: u64) -> GuestProgress {
    // Every poll advances its positive quantum: no same-instant retries.
    GuestProgress::new(
        name,
        Duration::from_nanos(start),
        Duration::from_nanos(deadline),
        0,
    )
    .unwrap()
}
fn next(progress: &mut GuestProgress, now: u64, last: &str) -> u64 {
    progress.next(Duration::from_nanos(now), Duration::from_millis(5))
        .unwrap_or_else(|error| panic!("{}: {error}; now={now}; last stop/output={last}; pending=supervisor command or outcome", progress.phase()))
        .as_nanos().try_into().unwrap()
}
#[test]
#[ignore = "requires a signed PV worker and native kernel/init assets"]
fn native_linux_program_exit_zero_and_nonzero_survive_snapshot_and_zero_runs() {
    let worker = input("SEEDBOX_PV_WORKER");
    let bootstrap = seedbox_machine::test_support::prepared_fixture::bootstrap("PV");
    for code in [0, 23] {
        let definition = seedbox_machine::test_support::prepared_application::from_program(
            &application(code as u32),
            128 << 20,
        );
        let mut machine = PVMachine::new(&worker, &bootstrap, definition).unwrap();
        let guard = HangGuard::new(
            "native exit/start/shutdown worker transport",
            Duration::from_secs(300),
        );
        // The generated application is three instructions and has no sleeps.
        // Allow five simulated seconds for Linux boot and supervisor startup;
        // restart and shutdown each get one second for control/exit/reap timers.
        let mut progress = phase("boot and immediate application exit", 0, 5_000_000_000);
        let mut now = 0;
        let mut last = String::from("boot not yet run");
        loop {
            guard.remaining().unwrap();
            let upto = next(&mut progress, now, &last);
            let (stop, outputs) = run(&mut machine, now, upto, &[]);
            last = format!("{stop:?}; {outputs:?}");
            assert!(!matches!(stop, StopReason::Exited { .. }));
            now = upto;
            if outputs.iter().any(|output| matches!(output.payload, GuestResponse::ApplicationOutcome { code: actual } if i32::from(actual) == code)) { break; }
        }
        let snapshot = machine.snapshot().unwrap();
        for mut branch in [
            machine,
            PVMachine::from_snapshot(&worker, &snapshot).unwrap(),
        ] {
            let (stop, outputs) = run(&mut branch, now, now, &[]);
            assert!(!matches!(stop, StopReason::Exited { .. }));
            assert!(outputs.is_empty());
            let id = std::num::NonZeroU32::new(91).unwrap();
            let (_, outputs) = run(
                &mut branch,
                now,
                now,
                &[GuestRequest::Application {
                    id,
                    command: seedbox_machine::pv::ApplicationCommand::Start,
                }],
            );
            assert!(outputs.is_empty());
            let mut ack = 0;
            let mut outcome = 0;
            let mut cursor = now;
            let mut progress = phase(
                "restart acknowledgement and outcome",
                now,
                now + 1_000_000_000,
            );
            last = format!("start command {id} queued; awaiting acknowledgement/outcome");
            while outcome == 0 {
                guard.remaining().unwrap();
                let upto = next(&mut progress, cursor, &last);
                let (stop, outputs) = run(&mut branch, cursor, upto, &[]);
                last = format!("{stop:?}; {outputs:?}; ack={ack}; outcome={outcome}");
                assert!(!matches!(stop, StopReason::Exited { .. }));
                for output in outputs {
                    match output.payload {
                        GuestResponse::CommandAcknowledged {
                            id: actual,
                            result: seedbox_machine::pv::CommandResult::Completed,
                        } if actual == id => ack += 1,
                        GuestResponse::ApplicationOutcome { code: actual } => {
                            assert_eq!(i32::from(actual), code);
                            outcome += 1;
                        }
                        _ => {}
                    }
                }
                cursor = upto;
            }
            assert_eq!((ack, outcome), (1, 1));
            run(&mut branch, cursor, cursor, &[GuestRequest::Shutdown]);
            last = String::from("shutdown queued");
            let mut progress = phase(
                "shutdown without repeated outcome",
                cursor,
                cursor + 1_000_000_000,
            );
            loop {
                guard.remaining().unwrap();
                let upto = next(&mut progress, cursor, &last);
                let (stop, outputs) = run(&mut branch, cursor, upto, &[]);
                last = format!("{stop:?}; {outputs:?}");
                assert!(!outputs.iter().any(|output| matches!(
                    output.payload,
                    GuestResponse::ApplicationOutcome { .. }
                )));
                if let StopReason::Exited { reason, .. } = stop {
                    assert_eq!(reason, ExitReason::Shutdown);
                    break;
                }
                cursor = upto;
            }
        }
    }
}
