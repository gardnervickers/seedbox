//! Executable fixtures, guarded spawning and held-before-exec probes.
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread::JoinHandle,
};

/// Create `path` with `bytes` and `mode`. The file is written and every
/// descriptor to it closed while forks are excluded, so it can be executed
/// immediately by this or any other thread.
pub fn install_executable(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let _writing = seedbox_exec::writing();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    drop(file);
    Ok(())
}

pub use seedbox_exec::{output, spawn, spawn_with, status, Guarded};

/// How a held child is started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spawn {
    /// Through [`spawn`]: the fork holds the shared exec-file guard while it
    /// is held, as every real spawn does. While such a child is held, a
    /// blocking executable write anywhere in the process waits for it, and
    /// any later spawn waits behind that queued writer. So while one is held,
    /// the test must not spawn or hold again, directly or through library
    /// code, unless nothing else in the binary can write an executable at the
    /// same time. Probe writers with `seedbox_exec::try_writing`, never
    /// [`install_executable`]. Most probes want [`Spawn::GuardForkOnly`].
    GuardWhileHeld,
    /// The exclusive guard is held across the fork only and released once the
    /// child is parked. At the fork no executable is being installed and no
    /// other spawn is between fork and exec, so the child copies neither an
    /// installer's descriptor nor another spawn's status pipe: sibling tests
    /// and later spawns proceed while it is held. It does copy any file this
    /// process opened for writing some other way, which reproduces an
    /// uncoordinated write.
    GuardForkOnly,
}

/// Children of this process held between fork and exec, each holding copies
/// of every descriptor the process had open at its fork.
///
/// Dropping releases every child before joining any. std's `Command::spawn`
/// waits until its status pipe closes, and a later fork can hold a copy of
/// an earlier child's status-pipe write end, so joining children one at a
/// time can deadlock. Drop also runs during a panic: no held child outlives
/// the test.
#[derive(Default)]
pub struct HeldChildren {
    held: Vec<Held>,
}

struct Held {
    guarded: bool,
    release: fs::File,
    spawned: JoinHandle<io::Result<Child>>,
}

impl HeldChildren {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fork one `/bin/sh -c 'exit 0'` on a new thread and return once the
    /// child is past fork and blocked before exec.
    ///
    /// A [`Spawn::GuardForkOnly`] hold needs exclusive access for its fork, so
    /// it is refused while this holder has a [`Spawn::GuardWhileHeld`] child,
    /// which would otherwise wait for itself forever.
    pub fn hold(&mut self, how: Spawn) -> io::Result<()> {
        if how == Spawn::GuardForkOnly && self.held.iter().any(|held| held.guarded) {
            return Err(io::Error::other(
                "a fork-only hold after a guard-holding child would deadlock",
            ));
        }
        let (mut ready, ready_write) = pipe()?;
        let (release_read, release) = pipe()?;
        let (ready_fd, release_fd) = (ready_write.as_raw_fd(), release_read.as_raw_fd());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]).stdin(Stdio::null());
        // Child only, after fork: async-signal-safe read/write; never the lock.
        unsafe {
            command.pre_exec(move || {
                let byte = 1u8;
                if libc::write(ready_fd, std::ptr::addr_of!(byte).cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                let mut released = 0u8;
                loop {
                    match libc::read(release_fd, std::ptr::addr_of_mut!(released).cast(), 1) {
                        1 => return Ok(()),
                        -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                            continue
                        }
                        _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
                    }
                }
            });
        }
        let fork_guard = (how == Spawn::GuardForkOnly).then(seedbox_exec::writing);
        let spawned = std::thread::spawn(move || {
            let _keep = (ready_write, release_read);
            match how {
                Spawn::GuardWhileHeld => spawn(&mut command),
                #[allow(
                    clippy::disallowed_methods,
                    reason = "the caller holds the exclusive guard across this fork only"
                )]
                Spawn::GuardForkOnly => command.spawn(),
            }
        });
        self.held.push(Held {
            guarded: how == Spawn::GuardWhileHeld,
            release,
            spawned,
        });
        let mut byte = [0];
        // EOF here means the spawn failed before the child reached pre_exec.
        let parked = ready.read_exact(&mut byte);
        drop(fork_guard);
        parked
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Let every child exec, then reap them all; each must exit successfully.
    pub fn release(mut self) -> io::Result<()> {
        self.release_all()
    }

    fn release_all(&mut self) -> io::Result<()> {
        let held = std::mem::take(&mut self.held);
        let mut handles = Vec::with_capacity(held.len());
        for Held {
            mut release,
            spawned,
            ..
        } in held
        {
            // Ignore: a child whose spawn already failed has closed its end.
            let _ = release.write_all(&[1]);
            drop(release);
            handles.push(spawned);
        }
        let mut result = Ok(());
        for spawned in handles {
            let outcome = spawned
                .join()
                .map_err(|_| io::Error::other("held spawn thread panicked"))
                .and_then(|spawned| spawned)
                .and_then(|mut child| child.wait());
            match outcome {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    result = result.and(Err(io::Error::other(format!("held child: {status}"))))
                }
                Err(error) => result = result.and(Err(error)),
            }
        }
        result
    }
}

impl Drop for HeldChildren {
    fn drop(&mut self) {
        let result = self.release_all();
        if !std::thread::panicking() {
            result.expect("held children exit successfully");
        }
    }
}

fn pipe() -> io::Result<(fs::File, fs::File)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let pair = unsafe { (fs::File::from_raw_fd(fds[0]), fs::File::from_raw_fd(fds[1])) };
    for fd in fds {
        // Portable CLOEXEC (no pipe2 on Darwin). The child keeps its two
        // ends by number in pre_exec, which runs before exec closes them.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pair)
}
