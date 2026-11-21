//! One place for a coordinator to park while many executors in other processes
//! run: the process's doorbell.
//!
//! The doorbell is a small shared-memory region holding two words, `pending`
//! and `asleep`, and a pipe. Every executor process spawned by this process
//! inherits the region and the pipe's write end ([`Doorbell::inherit`]). An
//! executor that publishes a reply [rings](Ringer::ring): it bumps `pending`,
//! and if it then takes the `asleep` mark it writes the ticket to the pipe.
//!
//! Before scanning the rings, the coordinator samples `pending`. To park,
//! it sets `asleep` and checks the sample again. If it sees progress it can
//! cancel only a mark no writer has taken. Once a writer takes the mark,
//! the coordinator consumes the owed ticket even if it has already observed
//! the reply. The per-worker pipes share the poll set so a worker that dies
//! between updating either word and writing the ticket cannot strand the wait.
//!
//! Only the claiming coordinator sets `asleep`, so a process has at most one
//! at a time. Another caller finds the doorbell claimed and waits in one of
//! its runs instead, on that executor's own pipe, which needs no doorbell.
//! Rings from executors the coordinator is not waiting for only wake it early.
//!
//! An executor is bound to the process's doorbell, not to a coordinator: a
//! park names itself to the executors it waits for, each time, through their
//! watch words. So an executor that comes to run for another caller rings
//! whoever watches it now, and can take no park that does not.
//!
//! The doorbell decides only when the coordinator looks; it carries no data,
//! so it cannot change what any run returns.
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    process::Command,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
        Mutex, OnceLock, PoisonError,
    },
};

/// The region's descriptor in an executor. PV places its transport at 196,
/// other spawn hooks their fixed descriptors at 197 to 199, and sources sit at
/// 200 and above.
pub const REGION_DESCRIPTOR: RawFd = 194;
/// The pipe's write end in an executor.
pub const BELL_DESCRIPTOR: RawFd = 195;
/// Each word on its own line.
const PENDING: usize = 0;
const ASLEEP: usize = 128;
const LENGTH: usize = 4096;

/// The mapped region.
struct Words {
    base: NonNull<u8>,
}
// SAFETY: the region is only ever accessed through atomics.
unsafe impl Send for Words {}
// SAFETY: as above.
unsafe impl Sync for Words {}
impl Words {
    fn map(descriptor: &OwnedFd) -> io::Result<Self> {
        // SAFETY: a new shared mapping of the whole object; nothing else in
        // this process aliases it except through atomics.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LENGTH,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            base: NonNull::new(base.cast()).ok_or_else(|| io::Error::other("mapped at null"))?,
        })
    }
    fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: both words are aligned and inside the mapping, which lives
        // as long as self, and are only accessed atomically.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset).cast()) }
    }
}
impl Drop for Words {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping this value owns.
        unsafe { libc::munmap(self.base.as_ptr().cast(), LENGTH) };
    }
}

/// The process's doorbell: its region, and both ends of its pipe.
pub struct Doorbell {
    words: Words,
    region: OwnedFd,
    /// Only the claiming coordinator reads it.
    read: std::io::PipeReader,
    write: OwnedFd,
    claimed: AtomicBool,
    next_ticket: AtomicU64,
    #[cfg(any(test, feature = "test-support"))]
    witness: Witness,
}

/// What tests wait for and count, to know that a park slept and how it woke.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
struct Witness {
    sleeping: AtomicU64,
    tickets: AtomicU64,
}

static PROCESS: OnceLock<Doorbell> = OnceLock::new();
static CREATING: Mutex<()> = Mutex::new(());

impl Doorbell {
    /// The shared notification word for workers spawned in this process.
    pub fn process() -> io::Result<&'static Doorbell> {
        Self::get_or_create(&PROCESS, &CREATING, Self::create)
    }
    pub fn existing() -> Option<&'static Doorbell> {
        PROCESS.get()
    }
    /// Only a created doorbell is kept. A creation that failed, for want of
    /// descriptors say, is tried again by the next caller. Creations take
    /// turns: two at once could each hold descriptors the other needed.
    fn get_or_create<'a>(
        cell: &'a OnceLock<Doorbell>,
        creating: &Mutex<()>,
        create: impl FnOnce() -> io::Result<Doorbell>,
    ) -> io::Result<&'a Doorbell> {
        if let Some(bell) = cell.get() {
            return Ok(bell);
        }
        let _turn = creating.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(bell) = cell.get() {
            return Ok(bell);
        }
        let bell = create()?;
        Ok(cell.get_or_init(|| bell))
    }
    /// A doorbell apart from the process's, never dropped: for a test's own
    /// coordinator and the executors it spawns.
    #[cfg(any(test, feature = "test-support"))]
    pub fn detached() -> io::Result<&'static Doorbell> {
        Ok(Box::leak(Box::new(Self::create()?)))
    }
    /// The ticket of a park that has taken its last look at `pending` and
    /// now only its ticket, or a watched pipe, can end; zero otherwise.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sleeping(&self) -> u64 {
        self.witness.sleeping.load(SeqCst)
    }
    /// Parks that ended by reading their ticket from the pipe.
    #[cfg(any(test, feature = "test-support"))]
    pub fn tickets_read(&self) -> u64 {
        self.witness.tickets.load(SeqCst)
    }
    fn create() -> io::Result<Self> {
        let region = super::shared_memory(LENGTH)?;
        let words = Words::map(&region)?;
        let (read, write) = std::io::pipe()?;
        let write: OwnedFd = write.into();
        Ok(Self {
            words,
            region,
            read,
            write,
            claimed: AtomicBool::new(false),
            next_ticket: AtomicU64::new(1),
            #[cfg(any(test, feature = "test-support"))]
            witness: Witness::default(),
        })
    }
    /// Give the child that `command` spawns the region at
    /// [`REGION_DESCRIPTOR`] and the pipe at [`BELL_DESCRIPTOR`].
    pub fn inherit(&self, command: &mut Command) -> io::Result<()> {
        let source = |fd: &OwnedFd| -> io::Result<OwnedFd> {
            // A source above every fixed child descriptor cannot be replaced
            // by another hook's dup2 before this one runs.
            // SAFETY: F_DUPFD_CLOEXEC returns a new descriptor or fails.
            let fd = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 200) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl returned a new descriptor, owned from here.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        };
        let (region, bell) = (source(&self.region)?, source(&self.write)?);
        // SAFETY: dup2 is async-signal-safe, and the closure owns its sources
        // until the Command drops.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(region.as_raw_fd(), REGION_DESCRIPTOR) < 0
                    || libc::dup2(bell.as_raw_fd(), BELL_DESCRIPTOR) < 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        Ok(())
    }
    /// Become the doorbell's coordinator until the result drops; `None` while
    /// another caller is.
    pub fn claim(&self) -> Option<Coordinator<'_>> {
        if self.claimed.swap(true, SeqCst) {
            return None;
        }
        Some(Coordinator {
            bell: self,
            seen: self.words.word(PENDING).load(SeqCst),
        })
    }
    /// A ringer in this process, as an executor's would be.
    pub fn ringer(&self) -> io::Result<Ringer> {
        let write = self.write.try_clone()?;
        Ok(Ringer {
            words: Words::map(&self.region)?,
            write,
        })
    }
}

/// The claiming coordinator's side.
pub struct Coordinator<'a> {
    bell: &'a Doorbell,
    /// `pending` at the last mark.
    seen: u64,
}
impl Coordinator<'_> {
    /// Note `pending` before a pass over the runs' replies, so that a reply
    /// published during the pass is seen by [`Coordinator::rung`].
    pub fn mark(&mut self) {
        self.seen = self.bell.words.word(PENDING).load(SeqCst);
    }
    /// Whether an executor rang since the last mark.
    pub fn rung(&self) -> bool {
        self.bell.words.word(PENDING).load(SeqCst) != self.seen
    }
    /// Wait on the same publication word sampled before the reply scan.
    pub fn spin(&self, action: super::idle::Action) {
        super::idle::wait_on_word(action, self.bell.words.word(PENDING), self.seen);
    }
    /// Arm the watch set for this park. A writer outside the set may also
    /// publish, but it cannot take this ticket: its death could otherwise
    /// strand the coordinator while none of its watched pipes has closed.
    pub fn park(&self, watch: impl FnOnce(u64) -> Option<Vec<RawFd>>) -> io::Result<Wake> {
        let ticket = self.bell.next_ticket.fetch_add(1, SeqCst);
        let Some(watch) = watch(ticket) else {
            return Ok(Wake::Unwatched);
        };
        let mut fds: Vec<_> = std::iter::once(self.bell.read.as_raw_fd())
            .chain(watch)
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let asleep = self.bell.words.word(ASLEEP);
        asleep.store(ticket, SeqCst);
        let wake = if self.rung() && asleep.swap(0, SeqCst) == ticket {
            // Cancelled, perhaps by a ring from an executor this park does
            // not watch. Such rings could cancel every park, so look without
            // waiting for a watched executor that exited: no later park may
            // get as far as the poll that would report it.
            poll(&mut fds[1..], 0).map(|()| exited(&fds).map_or(Wake::Published, Wake::Exited))
        } else {
            self.sleep(ticket, &mut fds)
        };
        if !matches!(wake, Ok(Wake::Published)) {
            // A writer that took the mark first pays late, and a later park
            // discards that ticket.
            asleep.store(0, SeqCst);
        }
        wake
    }
    /// Wait for `ticket` or for a watched pipe's event. Past this point no
    /// look at `pending` can end the park.
    fn sleep(&self, ticket: u64, fds: &mut [libc::pollfd]) -> io::Result<Wake> {
        #[cfg(any(test, feature = "test-support"))]
        self.bell.witness.sleeping.store(ticket, SeqCst);
        let wake = loop {
            if let Err(error) = poll(fds, -1) {
                break Err(error);
            }
            let mut paid = false;
            if fds[0].revents != 0 {
                let mut bytes = [0; 8];
                if let Err(error) = std::io::Read::read_exact(&mut &self.bell.read, &mut bytes) {
                    break Err(error);
                }
                // Any other ticket was owed to an earlier park, which ended
                // at a watched pipe's event after a live writer took its mark.
                paid = u64::from_ne_bytes(bytes) == ticket;
            }
            // An exit is reported whenever it is seen, ticket or not: the
            // caller finds every published reply on its next scan.
            if let Some(i) = exited(fds) {
                break Ok(Wake::Exited(i));
            }
            if paid {
                #[cfg(any(test, feature = "test-support"))]
                self.bell.witness.tickets.fetch_add(1, SeqCst);
                break Ok(Wake::Published);
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        self.bell.witness.sleeping.store(0, SeqCst);
        wake
    }
}
/// Poll until an event or `timeout` milliseconds; forever if negative.
fn poll(fds: &mut [libc::pollfd], timeout: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: fds is initialized and its descriptors outlive this call.
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
/// The first watched pipe, after the doorbell's own, with an event.
fn exited(fds: &[libc::pollfd]) -> Option<usize> {
    fds[1..].iter().position(|fd| fd.revents != 0)
}
impl Drop for Coordinator<'_> {
    fn drop(&mut self) {
        self.bell.claimed.store(false, SeqCst);
    }
}

/// How the coordinator should resume its scan.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    Published,
    Exited(usize),
    Unwatched,
}

/// An executor's side.
pub struct Ringer {
    words: Words,
    write: OwnedFd,
}
impl Ringer {
    /// The doorbell this executor's spawner passed at [`REGION_DESCRIPTOR`]
    /// and [`BELL_DESCRIPTOR`].
    ///
    /// # Safety
    /// The private executor entry owns these two inherited descriptors, and
    /// calls this exactly once, before starting its own child processes.
    pub unsafe fn inherited() -> io::Result<Self> {
        for fd in [REGION_DESCRIPTOR, BELL_DESCRIPTOR] {
            // SAFETY: F_GETFD only queries the descriptor table.
            if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
                return Err(io::Error::other(format!("doorbell descriptor {fd} absent")));
            }
        }
        // SAFETY: the spawner passed both descriptors for this process to own.
        let (region, write) = unsafe {
            (
                OwnedFd::from_raw_fd(REGION_DESCRIPTOR),
                OwnedFd::from_raw_fd(BELL_DESCRIPTOR),
            )
        };
        for fd in [&region, &write] {
            // SAFETY: F_SETFD on a descriptor this function owns: this
            // process's own spawns must not inherit it.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat is a valid out-pointer for this call.
        if unsafe { libc::fstat(region.as_raw_fd(), stat.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat succeeded and initialized stat.
        let size = unsafe { stat.assume_init() }.st_size;
        if !u64::try_from(size).is_ok_and(|size| size >= LENGTH as u64) {
            return Err(io::Error::other("doorbell region too small"));
        }
        // The mapping keeps the region; its descriptor closes here.
        Ok(Self {
            words: Words::map(&region)?,
            write,
        })
    }
    /// Announce a published reply: bump `pending`, then wake the coordinator
    /// if it marked a sleep.
    pub fn ring(&self, watch: &AtomicU64) -> io::Result<()> {
        self.words.word(PENDING).fetch_add(1, SeqCst);
        let asleep = self.words.word(ASLEEP);
        let ticket = asleep.load(SeqCst);
        if ticket != 0
            && watch.load(SeqCst) == ticket
            && asleep.compare_exchange(ticket, 0, SeqCst, SeqCst).is_ok()
        {
            let bytes = ticket.to_ne_bytes();
            loop {
                // SAFETY: a ticket fits in PIPE_BUF and is written atomically.
                let count = unsafe {
                    libc::write(self.write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                };
                if count == bytes.len() as isize {
                    return Ok(());
                }
                if count >= 0 {
                    return Err(io::Error::other("doorbell pipe split a ticket"));
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
