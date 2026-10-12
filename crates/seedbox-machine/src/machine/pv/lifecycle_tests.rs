use super::*;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

#[test]
#[ignore = "requires an arm64 hypervisor and SEEDBOX_PV_WORKER"]
fn reboot_clears_timers_at_current_time_and_preserves_entropy_across_snapshot() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    // ARM Image: PV handshake, five-nanosecond sleep, then PSCI SYSTEM_OFF.
    let words: [u32; 11] = [
        0xd2a1_4021,
        0xd280_0060,
        0xf900_1020,
        0xd2a1_4002, // MOV X2, #0x0a000000: shared PV time page.
        0xf940_0040, // LDR X0, [X2]: current boot time.
        0x9100_1400, // ADD X0, X0, #5: absolute sleep deadline.
        0xf900_0020,
        0xf900_0820,
        0xd280_0100,
        0xf2b0_8000,
        0xd400_0002,
    ];
    let mut kernel = vec![0; 64];
    kernel[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // Branch over Image header.
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel.extend(words.iter().flat_map(|word| word.to_le_bytes()));
    let mut machine = PVMachine::new(
        Path::new(&executable),
        kernel,
        b"initrd".to_vec(),
        7,
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(1)).unwrap(),
        StopReason::Blocked {
            reached: at(0),
            deadline: Some(at(5))
        }
    );
    let snapshot = machine.snapshot().unwrap();
    clock.set(3);
    machine.reboot(at(3)).unwrap();
    assert_eq!(clock.now(), 3);
    assert_eq!(machine.reached, 3);
    assert!(machine.state == State::Ready);
    assert_eq!(
        machine.run(&mut (), at(3)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(
        machine.run(&mut (), at(4)).unwrap(),
        StopReason::Blocked {
            reached: at(3),
            deadline: Some(at(8))
        }
    );
    let seed = machine.seed;
    let branch_clock = Arc::new(crate::clock::SimulationClock::new(3));
    let mut branch =
        PVMachine::from_snapshot(Path::new(&executable), &snapshot, branch_clock).unwrap();
    branch.reboot(at(3)).unwrap();
    assert_eq!(branch.seed, seed);
    branch.crash().unwrap();
    branch.reboot(at(3)).unwrap();
    assert_ne!(branch.seed, seed);
    clock.set(8);
    assert!(matches!(
        machine.run(&mut (), at(9)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    clock.set(9);
    machine.reboot(at(9)).unwrap();
    assert_eq!(
        machine.run(&mut (), at(9)).unwrap(),
        StopReason::BoundaryReached
    );
}
