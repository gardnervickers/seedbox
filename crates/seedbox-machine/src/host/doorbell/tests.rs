use super::*;
use seedbox_test_time::HangGuard;
use std::time::Duration;

/// Run `body` on its own thread, so a lost wakeup fails the test instead of
/// hanging it.
fn bounded<T: Send + 'static>(reason: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        sender.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)))
    });
    HangGuard::new(reason, Duration::from_secs(120))
        .recv(&receiver)
        .unwrap()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn doorbell() -> Doorbell {
    Doorbell::create(SLOTS).unwrap()
}
fn claim(bell: &Doorbell) -> Coordinator<'_> {
    bell.claim().unwrap().unwrap()
}

/// Wait until a parking coordinator has named its ticket in `watch` and set
/// its mark to it, as an executor would find them.
fn armed(bell: &Doorbell, slot: usize, watch: &AtomicU64) -> u64 {
    HangGuard::new("the coordinator arms its park", Duration::from_secs(60))
        .wait_for(|| {
            let ticket = watch.load(SeqCst);
            (ticket != 0 && bell.words.asleep(slot).load(SeqCst) == ticket).then_some(ticket)
        })
        .unwrap()
}

/// Park on `watch` alone, with no executor pipe.
fn park(coordinator: &mut Coordinator<'_>, watch: &AtomicU64) -> Wake {
    coordinator
        .park(|ticket| {
            watch.store(ticket, SeqCst);
            Some(vec![])
        })
        .unwrap()
}

#[test]
fn a_failed_creation_is_tried_again_and_a_created_doorbell_is_kept() {
    let cell = OnceLock::new();
    let failed = Doorbell::get_or_create(&cell, || Err(io::Error::other("no descriptors")));
    assert_eq!(failed.err().unwrap().to_string(), "no descriptors");
    assert!(cell.get().is_none(), "a failure is not kept");
    let created = Doorbell::get_or_create(&cell, || Doorbell::create(SLOTS)).unwrap();
    let kept = Doorbell::get_or_create(&cell, || panic!("created once")).unwrap();
    assert!(std::ptr::eq(created, kept));
}

#[test]
fn a_ring_after_the_mark_is_seen_on_the_word() {
    let bell = doorbell();
    let ringer = bell.ringer().unwrap();
    let mut coordinator = claim(&bell);
    coordinator.mark();
    assert!(!coordinator.rung());
    ringer.ring(&AtomicU64::new(0)).unwrap();
    assert!(coordinator.rung());
    coordinator.mark();
    assert!(!coordinator.rung());
}

#[test]
fn each_coordinator_holds_its_own_slot_until_it_drops() {
    let bell = Doorbell::create(2).unwrap();
    let first = claim(&bell);
    let second = claim(&bell);
    assert_ne!(first.slot, second.slot);
    assert!(bell.claim().unwrap().is_none(), "every slot is taken");
    drop(first);
    assert!(bell.claim().unwrap().is_some());
}

/// Park `parks` times, each ended by a ring from another thread that waits
/// only for the park's ticket to be named.
fn park_and_ring(bell: &Doorbell, parks: u64) {
    let mut coordinator = claim(bell);
    for _ in 0..parks {
        coordinator.mark();
        let ringer = bell.ringer().unwrap();
        let watch = AtomicU64::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while watch.load(SeqCst) == 0 {
                    std::hint::spin_loop();
                }
                ringer.ring(&watch).unwrap()
            });
            assert_eq!(park(&mut coordinator, &watch), Wake::Published);
        });
    }
    assert_eq!(coordinator.woken(), parks);
    assert_eq!(take(&coordinator.forwarded).unwrap(), None);
}

#[test]
fn every_park_ends_at_a_ring_from_another_thread() {
    bounded("each ring wakes the parked coordinator", || {
        let bell = doorbell();
        park_and_ring(&bell, 2000);
        assert_eq!(take(&bell.read).unwrap(), None, "every ticket was taken");
    });
}

#[test]
fn coordinators_sharing_a_doorbell_each_wake_at_their_own_rings() {
    bounded("each coordinator's rings wake it", || {
        let bell = doorbell();
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| park_and_ring(&bell, 1000));
            }
        });
        assert_eq!(take(&bell.read).unwrap(), None, "every ticket was taken");
    });
}

#[test]
fn a_ticket_another_coordinator_read_wakes_its_owner() {
    bounded("a forwarded ticket ends the park", || {
        let bell = doorbell();
        let mut owner = claim(&bell);
        let slot = owner.slot;
        owner.mark();
        let watch = AtomicU64::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // An executor takes the mark, and another coordinator reads
                // the ticket it wrote from the shared pipe.
                let ticket = armed(&bell, slot, &watch);
                assert_eq!(bell.words.asleep(slot).swap(0, SeqCst), ticket);
                bell.forward(ticket);
            });
            assert_eq!(park(&mut owner, &watch), Wake::Published);
        });
    });
}

#[test]
fn a_parked_coordinator_forwards_a_ticket_that_names_another() {
    bounded("the ticket reaches the coordinator it names", || {
        let bell = doorbell();
        let other = claim(&bell);
        let mut parked = claim(&bell);
        let slot = parked.slot;
        parked.mark();
        // A late payment to the other coordinator, which is not parked.
        let late = (1 << 40) | other.slot as u64;
        let watch = AtomicU64::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                armed(&bell, slot, &watch);
                let mut pipe = std::fs::File::from(bell.write.try_clone().unwrap());
                pipe.write_all(&late.to_ne_bytes()).unwrap();
                let guard = HangGuard::new("the late ticket is forwarded", Duration::from_secs(60));
                let forwarded = guard.wait_for(|| take(&other.forwarded).unwrap());
                assert_eq!(forwarded.unwrap(), late);
                // Forwarding did not end the park; its own ring does.
                bell.ringer().unwrap().ring(&watch).unwrap();
            });
            assert_eq!(park(&mut parked, &watch), Wake::Published);
        });
        assert_eq!(parked.woken(), 1);
    });
}

#[test]
fn a_park_ends_when_a_watched_executor_exits() {
    let bell = doorbell();
    let (read, write) = std::io::pipe().unwrap();
    let (live, _keep) = std::io::pipe().unwrap();
    bounded("a hang-up ends the park", move || {
        let mut coordinator = claim(&bell);
        coordinator.mark();
        let asleep = bell.words.asleep(coordinator.slot);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                // Simulate an executor dying after taking the wakeup debt
                // but before it can write the ticket.
                while asleep.swap(0, SeqCst) == 0 {
                    std::hint::spin_loop();
                }
                drop(write);
            });
            let fired = coordinator
                .park(|_| Some(vec![live.as_raw_fd(), read.as_raw_fd()]))
                .unwrap();
            assert_eq!(fired, Wake::Exited(1));
        });
        assert_eq!(coordinator.woken(), 0);
    });
}

#[test]
fn an_executor_inherits_the_doorbell_at_its_descriptors() {
    const CHILD: &str = "SEEDBOX_TEST_INHERITED_DOORBELL";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: this test's parent passes both owned descriptors.
        let ringer = unsafe { Ringer::inherited() }.unwrap();
        ringer.ring(&AtomicU64::new(0)).unwrap();
        return;
    }
    let bell = doorbell();
    let mut coordinator = claim(&bell);
    coordinator.mark();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env(CHILD, "1").args([
        "--exact",
        "host::doorbell::tests::an_executor_inherits_the_doorbell_at_its_descriptors",
    ]);
    bell.inherit(&mut command).unwrap();
    assert!(seedbox_test::process::status(&mut command)
        .unwrap()
        .success());
    assert!(coordinator.rung());
}

#[test]
fn a_ring_from_another_process_ends_the_park() {
    const CHILD: &str = "SEEDBOX_TEST_DOORBELL_RINGER";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: this test's parent passes both owned descriptors.
        let ringer = unsafe { Ringer::inherited() }.unwrap();
        // The parent names its park's ticket on stdin, where a worker reads
        // its transport's watch word.
        let mut ticket = [0; 8];
        let mut named = std::io::stdin().lock();
        named.read_exact(&mut ticket).unwrap();
        ringer
            .ring(&AtomicU64::new(u64::from_ne_bytes(ticket)))
            .unwrap();
        // Stay until the parent, awake, closes the pipe.
        assert_eq!(named.read(&mut ticket).unwrap(), 0);
        return;
    }
    bounded("the child's ring ends the park", || {
        let bell = doorbell();
        let mut coordinator = claim(&bell);
        coordinator.mark();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env(CHILD, "1")
            .args([
                "--exact",
                "host::doorbell::tests::a_ring_from_another_process_ends_the_park",
            ])
            .stdin(std::process::Stdio::piped());
        bell.inherit(&mut command).unwrap();
        let mut child = seedbox_test::process::spawn(&mut command).unwrap();
        let mut name = child.stdin.take().unwrap();
        let woken = coordinator.park(|ticket| {
            name.write_all(&ticket.to_ne_bytes()).unwrap();
            Some(vec![])
        });
        assert_eq!(woken.unwrap(), Wake::Published);
        drop(name);
        assert!(child.wait().unwrap().success());
        assert_eq!(coordinator.woken(), 1);
    });
}

#[test]
fn only_an_executor_watched_with_the_ticket_can_take_it() {
    let bell = doorbell();
    let ringer = bell.ringer().unwrap();
    let (first, second) = (claim(&bell), claim(&bell));
    let ticket = (27 << SLOT_BITS) | first.slot as u64;
    let armed = bell.words.asleep(first.slot);
    armed.store(ticket, SeqCst);
    // Watched for an earlier park of the same coordinator, for another
    // coordinator's park, and not at all.
    let earlier = (26 << SLOT_BITS) | first.slot as u64;
    let another = (27 << SLOT_BITS) | second.slot as u64;
    for watch in [earlier, another, 0] {
        ringer.ring(&AtomicU64::new(watch)).unwrap();
        assert_eq!(armed.load(SeqCst), ticket, "{watch}");
    }
    ringer.ring(&AtomicU64::new(ticket)).unwrap();
    assert_eq!(armed.load(SeqCst), 0);
    assert_eq!(take(&bell.read).unwrap(), Some(ticket));
    assert_eq!(take(&bell.read).unwrap(), None);
}
