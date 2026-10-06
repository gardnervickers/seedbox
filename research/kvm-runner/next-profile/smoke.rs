mod progress;
use progress::*;
fn main() {
    assert_eq!(check_host("6.18.54", &[HOST_MICROCODE; 2]), Ok(()));
    assert_eq!(
        check_host("6.18.55", &[HOST_MICROCODE]),
        Err(Error::HostChanged)
    );
    assert_eq!(
        check_host("6.18.54", &[HOST_MICROCODE + 1]),
        Err(Error::HostChanged)
    );
    assert_eq!(check_host("6.18.54", &[]), Err(Error::HostChanged));
    let calibration = Calibration {
        native_cpu_ns: 5,
        native_branches: 3,
        benchmark_input: "synthetic-only".into(),
        counter_profile: "synthetic-count".into(),
        native_evidence: "not-a-native-measurement".into(),
        qualification: Qualification::SyntheticSmoke,
    };
    assert_eq!(
        calibration.require_qualified(),
        Err(Error::CalibrationNotQualified)
    );
    let mut one = CountClock::new(calibration.clone(), 1, 0, 0).unwrap();
    let mut split = one.clone();
    one.import(1, 3).unwrap();
    for n in 1..=3 {
        split.import(1, n).unwrap();
    }
    assert_eq!(one, split); // Reject per-stop rounding that loses elapsed work.
    assert_eq!(split.import(1, 3), Ok(0)); // Completion/debug administration costs no extra time.
    assert_eq!(split.import(1, 2), Err(Error::CounterWentBackwards));
    assert_eq!(split.import(2, 4), Err(Error::EpochChanged));
    split.idle_jump(100).unwrap();
    split.import(1, 6).unwrap();
    assert_eq!(split.now_ns, 105);
    assert_eq!(split.branches_until(106), Ok(1));
    let owner = Owner {
        machine: 1,
        incarnation: 2,
        request: 7,
    };
    let page = clock_page(&split, owner, 2, [0x42; 32]).unwrap();
    assert_eq!(page.len(), 128);
    assert_eq!(u64::from_le_bytes(page[48..56].try_into().unwrap()), 105);
    assert_eq!(page[4], 0); // Synthetic evidence cannot masquerade as qualified guest time.
    assert_eq!(
        clock_page(&split, owner, 3, [0; 32]),
        Err(Error::InvalidCalibration)
    );
    let mut queue = InterruptQueue::default();
    queue
        .enqueue(InterruptRequest {
            owner,
            registration: 4,
            deadline_ns: 105,
            requested_count: 6,
            vector: 0x30,
        })
        .unwrap();
    let ready = LegalWindow {
        kvm_ready: true,
        if_enabled: true,
        interrupt_shadow: false,
    };
    assert_eq!(
        queue.decision(StopPhase::PendingEmulatedIo, 105, owner, ready),
        Delivery::CompleteIoFirst
    );
    assert_eq!(
        queue.decision(StopPhase::DebugCorrectionAdministration, 105, owner, ready),
        Delivery::AdministrationOnly
    );
    assert!(matches!(
        queue.decision(
            StopPhase::ExactPreemptionBoundary,
            105,
            owner,
            LegalWindow {
                interrupt_shadow: true,
                ..ready
            }
        ),
        Delivery::RequestWindow(_)
    ));
    assert!(matches!(
        queue.decision(StopPhase::NaturalSemanticBoundary, 105, owner, ready),
        Delivery::Inject(_)
    ));
    assert!(matches!(
        queue.decision(StopPhase::InterruptWindowAdministration, 105, owner, ready),
        Delivery::Inject(_)
    ));
    assert_eq!(
        queue.decision(
            StopPhase::ExactPreemptionBoundary,
            105,
            Owner {
                incarnation: 3,
                ..owner
            },
            ready
        ),
        Delivery::RejectStale(owner)
    );
    queue.injected(owner, 8, 108, 0x401097).unwrap();
    assert_eq!(queue.injected[0].request.requested_count, 6);
    assert_eq!(queue.injected[0].actual_count, 8);
    assert_eq!(queue.injected(owner, 9, 109, 0), Err(Error::UnknownRequest));
    let stale = Owner {
        request: 9,
        ..owner
    };
    queue
        .enqueue(InterruptRequest {
            owner: stale,
            registration: 5,
            deadline_ns: 105,
            requested_count: 6,
            vector: 0x30,
        })
        .unwrap();
    queue.reject_stale(stale).unwrap();
    assert!(queue.pending.is_empty());
    assert_ne!(
        seeded_stop(0, 10, 20, 1, 100).unwrap(),
        seeded_stop(0, 10, 20, 2, 100).unwrap()
    );
    assert_eq!(seeded_stop(0, 10, 20, 2, 7), Ok(7));
    // Seed choices change actual pending timer IRQ targets, not only host stops.
    // This timing-fault allowance is synthetic and not a selected default.
    let timer_a = seeded_timer(&split, owner, 8, 106, 0x30, 1, 3).unwrap();
    let timer_b = seeded_timer(&split, owner, 8, 106, 0x30, 2, 3).unwrap();
    assert_ne!(
        timer_a.request.requested_count,
        timer_b.request.requested_count
    );
    assert!(timer_a.request.deadline_ns >= timer_a.programmed_deadline_ns);
    let mut seeded_irqs = InterruptQueue::default();
    seeded_irqs.enqueue(timer_a.request.clone()).unwrap();
    assert!(matches!(
        seeded_irqs.decision(
            StopPhase::ExactPreemptionBoundary,
            timer_a.request.deadline_ns,
            owner,
            ready
        ),
        Delivery::Inject(_)
    ));
    seeded_irqs
        .injected(
            owner,
            timer_a.request.requested_count,
            timer_a.request.deadline_ns,
            0x401097,
        )
        .unwrap();
    assert_eq!(seeded_irqs.injected[0].request.vector, 0x30);
    assert_eq!(
        seeded_timer(&split, owner, 8, 106, 0x30, 99, 0)
            .unwrap()
            .request
            .requested_count,
        7
    );
    println!(
        "{{\"status\":\"PASS\",\"scope\":\"off-host synthetic clock/IRQ contract smoke\",\"hardware_or_guest_qualification\":false,\"native_factor_selected\":false,\"clock_page_bytes\":128,\"retired_policy\":\"1000ns per clock read\",\"host_kernel_pin\":\"6.18.54\",\"microcode_pin\":\"0xa20102e\"}}"
    );
}
