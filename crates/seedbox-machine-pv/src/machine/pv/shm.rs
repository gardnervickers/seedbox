//! Shared-memory transport for the private worker protocol.
//!
//! The client creates one region per worker, passes it to the worker at spawn
//! as [`DESCRIPTOR`] and attaches it with its first request on the pipes. From
//! then on every protocol byte, framed exactly as it was on the pipes, goes
//! through two byte rings in the region, one per direction. The pipes carry
//! only wakeups, and their EOF still means the peer exited.
//!
//! A message is written straight into its ring and published by one commit,
//! as with an Agrona `OneToOneRingBuffer` claim: the writer
//! [claims](Stream::claim) the ring, writes the message's pieces, and
//! [commits](Claim::commit), so its peer sees and wakes for whole messages.
//! A message larger than the free room is published in pieces as room frees.
//!
//! A worker given the process's doorbell ([`seedbox_machine::host::doorbell`])
//! also rings it at every publish, so a client with runs in flight in many
//! workers can park in one place.
//!
//! A side waiting for bytes or room follows its idle strategy
//! ([`seedbox_machine::host::idle`]): it may spin for a bounded time, then marks
//! that wait in the ring and blocks reading one byte from its pipe. A side that
//! publishes bytes or frees room and finds the matching mark clears it and
//! writes that byte, so every sleep takes exactly one wakeup.
// While the peer answers within the spin, a call costs a cache-line handoff
// instead of two cross-process wakeups and several pipe syscalls. The spin
// never yields: a thread that yields without marking its wait is owed no
// wakeup, so on a busy host it can stay descheduled long after its peer
// publishes, while a thread asleep on its pipe wakes when the byte lands.
// A VM can spin across the gap between rounds. The coordinator uses a
// shorter spin. Both park sooner after repeated waits without progress.
use super::protocol::{self, Response};
use seedbox_machine::host::{
    doorbell::Ringer,
    idle::{Action, IdleStrategy, Outcome, SpinThenSleep},
};
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::Command,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering::SeqCst},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, String>;

/// The region's descriptor in the worker. Other Seedbox spawn hooks place
/// their fixed descriptors at 197 to 199 and keep sources at 200 and above.
pub(super) const DESCRIPTOR: i32 = 196;
/// Bytes in each ring; a longer message passes through in pieces.
pub(super) const CAPACITY: u64 = 256 << 10;
/// Workers wait between requests across whole Driver rounds. Park after this
/// bounded spin, or sooner when consecutive spins have found no work.
const WORKER_SPIN: Duration = Duration::from_micros(1000);
/// Control words, each on its own 128-byte line, fill the first 16 KiB. The
/// request ring follows, then the response ring.
const LINE: usize = 128;
const CONTROL: usize = 16 << 10;

/// One mapped region: the control words and both rings.
pub(super) struct Region {
    base: NonNull<u8>,
    length: usize,
    capacity: u64,
}
// SAFETY: the region is shared memory reached only through its stream, which
// one thread at a time owns; each ring has a single writer and reader.
unsafe impl Send for Region {}

impl Region {
    fn length(capacity: u64) -> Result<usize> {
        if !capacity.is_power_of_two() || !(64..=1 << 30).contains(&capacity) {
            return Err(format!("PV transport capacity {capacity}"));
        }
        Ok(CONTROL + 2 * capacity as usize)
    }
    /// A new zeroed region and the descriptor to pass to its worker.
    pub(super) fn create(capacity: u64) -> Result<(Self, OwnedFd)> {
        let descriptor = seedbox_machine::host::shared_memory(Self::length(capacity)?)
            .map_err(|e| format!("PV transport region: {e}"))?;
        Ok((Self::map(&descriptor, capacity)?, descriptor))
    }
    /// The worker's end of the region its client passed at [`DESCRIPTOR`].
    fn inherited(capacity: u64) -> Result<Self> {
        let length = Self::length(capacity)?;
        // SAFETY: F_GETFD only queries the descriptor table.
        if unsafe { libc::fcntl(DESCRIPTOR, libc::F_GETFD) } < 0 {
            return Err("PV transport descriptor absent".into());
        }
        // SAFETY: the client passed this descriptor for the worker to own. It
        // closes when this function returns; the mapping keeps the region.
        let descriptor = unsafe { OwnedFd::from_raw_fd(DESCRIPTOR) };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat is a valid out-pointer for this call.
        if unsafe { libc::fstat(descriptor.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
            return Err(format!(
                "PV transport descriptor: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: fstat succeeded and initialized stat.
        let size = unsafe { stat.assume_init() }.st_size;
        // At least the length: macOS rounds shared-memory objects up to pages.
        if !u64::try_from(size).is_ok_and(|size| size >= length as u64) {
            return Err("PV transport region smaller than its capacity".into());
        }
        Self::map(&descriptor, capacity)
    }
    fn map(descriptor: &OwnedFd, capacity: u64) -> Result<Self> {
        let length = Self::length(capacity)?;
        // SAFETY: a new shared mapping of the whole object; nothing else in
        // this process aliases it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(format!("PV transport map: {}", io::Error::last_os_error()));
        }
        Ok(Self {
            base: NonNull::new(base.cast()).ok_or("PV transport map at null")?,
            length,
            capacity,
        })
    }
    fn word(&self, index: usize) -> &AtomicU64 {
        // SAFETY: control words are aligned, inside the mapping and only ever
        // accessed atomically; the mapping lives as long as self.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(index * LINE).cast()) }
    }
    /// The first data byte of `ring`.
    fn data(&self, ring: Ring) -> *mut u8 {
        // SAFETY: both rings lie inside the mapping.
        unsafe {
            self.base
                .as_ptr()
                .add(CONTROL + ring as usize * self.capacity as usize)
        }
    }
}
impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping this value owns.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.length) };
    }
}

/// Give the child that `command` spawns `descriptor` as [`DESCRIPTOR`].
pub(super) fn inherit(descriptor: OwnedFd, command: &mut Command) -> Result<()> {
    // A source above every fixed child descriptor cannot be replaced by another
    // hook's dup2 before this one runs.
    // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor or fails.
    let fd = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
    if fd < 0 {
        return Err(format!(
            "PV transport descriptor: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: fcntl returned a new descriptor, which the closure below owns.
    let source = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: dup2 is async-signal-safe, and the closure owns its source until
    // the Command drops.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source.as_raw_fd(), DESCRIPTOR) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(())
}

/// The worker's end of start-up: map the region the client passed, and the
/// process doorbell if it passed one, answer its attach request on `wake`,
/// then serve through the region.
pub(crate) fn accept<R: Read, W: Write>(
    capacity: u64,
    sleep: R,
    mut wake: W,
) -> Result<Stream<R, W>> {
    let attached = Region::inherited(capacity).and_then(|region| {
        // SAFETY: accept is the worker entry's one attach handshake.
        let bell = unsafe { Ringer::inherited() }.map_err(|e| format!("PV doorbell: {e}"))?;
        Ok((region, bell))
    });
    let answer = match &attached {
        Ok(_) => Response::Attached,
        Err(error) => Response::Error(error.clone()),
    };
    protocol::send(&mut wake, &answer)?;
    let (region, bell) = attached?;
    let idle = Box::new(SpinThenSleep::new(WORKER_SPIN));
    let mut stream = Stream::new(region, Side::Worker, sleep, wake, idle);
    stream.bell = Some(bell);
    Ok(stream)
}

/// Requests go from the client to the worker, responses back.
#[derive(Clone, Copy)]
enum Ring {
    Requests = 0,
    Responses = 1,
}
impl Ring {
    /// Each ring's control words: bytes published, bytes read, and the marks
    /// of a reader waiting for bytes and a writer waiting for room.
    fn word(self, field: usize) -> usize {
        self as usize * 5 + field
    }
}
const WRITTEN: usize = 0;
const READ: usize = 1;
const READER_ASLEEP: usize = 2;
const WRITER_ASLEEP: usize = 3;
/// Absolute end of the one message in flight, including its trailer.
const MESSAGE_END: usize = 4;
/// Coordinator subscription, outside the two five-word ring headers.
const WATCH: usize = 10;

#[derive(Clone, Copy)]
pub(super) enum Side {
    Client,
    Worker,
}
impl Side {
    fn rings(self) -> (Ring, Ring) {
        match self {
            Self::Client => (Ring::Responses, Ring::Requests),
            Self::Worker => (Ring::Requests, Ring::Responses),
        }
    }
}

/// One side of the region as a byte stream. It sleeps on `sleep` and wakes
/// its peer through `wake`. Written bytes reach the peer at `flush`. Every
/// read publishes its position, so a reader of small pieces buffers it.
pub(crate) struct Stream<R, W> {
    region: Region,
    incoming: Ring,
    outgoing: Ring,
    sleep: R,
    wake: W,
    idle: Box<dyn IdleStrategy>,
    /// Incoming bytes read, and the peer's written position as last seen.
    read: u64,
    readable: u64,
    /// Outgoing bytes written and published, and the peer's read position as
    /// last seen.
    written: u64,
    published: u64,
    drained: u64,
    /// Tail of a message larger than its ring. Only the client keeps this
    /// between duty cycles. Small messages stay entirely in shared memory.
    tail: io::Cursor<Vec<u8>>,
    /// The process doorbell, rung at every publish (a worker's, if given).
    bell: Option<Ringer>,
}
impl<R: Read, W: Write> Stream<R, W> {
    pub(super) fn new(
        region: Region,
        side: Side,
        sleep: R,
        wake: W,
        idle: Box<dyn IdleStrategy>,
    ) -> Self {
        let (incoming, outgoing) = side.rings();
        Self {
            region,
            incoming,
            outgoing,
            sleep,
            wake,
            idle,
            read: 0,
            readable: 0,
            written: 0,
            published: 0,
            drained: 0,
            tail: io::Cursor::default(),
            bell: None,
        }
    }
    /// The client's end of an attached region.
    pub(super) fn client(region: Region, sleep: R, wake: W) -> Self {
        let idle = Box::new(SpinThenSleep::default());
        Self::new(region, Side::Client, sleep, wake, idle)
    }
    /// Wait until control word `position` differs from `seen`: spin as the
    /// idle strategy says, then set the `mark` word and sleep on this side's
    /// pipe. Returns the new position, or `None` if the peer exited
    /// without moving it.
    fn wait(&mut self, position: usize, seen: u64, mark: usize) -> io::Result<Option<u64>> {
        let word = self.region.word(position);
        let started = Instant::now();
        let mut attempt = 0;
        while self.idle.idle(attempt, started.elapsed()) == Action::Spin {
            for _ in 0..64 {
                let now = word.load(SeqCst);
                if now != seen {
                    self.idle.reset(Outcome::Progress);
                    return Ok(Some(now));
                }
                std::hint::spin_loop();
            }
            attempt += 1;
        }
        let slept = self.sleep(position, seen, mark);
        self.idle.reset(if attempt > 0 {
            Outcome::SleptAfterTrying
        } else {
            Outcome::SleptAtOnce
        });
        slept
    }
    /// Mark the wait in control word `mark` and sleep on this side's pipe
    /// until `position` differs from `seen`.
    fn sleep(&mut self, position: usize, seen: u64, mark: usize) -> io::Result<Option<u64>> {
        let position = self.region.word(position);
        let mark = self.region.word(mark);
        loop {
            mark.store(1, SeqCst);
            // Either this load sees the peer's progress or the peer sees the mark.
            let now = position.load(SeqCst);
            if now != seen {
                // A peer that cleared the mark first has also sent the wakeup.
                if mark.swap(0, SeqCst) == 0 {
                    doorbell(&mut self.sleep)?;
                }
                return Ok(Some(now));
            }
            if !doorbell(&mut self.sleep)? {
                // The peer exited, perhaps after publishing but before its wakeup.
                let now = position.load(SeqCst);
                return Ok((now != seen).then_some(now));
            }
        }
    }
    /// Whether a read would find bytes without waiting.
    #[cfg(test)]
    pub(super) fn readable(&self) -> bool {
        self.readable != self.read
            || self.region.word(self.incoming.word(WRITTEN)).load(SeqCst) != self.read
    }
    /// Claim the currently free interval. The peer sees none of the bytes
    /// until commit. If encoding fails the claim drops without publication.
    pub(super) fn claim(&mut self) -> io::Result<Claim<'_, R, W>> {
        let drained = self.region.word(self.outgoing.word(READ)).load(SeqCst);
        let used = self
            .written
            .checked_sub(drained)
            .filter(|&n| n <= self.region.capacity)
            .ok_or_else(corrupt)?;
        let capacity = (self.region.capacity - used) as usize;
        debug_assert!(self.tail.get_ref().is_empty());
        self.drained = drained;
        Ok(Claim {
            stream: self,
            capacity,
            used: 0,
            spill: Vec::new(),
        })
    }

    /// Encode one complete message, including its binary trailer. On error
    /// the uncommitted interval is still free for the producer.
    pub(crate) fn message(
        &mut self,
        header: &impl serde::Serialize,
        trailer: impl FnOnce(&mut Claim<'_, R, W>) -> Result<()>,
    ) -> Result<()> {
        let mut claim = self.claim().map_err(|e| e.to_string())?;
        claim.header(header)?;
        trailer(&mut claim)?;
        claim
            .commit()
            .map_err(|e| format!("PV protocol write: {e}"))
    }

    /// Copy as much of the outstanding request as the worker has made room
    /// for. A worker draining a partial request rings the same shared bell.
    pub(super) fn advance_send(&mut self) -> io::Result<()> {
        if self.tail.get_ref().is_empty() {
            return Ok(());
        }
        let mut tail = std::mem::take(&mut self.tail);
        let pos = tail.position() as usize;
        let mut claim = self.claim()?;
        let n = claim.capacity.min(tail.get_ref().len() - pos);
        claim.copy(0, &tail.get_ref()[pos..pos + n]);
        self.written += n as u64;
        self.publish()?;
        if pos + n < tail.get_ref().len() {
            tail.set_position((pos + n) as u64);
            self.tail = tail;
        }
        Ok(())
    }
    /// The synchronous caller can wait for space using the ordinary pipe.
    pub(crate) fn finish_send(&mut self) -> io::Result<()> {
        let tail = std::mem::take(&mut self.tail);
        self.write_all(&tail.get_ref()[tail.position() as usize..])?;
        self.publish()
    }
    /// Drain available fragments of an oversized reply to release space.
    /// The common path leaves the complete reply in the ring for decoding.
    pub(super) fn poll_message(&mut self, prefix: &mut Vec<u8>) -> io::Result<bool> {
        let written = self.region.word(self.incoming.word(WRITTEN)).load(SeqCst);
        if written == self.read {
            return Ok(false);
        }
        let end = self
            .region
            .word(self.incoming.word(MESSAGE_END))
            .load(SeqCst);
        if written == end {
            return Ok(true);
        }
        let available = written
            .checked_sub(self.read)
            .filter(|&n| n <= self.region.capacity && written < end)
            .ok_or_else(corrupt)? as usize;
        let start = prefix.len();
        prefix.resize(start + available, 0);
        self.read_exact(&mut prefix[start..])?;
        Ok(false)
    }
    /// Publish every written byte and wake a waiting peer.
    fn publish(&mut self) -> io::Result<()> {
        if self.published != self.written {
            let ring = self.outgoing;
            self.region
                .word(ring.word(WRITTEN))
                .store(self.written, SeqCst);
            self.published = self.written;
            self.wake(ring.word(READER_ASLEEP))?;
            if let Some(bell) = &self.bell {
                bell.ring(self.region.word(WATCH))?;
            }
        }
        Ok(())
    }
    /// The pipe this side sleeps on; it reaches EOF when the peer exits.
    pub(super) fn sleeper(&self, ticket: u64) -> &R {
        self.region.word(WATCH).store(ticket, SeqCst);
        &self.sleep
    }
    /// Wake the peer if it marked a wait in control word `mark`.
    fn wake(&mut self, mark: usize) -> io::Result<()> {
        let mark = self.region.word(mark);
        if mark.load(SeqCst) != 0 && mark.swap(0, SeqCst) != 0 {
            self.wake.write_all(&[0])?;
            self.wake.flush()?;
        }
        Ok(())
    }
}
/// Take one wakeup; false at EOF.
fn doorbell(sleep: &mut impl Read) -> io::Result<bool> {
    let mut byte = [0];
    loop {
        match sleep.read(&mut byte) {
            Ok(count) => return Ok(count == 1),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
fn corrupt() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "PV transport positions")
}
impl<R: Read, W: Write> Read for Stream<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ring = self.incoming;
        let capacity = self.region.capacity;
        if self.readable == self.read {
            let written = ring.word(WRITTEN);
            let mut now = self.region.word(written).load(SeqCst);
            if now == self.read {
                match self.wait(written, self.read, ring.word(READER_ASLEEP))? {
                    Some(position) => now = position,
                    None => return Ok(0),
                }
            }
            if now < self.read || now - self.read > capacity {
                return Err(corrupt());
            }
            self.readable = now;
        }
        let count = buf.len().min((self.readable - self.read) as usize);
        let start = (self.read % capacity) as usize;
        let first = count.min(capacity as usize - start);
        let data = self.region.data(ring);
        // SAFETY: the peer published these bytes and reuses their room only
        // after this side publishes its new read position below.
        unsafe {
            std::ptr::copy_nonoverlapping(data.add(start), buf.as_mut_ptr(), first);
            std::ptr::copy_nonoverlapping(data, buf.as_mut_ptr().add(first), count - first);
        }
        self.read += count as u64;
        self.region.word(ring.word(READ)).store(self.read, SeqCst);
        self.wake(ring.word(WRITER_ASLEEP))?;
        if let Some(bell) = &self.bell {
            // A fragmented request needs another coordinator pass to send
            // its next piece. Whole requests produce only the reply ring.
            if self.read < self.region.word(ring.word(MESSAGE_END)).load(SeqCst)
                && self.read == self.readable
            {
                bell.ring(self.region.word(WATCH))?;
            }
        }
        Ok(count)
    }
}
impl<R: Read, W: Write> Write for Stream<R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ring = self.outgoing;
        let capacity = self.region.capacity;
        if self.written - self.drained == capacity {
            let read = ring.word(READ);
            let mut now = self.region.word(read).load(SeqCst);
            if now == self.drained {
                // The peer can only drain published bytes, even of a claim.
                self.publish()?;
                match self.wait(read, self.drained, ring.word(WRITER_ASLEEP))? {
                    Some(position) => now = position,
                    None => return Err(io::ErrorKind::BrokenPipe.into()),
                }
            }
            if now < self.drained || now > self.published {
                return Err(corrupt());
            }
            self.drained = now;
        }
        let count = buf
            .len()
            .min((capacity - (self.written - self.drained)) as usize);
        let start = (self.written % capacity) as usize;
        let first = count.min(capacity as usize - start);
        let data = self.region.data(ring);
        // SAFETY: the peer has read past this room, and reads only published
        // bytes, which these are not yet.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), data.add(start), first);
            std::ptr::copy_nonoverlapping(buf.as_ptr().add(first), data, count - first);
        }
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.publish()
    }
}

/// A single producer owns both sides of the wrap point until commit. Only
/// messages larger than the free interval allocate a spill buffer.
pub(crate) struct Claim<'a, R, W> {
    stream: &'a mut Stream<R, W>,
    capacity: usize,
    used: usize,
    spill: Vec<u8>,
}
impl<R: Read, W: Write> Claim<'_, R, W> {
    fn copy(&mut self, offset: usize, bytes: &[u8]) {
        let n = bytes.len().min(self.capacity.saturating_sub(offset));
        if n > 0 {
            let stream = &self.stream;
            let size = stream.region.capacity as usize;
            let start = (stream.written as usize + offset) & (size - 1);
            let first = n.min(size - start);
            let data = stream.region.data(stream.outgoing);
            // SAFETY: the claim exclusively owns this free interval. The
            // second copy uses only its portion after the wrap point.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(start), first);
                std::ptr::copy_nonoverlapping(bytes.as_ptr().add(first), data, n - first);
            }
        }
        if n < bytes.len() {
            let at = (offset + n) - self.capacity;
            let len = bytes.len() - n;
            if at == self.spill.len() {
                self.spill.extend_from_slice(&bytes[n..]);
            } else {
                self.spill[at..at + len].copy_from_slice(&bytes[n..]);
            }
        }
    }
    /// Encode the length-prefixed JSON header directly into the claim.
    pub(super) fn header(&mut self, value: &impl serde::Serialize) -> Result<()> {
        let at = self.used;
        self.write_all(&[0; 4]).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut *self, value).map_err(|e| format!("PV encode: {e}"))?;
        let len = self.used - at - 4;
        if len > protocol::MAX_MESSAGE {
            return Err("PV protocol message too large".into());
        }
        self.copy(at, &(len as u32).to_le_bytes());
        Ok(())
    }
    pub(super) fn commit(self) -> io::Result<()> {
        let stream = self.stream;
        let end = stream.written + self.used as u64;
        stream
            .region
            .word(stream.outgoing.word(MESSAGE_END))
            .store(end, SeqCst);
        stream.written += self.used.min(self.capacity) as u64;
        stream.tail = io::Cursor::new(self.spill);
        stream.publish()
    }
}
impl<R: Read, W: Write> Write for Claim<'_, R, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.copy(self.used, buf);
        self.used += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
