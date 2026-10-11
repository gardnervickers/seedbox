//! Host policy for the guest's shared time page, independent of KVM entry.
use super::{page_entropy, pv::throttled_ns, pv_plan::clock_plan, *};

pub(super) trait Fields {
    fn get(&self, index: usize) -> u64;
    fn set(&mut self, index: usize, value: u64);
}
impl Fields for Machine {
    fn get(&self, index: usize) -> u64 {
        self.page_get(index)
    }
    fn set(&mut self, index: usize, value: u64) {
        self.page_set(index, value);
    }
}

pub(super) fn enable_timer(page: &mut impl Fields, requested: bool, abi: u64) -> bool {
    let enabled = requested && abi >= ABI_PAGE_TIMER;
    if enabled {
        page.set(PAGE_TIMER_DEADLINE, TIMER_DISARMED);
        page.set(PAGE_CAPABILITIES, CAP_PAGE_TIMER);
    }
    enabled
}
pub(super) fn release_mode(abi: u64, barrier: bool) -> Reseed {
    if barrier {
        Reseed::Barrier
    } else if abi < ABI_PAGE_RESEED {
        Reseed::Unsupported
    } else {
        Reseed::Page
    }
}
pub(super) fn publish_entropy(page: &mut impl Fields, entropy: &[u32; ENTROPY_WORDS]) -> u64 {
    let generation = page.get(PAGE_ENTROPY_GEN) + 1;
    for (i, value) in page_entropy(entropy).into_iter().enumerate() {
        page.set(PAGE_ENTROPY + i, value);
    }
    // The guest reads the generation first: publish it after every entropy word.
    page.set(PAGE_ENTROPY_GEN, generation);
    generation
}
// An ABI-4 arm consists of guest loads and stores. A watchdog resume must
// retain its published plan and cost until a legal exit: rewriting even an
// equivalent plan could tear those loads or relax an overdue bound.
pub(super) fn prepare(
    page: &mut impl Fields,
    time: u64,
    seeded_cost: u64,
    throttle: Option<u64>,
    soft: u64,
    timer: bool,
    legal: bool,
) -> u64 {
    let cost = match throttle {
        None => seeded_cost,
        Some(_) if timer && !legal && page.get(PAGE_SYSCALL_NS) != 0 => page.get(PAGE_SYSCALL_NS),
        Some(factor) => throttled_ns(seeded_cost, factor),
    };
    if let Some((bound, target)) =
        clock_plan(time, page.get(PAGE_SYSCALLS), cost, soft, timer, legal)
    {
        page.set(PAGE_BOUND, bound);
        page.set(PAGE_SYSCALL_TARGET, target);
        if timer {
            page.set(PAGE_SYSCALL_NS, cost);
            page.set(PAGE_SYSCALLS0, page.get(PAGE_SYSCALLS));
            page.set(PAGE_PLAN_TIME, time);
        }
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Page {
        words: [u64; 22],
        writes: Vec<usize>,
    }
    impl Page {
        fn fresh() -> Self {
            Self {
                words: [0; 22],
                writes: Vec::new(),
            }
        }
    }
    impl Fields for Page {
        fn get(&self, index: usize) -> u64 {
            self.words[index]
        }
        fn set(&mut self, index: usize, value: u64) {
            self.words[index] = value;
            self.writes.push(index);
        }
    }
    #[test]
    fn timer_negotiation_changes_only_the_separate_deadline_and_capability_words() {
        for requested in [false, true] {
            for abi in 0..=ABI_PAGE_TIMER {
                let mut page = Page::fresh();
                let active = requested && abi >= ABI_PAGE_TIMER;
                assert_eq!(enable_timer(&mut page, requested, abi), active);
                assert_eq!(
                    page.words[PAGE_CAPABILITIES],
                    if active { CAP_PAGE_TIMER } else { 0 }
                );
                assert_eq!(
                    page.words[PAGE_TIMER_DEADLINE],
                    if active { TIMER_DISARMED } else { 0 }
                );
                assert!(page
                    .writes
                    .iter()
                    .all(|&i| [PAGE_CAPABILITIES, PAGE_TIMER_DEADLINE].contains(&i)));
            }
        }
    }
    #[test]
    fn fork_mode_preserves_old_abis_and_barrier_delivery() {
        for abi in 0..=ABI_PAGE_TIMER {
            assert_eq!(release_mode(abi, true), Reseed::Barrier);
            assert_eq!(
                release_mode(abi, false),
                if abi < ABI_PAGE_RESEED {
                    Reseed::Unsupported
                } else {
                    Reseed::Page
                }
            );
        }
    }
    #[test]
    fn reseeding_preserves_clock_fields_and_publishes_all_entropy_before_generation() {
        let mut page = Page {
            words: std::array::from_fn(|i| 100 + i as u64),
            writes: vec![],
        };
        let before = page.words;
        assert_eq!(publish_entropy(&mut page, &[42; ENTROPY_WORDS]), 110);
        assert_eq!(&page.words[..9], &before[..9]);
        assert_eq!(&page.words[14..], &before[14..]);
        assert_eq!(&page.words[10..14], &page_entropy(&[42; ENTROPY_WORDS]));
        assert_eq!(page.writes, [10, 11, 12, 13, 9]);
        assert_eq!(publish_entropy(&mut page, &[43; ENTROPY_WORDS]), 111);
        assert_eq!(&page.words[10..14], &page_entropy(&[43; ENTROPY_WORDS]));
    }
    #[test]
    fn throttle_and_heal_charge_from_the_next_legal_plan() {
        let mut page = Page::fresh();
        page.words[PAGE_SYSCALLS] = 17;
        for (throttle, legal, expected) in [
            (None, true, 937),
            (Some(4000), false, 937),
            (Some(4000), true, 3748),
            (Some(1000), false, 3748),
            (Some(1000), true, 937),
        ] {
            assert_eq!(
                prepare(&mut page, 0, 937, throttle, 1_000_000, true, legal),
                expected
            );
            assert_eq!(page.words[PAGE_SYSCALL_NS], expected);
        }
    }
    #[test]
    fn watchdog_preparation_never_tears_a_guest_plan_or_adopts_a_raw_deadline() {
        let mut page = Page::fresh();
        page.words[PAGE_SYSCALLS] = 17;
        prepare(&mut page, 0, 1000, None, 100_000, true, true);
        let plan = page.words;
        for time in [0, 9999, 10000, 10293] {
            page.words[PAGE_SYSCALLS] = 24;
            for deadline in [5000, 20000, TIMER_DISARMED] {
                page.words[PAGE_TIMER_DEADLINE] = deadline;
                page.writes.clear();
                prepare(&mut page, time, 1000, None, 100_000, true, false);
                assert!(page.writes.is_empty());
                for index in [
                    PAGE_BOUND,
                    PAGE_SYSCALL_TARGET,
                    PAGE_SYSCALL_NS,
                    PAGE_SYSCALLS0,
                    PAGE_PLAN_TIME,
                ] {
                    assert_eq!(page.words[index], plan[index]);
                }
            }
        }
    }
}
