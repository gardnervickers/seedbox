//! Retained snapshots on real HVF: versions taken, rewound to and exported in
//! one VM's process, against capture files and restores into a new VM.
//!
//! Every snapshot and rewind here also compares all of RAM with its version,
//! so a write that escaped tracking fails the test that made it.
use super::*;
use crate::machine::pv::worker::{memory::GRANULE, versions::Full};
use std::path::Path;

/// Take a version with no limit; returns its id and the RAM pages it copied.
fn snapshot(vm: &mut Machine) -> (u64, usize) {
    let taken = vm.snapshot(u64::MAX, true).unwrap().expect("no limit");
    (taken.id, taken.pages)
}

/// Run a guest that yields to its bounds through those after `from` up to `to`.
fn advance(vm: &mut Machine, from: u64, to: u64) {
    for horizon in (from + 1_000..=to).step_by(1_000) {
        let now = vm.state.now;
        vm.run(now, horizon, 1000, false, &[], Vec::new(), &[])
            .unwrap();
    }
}

/// Where two snapshot files first differ.
fn difference(a: &Path, b: &Path) -> Option<usize> {
    let (a, b) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    (a != b).then(|| {
        a.iter()
            .zip(&b)
            .position(|(a, b)| a != b)
            .unwrap_or(a.len().min(b.len()))
    })
}

/// The guest-visible results of two futex runs are the same.
fn same(run: &Run, expected: &Run, what: &str) {
    assert_eq!(run.words, expected.words, "{what}");
    assert_eq!(run.registers, expected.registers, "{what}");
    assert_eq!(run.evidence, expected.evidence, "{what}");
}

/// A version exports as the file a capture wrote at that moment, however far
/// the machine has run since and whatever versions came after; and a machine
/// rewound to it is one a capture cannot tell from the machine it was.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn an_exported_version_is_the_capture_of_its_moment() {
    let kernel = image(FUTEX);
    let mut vm = machine(&kernel);
    let root = tempfile::tempdir().unwrap();
    let file = |name: &str| root.path().join(name);
    advance(&mut vm, 0, 8_000);
    vm.capture(&file("early")).unwrap();
    vm.capture(&file("early again")).unwrap();
    assert_eq!(difference(&file("early"), &file("early again")), None);
    let (early, _) = snapshot(&mut vm);
    vm.export(early, &file("early exported")).unwrap();
    assert_eq!(difference(&file("early exported"), &file("early")), None);

    advance(&mut vm, 8_000, 16_000);
    vm.capture(&file("late")).unwrap();
    assert!(difference(&file("late"), &file("early")).is_some());
    let (late, _) = snapshot(&mut vm);
    advance(&mut vm, 16_000, 24_000);
    vm.export(early, &file("early after")).unwrap();
    assert_eq!(difference(&file("early after"), &file("early")), None);
    vm.export(late, &file("late exported")).unwrap();
    assert_eq!(difference(&file("late exported"), &file("late")), None);

    vm.rewind(early, true).unwrap();
    vm.capture(&file("early rewound")).unwrap();
    assert_eq!(difference(&file("early rewound"), &file("early")), None);
    vm.rewind(late, true).unwrap();
    vm.capture(&file("late rewound")).unwrap();
    assert_eq!(difference(&file("late rewound"), &file("late")), None);
}

/// Write tracking changes nothing the guest can see. The futex guest runs
/// with its MMU on and writes only inside exclusive pairs, so after every
/// snapshot the first write fault of tracking lands inside one; its words,
/// registers, execution chain and stopped state must be those of a run that
/// never tracked, whether snapshots come at every bound or a few.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn write_tracking_is_invisible_to_the_guest() {
    let untracked = futex_run(false);
    for every in [1, 7, 64] {
        let kernel = image(FUTEX);
        let mut vm = machine(&kernel);
        let pages = vm.ram.pages();
        let (mut taken, mut copied, mut last) = (0, 0, None);
        futex_drive(&mut vm, 0, |vm, horizon| {
            if horizon / 1_000 % every != 0 {
                return;
            }
            let (id, pages) = snapshot(vm);
            if let Some(last) = last.replace(id) {
                vm.versions.release(last).unwrap();
                copied += pages;
            }
            taken += 1;
        });
        let tracked = futex_result(&mut vm);
        eprintln!(
            "tracking: {taken} snapshots every {every} bounds copied {copied} of {pages} pages \
             after the first; {} retried stores against {} untracked",
            tracked.retries, untracked.retries
        );
        same(&tracked, &untracked, &format!("snapshots every {every}"));
        // The guest kept writing between snapshots, and each took those pages only.
        assert!(taken > 2, "every {every}: {taken} snapshots");
        assert!(
            copied > 0 && copied < pages,
            "every {every}: {copied} pages"
        );
    }
}

/// A guest rewound to a version repeats the run it made from there: to
/// versions behind and ahead of where it is, to the same one twice, and from
/// the version's exported file in a new VM, each run ends with the words,
/// registers, execution chain and stopped state of the run that never went
/// back.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn a_rewound_guest_repeats_its_run() {
    let untracked = futex_run(false);
    let kernel = image(FUTEX);
    let mut vm = machine(&kernel);
    let pages = vm.ram.pages();
    let mut versions = Vec::new();
    futex_drive(&mut vm, 0, |vm, horizon| {
        if horizon / 1_000 % 60 == 7 && core(vm, 27) == 0 {
            versions.push((snapshot(vm).0, horizon, futex_words(vm)));
        }
    });
    assert!(versions.len() >= 3, "{} versions", versions.len());
    let continued = futex_result(&mut vm);
    same(&continued, &untracked, "continued");
    let order = versions.iter().rev().chain(&versions).chain(&versions[..1]);
    for (round, &(id, horizon, words)) in order.enumerate() {
        let rewritten = vm.rewind(id, true).unwrap();
        // What changed since the version, not all of RAM.
        assert!(
            rewritten > 0 && rewritten < pages / 8,
            "round {round}: {rewritten} of {pages} pages"
        );
        assert_eq!(vm.state.now, horizon, "round {round}");
        assert_eq!(futex_words(&vm), words, "round {round}");
        assert_ne!(words, continued.words, "round {round}");
        futex_drive(&mut vm, horizon, |_, _| ());
        same(
            &futex_result(&mut vm),
            &continued,
            &format!("round {round}"),
        );
    }
    let (id, horizon, words) = versions[1];
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("exported.snapshot");
    vm.export(id, &path).unwrap();
    let inputs = inputs(&vm, &kernel);
    // HVF allows one VM per process.
    drop(vm);
    let mut restored = restore_machine(&inputs, read_snapshot(&path, &inputs).unwrap()).unwrap();
    assert_eq!(futex_words(&restored), words);
    futex_drive(&mut restored, horizon, |_, _| ());
    same(&futex_result(&mut restored), &continued, "exported");
}

/// A rewind puts back code the guest has replaced and run since. Each round
/// the patching guest calls a routine before rewriting it, so the first call
/// after a rewind runs the version's instruction while the instruction cache
/// last held a later one.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn a_rewound_guest_runs_the_code_it_had() {
    let state = |vm: &mut Machine| (core(vm, 26), core(vm, 27), evidence(vm));
    let kernel = image(PATCH);
    let mut vm = machine(&kernel);
    advance(&mut vm, 0, 8_000);
    let (version, _) = snapshot(&mut vm);
    let early = state(&mut vm);
    advance(&mut vm, 8_000, 40_000);
    let continued = state(&mut vm);
    // The routine returned each earlier round's number.
    let (rounds, sum, _) = continued;
    assert!(early.0 > 0 && rounds > early.0 && rounds < PATCH_ROUNDS);
    assert_eq!(sum, rounds * (rounds - 1) / 2);
    for round in 0..64 {
        assert_eq!(vm.rewind(version, true).unwrap(), 1, "round {round}");
        assert!(state(&mut vm) == early, "round {round}");
        advance(&mut vm, 8_000, 40_000);
        let (rounds, sum, evidence) = state(&mut vm);
        assert_eq!((rounds, sum), (continued.0, continued.1), "round {round}");
        assert!(evidence == continued.2, "round {round}");
    }
}

/// Rewinding in place keeps what only the hypervisor holds. With the timer
/// interrupt pending in the GIC, active, or deferred behind masked IRQs when
/// each version is taken, every rewound run takes each interrupt once and ends
/// as the run that never went back did; a version taken again on a rewound
/// run is, byte for byte, the one taken at that moment the first time.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn rewinds_keep_pending_active_and_masked_interrupts() {
    let end = 40 * PERIOD + PERIOD / 2;
    for (mode, name, early) in [
        (PENDING, "pending", 10 * PERIOD),
        (ACTIVE, "active", 10 * PERIOD + PERIOD / 2),
        (MASKED, "masked", 9 * PERIOD),
    ] {
        let late = early + 12 * PERIOD;
        let kernel = image(mode);
        let mut vm = machine(&kernel);
        run_to(&mut vm, early);
        let (first, _) = snapshot(&mut vm);
        run_to(&mut vm, late);
        let (second, _) = snapshot(&mut vm);
        run_to(&mut vm, end);
        let continued = finish(&mut vm);
        assert!(continued.ticks >= 30, "{name}: {} ticks", continued.ticks);
        let check = |run: Final, what: &str| {
            assert_eq!(
                (run.ticks, run.uart),
                (continued.ticks, continued.uart),
                "{name} {what}"
            );
            assert_eq!(run.evidence, continued.evidence, "{name} {what}");
        };
        for (round, from_first) in [true, false, true, true, false].into_iter().enumerate() {
            if from_first {
                vm.rewind(first, true).unwrap();
                assert_eq!(vm.state.now, early, "{name}");
                run_to(&mut vm, late);
            } else {
                vm.rewind(second, true).unwrap();
            }
            assert_eq!(vm.state.now, late, "{name}");
            run_to(&mut vm, end);
            check(finish(&mut vm), &format!("round {round}"));
        }
        let root = tempfile::tempdir().unwrap();
        let file = |name: &str| root.path().join(name);
        vm.rewind(first, true).unwrap();
        run_to(&mut vm, late);
        let (again, _) = snapshot(&mut vm);
        vm.export(second, &file("second")).unwrap();
        vm.export(again, &file("again")).unwrap();
        assert_eq!(difference(&file("again"), &file("second")), None, "{name}");
        run_to(&mut vm, end);
        check(finish(&mut vm), "after a later version");
        vm.rewind(again, true).unwrap();
        run_to(&mut vm, end);
        check(finish(&mut vm), "from the later version");

        vm.export(first, &file("first")).unwrap();
        let inputs = inputs(&vm, &kernel);
        // HVF allows one VM per process.
        drop(vm);
        let mut restored =
            restore_machine(&inputs, read_snapshot(&file("first"), &inputs).unwrap()).unwrap();
        run_to(&mut restored, late);
        run_to(&mut restored, end);
        check(finish(&mut restored), "exported");
    }
}

/// A snapshot the store has no room for is refused whole: the versions held
/// stay as they were, the pages it would have copied are still known to be
/// written, and releasing a version frees the pages only it held.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn a_full_store_keeps_what_it_holds() {
    let page = GRANULE as u64;
    let kernel = image(FUTEX);
    let mut vm = machine(&kernel);
    assert!(matches!(vm.snapshot(0, true).unwrap(), Err(Full)));
    assert_eq!(vm.versions.held(), 0);
    advance(&mut vm, 0, 4_000);
    let (first, copied) = snapshot(&mut vm);
    let held = vm.versions.held();
    // Zero pages are not stored.
    assert!(copied > 0 && copied < vm.ram.pages(), "{copied} pages");
    assert_eq!(held, copied as u64 * page);
    let words = futex_words(&vm);

    advance(&mut vm, 4_000, 8_000);
    let later = futex_words(&vm);
    assert_ne!(later, words);
    assert!(matches!(vm.snapshot(held, true).unwrap(), Err(Full)));
    assert_eq!(vm.versions.held(), held);
    let (second, copied) = snapshot(&mut vm);
    assert!(copied > 0, "the refused snapshot forgot the written pages");
    assert_eq!(vm.versions.held(), held + copied as u64 * page);

    assert_eq!(vm.rewind(first, true).unwrap(), copied);
    assert_eq!(futex_words(&vm), words);
    assert_eq!(vm.rewind(second, true).unwrap(), copied);
    assert_eq!(futex_words(&vm), later);
    vm.versions.release(first).unwrap();
    assert_eq!(vm.versions.held(), held);
    assert_eq!(
        vm.rewind(first, true).unwrap_err(),
        "PV unknown retained snapshot"
    );
    assert_eq!(
        vm.versions.release(first).unwrap_err(),
        "PV unknown retained snapshot"
    );
    // The refused rewind left the machine as it was.
    advance(&mut vm, 8_000, 12_000);
    assert_ne!(futex_words(&vm), later);
    assert_eq!(vm.rewind(second, true).unwrap(), copied);
    assert_eq!(futex_words(&vm), later);
}
