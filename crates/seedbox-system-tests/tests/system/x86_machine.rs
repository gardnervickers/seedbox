//! These probes need real instruction entry, kernel DMA and vCPU restoration.
//! Page policy and descriptor/model validation have ordinary unit coverage.
use kvm_ioctls::Kvm;
use seedbox_vmm::machine::{pv::PvClock, BootSpec, Machine, MachineSnapshot, Stop};
use std::time::Duration;
use vmm_core::rng::Rng;
const INITRD_ADDR: usize = 0x0800_0000;

fn store(code: &mut Vec<u8>, address: u32, value: u64) {
    for (address, value) in [(address, value as u32), (address + 4, (value >> 32) as u32)] {
        code.extend_from_slice(&[0xc7, 0x05]);
        code.extend_from_slice(&address.to_le_bytes());
        code.extend_from_slice(&value.to_le_bytes());
    }
}
fn signal(code: &mut Vec<u8>, byte: u8) {
    code.extend_from_slice(&[0xb0, byte, 0xba, 0xe9, 0, 0, 0, 0xee]);
}
fn negotiate(code: &mut Vec<u8>, abi: u64) {
    store(code, 0xd0000030, 2); // guest implements PV time
    store(code, 0xd0000020, 0xffff); // retain IRQ0 until the fixture unmasks it
    code.extend_from_slice(&[0xb8]);
    code.extend_from_slice(&(abi as u32).to_le_bytes());
    code.extend_from_slice(&[0xba, 0x28, 0x05, 0, 0, 0xef]);
}
fn machine(kvm: &Kvm, code: &[u8], timer: bool) -> Machine {
    let mut kernel = vec![0; 1024];
    kernel[0x1f1] = 1;
    kernel.push(0xfb); // STI, through the actual boot handoff
    kernel.extend_from_slice(code);
    Machine::new(
        kvm,
        "native-probe",
        &BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            barrier: false,
            page_timer: timer,
        },
    )
    .unwrap()
}

pub fn page_timer_and_watchdog(kvm: &Kvm) {
    // Unit tests cover every ABI/policy combination. Native execution checks
    // the three distinct guest-port paths: optional, enabled and rejected.
    for (requested, abi) in [(false, 3), (true, 4), (true, 3)] {
        let mut code = vec![];
        negotiate(&mut code, abi);
        code.push(0xf4);
        let mut m = machine(kvm, &code, requested);
        let result = m.run_until(
            &mut PvClock::new(Duration::from_secs(10)).unwrap(),
            1_000_000,
            0,
            &mut vec![],
        );
        if requested && abi < 4 {
            assert_eq!(result.unwrap_err(), "the guest kernel lacks page timer support; rebuild it from kernel/ or drop --page-timer");
            assert_eq!(m.time_page()[15], 0);
        } else {
            assert_eq!(result.unwrap(), Stop::Halted);
            assert_eq!(m.abi(), abi);
            assert_eq!(m.time_page()[15], u64::from(requested));
        }
    }
    // A store/yield/halt interrupted between guest instructions must retain
    // the tightened bound, adopted deadline, IRQ and deterministic witness.
    for (old, deadline) in [
        (None, 900_000u64),
        (Some(20_000), 900_000),
        (None, 20_000),
        (Some(20_000), u64::MAX),
    ] {
        let mut code = vec![];
        negotiate(&mut code, 4);
        if let Some(old) = old {
            store(&mut code, 0xd0000070, old);
        }
        store(&mut code, 0xd0000008, 30_000);
        signal(&mut code, b'S');
        store(&mut code, 0xd0000008, 30_000);
        store(&mut code, 0xd0000070, deadline);
        code.extend_from_slice(&[0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd, 0xf4]);
        // Wake without entering an absent handler: unmask at a committed
        // console boundary, then let KVM inject through the installed IDT.
        store(&mut code, 0xd0000020, 0);
        // Commit the PIC unmask through real PIO before the console stop. A
        // page store alone is adopted only at the next legal entry.
        code.extend_from_slice(&[0xb0, 0, 0xba, 0x21, 0, 0, 0, 0xee]);
        signal(&mut code, b'U');
        store(&mut code, 0xd0000020, 0xffff);
        code.push(0xf4);
        let mut expected = None;
        for period in [Duration::from_secs(10), Duration::from_micros(100)] {
            let mut m = machine(kvm, &code, true);
            m.world.observe_console = true;
            m.timer_jitter = 1000;
            m.tolerance = 100_000;
            m.exact_timer_ppm = 500_000;
            m.timer_rng = Some(Rng::new(1009, "timer/native-probe"));
            // Install a protected-mode interrupt gate and an INC/IRET handler.
            let memory = m.memory();
            let handler = 0x100400u32;
            let gate = 0x2000 + 0x30 * 8;
            memory[gate..gate + 2].copy_from_slice(&(handler as u16).to_le_bytes());
            memory[gate + 2..gate + 4].copy_from_slice(&8u16.to_le_bytes());
            memory[gate + 5] = 0x8e;
            memory[gate + 6..gate + 8].copy_from_slice(&((handler >> 16) as u16).to_le_bytes());
            memory[0x3000..0x3002].copy_from_slice(&0x187u16.to_le_bytes());
            memory[0x3002..0x3006].copy_from_slice(&0x2000u32.to_le_bytes());
            memory[0x3010..0x3012].copy_from_slice(&23u16.to_le_bytes());
            memory[0x3012..0x3016].copy_from_slice(&0x4000u32.to_le_bytes());
            memory[0x4008..0x4010].copy_from_slice(&0x00cf9b000000ffffu64.to_le_bytes());
            memory[0x4010..0x4018].copy_from_slice(&0x00cf93000000ffffu64.to_le_bytes());
            memory[handler as usize..handler as usize + 7]
                .copy_from_slice(&[0xff, 0x05, 0, 0x50, 0x08, 0, 0xcf]);
            // Interrupt entry reloads CS from a guest GDT, even though boot
            // supplied cached segment state. Load both tables in the guest.
            let original = m.memory()[0x100000..0x100000 + code.len() + 1].to_vec();
            m.memory()[0x100000..0x10000e].copy_from_slice(&[
                0x0f, 0x01, 0x15, 0x10, 0x30, 0, 0, 0x0f, 0x01, 0x1d, 0, 0x30, 0, 0,
            ]);
            m.memory()[0x10000e..0x10000e + original.len()].copy_from_slice(&original);
            let mut clock = PvClock::new(period).unwrap();
            assert_eq!(
                m.run_until(&mut clock, 1_000_000, 0, &mut vec![]).unwrap(),
                Stop::Control
            );
            m.world.clear_effects();
            m.world.observe_console = false;
            assert_eq!(
                m.run_until(&mut clock, 1_000_000, 0, &mut vec![]).unwrap(),
                Stop::Halted
            );
            assert_eq!(m.time, 30_000);
            if period < Duration::from_secs(1) {
                assert!(m.stats.eintr > 0);
            }
            let adopted = m.next_deadline();
            let pending = old.is_some() || deadline <= 30_000;
            let wake = if pending { m.time } else { adopted };
            if !pending {
                assert!((deadline..=deadline + 1000).contains(&wake));
            }
            m.wake(wake);
            m.world.observe_console = true;
            assert_eq!(
                m.run_until(&mut clock, wake + 1_000_000, 0, &mut vec![])
                    .unwrap(),
                Stop::Control
            );
            assert_eq!(m.stats.irqs, 1);
            m.world.clear_effects();
            m.world.observe_console = false;
            assert_eq!(
                m.run_until(&mut clock, wake + 1_000_000, 0, &mut vec![])
                    .unwrap(),
                Stop::Halted
            );
            assert_eq!(m.stats.irqs, 1);
            assert_eq!(&m.memory()[0x85000..0x85004], &1u32.to_le_bytes());
            assert!(!m.has_pending_irq());
            let observed = (
                wake,
                adopted,
                m.stats.irqs,
                m.exit_witness(),
                m.timer_rng.as_ref().unwrap().counter(),
            );
            if let Some(expected) = &expected {
                assert_eq!(&observed, expected);
            } else {
                expected = Some(observed);
            }
        }
    }
    let mut code = vec![];
    negotiate(&mut code, 4);
    store(&mut code, 0xd0000070, 20_000);
    store(&mut code, 0xd0000038, 20_000);
    code.extend_from_slice(&[
        0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd, 0xba, 0x2c, 0x05, 0, 0, 0xb0, 1, 0xee, 0xf4,
    ]);
    let mut witness = None;
    for period in [Duration::from_secs(10), Duration::from_micros(100)] {
        let mut m = machine(kvm, &code, true);
        assert_eq!(
            m.run_until(
                &mut PvClock::new(period).unwrap(),
                1_000_000,
                0,
                &mut vec![]
            )
            .unwrap(),
            Stop::Halted
        );
        assert_eq!(m.time, 20_000);
        assert_eq!(m.next_deadline(), u64::MAX);
        if let Some(expected) = &witness {
            assert_eq!(&m.exit_witness(), expected);
        } else {
            witness = Some(m.exit_witness());
        }
    }
}

pub fn changing_idle_periods_and_clock_adoption(kvm: &Kvm) {
    for arm in [false, true] {
        let mut code = vec![];
        store(&mut code, 0xd0000008, 5_000_000);
        if arm {
            code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0xba, 0x30, 0x05, 0, 0, 0x66, 0xef]);
        }
        code.push(0xf4);
        let mut m = machine(kvm, &code, false);
        let mut clock = PvClock::new(Duration::from_millis(20)).unwrap();
        let mut halted = false;
        for _ in 0..100 {
            let stop = m.run_until(&mut clock, 10_000_000, 0, &mut vec![]).unwrap();
            if stop != Stop::Reached {
                assert_eq!(stop, Stop::Halted);
                halted = true;
                break;
            }
        }
        assert!(halted);
        assert!(m.time >= 5_000_000);
        assert!(m.next_deadline() == u64::MAX || m.next_deadline() > 5_000_000);
    }
    let start = 1_010_599_858u64;
    let periods = [(100_001, 500_003), (700_007, 300_011), (200_013, 800_017)];
    let delay = [0xb9, 0, 0, 0, 0x01, 0x49, 0x75, 0xfd];
    let mut code = vec![];
    negotiate(&mut code, 4);
    for (round, (first, second)) in periods.into_iter().enumerate() {
        let origin = start + round as u64 * 2_000_000;
        store(&mut code, 0xd0000008, origin);
        code.extend_from_slice(&delay);
        store(&mut code, 0xd0000070, origin + first);
        store(&mut code, 0xd0000038, origin + first);
        code.extend_from_slice(&delay);
        store(&mut code, 0xd0000070, origin + second);
        store(&mut code, 0xd0000038, origin + first.min(second));
        code.extend_from_slice(&delay);
        code.extend_from_slice(&[0xba, 0x2c, 0x05, 0, 0, 0xb0, 1, 0xee, 0xf4]);
    }
    let mut expected = None;
    for period in [Duration::from_secs(10), Duration::from_micros(100)] {
        let mut m = machine(kvm, &code, true);
        m.timer_jitter = 1000;
        m.exact_timer_ppm = 500_000;
        m.timer_rng = Some(Rng::new(1009, "timer/pv-probe"));
        let mut clock = PvClock::new(period).unwrap();
        let mut evidence = vec![];
        for (round, (first, second)) in periods.into_iter().enumerate() {
            let origin = start + round as u64 * 2_000_000;
            m.wake(origin);
            assert_eq!(
                m.run_until(&mut clock, origin + 1_000_000, 0, &mut vec![])
                    .unwrap(),
                Stop::Halted
            );
            assert_eq!(m.time, origin + first.min(second));
            assert!((origin + second..=origin + second + 1000).contains(&m.next_deadline()));
            evidence.push((
                m.time,
                m.next_deadline(),
                m.time_page()[7],
                m.exit_witness(),
                m.timer_rng.as_ref().unwrap().counter(),
            ));
        }
        if period < Duration::from_secs(1) {
            assert!(m.stats.eintr > 0);
        }
        if let Some(expected) = &expected {
            assert_eq!(&evidence, expected);
        } else {
            expected = Some(evidence);
        }
    }
}

pub fn checkpoint_and_interrupted_compute(kvm: &Kvm) {
    let mut code = vec![];
    negotiate(&mut code, 3);
    signal(&mut code, b'S');
    code.extend_from_slice(&[0xb9, 0, 0, 0, 0x20, 0x49, 0x75, 0xfd, 0xf4]);
    let mut m = machine(kvm, &code, false);
    m.syscall_ns = 937;
    m.world.observe_console = true;
    let mut clock = PvClock::new(Duration::from_secs(10)).unwrap();
    assert_eq!(
        m.run_until(&mut clock, 1_000_000, 0, &mut vec![]).unwrap(),
        Stop::Control
    );
    m.world.clear_effects();
    m.world.observe_console = false;
    m.pause().unwrap();
    let page = m.time_page();
    let saved = m.snapshot().unwrap();
    let directory = tempfile::tempdir().unwrap();
    saved.persist(&directory.path().join("checkpoint")).unwrap();
    let saved: MachineSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
    drop(m);
    let mut expected = None;
    for period in [Duration::from_secs(10), Duration::from_millis(2)] {
        let mut m = machine(kvm, &code, false);
        m.restore(&saved).unwrap();
        assert!(m.is_paused());
        assert_eq!(m.syscall_ns, 937);
        assert_eq!(m.time_page(), page);
        m.reseed_time_page([42; 8]).unwrap();
        assert!(m.is_paused());
        assert_eq!(m.time, 0);
        assert_eq!(&m.time_page()[..9], &page[..9]);
        m.resume(0).unwrap();
        let mut clock = PvClock::new(period).unwrap();
        assert_eq!(
            m.run_until(&mut clock, 1_000_000_000, 0, &mut vec![])
                .unwrap(),
            Stop::Halted
        );
        let registers = m.registers().unwrap();
        assert_eq!(registers.rcx, 0);
        assert_eq!(m.time, 0);
        if period < Duration::from_secs(1) {
            assert!(m.stats.eintr > 0);
        }
        let observed = (
            registers.rip,
            registers.rcx,
            m.exit_witness(),
            m.time_page(),
        );
        if let Some(expected) = &expected {
            assert_eq!(&observed, expected);
        } else {
            expected = Some(observed);
        }
    }
}
pub fn event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks(kvm: &Kvm) {
    for site in ["entry", "slice", "bounded_slice", "halt", "charged_io"] {
        for (event_idx, deferred) in [(false, false), (true, false), (false, true), (true, true)] {
            let case = format!("site={site} event_idx={event_idx} deferred={deferred}");
            // Publish a head, then yield to the slice end, halt, or start
            // charged port I/O. Entry starts with an already posted head.
            let mut code = vec![0xfb, 0xc7, 0x05, 0x02, 0x20, 0x08, 0x00, 1, 0, 0, 0];
            if site == "slice" || site == "bounded_slice" {
                code.extend_from_slice(&[0xb0, 1, 0xba, 0x2c, 0x05, 0, 0, 0xee]);
            }
            if site == "charged_io" {
                code.extend_from_slice(&[0xba, 0x30, 0x05, 0, 0, 0xee]);
            }
            code.push(0xf4);
            let mut kernel = vec![0; 1024];
            kernel[0x1f1] = 1;
            kernel.extend(code);
            let spec = BootSpec {
                kernel: &kernel,
                initrd: &[],
                cmdline: String::new(),
                mem: INITRD_ADDR + 0x100000,
                rng_seed: [0; 32],
                mac: Some([2, 0, 0, 0, 0, 1]),
                control_channel: false,
                disk: None,
                barrier: false,
                page_timer: false,
            };
            let mut m = Machine::new(kvm, "net-poll", &spec).unwrap();
            let mut net = m.net.take().unwrap();
            let memory = m.memory();
            net.set_deferred_tx(deferred);
            for (off, v) in [
                (0x020, (1 << 5) | if event_idx { 1 << 29 } else { 0 }),
                (0x024, 1),
                (0x020, 1),
                (0x070, 15),
                (0x030, 1),
                (0x038, 1),
                (0x080, 0x81000),
                (0x090, 0x82000),
                (0x0a0, 0x83000),
                (0x044, 1),
            ] {
                net.write(memory, off, v).unwrap();
            }
            memory[0x81000..0x81008].copy_from_slice(&0x84000u64.to_le_bytes());
            memory[0x81008..0x8100c].copy_from_slice(&17u32.to_le_bytes());
            memory[0x8400c..0x84011].copy_from_slice(b"frame");
            if site == "entry" {
                memory[0x82002..0x82004].copy_from_slice(&1u16.to_le_bytes());
            }
            m.net = Some(net);
            let mut frames = Vec::new();
            let (limit, early) = if site == "bounded_slice" {
                (1_000_000, 100_000)
            } else {
                (500, 0)
            };
            let stop = m
                .run_until(
                    &mut PvClock::new(std::time::Duration::from_millis(20)).unwrap(),
                    limit,
                    early,
                    &mut frames,
                )
                .unwrap();
            if site == "bounded_slice" {
                assert_eq!(
                    m.time,
                    if deferred {
                        vmm_core::virtio_network::TX_POLL_NS
                    } else {
                        limit - early
                    },
                    "{case}"
                );
            }
            if event_idx || deferred {
                assert_eq!(stop, Stop::Sent, "{case}");
                assert_eq!(frames, [b"frame".to_vec()], "{case}");
                // used->idx is at used + 2; used + 12 is avail_event
                // for this one-entry queue. Only one head was posted.
                assert_eq!(
                    u16::from_le_bytes(m.memory()[0x83002..0x83004].try_into().unwrap()),
                    1,
                    "{case}"
                );
            } else {
                assert_eq!(
                    stop,
                    if site == "slice" || site == "bounded_slice" || site == "charged_io" {
                        Stop::Reached
                    } else {
                        Stop::Halted
                    },
                    "{case}"
                );
                assert!(frames.is_empty(), "{case}");
            }
            let net = m.net.as_ref().unwrap();
            assert_eq!(net.stats().doorbell_exits, 0, "{case}");
            assert_eq!(
                net.stats().tx_frames,
                u64::from(event_idx || deferred),
                "{case}"
            );
            assert_eq!(
                net.stats().tx_polled_at_boundary,
                u64::from((event_idx || deferred) && site != "entry"),
                "{case}"
            );
            assert!(net.dirty.is_empty(), "{case}"); // Machine adopted all DMA writes.
        }
    }
}

pub fn progress_debt_stops_execution_and_survives_restore(kvm: &Kvm) {
    // Real KVM instruction entry is needed to prove the guest cannot execute
    // ahead while stopped CPU charge remains, including watchdog and restore.
    let mut code = vec![];
    negotiate(&mut code, 4);
    store(&mut code, 0xd0000070, 15_000); // arm through the real guest ABI
    code.extend_from_slice(&[
        0xc7, 0x05, 0x18, 0, 0, 0xd0, 25, 0, 0, 0, // syscalls = 25
        0xb9, 0, 0, 0, 1, 0x49, 0x75, 0xfd, // force watchdog interruptions
        0xba, 0x2c, 0x05, 0, 0, 0x31, 0xc0, 0xee, // yield(0)
        0xba, 0xe9, 0, 0, 0, 0xb0, b'X', 0xee, // console
        0xba, 0xf4, 0, 0, 0, 0x31, 0xc0, 0xee, // exit(0)
    ]);
    let mut reference = None;
    for watchdog in [Duration::from_secs(10), Duration::from_micros(100)] {
        let mut m = machine(kvm, &code, true);
        let mut clock = PvClock::new(watchdog).unwrap();
        let mut frames = Vec::new();
        assert_eq!(
            m.run_until(&mut clock, 10_000, 0, &mut frames).unwrap(),
            Stop::Reached
        );
        assert_eq!(m.time, 10_000);
        assert!(m.com1.out.is_empty());
        if watchdog == Duration::from_micros(100) {
            assert!(m.stats.eintr > 0, "exercise watchdog landings with debt");
        }
        let saved = m.snapshot().unwrap();
        let MachineSnapshot::Memory(memory) = &saved else {
            panic!("expected in-memory snapshot");
        };
        assert_eq!(
            serde_json::to_value(&**memory.as_ref()).unwrap()["pv_debt_ns"],
            15_000
        );
        for restored in [false, true] {
            if restored {
                m.restore(&saved).unwrap();
                clock.restore_boundary().unwrap();
            }
            assert_eq!(m.next_deadline(), 15_000);
            let exits = m.stats.exits;
            assert_eq!(
                m.run_until(&mut clock, 20_000, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert_eq!(m.time, 20_000);
            assert_eq!(m.stats.exits, exits, "debt must drain without KVM_RUN");
            assert!(m.com1.out.is_empty());
            assert_eq!(m.next_deadline(), u64::MAX);
            assert_eq!(
                m.run_until(&mut clock, 30_000, 0, &mut frames).unwrap(),
                Stop::Exited
            );
            assert_eq!(m.time, 25_000);
            assert_eq!(m.com1.out, b"X");
            let result = (m.time, m.exit_witness(), m.com1.out.clone());
            if let Some(want) = &reference {
                assert_eq!(&result, want);
            } else {
                reference = Some(result);
            }
        }
    }
}
