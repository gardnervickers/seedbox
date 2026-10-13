//! Actual application outcomes leave the kernel and retained launch recipe alive.
use super::*;
use crate::{
    clock::SimulationClock,
    pv::{Inbox, Outbox, Output, Request as GuestRequest, Response as GuestResponse},
    rng::SeededRng,
};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name).expect(name).into()
}
fn application(code: u32) -> Vec<u8> {
    assert!(code < 65536);
    let words = [0xd280_0000u32 | (code << 5), 0xd280_0ba8, 0xd400_0001]; // exit(code)
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&183u16.to_le_bytes());
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
    let length = (elf.len() + words.len() * 4) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    for word in words {
        elf.extend_from_slice(&word.to_le_bytes());
    }
    elf
}
fn replace_application(archive: &[u8], application: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut offset = 0;
    let mut replaced = false;
    loop {
        let mut header = archive[offset..offset + 110].to_vec();
        assert_eq!(&header[..6], b"070701");
        let number =
            |a, b| usize::from_str_radix(std::str::from_utf8(&header[a..b]).unwrap(), 16).unwrap();
        let size = number(54, 62);
        let namesize = number(94, 102);
        let name = &archive[offset + 110..offset + 110 + namesize];
        let start = (offset + 110 + namesize + 3) & !3;
        let end = start + size;
        let data = if name == b"application\0"
            || name == b"./application\0"
            || name == b"rootfs/application\0"
        {
            replaced = true;
            header[54..62].copy_from_slice(format!("{:08x}", application.len()).as_bytes());
            application
        } else {
            &archive[start..end]
        };
        result.extend(header);
        result.extend_from_slice(name);
        while !result.len().is_multiple_of(4) {
            result.push(0);
        }
        result.extend_from_slice(data);
        while !result.len().is_multiple_of(4) {
            result.push(0);
        }
        if name == b"TRAILER!!!\0" {
            break;
        }
        offset = (end + 3) & !3;
    }
    assert!(replaced);
    result
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
        .map(|payload| crate::pv::Input {
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
#[test]
#[ignore = "requires signed SEEDBOX_PV_WORKER, SEEDBOX_PV_KERNEL and SEEDBOX_PV_SERVICE_INITRD"]
fn native_linux_program_exit_zero_and_nonzero_survive_snapshot_and_zero_runs() {
    let worker = input("SEEDBOX_PV_WORKER");
    let kernel = fs::read(input("SEEDBOX_PV_KERNEL")).unwrap();
    let archive = fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap();
    for code in [0, 23] {
        let mut machine = PVMachine::from_boot_inputs(
            &worker,
            kernel.clone(),
            replace_application(&archive, &application(code as u32)),
        )
        .unwrap();
        let start = Instant::now();
        let mut now = 0;
        loop {
            assert!(start.elapsed() < Duration::from_secs(120));
            let (stop, outputs) = run(&mut machine, now, now + 5_000_000, &[]);
            assert!(!matches!(stop, StopReason::Exited { .. }));
            now += 5_000_000;
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
                    command: crate::pv::ApplicationCommand::Start,
                }],
            );
            assert!(outputs.is_empty());
            let mut ack = 0;
            let mut outcome = 0;
            let mut cursor = now;
            while outcome == 0 {
                let (stop, outputs) = run(&mut branch, cursor, cursor + 5_000_000, &[]);
                assert!(!matches!(stop, StopReason::Exited { .. }));
                for output in outputs {
                    match output.payload {
                        GuestResponse::CommandAcknowledged {
                            id: actual,
                            result: crate::pv::CommandResult::Completed,
                        } if actual == id => ack += 1,
                        GuestResponse::ApplicationOutcome { code: actual } => {
                            assert_eq!(i32::from(actual), code);
                            outcome += 1;
                        }
                        _ => {}
                    }
                }
                cursor += 5_000_000;
                assert!(start.elapsed() < Duration::from_secs(120));
            }
            assert_eq!((ack, outcome), (1, 1));
            run(&mut branch, cursor, cursor, &[GuestRequest::Shutdown]);
            loop {
                let (stop, outputs) = run(&mut branch, cursor, cursor + 5_000_000, &[]);
                assert!(!outputs.iter().any(|output| matches!(
                    output.payload,
                    GuestResponse::ApplicationOutcome { .. }
                )));
                if let StopReason::Exited { reason, .. } = stop {
                    assert_eq!(reason, ExitReason::Shutdown);
                    break;
                }
                cursor += 5_000_000;
                assert!(start.elapsed() < Duration::from_secs(120));
            }
        }
    }
}
