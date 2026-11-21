use super::*;
use seedbox_machine::host::idle::SpinThenSleep;
use std::io::{PipeReader, PipeWriter};
use std::time::Duration;

type End = Stream<PipeReader, PipeWriter>;

/// How a test's ends wait: a strategy, or spin-then-sleep with this spin.
#[derive(Clone, Copy, Debug)]
enum Wait {
    Spin(Duration),
    Strategy(Idle),
}
impl Wait {
    fn build(self) -> Box<dyn IdleStrategy> {
        match self {
            Self::Spin(spin) => Box::new(SpinThenSleep::new(spin)),
            Self::Strategy(idle) => idle.build(),
        }
    }
}
/// Every way a test's ends can wait: no spin, so every wait sleeps, and each
/// built-in strategy.
fn waits() -> Vec<Wait> {
    std::iter::once(Wait::Spin(Duration::ZERO))
        .chain(Idle::ALL.map(Wait::Strategy))
        .collect()
}

/// Both ends of one region in this process, each with its own pipe.
fn pair(capacity: u64, spin: Duration) -> (End, End) {
    pair_waiting(capacity, Wait::Spin(spin))
}
fn pair_waiting(capacity: u64, wait: Wait) -> (End, End) {
    let (region, descriptor) = Region::create(capacity).unwrap();
    let peer = Region::map(&descriptor, capacity).unwrap();
    let (client_sleep, worker_wake) = std::io::pipe().unwrap();
    let (worker_sleep, client_wake) = std::io::pipe().unwrap();
    (
        Stream::new(
            region,
            Side::Client,
            client_sleep,
            client_wake,
            wait.build(),
        ),
        Stream::new(peer, Side::Worker, worker_sleep, worker_wake, wait.build()),
    )
}

fn message(length: usize, seed: u8) -> Vec<u8> {
    (0..length).map(|i| seed.wrapping_add(i as u8)).collect()
}

fn send(end: &mut End, bytes: &[u8]) {
    end.write_all(&(bytes.len() as u32).to_le_bytes()).unwrap();
    end.write_all(bytes).unwrap();
    end.flush().unwrap();
}

fn receive(end: &mut End) -> Vec<u8> {
    let mut length = [0; 4];
    end.read_exact(&mut length).unwrap();
    let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
    end.read_exact(&mut bytes).unwrap();
    bytes
}

/// Run `body` on its own thread, so a lost wakeup fails the test instead of
/// hanging it.
fn bounded<T: Send + 'static>(reason: &str, body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        sender.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)))
    });
    seedbox_test_time::HangGuard::new(reason, Duration::from_secs(120))
        .recv(&receiver)
        .unwrap()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// The worker echoes each message and the client checks every echo.
fn echo(capacity: u64, wait: Wait, lengths: Vec<usize>) -> (End, End) {
    bounded("every message echoed", move || {
        let (mut client, mut worker) = pair_waiting(capacity, wait);
        let count = lengths.len();
        let echo = std::thread::spawn(move || {
            for _ in 0..count {
                let bytes = receive(&mut worker);
                send(&mut worker, &bytes);
            }
            worker
        });
        for (seed, &length) in lengths.iter().enumerate() {
            let bytes = message(length, seed as u8);
            send(&mut client, &bytes);
            assert_eq!(receive(&mut client), bytes, "{length}");
        }
        (client, echo.join().unwrap())
    })
}

/// No wakeup is left unread in `end`'s pipe.
fn no_pending_wakeup(end: &mut End) {
    let fd = end.sleep.as_raw_fd();
    // SAFETY: F_SETFL on a pipe this test owns.
    assert!(unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
    let error = end.sleep.read(&mut [0]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn messages_round_trip_through_small_and_wrapping_rings() {
    let lengths = vec![0, 1, 59, 60, 61, 64, 65, 200, 4096, 100_000];
    // Every strategy delivers the same bytes in the same order.
    for wait in waits() {
        echo(64, wait, lengths.clone());
        echo(CAPACITY, wait, lengths.clone());
    }
}

#[test]
fn every_sleep_takes_exactly_one_wakeup() {
    // Without spinning every wait sleeps, so marks often race with progress.
    let lengths: Vec<usize> = (0..20_000).map(|i| i % 97).collect();
    let (mut client, mut worker) = echo(64, Wait::Spin(Duration::ZERO), lengths);
    no_pending_wakeup(&mut client);
    no_pending_wakeup(&mut worker);
    for ring in [Ring::Requests, Ring::Responses] {
        for mark in [READER_ASLEEP, WRITER_ASLEEP] {
            assert_eq!(client.region.word(ring.word(mark)).load(SeqCst), 0);
        }
    }
}

#[test]
fn a_closed_peer_ends_reads_and_breaks_writes() {
    bounded("reads and writes end at a closed peer", || {
        for wait in waits() {
            // Published bytes stay readable; then EOF.
            let (mut client, mut worker) = pair_waiting(64, wait);
            worker.write_all(&[1, 2, 3]).unwrap();
            worker.flush().unwrap();
            drop(worker);
            let mut bytes = [0; 8];
            assert_eq!(client.read(&mut bytes).unwrap(), 3);
            assert_eq!(bytes[..3], [1, 2, 3]);
            assert_eq!(client.read(&mut bytes).unwrap(), 0);

            // A writer waiting for room learns that the reader is gone.
            let (mut client, worker) = pair_waiting(64, wait);
            drop(worker);
            let error = client.write_all(&[0; 65]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
    })
}

#[test]
fn bytes_a_peer_published_before_exiting_stay_readable() {
    // The peer publishes and claims the sleeping reader's mark, then exits
    // before its wakeup byte: the reader still gets the bytes, then EOF.
    let (mut client, mut worker) = pair(64, Duration::ZERO);
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = [0; 8];
        let count = client.read(&mut bytes).unwrap();
        let first = bytes[..count].to_vec();
        sender.send((first, client.read(&mut bytes).unwrap()))
    });
    worker.write_all(&[1, 2, 3]).unwrap();
    let guard =
        seedbox_test_time::HangGuard::new("reader sleeps, then reads", Duration::from_secs(30));
    let mark = worker.region.word(Ring::Responses.word(READER_ASLEEP));
    guard
        .wait_for(|| (mark.load(SeqCst) != 0).then_some(()))
        .unwrap();
    worker
        .region
        .word(Ring::Responses.word(WRITTEN))
        .store(3, SeqCst);
    mark.swap(0, SeqCst);
    drop(worker);
    assert_eq!(guard.recv(&receiver).unwrap(), (vec![1, 2, 3], 0));
}

fn assert_corrupt(result: io::Result<usize>) {
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn positions_outside_the_ring_are_errors_not_reads() {
    // The writer claims more than a whole ring.
    let (mut client, worker) = pair(64, SpinThenSleep::SPIN);
    let written = worker.region.word(Ring::Responses.word(WRITTEN));
    written.store(65, SeqCst);
    assert_corrupt(client.read(&mut [0; 8]));

    // The writer's position moves back behind bytes already read.
    let (mut client, mut worker) = pair(64, SpinThenSleep::SPIN);
    worker.write_all(&[0; 8]).unwrap();
    worker.flush().unwrap();
    assert_eq!(client.read(&mut [0; 8]).unwrap(), 8);
    let written = worker.region.word(Ring::Responses.word(WRITTEN));
    written.store(4, SeqCst);
    assert_corrupt(client.read(&mut [0; 8]));

    // The reader claims bytes written but not yet published.
    let (mut client, worker) = pair(64, SpinThenSleep::SPIN);
    client.write_all(&[0; 64]).unwrap();
    let read = worker.region.word(Ring::Requests.word(READ));
    read.store(1, SeqCst);
    assert_corrupt(client.write(&[0]));

    // The reader's position moves back behind room already reused.
    let (mut client, worker) = pair(64, SpinThenSleep::SPIN);
    client.write_all(&[0; 64]).unwrap();
    client.flush().unwrap();
    let read = worker.region.word(Ring::Requests.word(READ));
    read.store(64, SeqCst);
    client.write_all(&[0; 64]).unwrap();
    read.store(32, SeqCst);
    assert_corrupt(client.write(&[0]));
}

#[test]
fn only_powers_of_two_within_bounds_are_capacities() {
    for capacity in [0, 32, 100, 1 << 31] {
        assert!(Region::create(capacity).is_err(), "{capacity}");
    }
    assert!(Region::create(64).is_ok());
}

#[test]
fn a_claimed_message_is_published_whole_at_its_commit() {
    let (mut client, mut worker) = pair(1 << 10, Duration::ZERO);
    let bytes = message(300, 7);
    let mut claim = client.claim().unwrap();
    claim
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .unwrap();
    claim.flush().unwrap();
    claim.write_all(&bytes).unwrap();
    claim.flush().unwrap();
    assert!(!worker.readable(), "nothing is visible before the commit");
    claim.commit().unwrap();
    assert!(worker.readable());
    assert_eq!(receive(&mut worker), bytes);
    no_pending_wakeup(&mut worker);
}

#[test]
fn a_claim_larger_than_the_ring_streams_through_it() {
    for wait in waits() {
        let (mut client, mut worker) = pair_waiting(64, wait);
        let bytes = message(1000, 3);
        let reader = std::thread::spawn(move || {
            let got = receive(&mut worker);
            (got, worker)
        });
        let sent = bytes.clone();
        let (got, worker) = bounded("an oversized claim", move || {
            let mut claim = client.claim().unwrap();
            claim.write_all(&(sent.len() as u32).to_le_bytes()).unwrap();
            claim.write_all(&sent).unwrap();
            claim.commit().unwrap();
            client.finish_send().unwrap();
            let received = reader.join().unwrap();
            drop(client);
            received
        });
        assert_eq!(got, bytes, "{wait:?}");
        drop(worker);
    }
}

#[test]
fn polling_fragments_preserves_a_wrapped_header_and_large_trailer() {
    let (mut client, mut worker) = pair(64, Duration::ZERO);
    let mut prefix = Vec::new();
    for len in [63, 257, 4096, 7] {
        let payload = message(len, 19);
        let mut claim = client.claim().unwrap();
        claim.header(&payload).unwrap();
        claim.write_all(&payload).unwrap();
        claim.commit().unwrap();
        // The peer takes one available interval per pass. Neither side has
        // a helper thread: a blocking operation here cannot make progress.
        while !worker.poll_message(&mut prefix).unwrap() {
            client.advance_send().unwrap();
        }
        let mut input = prefix.as_slice().chain(&mut worker);
        let header: Vec<u8> = protocol::receive(&mut input).unwrap().unwrap();
        assert_eq!(header, payload);
        let mut received = vec![0; len];
        input.read_exact(&mut received).unwrap();
        assert_eq!(received, payload);
        prefix.clear();
    }
}

#[test]
fn polling_a_dead_worker_observes_eof_without_parking() {
    bounded("a polling client observes the worker's pipe hangup", || {
        for bytes in [0, 96] {
            let (mut client, mut worker) = pair_waiting(64, Wait::Strategy(Idle::BusySpin));
            if bytes > 0 {
                let mut claim = worker.claim().unwrap();
                claim.write_all(&vec![7; bytes]).unwrap();
                claim.commit().unwrap();
            }
            drop(worker);
            let mut prefix = Vec::new();
            loop {
                match client.poll_message(&mut prefix) {
                    Ok(false) => continue,
                    Ok(true) => panic!("no complete reply was published"),
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                        assert_eq!(prefix, vec![7; bytes.min(64)]);
                        break;
                    }
                }
            }
        }
    });
}
