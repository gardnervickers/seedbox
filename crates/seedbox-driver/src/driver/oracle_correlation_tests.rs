use super::*;
#[derive(Clone)]
struct Park;
impl Machine for Park {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(&mut self, _: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Self::Error> {
        unreachable!()
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
#[test]
fn retired_boot_ack_does_not_complete_current_boot_pending_stop() {
    let mut driver = fixture::driver(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        1,
        SeededRng::new(0),
        || Ok(()),
        || Ok(vec![Park]),
    )
    .unwrap();
    let command = NonZeroU32::new(7).unwrap();
    driver.oracle_stops.insert((1, command));
    // The one committed boot advances only through the transition path.
    driver
        .control
        .admission(
            &mut driver.machines,
            MachineId(0),
            Timestamp::from_nanos(0),
            crate::oracle::observation::Transition::Reboot { new_boot: 1 },
        )
        .unwrap();
    driver
        .control
        .offered(
            MachineId(0),
            command,
            crate::oracle::service::CommandKind::Application(crate::pv::ApplicationCommand::Stop),
            crate::oracle::observation::Origin::External,
        )
        .unwrap();
    driver
        .control
        .accepted(
            MachineId(0),
            &mut driver.machines[0].lifecycle,
            1,
            command,
            Timestamp::from_nanos(0),
            Timestamp::from_nanos(0),
        )
        .unwrap();
    let ack = |boot| MachineResponse {
        machine: MachineId(0),
        boot,
        at: Timestamp::from_nanos(0),
        payload: Response::CommandAcknowledged {
            id: command,
            result: crate::pv::CommandResult::Completed,
        },
    };
    driver.machines[0].staged.push(ack(0));
    driver.commit_boundary().unwrap();
    assert!(driver.finished.is_none());
    assert!(driver.oracle_stops.contains(&(1, command)));
    driver.machines[0].staged.push(ack(1));
    driver.commit_boundary().unwrap();
    assert_eq!(
        driver.finished,
        Some((Timestamp::from_nanos(0), ExitReason::Shutdown))
    );
    assert_eq!(fixture::outputs(&driver.drain_events()).len(), 2);
}

#[test]
fn first_terminal_response_wins_and_physical_shutdown_is_only_a_fallback() {
    for stop_first in [false, true] {
        let mut driver = fixture::driver(
            Timestamp::from_nanos(0),
            Duration::from_nanos(10),
            1,
            SeededRng::new(0),
            || Ok(()),
            || Ok(vec![Park]),
        )
        .unwrap();
        let id = NonZeroU32::new(7).unwrap();
        driver.oracle_stops.insert((0, id));
        driver
            .control
            .offered(
                MachineId(0),
                id,
                crate::oracle::service::CommandKind::Application(
                    crate::pv::ApplicationCommand::Stop,
                ),
                crate::oracle::observation::Origin::External,
            )
            .unwrap();
        driver
            .control
            .accepted(
                MachineId(0),
                &mut driver.machines[0].lifecycle,
                0,
                id,
                Timestamp::from_nanos(0),
                Timestamp::from_nanos(0),
            )
            .unwrap();
        driver.machines[0].stopped = StopReason::Exited {
            reached: Timestamp::from_nanos(0),
            reason: ExitReason::Shutdown,
        };
        let outcome = Response::ApplicationOutcome { code: 23 };
        let stop = Response::CommandAcknowledged {
            id,
            result: crate::pv::CommandResult::Completed,
        };
        let responses = if stop_first {
            vec![stop, outcome]
        } else {
            vec![outcome, stop]
        };
        driver.machines[0].staged = responses
            .into_iter()
            .map(|payload| MachineResponse {
                machine: MachineId(0),
                boot: 0,
                at: Timestamp::from_nanos(0),
                payload,
            })
            .collect();
        driver.commit_boundary().unwrap();
        assert_eq!(
            driver.finished,
            Some((
                Timestamp::from_nanos(0),
                if stop_first {
                    ExitReason::Shutdown
                } else {
                    ExitReason::ProgramExit { code: 23 }
                }
            ))
        );
        assert_eq!(fixture::outputs(&driver.drain_events()).len(), 2);
    }
}
