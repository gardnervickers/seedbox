//! Guest-visible PV plans. An enabled page arm can be interrupted between any
//! of its loads/stores, so the whole plan stays fixed until a legal exit.

pub fn clock_plan(
    t0: u64,
    s0: u64,
    ns: u64,
    soft: u64,
    page_timer: bool,
    legal: bool,
) -> Option<(u64, u64)> {
    if page_timer && !legal {
        return None;
    }
    Some((soft, s0 + soft.saturating_sub(t0).div_ceil(ns).max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The kernel only tightens the bound, retaining the original plan origin.
    fn arm(plan: (u64, u64), origin: (u64, u64), ns: u64, deadline: u64) -> (u64, u64) {
        if deadline >= plan.0 {
            return plan;
        }
        let left = deadline.saturating_sub(origin.0);
        (
            deadline,
            origin.1 + (left / ns + u64::from(!left.is_multiple_of(ns))).max(1),
        )
    }

    #[test]
    fn rearm_and_overdue_targets_survive_every_split() {
        for ns in [900, 937, 1000, 1100] {
            let origin = (1_010_599_858, 17);
            let soft = origin.0 + 1_000_000;
            for deadlines in [
                [origin.0 + 100_001, origin.0 + 500_003], // later re-arm must retain the tighter bound
                [origin.0 + 500_003, origin.0 + 100_001],
                [origin.0, origin.0 - 1], // left=0: target remains s0+1
            ] {
                let initial = clock_plan(origin.0, origin.1, ns, soft, true, true).unwrap();
                let expected = arm(
                    arm(initial, origin, ns, deadlines[0]),
                    origin,
                    ns,
                    deadlines[1],
                );
                for split in 0..=3 {
                    let mut plan = initial;
                    for (point, deadline) in [Some(deadlines[0]), Some(deadlines[1]), None, None]
                        .into_iter()
                        .enumerate()
                    {
                        if point == split {
                            // The host may have charged syscalls and read either
                            // raw arm. Neither permits a guest-visible replan.
                            if let Some(next) =
                                clock_plan(origin.0 + 7 * ns, origin.1 + 7, ns, soft, true, false)
                            {
                                plan = next;
                            }
                        }
                        if let Some(deadline) = deadline {
                            plan = arm(plan, origin, ns, deadline);
                        }
                    }
                    assert_eq!(
                        plan, expected,
                        "ns={ns} split={split} deadlines={deadlines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn legal_plan_matches_guest_rounding_and_feature_off_replans() {
        for t0 in [0, 987654] {
            for s0 in [0, 54321] {
                for ns in [1, 900, 937, 1000, 1100] {
                    for offset in [0, 1, ns - 1, ns, ns + 1, 16 * ns - 1, 16 * ns + 1] {
                        let soft = t0 + offset;
                        let want = arm((u64::MAX, u64::MAX), (t0, s0), ns, soft);
                        assert_eq!(clock_plan(t0, s0, ns, soft, true, true), Some(want));
                        for legal in [false, true] {
                            assert_eq!(clock_plan(t0, s0, ns, soft, false, legal), Some(want));
                        }
                    }
                }
            }
        }
    }
}
