use super::*;

fn op(client: u32, call: Call, invoke: u64, response: Option<(u64, Ret)>) -> Op {
    Op {
        client,
        call,
        invoke,
        response,
    }
}

/// Exhaustive reference: try every order of every subset of unknown
/// operations with all completed ones, respecting real-time order.
fn reference(initial: Option<i64>, ops: &[Op]) -> bool {
    fn search(state: Option<i64>, ops: &[Op], placed: &mut Vec<bool>) -> bool {
        let pending: Vec<usize> = (0..ops.len())
            .filter(|&i| !placed[i] && ops[i].response.is_some())
            .collect();
        if pending.is_empty() {
            return true;
        }
        for i in 0..ops.len() {
            if placed[i] {
                continue;
            }
            // Real-time order: nothing unplaced and completed may have
            // responded before this one was invoked.
            if pending
                .iter()
                .any(|&j| j != i && ops[j].response.unwrap().0 < ops[i].invoke)
            {
                continue;
            }
            let (next, ret) = step(state, ops[i].call);
            if ops[i].response.is_some_and(|(_, r)| r != ret) {
                continue;
            }
            placed[i] = true;
            if search(next, ops, placed) {
                return true;
            }
            placed[i] = false;
        }
        false
    }
    search(initial, ops, &mut vec![false; ops.len()])
}

#[test]
fn a_lost_compare_and_set_race_is_not_linearizable() {
    // Two clients both read 0, both CAS 0 -> theirs, and both succeed.
    let ops = [
        op(1, Call::Write(0), 1, Some((2, Ret::Write))),
        op(
            1,
            Call::Cas {
                expected: 0,
                new: 1,
            },
            3,
            Some((6, Ret::Cas(true))),
        ),
        op(
            2,
            Call::Cas {
                expected: 0,
                new: 2,
            },
            4,
            Some((5, Ret::Cas(true))),
        ),
        op(1, Call::Read, 7, Some((8, Ret::Read(Some(1))))),
    ];
    assert!(matches!(
        check_register(None, &ops, 10_000),
        Verdict::Violation { .. }
    ));
    // One success and one failure is fine.
    let mut fine = ops;
    fine[2].response = Some((5, Ret::Cas(false)));
    assert_eq!(check_register(None, &fine, 10_000), Verdict::Linearizable);
}

#[test]
fn unknown_outcomes_may_take_effect_late_or_never() {
    let ops = [
        op(1, Call::Write(5), 1, None),
        op(2, Call::Read, 2, Some((3, Ret::Read(None)))),
        op(2, Call::Read, 4, Some((5, Ret::Read(Some(5))))),
    ];
    assert_eq!(check_register(None, &ops, 10_000), Verdict::Linearizable);
    let never = [ops[0], ops[1]];
    assert_eq!(check_register(None, &never, 10_000), Verdict::Linearizable);
    // But a stale read after the write completed is a violation.
    let stale = [
        op(1, Call::Write(5), 1, Some((2, Ret::Write))),
        op(2, Call::Read, 3, Some((4, Ret::Read(None)))),
    ];
    assert!(matches!(
        check_register(None, &stale, 100),
        Verdict::Violation { .. }
    ));
    assert!(matches!(
        check_register(None, &stale, 1),
        Verdict::Inconclusive { .. }
    ));
}

#[test]
fn search_agrees_with_an_exhaustive_reference_on_random_histories() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    let mut violations = 0;
    for _ in 0..3000 {
        let n = 1 + next(6) as usize;
        let mut clock = 0;
        let mut open: Vec<(usize, u64)> = Vec::new();
        let mut ops = Vec::new();
        while ops.len() < n || !open.is_empty() {
            if ops.len() < n && (open.is_empty() || next(2) == 0) {
                clock += 1;
                let call = match next(3) {
                    0 => Call::Read,
                    1 => Call::Write(next(3) as i64),
                    _ => Call::Cas {
                        expected: next(3) as i64,
                        new: next(3) as i64,
                    },
                };
                ops.push(op(0, call, clock, None));
                open.push((ops.len() - 1, clock));
            } else {
                let (i, _) = open.remove(next(open.len() as u64) as usize);
                clock += 1;
                if next(5) == 0 {
                    continue; // unknown outcome
                }
                let ret = match ops[i].call {
                    Call::Read => Ret::Read(match next(4) {
                        0 => None,
                        v => Some(v as i64 - 1),
                    }),
                    Call::Write(_) => Ret::Write,
                    Call::Cas { .. } => Ret::Cas(next(2) == 0),
                };
                ops[i].response = Some((clock, ret));
            }
        }
        let expected = reference(None, &ops);
        let verdict = check_register(None, &ops, 1_000_000);
        assert_eq!(verdict == Verdict::Linearizable, expected, "{ops:?}");
        violations += usize::from(!expected);
    }
    assert!(violations > 100 && violations < 2900, "{violations}");
}

#[test]
fn recorded_histories_and_duplicates() {
    let history = History::new();
    let write = history.invoke(1, Call::Write(3));
    let read = history.invoke(2, Call::Read);
    history.respond(write, Ret::Write);
    history.respond(read, Ret::Read(Some(3)));
    let ops = history.ops();
    assert_eq!((ops[0].invoke, ops[0].response.unwrap().0), (1, 3));
    assert_eq!(check_register(None, &ops, 1000), Verdict::Linearizable);
    assert_eq!(duplicates(&["a", "b", "a", "c", "a"]), [("a", 3)]);
}
