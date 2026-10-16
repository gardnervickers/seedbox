use super::*;

fn guard() -> HangGuard {
    // A thread handshake normally takes milliseconds; two minutes is only a
    // deadlock escape for a severely loaded host, not a completion threshold.
    HangGuard::new(
        "test-time unit-test actors must finish their explicit handshake",
        Duration::from_secs(120),
    )
}

#[test]
fn controlled_clock_is_shared_monotonic_and_unchanged_on_errors() {
    let clock = ControlledClock::new(Duration::from_secs(3));
    let clone = clock.clone();
    assert_eq!(clone.now(), Duration::from_secs(3));
    clone.advance_by(Duration::from_millis(7)).unwrap();
    assert_eq!(clock.now(), Duration::from_millis(3007));
    assert!(matches!(
        clock.advance_to(Duration::ZERO),
        Err(ClockError::Backwards { .. })
    ));
    assert_eq!(clone.now(), Duration::from_millis(3007));
    clock.advance_to(Duration::MAX).unwrap();
    assert_eq!(
        clone.advance_by(Duration::from_nanos(1)),
        Err(ClockError::Overflow)
    );
    assert_eq!(clock.now(), Duration::MAX);
}

#[test]
fn handshake_proves_clock_observation_before_and_after_explicit_release() {
    let guard = guard();
    let clock = ControlledClock::new(Duration::from_secs(3));
    let (control, participant) = handshake();
    std::thread::scope(|scope| {
        let observed = clock.clone();
        let guard = &guard;
        let worker = scope.spawn(move || {
            assert_eq!(observed.now(), Duration::from_secs(3));
            participant.enter_and_wait(guard).unwrap();
            observed.now()
        });
        control.wait_entered(guard).unwrap();
        clock.advance_to(Duration::from_secs(7)).unwrap();
        control.proceed().unwrap();
        assert_eq!(worker.join().unwrap(), Duration::from_secs(7));
    });
}

#[test]
fn controller_drop_before_entry_is_cancellation_not_permission() {
    let (control, participant) = handshake();
    drop(control);
    assert_eq!(
        participant.enter_and_wait(&guard()),
        Err(WaitError::Cancelled)
    );
}

#[test]
fn missing_participant_is_cancellation_not_entry() {
    let (control, participant) = handshake();
    drop(participant);
    assert_eq!(control.wait_entered(&guard()), Err(WaitError::Cancelled));
    assert_eq!(control.proceed(), Err(WaitError::Cancelled));
}

#[test]
fn permission_cannot_precede_the_observed_entry() {
    let (control, participant) = handshake();
    assert_eq!(control.proceed(), Err(WaitError::ProceedBeforeEntered));
    assert_eq!(
        participant.enter_and_wait(&guard()),
        Err(WaitError::Cancelled)
    );
}

#[test]
fn cancelling_an_entered_actor_wakes_it_without_permission() {
    let guard = guard();
    let (control, participant) = handshake();
    std::thread::scope(|scope| {
        let guard = &guard;
        let worker = scope.spawn(move || participant.enter_and_wait(guard));
        control.wait_entered(guard).unwrap();
        drop(control);
        assert_eq!(worker.join().unwrap(), Err(WaitError::Cancelled));
    });
}

#[test]
fn unwinding_the_controller_cancels_instead_of_stranding_the_participant() {
    let guard = guard();
    let (control, participant) = handshake();
    std::thread::scope(|scope| {
        let guard = &guard;
        let worker = scope.spawn(move || participant.enter_and_wait(guard));
        let failure = std::panic::catch_unwind(move || {
            control.wait_entered(guard).unwrap();
            panic!("test-owned failure before proceed");
        });
        assert!(failure.is_err());
        assert_eq!(worker.join().unwrap(), Err(WaitError::Cancelled));
    });
}

#[test]
fn expired_guard_is_an_infrastructure_error_not_a_successful_absence() {
    let guard = HangGuard::new(
        "deliberately exhausted infrastructure budget",
        Duration::ZERO,
    );
    let error = guard.clone().wait_for(|| None::<()>).unwrap_err();
    assert_eq!(
        error,
        WaitError::Hang {
            reason: "deliberately exhausted infrastructure budget".into(),
            limit: Duration::ZERO
        }
    );
    assert_eq!(guard.remaining().unwrap_err(), error);
    let (sender, receiver) = mpsc::channel::<u8>();
    assert_eq!(guard.recv(&receiver), Err(error));
    sender.send(23).unwrap();
    // An already observed positive event needs no further host waiting.
    assert_eq!(guard.recv(&receiver), Ok(23));
    assert_eq!(guard.wait_for(|| Some(7)), Ok(7));
    drop(sender);
    assert_eq!(guard.recv(&receiver), Err(WaitError::Cancelled));
}

#[test]
fn strict_progress_rejects_stalls_and_caps_the_requested_deadline() {
    let mut budget = GuestProgress::new("boot", Duration::ZERO, Duration::from_secs(2), 0).unwrap();
    assert_eq!(budget.phase(), "boot");
    assert_eq!(
        budget.next(Duration::ZERO, Duration::from_secs(3)),
        Ok(Duration::from_secs(2))
    );
    assert_eq!(
        budget.next(Duration::ZERO, Duration::from_secs(1)),
        Err(ProgressError::NoProgress)
    );
    assert_eq!(
        budget.next(Duration::from_secs(1), Duration::from_secs(2)),
        Ok(Duration::from_secs(2))
    );
    assert_eq!(
        budget.next(Duration::from_secs(2), Duration::from_secs(1)),
        Err(ProgressError::Deadline)
    );
}

#[test]
fn legitimate_same_instant_rounds_are_finite_and_reset_after_advancement() {
    let mut budget = GuestProgress::new(
        "due-only admission",
        Duration::ZERO,
        Duration::from_secs(3),
        1,
    )
    .unwrap();
    let quantum = Duration::from_millis(1);
    budget.next(Duration::ZERO, quantum).unwrap();
    budget.next(Duration::ZERO, quantum).unwrap();
    assert_eq!(
        budget.next(Duration::ZERO, quantum),
        Err(ProgressError::NoProgress)
    );
    budget.next(quantum, quantum).unwrap();
    budget.next(quantum, quantum).unwrap();
    assert_eq!(
        budget.next(quantum, quantum),
        Err(ProgressError::NoProgress)
    );
    assert_eq!(
        budget.next(Duration::ZERO, quantum),
        Err(ProgressError::Backwards)
    );
    assert_eq!(
        budget.next(quantum, Duration::ZERO),
        Err(ProgressError::ZeroQuantum)
    );
}

#[test]
fn executing_forever_without_finishing_exhausts_only_the_simulated_budget() {
    for quantum in [Duration::from_millis(1), Duration::from_nanos(131_071)] {
        let end = Duration::from_millis(53);
        let mut budget = GuestProgress::new("finite workload", Duration::ZERO, end, 0).unwrap();
        let mut now = Duration::ZERO;
        while let Ok(next) = budget.next(now, quantum) {
            now = next;
        }
        assert_eq!(now, end);
        assert_eq!(budget.next(now, quantum), Err(ProgressError::Deadline));
    }
}

#[test]
fn invalid_progress_configuration_is_not_an_execution_result() {
    assert_eq!(
        GuestProgress::new("", Duration::ZERO, Duration::from_secs(1), 0).unwrap_err(),
        ProgressError::InvalidBudget
    );
    assert_eq!(
        GuestProgress::new("empty budget", Duration::ZERO, Duration::ZERO, 0).unwrap_err(),
        ProgressError::InvalidBudget
    );
    let mut budget = GuestProgress::new(
        "nonzero origin",
        Duration::from_secs(3),
        Duration::from_secs(4),
        0,
    )
    .unwrap();
    assert_eq!(
        budget.next(Duration::ZERO, Duration::from_millis(1)),
        Err(ProgressError::Backwards)
    );
}
