use super::*;
use crate::driver::{Action, Driver, StepOutcome};

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

fn kernel(words: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; 64 + words.len() * 4];
    bytes[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // B +64, skip Image header.
    bytes[16..24].copy_from_slice(&4096u64.to_le_bytes());
    bytes[56..60].copy_from_slice(b"ARM\x64");
    for (index, word) in words.iter().enumerate() {
        bytes[64 + index * 4..68 + index * 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn timed_guest() -> Vec<u8> {
    kernel(&[
        0xd2a1_4021, // MOV X1, #0x0a010000 (PV event).
        0xd280_0060, // MOV X0, #3 (ABI).
        0xf900_1020, // STR X0, [X1,#32] (ready).
        0xd280_00a0, // MOV X0, #5.
        0xf900_0020, // STR X0, [X1] (arm timer).
        0xf900_0820, // STR X0, [X1,#16] (idle).
        0xd280_0100, // MOV X0, #8.
        0xf2b0_8000, // MOVK X0, #0x8400, LSL #16 (PSCI SYSTEM_OFF).
        0xd400_0002, // HVC #0.
    ])
}

#[test]
fn invalid_inputs_fail_before_spawning_worker() {
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let error = PVMachine::new(Path::new("/no-worker"), vec![], vec![], 1, clock)
        .err()
        .unwrap();
    assert!(!error.to_string().contains("machine worker"));
}

// Invoke explicitly with SEEDBOX_PV_WORKER set to a signed, current ARM worker.
// Default portable tests do not open HVF/KVM.
#[test]
#[ignore = "requires an arm64 hypervisor and SEEDBOX_PV_WORKER"]
fn native_driver_isolates_two_guests_and_restores_independent_snapshots() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let mut driver = Driver::new(at(0), (), |clock| {
        (0..2)
            .map(|_| {
                PVMachine::new(
                    executable,
                    timed_guest(),
                    b"initrd".to_vec(),
                    7,
                    clock.clone(),
                )
            })
            .collect()
    })
    .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    let snapshot = driver
        .with_barrier(|barrier| {
            let ids: Vec<_> = barrier.machines().map(|(id, _)| id).collect();
            for id in &ids {
                assert_eq!(
                    barrier.stopped(*id)?,
                    StopReason::Blocked {
                        reached: at(0),
                        deadline: Some(at(5))
                    }
                );
            }
            let snapshot = barrier.snapshot(ids[0])?;
            barrier.schedule(at(3), Action::Crash(ids[1]))?;
            Ok(snapshot)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(3) }
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished { at: at(5) }
    );
    drop(driver);
    // The checkpoint survives the workers and temporary boot/checkpoint files.
    let mut restored = Driver::new(at(0), (), |clock| {
        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
            .map_err(std::io::Error::other)?])
    })
    .unwrap();
    assert_eq!(
        restored.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    assert_eq!(
        restored.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        restored.step(at(10)).unwrap(),
        StepOutcome::Finished { at: at(5) }
    );
    let mut second = Driver::new(at(0), (), |clock| {
        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
            .map_err(std::io::Error::other)?])
    })
    .unwrap();
    assert_eq!(
        second.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    assert_eq!(
        second.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        second.step(at(10)).unwrap(),
        StepOutcome::Finished { at: at(5) }
    );
}

#[test]
#[ignore = "requires an arm64 hypervisor and SEEDBOX_PV_WORKER"]
fn native_indefinite_idle_and_capture_failure_keep_valid_machine() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(37));
    let code = kernel(&[0xd2a1_4021, 0xd280_0060, 0xf900_1020, 0xf900_0820]);
    let mut machine = PVMachine::new(
        Path::new(&executable),
        code,
        b"initrd".to_vec(),
        7,
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
    assert_eq!(clock.now(), 37);
    let existing = machine.directory.path().join("kernel");
    assert!(matches!(
        machine
            .worker()
            .unwrap()
            .call(Operation::Snapshot { artifact: existing })
            .unwrap(),
        Answer::SnapshotFailed(_)
    ));
    let snapshot = machine.snapshot().unwrap();
    assert_eq!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
    machine.crash().unwrap();
    assert!(machine.run(&mut (), at(100)).is_err());
    assert!(machine.snapshot().is_ok());
    drop(machine);
    let mut branch = PVMachine::from_snapshot(Path::new(&executable), &snapshot, clock).unwrap();
    assert_eq!(
        branch.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
}

fn linux_initrd() -> Vec<u8> {
    let message = b"PV_LINUX_PASS\n";
    let words: [u32; 15] = [
        0xd280_0020, // MOV X0,#1 (stdout).
        0x1000_01c1, // ADR X1,+56 (message).
        0xd280_0002 | ((message.len() as u32) << 5),
        0xd280_0808, // MOV X8,#64 (__NR_write).
        0xd400_0001, // SVC #0.
        0xd29b_d5a0, // MOV X0,#0xdead.
        0xf2bf_dc20, // MOVK X0,#0xfee1,LSL #16.
        0xd283_2d21, // MOV X1,#0x1969.
        0xf2a5_0241, // MOVK X1,#0x2812,LSL #16.
        0xd29f_db82, // MOV X2,#0xfedc.
        0xf2a8_6422, // MOVK X2,#0x4321,LSL #16 (POWER_OFF).
        0xd280_0003, // MOV X3,#0.
        0xd280_11c8, // MOV X8,#142 (__NR_reboot).
        0xd400_0001, // SVC #0.
        0x1400_0000, // B . if reboot unexpectedly returns.
    ];
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC.
    elf[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64.
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&0x400078u64.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD.
    elf[68..72].copy_from_slice(&5u32.to_le_bytes()); // Read/execute.
    elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
    elf[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
    let length = (elf.len() + words.len() * 4 + message.len()) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    for word in words {
        elf.extend_from_slice(&word.to_le_bytes());
    }
    elf.extend_from_slice(message);
    let mut archive = Vec::new();
    for (name, mode, data, major, minor) in [
        ("dev", 0o040755, &[][..], 0, 0),
        ("dev/console", 0o020600, &[][..], 5, 1),
        ("init", 0o100755, &elf[..], 0, 0),
        ("TRAILER!!!", 0, &[][..], 0, 0),
    ] {
        archive.extend_from_slice(b"070701");
        for field in [
            1,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            major,
            minor,
            name.len() + 1,
            0,
        ] {
            archive.extend_from_slice(format!("{field:08x}").as_bytes());
        }
        archive.extend_from_slice(name.as_bytes());
        archive.push(0);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
        archive.extend_from_slice(data);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
    }
    archive
}

#[test]
#[ignore = "requires an arm64 hypervisor, SEEDBOX_PV_WORKER and SEEDBOX_PV_KERNEL"]
fn native_linux_boot_and_program_poweroff_use_the_driver() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let kernel_path = std::env::var_os("SEEDBOX_PV_KERNEL").expect("set SEEDBOX_PV_KERNEL");
    let kernel = fs::read(kernel_path).unwrap();
    let mut driver = Driver::new(at(0), (), |clock| {
        Ok(vec![PVMachine::new(
            Path::new(&executable),
            kernel,
            linux_initrd(),
            7,
            clock,
        )?])
    })
    .unwrap();
    for tick in 1..=10_000 {
        match driver.step(at(tick * 1_000_000)).unwrap() {
            StepOutcome::Finished { .. } => {
                driver
                    .with_barrier(|barrier| {
                        let (_, machine) = barrier.machines().next().unwrap();
                        let console = String::from_utf8_lossy(&machine.console);
                        assert!(console.contains("PV_LINUX_PASS"), "{console}");
                        Ok(())
                    })
                    .unwrap();
                return;
            }
            StepOutcome::Blocked { .. } => panic!("Linux blocked without a timer"),
            _ => {}
        }
    }
    panic!("Linux failed to power off within the simulation bound");
}

#[test]
#[ignore = "requires an arm64 hypervisor and SEEDBOX_PV_WORKER"]
fn native_horizon_is_accepted_before_following_poweroff() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let code = kernel(&[
        0xd2a1_4021,
        0xd280_0060,
        0xf900_1020, // Ready handshake.
        0xf900_0c20, // STR X0,[X1,#24] (yield to published bound).
        0xd280_0100,
        0xf2b0_8000,
        0xd400_0002, // PSCI power off.
    ]);
    let mut driver = Driver::new(at(0), (), |clock| {
        Ok(vec![PVMachine::new(
            executable,
            code,
            b"initrd".to_vec(),
            1,
            clock,
        )?])
    })
    .unwrap();
    assert_eq!(
        driver.step(at(40)).unwrap(),
        StepOutcome::BarrierReached { at: at(40) }
    );
    let snapshot = driver
        .with_barrier(|barrier| {
            let id = barrier.machines().next().unwrap().0;
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            barrier.snapshot(id)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(50)).unwrap(),
        StepOutcome::Finished { at: at(40) }
    );
    drop(driver);
    let mut branch = Driver::new(at(40), (), |clock| {
        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
            .map_err(std::io::Error::other)?])
    })
    .unwrap();
    assert_eq!(
        branch.step(at(50)).unwrap(),
        StepOutcome::Finished { at: at(40) }
    );
}
