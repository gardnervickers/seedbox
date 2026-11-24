use super::*;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(n: u64, id: u32) -> Input {
    Input {
        at: at(n),
        payload: Request::Application {
            id: NonZeroU32::new(id).unwrap(),
            command: ApplicationCommand::Start,
        },
    }
}
#[test]
fn take_transfers_only_due_prefix_in_admission_order() {
    let a = input(2, 1);
    let b = input(2, 2);
    let future = input(7, 3);
    let mut pending = VecDeque::from([a.clone(), b.clone(), future.clone()]);
    {
        let mut inbox = Inbox::new(&mut pending, at(2));
        assert_eq!(inbox.front(), Some(&a));
        assert_eq!(inbox.front(), Some(&a)); // Inspection does not accept.
        assert_eq!(inbox.take(), Some(a));
        assert_eq!(inbox.take(), Some(b));
        assert_eq!(inbox.front(), None);
        assert_eq!(inbox.take(), None);
    }
    assert_eq!(pending, VecDeque::from([future.clone()]));
    assert_eq!(Inbox::new(&mut pending, at(7)).take(), Some(future));
}
#[test]
fn dropped_view_preserves_unaccepted_work_and_next_view_sees_new_admissions() {
    let first = input(0, 1);
    let after_action = input(0, 2);
    let mut pending = VecDeque::from([first.clone()]);
    {
        let inbox = Inbox::new(&mut pending, at(0));
        assert_eq!(inbox.front(), Some(&first));
    }
    assert_eq!(pending.pop_front(), Some(first)); // Caller owns unaccepted work.
                                                  // Caller processes intervening lifecycle action before this admission.
    pending.push_back(after_action.clone());
    assert_eq!(Inbox::new(&mut pending, at(0)).take(), Some(after_action));
}
#[test]
fn outbox_rejects_outside_interval_without_changing_staging() {
    let mut staged = Vec::new();
    {
        let mut outbox = Outbox::new(&mut staged, at(5), at(8));
        assert_eq!(
            outbox.emit(Output {
                at: at(4),
                payload: Response::Ready
            }),
            Err(InvalidOutputTime)
        );
        outbox
            .emit(Output {
                at: at(5),
                payload: Response::Ready,
            })
            .unwrap();
        outbox
            .emit(Output {
                at: at(8),
                payload: Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: vec![0, 255],
                },
            })
            .unwrap();
        assert_eq!(
            outbox.emit(Output {
                at: at(9),
                payload: Response::Ready
            }),
            Err(InvalidOutputTime)
        );
    }
    assert_eq!(staged.len(), 2);
    assert_eq!(staged[0].at, at(5));
    assert_eq!(staged[1].at, at(8));
    assert_eq!(
        staged[1].payload,
        Response::ApplicationOutput {
            stream: Stream::Stdout,
            bytes: vec![0, 255]
        }
    );
}
#[test]
fn zero_budget_outbox_keeps_same_time_emission_order() {
    let mut staged = Vec::new();
    {
        let mut outbox = Outbox::new(&mut staged, at(3), at(3));
        outbox
            .emit(Output {
                at: at(3),
                payload: Response::Ready,
            })
            .unwrap();
        outbox
            .emit(Output {
                at: at(3),
                payload: Response::ApplicationOutcome { code: 7 },
            })
            .unwrap();
    }
    assert_eq!(
        staged
            .iter()
            .map(|event| &event.payload)
            .collect::<Vec<_>>(),
        [&Response::Ready, &Response::ApplicationOutcome { code: 7 }]
    );
}
#[test]
fn a_timed_inbox_offers_inputs_inside_the_run_after_the_due_ones() {
    let due = input(2, 1);
    let first = input(5, 2);
    let second = input(5, 3);
    let at_bound = input(9, 4);
    let mut pending =
        VecDeque::from([due.clone(), first.clone(), second.clone(), at_bound.clone()]);
    {
        // Without a timed end nothing past the due time is offered.
        let inbox = Inbox::new(&mut pending, at(2));
        assert_eq!(inbox.timed().count(), 0);
    }
    {
        let mut inbox = Inbox::new(&mut pending, at(2)).with_timed(at(9));
        // Timed inputs are dated after the due time and before the bound.
        assert_eq!(inbox.timed().collect::<Vec<_>>(), [&first, &second]);
        // Due inputs come first.
        assert_eq!(inbox.take_timed(), None);
        assert_eq!(inbox.take(), Some(due));
        assert_eq!(inbox.take(), None);
        assert_eq!(inbox.take_timed(), Some(first));
        assert_eq!(inbox.timed().collect::<Vec<_>>(), [&second]);
    }
    // What the machine did not reach stays queued.
    assert_eq!(pending, VecDeque::from([second, at_bound]));
}
