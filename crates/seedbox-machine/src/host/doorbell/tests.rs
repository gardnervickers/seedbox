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

/// Park on `watch` alone, with no executor pipe.
fn park(coordinator: &mut Coordinator<'_>, watch: &AtomicU64) -> Wake {
    coordinator
        .park(|ticket| {
            watch.store(ticket, SeqCst);
            Some(vec![])
        })
        .unwrap()
}

/// Wait until the park that named its ticket in `watch` has taken its last
/// look at `pending`: from then on only reading that ticket can end it.
fn asleep(bell: &Doorbell, watch: &AtomicU64) -> u64 {
    HangGuard::new("the coordinator sleeps", Duration::from_secs(60))
        .wait_for(|| {
            let ticket = watch.load(SeqCst);
            (ticket != 0 && bell.sleeping() == ticket).then_some(ticket)
        })
        .unwrap()
}

/// No ticket is left unread in the doorbell's pipe.
fn no_pending_ticket(bell: &Doorbell) {
    let mut fd = libc::pollfd {
        fd: bell.read.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, zero timeout checks the pipe state.
    assert_eq!(unsafe { libc::poll(&mut fd, 1, 0) }, 0);
}

#[test]
fn a_failed_creation_is_tried_again_and_a_created_doorbell_is_kept() {
    let (cell, creating) = (OnceLock::new(), Mutex::new(()));
    let out = || Err(io::Error::other("no descriptors"));
    let failed = Doorbell::get_or_create(&cell, &creating, out);
    assert_eq!(failed.err().unwrap().to_string(), "no descriptors");
    assert!(cell.get().is_none(), "a failure is not kept");
    let created = Doorbell::get_or_create(&cell, &creating, Doorbell::create).unwrap();
    let kept = Doorbell::get_or_create(&cell, &creating, || panic!("created once")).unwrap();
    assert!(std::ptr::eq(created, kept));
}

#[test]
fn concurrent_first_callers_create_one_doorbell() {
    let (cell, creating) = (OnceLock::new(), Mutex::new(()));
    let created = AtomicU64::new(0);
    let create = || {
        created.fetch_add(1, SeqCst);
        Doorbell::create()
    };
    let bells: Vec<usize> = std::thread::scope(|scope| {
        let callers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let bell = Doorbell::get_or_create(&cell, &creating, create).unwrap();
                    std::ptr::from_ref(bell) as usize
                })
            })
            .collect();
        callers.into_iter().map(|c| c.join().unwrap()).collect()
    });
    assert_eq!(created.load(SeqCst), 1, "creations take turns and recheck");
    assert!(bells.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn a_ring_after_the_mark_is_seen_on_the_word() {
    let bell = Doorbell::create().unwrap();
    let ringer = bell.ringer().unwrap();
    let mut coordinator = bell.claim().unwrap();
    coordinator.mark();
    assert!(!coordinator.rung());
    ringer.ring(&AtomicU64::new(0)).unwrap();
    assert!(coordinator.rung());
    coordinator.mark();
    assert!(!coordinator.rung());
}

#[test]
fn one_coordinator_at_a_time() {
    let bell = Doorbell::create().unwrap();
    let first = bell.claim().unwrap();
    assert!(bell.claim().is_none());
    drop(first);
    assert!(bell.claim().is_some());
}

#[test]
fn every_park_ends_at_a_ring_from_another_thread() {
    let bell = Doorbell::create().unwrap();
    bounded("each ring wakes the parked coordinator", move || {
        let mut coordinator = bell.claim().unwrap();
        // The ring races the park: it cancels the park or pays its ticket.
        for _ in 0..2000 {
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
        no_pending_ticket(&bell);
    });
}

#[test]
fn a_park_past_its_last_look_ends_only_by_reading_its_ticket() {
    let bell = Doorbell::create().unwrap();
    bounded("each ticket wakes the sleeping coordinator", move || {
        let mut coordinator = bell.claim().unwrap();
        let parks = 200;
        for _ in 0..parks {
            coordinator.mark();
            let ringer = bell.ringer().unwrap();
            let watch = AtomicU64::new(0);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    asleep(&bell, &watch);
                    ringer.ring(&watch).unwrap()
                });
                assert_eq!(park(&mut coordinator, &watch), Wake::Published);
            });
        }
        assert_eq!(bell.tickets_read(), parks);
        no_pending_ticket(&bell);
    });
}

#[test]
fn an_executor_rings_whichever_coordinator_watches_it_now() {
    let bell = Doorbell::create().unwrap();
    bounded("each coordinator in turn is woken", move || {
        // The executor's watch word stays with it when the machine it runs
        // changes hands, as a retained worker's does when another Driver's
        // machine is rewound into it.
        let (ringer, watch) = (bell.ringer().unwrap(), AtomicU64::new(0));
        let mut tickets = Vec::new();
        for _ in 0..2 {
            let mut coordinator = bell.claim().unwrap();
            coordinator.mark();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    tickets.push(asleep(&bell, &watch));
                    ringer.ring(&watch).unwrap()
                });
                assert_eq!(park(&mut coordinator, &watch), Wake::Published);
            });
        }
        assert_ne!(tickets[0], tickets[1]);
        assert_eq!(bell.tickets_read(), 2);
    });
}

#[test]
fn a_park_ends_when_a_watched_executor_exits() {
    let bell = Doorbell::create().unwrap();
    let (read, write) = std::io::pipe().unwrap();
    let (live, _keep) = std::io::pipe().unwrap();
    bounded("a hang-up ends the park", move || {
        let mut coordinator = bell.claim().unwrap();
        coordinator.mark();
        let asleep = bell.words.word(ASLEEP);
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
        assert_eq!(bell.tickets_read(), 0);
    });
}

#[test]
fn a_cancelled_park_still_reports_a_watched_executor_that_exited() {
    let bell = Doorbell::create().unwrap();
    // Guarded from its first park: one that failed to cancel would wait on
    // pipes that are both still open.
    bounded("every cancelled park returns", move || {
        let mut coordinator = bell.claim().unwrap();
        let ringer = bell.ringer().unwrap();
        let (live, _keep) = std::io::pipe().unwrap();
        let (read, write) = std::io::pipe().unwrap();
        // An executor this park does not watch rings before every park, so
        // each park is cancelled before it would poll.
        let mut cancelled_park = || {
            coordinator.mark();
            ringer.ring(&AtomicU64::new(0)).unwrap();
            let watched = vec![live.as_raw_fd(), read.as_raw_fd()];
            coordinator.park(|_| Some(watched)).unwrap()
        };
        assert_eq!(cancelled_park(), Wake::Published);
        drop(write);
        // The hang-up shows once no forked child of another test still holds
        // a copy of the pipe; until then a cancelled park reports only the
        // ring, however many parks that takes.
        let wake = HangGuard::new("a cancelled park reports the exit", Duration::from_secs(60))
            .wait_for(|| Some(cancelled_park()).filter(|wake| *wake != Wake::Published))
            .unwrap();
        assert_eq!(wake, Wake::Exited(1));
        assert_eq!(bell.words.word(ASLEEP).load(SeqCst), 0);
        assert_eq!(bell.tickets_read(), 0);
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
    let bell = Doorbell::create().unwrap();
    let mut coordinator = bell.claim().unwrap();
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
fn a_ticket_from_another_process_wakes_the_sleeping_coordinator() {
    use std::io::{Read, Write};
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
    let bell = Doorbell::create().unwrap();
    bounded("the child's ticket ends the park", move || {
        let mut coordinator = bell.claim().unwrap();
        coordinator.mark();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env(CHILD, "1")
            .args([
                "--exact",
                "host::doorbell::tests::a_ticket_from_another_process_wakes_the_sleeping_coordinator",
            ])
            .stdin(std::process::Stdio::piped());
        bell.inherit(&mut command).unwrap();
        let mut child = seedbox_test::process::spawn(&mut command).unwrap();
        let mut name = child.stdin.take().unwrap();
        let watch = AtomicU64::new(0);
        std::thread::scope(|scope| {
            // The child learns the ticket only once the park can no longer
            // be cancelled, so its ring has to pay the ticket.
            scope.spawn(|| {
                let ticket = asleep(&bell, &watch);
                name.write_all(&ticket.to_ne_bytes()).unwrap();
                name
            });
            assert_eq!(park(&mut coordinator, &watch), Wake::Published);
        });
        assert_eq!(bell.tickets_read(), 1);
        assert!(child.wait().unwrap().success());
        no_pending_ticket(&bell);
    });
}

#[test]
fn an_unwatched_publisher_cannot_claim_the_ticket() {
    let bell = Doorbell::create().unwrap();
    let ringer = bell.ringer().unwrap();
    let armed = bell.words.word(ASLEEP);
    let ticket = 27;
    armed.store(ticket, SeqCst);
    ringer.ring(&AtomicU64::new(ticket - 1)).unwrap();
    assert_eq!(armed.load(SeqCst), ticket);
    ringer.ring(&AtomicU64::new(ticket)).unwrap();
    assert_eq!(armed.load(SeqCst), 0);
    let mut bytes = [0; 8];
    std::io::Read::read_exact(&mut &bell.read, &mut bytes).unwrap();
    assert_eq!(u64::from_ne_bytes(bytes), ticket);
}
