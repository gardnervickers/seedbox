use super::*;

#[test]
fn public_bounded_prepare_retries_are_pure_and_commit_binds_the_exact_proposal() {
    for workers in [1, 4] {
        let (_root, mut worker, runs) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        assert!(worker.state.world.readiness.latched);
        assert_eq!(
            playback(
                &mut worker,
                1,
                Command::SetRate {
                    rate: host::PlaybackRate::Paused
                }
            )
            .error,
            None
        );
        let before = fingerprint(&mut worker);
        let calls = runs.load(Ordering::SeqCst);
        let action = seedbox_engine_protocol::control::Action::ClockDrift {
            handle: "caller-alias".into(),
            machine: "n1".into(),
            rate_ppb: None,
            max_rate_ppb: Some(100_000_000),
        };
        let command = Command::PrepareFault {
            request_id: "public-bound".into(),
            action: action.clone(),
            timeout_ns: 5_000_000_000,
        };
        let first = worker
            .prepare_request(&request(2, command.clone()))
            .unwrap();
        assert_eq!(first.error, None);
        let intervention = first.intervention.unwrap();
        assert_eq!(intervention.plan.action, action);
        assert_eq!(intervention.plan.id, 1);
        for id in 3..6 {
            let again = worker
                .prepare_request(&request(id, command.clone()))
                .unwrap();
            assert_eq!(again.error, None);
            assert_eq!(again.intervention.as_ref(), Some(&intervention));
            assert_eq!(fingerprint(&mut worker), before);
            assert_eq!(runs.load(Ordering::SeqCst), calls);
        }
        let blocked = worker
            .prepare_request(&request(
                6,
                Command::PrepareFault {
                    request_id: "other".into(),
                    action: seedbox_engine_protocol::control::Action::Crash {
                        machine: "n1".into(),
                    },
                    timeout_ns: 5_000_000_000,
                },
            ))
            .unwrap();
        assert_eq!(blocked.error, Some(ControlError::FaultPending));
        assert_eq!(fingerprint(&mut worker), before);
        let committed = worker
            .commit(&request(
                7,
                Command::CommitFault {
                    request_id: "public-bound".into(),
                },
            ))
            .unwrap();
        assert_eq!(committed.error, None);
        assert_eq!(committed.intervention.as_ref(), Some(&intervention));
        assert_eq!(worker.state.world.input_program.next, 1);
        assert_eq!(
            worker.state.world.input_program.admissions[0].plan,
            intervention.plan
        );
    }
}
#[test]
fn public_authored_clock_draw_heal_draw_runs_in_order_without_a_service_alias_table() {
    let mut results = vec![];
    for workers in [1, 4] {
        let (_root, mut worker, _) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        let draw = seedbox_engine_protocol::control::Action::ClockDrift {
            handle: "caller-alias".into(),
            machine: "n1".into(),
            rate_ppb: None,
            max_rate_ppb: Some(100_000_000),
        };
        worker.state.world.input_program.inputs = [
            ("draw-1", draw.clone()),
            (
                "heal",
                seedbox_engine_protocol::control::Action::Heal {
                    handle: "caller-alias".into(),
                },
            ),
            ("draw-2", draw),
        ]
        .into_iter()
        .map(
            |(key, action)| seedbox_engine_protocol::input_program::TimedInput {
                key: key.into(),
                at_ns: 10,
                timeout_ns: 5_000_000_000,
                action,
            },
        )
        .collect();
        for next in 1..=3 {
            assert!(worker.authored_next().unwrap());
            assert_eq!(worker.state.world.input_program.next, next);
            assert_eq!(worker.playback().unwrap().position.at.as_nanos(), 10);
        }
        assert!(!worker.authored_next().unwrap());
        assert!(worker.state.world.input_program.rejections.is_empty());
        assert_eq!(
            worker
                .state
                .world
                .input_program
                .admissions
                .iter()
                .map(|row| row.plan.id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        worker.state.world.input_program.validate_at(10).unwrap();
        results.push(worker.publisher.published.clone());
    }
    assert!(!results[0].is_empty());
    assert_eq!(results[0], results[1]);
}
