//! One place for a coordinator to park while many executors in other processes
//! run: the process's doorbell.
//!
//! The doorbell is a small shared-memory region and a pipe. The region holds a
//! `pending` word and one `asleep` word for each coordinator in the process.
//! Every executor process spawned by this process inherits the region and the
//! pipe's write end ([`Doorbell::inherit`]). An executor that publishes a reply
//! [rings](Ringer::ring): it bumps `pending`, and if it then takes the `asleep`
//! mark of the coordinator watching it, it writes that mark, a ticket, to the
//! pipe.
//!
//! Before scanning the rings, a coordinator samples `pending`. To park, it
//! names a ticket to the executors it waits for, sets its `asleep` word to the
//! ticket and checks the sample again. If it sees progress it can cancel only
//! a mark no executor has taken. Once an executor takes the mark, the
//! coordinator consumes the owed ticket even if it has already observed the
//! reply. The per-worker pipes share the poll set so a worker that dies
//! between updating either word and writing the ticket cannot strand the wait.
//!
//! An executor is bound to no coordinator: a worker one Driver spawned can
//! later run a machine of another, so each park names its coordinator anew. A
//! ticket carries its coordinator's slot. Every parked coordinator reads the
//! one pipe, and one that reads another's ticket forwards it through that
//! coordinator's own pipe. A process with a single coordinator reads only its
//! own tickets. Rings from executors a coordinator is not waiting for only
//! wake it early.
//!
//! The doorbell decides only when a coordinator looks; it carries no data,
//! so it cannot change what any run returns.
use std::{
    io::{self, PipeReader, PipeWriter, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::CommandExt,
    },
    process::Command,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Mutex, MutexGuard, OnceLock, PoisonError,
    },
};

/// The region's descriptor in an executor. PV places its transport at 196,
/// other spawn hooks their fixed descriptors at 197 to 199, and sources sit at
/// 200 and above.
pub const REGION_DESCRIPTOR: RawFd = 194;
/// The pipe's write end in an executor.
pub const BELL_DESCRIPTOR: RawFd = 195;
/// Each word on its own line: `pending`, then every slot's `asleep`.
const LINE: usize = 128;
/// A ticket's low bits name its coordinator's slot.
const SLOT_BITS: u32 = 7;
/// Coordinators one process can hold at once.
const SLOTS: usize = (1 << SLOT_BITS) - 1;
const LENGTH: usize = (1 + SLOTS) * LINE;

/// The slot a ticket names; none for zero, which is no ticket.
fn slot_of(ticket: u64) -> Option<usize> {
    let slot = (ticket & ((1 << SLOT_BITS) - 1)) as usize;
    (ticket != 0 && slot < SLOTS).then_some(slot)
}

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
    fn word(&self, line: usize) -> &AtomicU64 {
        assert!(line <= SLOTS, "doorbell word {line}");
        // SAFETY: every line is aligned and inside the mapping, which lives
        // as long as self, and is only accessed atomically.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(line * LINE).cast()) }
    }
    fn pending(&self) -> &AtomicU64 {
        self.word(0)
    }
    fn asleep(&self, slot: usize) -> &AtomicU64 {
        self.word(1 + slot)
    }
}
impl Drop for Words {
    fn drop(&mut self) {
        // SAFETY: unmaps exactly the mapping this value owns.
        unsafe { libc::munmap(self.base.as_ptr().cast(), LENGTH) };
    }
}

/// Make reads or writes of `pipe` return instead of waiting.
fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: F_SETFL on a descriptor the caller owns.
    if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Take one ticket from a nonblocking pipe that carries only whole tickets;
/// none while it is empty.
fn take(mut pipe: &PipeReader) -> io::Result<Option<u64>> {
    let mut bytes = [0; 8];
    loop {
        match pipe.read(&mut bytes) {
            Ok(8) => return Ok(Some(u64::from_ne_bytes(bytes))),
            Ok(0) => return Ok(None),
            Ok(_) => return Err(io::Error::other("doorbell pipe split a ticket")),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// The process's doorbell: its region, and both ends of its pipe.
pub struct Doorbell {
    words: Words,
    region: OwnedFd,
    /// Nonblocking: every parked coordinator reads it.
    read: PipeReader,
    write: OwnedFd,
    /// Each claimed slot's pipe for tickets another coordinator read.
    slots: Mutex<Vec<Option<PipeWriter>>>,
    next_ticket: AtomicU64,
}

static PROCESS: OnceLock<Doorbell> = OnceLock::new();

impl Doorbell {
    /// The shared notification word for workers spawned in this process.
    pub fn process() -> io::Result<&'static Doorbell> {
        Self::get_or_create(&PROCESS, || Self::create(SLOTS))
    }
    pub fn existing() -> Option<&'static Doorbell> {
        PROCESS.get()
    }
    /// Only a created doorbell is kept. A creation that failed, for want of
    /// descriptors say, is tried again by the next caller.
    fn get_or_create(
        cell: &OnceLock<Doorbell>,
        create: impl FnOnce() -> io::Result<Doorbell>,
    ) -> io::Result<&Doorbell> {
        if let Some(bell) = cell.get() {
            return Ok(bell);
        }
        let bell = create()?;
        // A creator that lost the race adopts the winner's and drops its own.
        Ok(cell.get_or_init(|| bell))
    }
    /// A doorbell whose first `slots` slots can be claimed.
    fn create(slots: usize) -> io::Result<Self> {
        let region = super::shared_memory(LENGTH)?;
        let words = Words::map(&region)?;
        let (read, write) = std::io::pipe()?;
        nonblocking(&read)?;
        Ok(Self {
            words,
            region,
            read,
            write: write.into(),
            slots: Mutex::new((0..slots.min(SLOTS)).map(|_| None).collect()),
            next_ticket: AtomicU64::new(1),
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
    /// Become one of the doorbell's coordinators until the result drops;
    /// `None` while every slot is taken.
    pub fn claim(&self) -> io::Result<Option<Coordinator<'_>>> {
        let mut slots = self.slots();
        let Some(slot) = slots.iter().position(Option::is_none) else {
            return Ok(None);
        };
        let (forwarded, forward) = std::io::pipe()?;
        nonblocking(&forwarded)?;
        nonblocking(&forward)?;
        slots[slot] = Some(forward);
        Ok(Some(Coordinator {
            bell: self,
            slot,
            seen: self.words.pending().load(SeqCst),
            forwarded,
            woken: 0,
        }))
    }
    /// A ringer in this process, as an executor's would be.
    pub fn ringer(&self) -> io::Result<Ringer> {
        let write = self.write.try_clone()?;
        Ok(Ringer {
            words: Words::map(&self.region)?,
            write,
        })
    }
    fn slots(&self) -> MutexGuard<'_, Vec<Option<PipeWriter>>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
    /// Pass a ticket read from the shared pipe to the coordinator it names.
    /// A ticket for a slot no one holds is a late payment to a coordinator
    /// that has gone, and so is one its coordinator's pipe has no room for.
    fn forward(&self, ticket: u64) {
        let slots = self.slots();
        let forward = slot_of(ticket).and_then(|slot| slots.get(slot)?.as_ref());
        if let Some(mut forward) = forward {
            // A whole ticket or nothing: the pipe is nonblocking.
            let _ = forward.write(&ticket.to_ne_bytes());
        }
    }
}

/// One coordinator's side.
pub struct Coordinator<'a> {
    bell: &'a Doorbell,
    slot: usize,
    /// `pending` at the last mark.
    seen: u64,
    /// Tickets naming this slot that another coordinator read.
    forwarded: PipeReader,
    woken: u64,
}
impl Coordinator<'_> {
    /// Note `pending` before a pass over the runs' replies, so that a reply
    /// published during the pass is seen by [`Coordinator::rung`].
    pub fn mark(&mut self) {
        self.seen = self.bell.words.pending().load(SeqCst);
    }
    /// Whether an executor rang since the last mark.
    pub fn rung(&self) -> bool {
        self.bell.words.pending().load(SeqCst) != self.seen
    }
    /// Parks that a ring ended, by its ticket or before the sleep began. A
    /// witness for tests that a wait went through the doorbell.
    pub fn woken(&self) -> u64 {
        self.woken
    }
    /// Wait on the same publication word sampled before the reply scan.
    pub fn spin(&self, action: super::idle::Action) {
        super::idle::wait_on_word(action, self.bell.words.pending(), self.seen);
    }
    /// Arm the watch set for this park. A writer outside the set may also
    /// publish, but it cannot take this ticket: its death could otherwise
    /// strand the coordinator while none of its watched pipes has closed.
    pub fn park(&mut self, watch: impl FnOnce(u64) -> Option<Vec<RawFd>>) -> io::Result<Wake> {
        let sequence = self.bell.next_ticket.fetch_add(1, SeqCst);
        let ticket = (sequence << SLOT_BITS) | self.slot as u64;
        let Some(watch) = watch(ticket) else {
            return Ok(Wake::Unwatched);
        };
        let mut fds: Vec<_> = [self.bell.read.as_raw_fd(), self.forwarded.as_raw_fd()]
            .into_iter()
            .chain(watch)
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let asleep = self.bell.words.asleep(self.slot);
        asleep.store(ticket, SeqCst);
        let wake = if self.rung() && asleep.swap(0, SeqCst) == ticket {
            Ok(Wake::Published)
        } else {
            self.sleep(ticket, &mut fds)
        };
        match wake {
            Ok(Wake::Published) => self.woken += 1,
            // An executor that took the mark first pays late, and a later
            // park discards that ticket.
            _ => asleep.store(0, SeqCst),
        }
        wake
    }
    /// Wait for `ticket`, on either pipe, or for a watched pipe's event.
    fn sleep(&self, ticket: u64, fds: &mut [libc::pollfd]) -> io::Result<Wake> {
        loop {
            // SAFETY: fds is initialized and its descriptors outlive this wait.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // A ticket of this slot other than `ticket` was owed to an earlier
            // park, which ended at a watched pipe's event.
            if fds[0].revents != 0 {
                // None if another coordinator read the pipe first.
                match take(&self.bell.read)? {
                    Some(read) if read == ticket => return Ok(Wake::Published),
                    Some(read) if slot_of(read) == Some(self.slot) => {}
                    Some(read) => self.bell.forward(read),
                    None => {}
                }
            }
            if fds[1].revents != 0 {
                while let Some(read) = take(&self.forwarded)? {
                    if read == ticket {
                        return Ok(Wake::Published);
                    }
                }
            }
            if let Some(i) = fds[2..].iter().position(|fd| fd.revents != 0) {
                return Ok(Wake::Exited(i));
            }
        }
    }
}
impl Drop for Coordinator<'_> {
    fn drop(&mut self) {
        self.bell.words.asleep(self.slot).store(0, SeqCst);
        self.bell.slots()[self.slot] = None;
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
    /// whose ticket `watch` holds, if it marked a sleep with that ticket.
    pub fn ring(&self, watch: &AtomicU64) -> io::Result<()> {
        self.words.pending().fetch_add(1, SeqCst);
        // A parking coordinator stores `watch`, then its mark, then looks at
        // `pending` again: it sees this ring, or these loads see both stores.
        let ticket = watch.load(SeqCst);
        let Some(slot) = slot_of(ticket) else {
            return Ok(());
        };
        let asleep = self.words.asleep(slot);
        if asleep.load(SeqCst) == ticket
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
