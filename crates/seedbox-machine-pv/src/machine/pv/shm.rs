//! Shared-memory transport for the private worker protocol.
//!
//! The client creates one region per worker, passes it to the worker at spawn
//! as [`DESCRIPTOR`] and attaches it with its first request on the pipes. From
//! then on every protocol byte, framed exactly as it was on the pipes, goes
//! through two byte rings in the region, one per direction. The pipes carry
//! only wakeups, and their EOF still means the peer exited.
//!
//! A side waiting for bytes or room may spin for a bounded time, then marks
//! that wait in the ring and blocks reading one byte from its pipe. A side that
//! publishes bytes or frees room and finds the matching mark clears it and
//! writes that byte, so every sleep takes exactly one wakeup.
// While the peer answers within the spin, a call costs a cache-line handoff
// instead of two cross-process wakeups and several pipe syscalls. The spin
// never yields: a thread that yields without marking its wait is owed no
// wakeup, so on a busy host it can stay descheduled long after its peer
// publishes, while a thread asleep on its pipe wakes when the byte lands.
// A spin pays only while the peer runs on another core. On a host with no idle
// core the spinner holds the core its peer needs and its spins run out, so
// after FUTILE such spins in a row a side sleeps at once, spinning again only
// on every PROBE-th wait to notice a core coming free.
use super::protocol::{self, Response};
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
/// How long a waiting side spins before it sleeps. Most waits for a guest run
/// or for the client's next call end within it.
const SPIN: Duration = Duration::from_micros(200);
/// Spins in a row that ran out before a side stops spinning, and how often a
/// side that stopped spins anyway.
const FUTILE: u32 = 4;
const PROBE: u64 = 64;
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
        let descriptor =
            anonymous(Self::length(capacity)?).map_err(|e| format!("PV transport region: {e}"))?;
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

/// An unnamed shared-memory object of `length` zero bytes, close-on-exec.
#[cfg(target_os = "linux")]
fn anonymous(length: usize) -> io::Result<OwnedFd> {
    // SAFETY: a static name and flags; the result is checked.
    let fd = unsafe { libc::memfd_create(c"seedbox-pv-transport".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new descriptor.
    resize(unsafe { OwnedFd::from_raw_fd(fd) }, length)
}
/// macOS has no memfd: open a uniquely named object and unlink it at once.
#[cfg(target_os = "macos")]
fn anonymous(length: usize) -> io::Result<OwnedFd> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        // Short enough for macOS's 31-byte limit on these names.
        let name = std::ffi::CString::new(format!(
            "/sbx-pv.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
        .map_err(io::Error::other)?;
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL;
        // SAFETY: a valid name; the mode is passed as the variadic argument.
        let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(error);
        }
        // SAFETY: shm_open returned a new descriptor.
        let descriptor = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: a valid name; the object lives on through its descriptor.
        unsafe { libc::shm_unlink(name.as_ptr()) };
        // POSIX already makes it close-on-exec; spawns on other threads rely on that.
        // SAFETY: F_SETFD on a descriptor this function owns.
        if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        return resize(descriptor, length);
    }
}
fn resize(descriptor: OwnedFd, length: usize) -> io::Result<OwnedFd> {
    let length = libc::off_t::try_from(length).map_err(io::Error::other)?;
    // SAFETY: ftruncate on a descriptor this function owns.
    if unsafe { libc::ftruncate(descriptor.as_raw_fd(), length) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(descriptor)
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

/// The worker's end of start-up: map the region the client passed, answer its
/// attach request on `wake`, then serve through the region.
pub(crate) fn accept<R: Read, W: Write>(
    capacity: u64,
    sleep: R,
    mut wake: W,
) -> Result<Stream<R, W>> {
    let region = Region::inherited(capacity);
    let answer = match &region {
        Ok(_) => Response::Attached,
        Err(error) => Response::Error(error.clone()),
    };
    protocol::send(&mut wake, &answer)?;
    Ok(Stream::new(region?, Side::Worker, sleep, wake, SPIN))
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
        self as usize * 4 + field
    }
}
const WRITTEN: usize = 0;
const READ: usize = 1;
const READER_ASLEEP: usize = 2;
const WRITER_ASLEEP: usize = 3;

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
    spin: Duration,
    /// Waits so far, and spins in a row that ran out.
    waits: u64,
    futile: u32,
    /// Incoming bytes read, and the peer's written position as last seen.
    read: u64,
    readable: u64,
    /// Outgoing bytes written and published, and the peer's read position as
    /// last seen.
    written: u64,
    published: u64,
    drained: u64,
}
impl<R: Read, W: Write> Stream<R, W> {
    pub(super) fn new(region: Region, side: Side, sleep: R, wake: W, spin: Duration) -> Self {
        let (incoming, outgoing) = side.rings();
        Self {
            region,
            incoming,
            outgoing,
            sleep,
            wake,
            spin,
            waits: 0,
            futile: 0,
            read: 0,
            readable: 0,
            written: 0,
            published: 0,
            drained: 0,
        }
    }
    /// The client's end of an attached region.
    pub(super) fn client(region: Region, sleep: R, wake: W) -> Self {
        Self::new(region, Side::Client, sleep, wake, SPIN)
    }
    /// Wait until control word `position` differs from `seen`: perhaps spin,
    /// then set the `mark` word and sleep on this side's pipe. Returns the new
    /// position, or `None` if the peer exited without moving it.
    fn wait(&mut self, position: usize, seen: u64, mark: usize) -> io::Result<Option<u64>> {
        let position = self.region.word(position);
        self.waits += 1;
        if self.futile < FUTILE || self.waits.is_multiple_of(PROBE) {
            let start = Instant::now();
            loop {
                for _ in 0..64 {
                    let now = position.load(SeqCst);
                    if now != seen {
                        self.futile = 0;
                        return Ok(Some(now));
                    }
                    std::hint::spin_loop();
                }
                if start.elapsed() >= self.spin {
                    break;
                }
            }
            self.futile = self.futile.saturating_add(1);
        }
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
                // The peer can only drain published bytes.
                self.flush()?;
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
        if self.published != self.written {
            let ring = self.outgoing;
            self.region
                .word(ring.word(WRITTEN))
                .store(self.written, SeqCst);
            self.published = self.written;
            self.wake(ring.word(READER_ASLEEP))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
