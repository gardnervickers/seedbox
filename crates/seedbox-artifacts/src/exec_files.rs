//! Process-wide ordering between writing a file that will be executed and
//! starting any child process.
//!
//! A child forked while a writable descriptor to a soon-to-be-executed file is
//! open keeps its copy of that descriptor until the child's own `exec`.
//! `FD_CLOEXEC` only acts at that `exec`; closing or renaming the parent's file
//! does not revoke the copy. Executing the file in that window fails with
//! `ETXTBSY`. A server runs several executions on threads of one process, so
//! one execution's write can overlap another's fork.
//!
//! The rule: [`writing`] is held exclusively from opening a file for write until
//! every descriptor to it is closed, for every file this process will later
//! execute (or hand to another process to execute). [`spawn`] holds the lock
//! shared across fork and exec: `Command::spawn` returns only after the child
//! has exec'd or failed. Spawns share with each other and never wait for one
//! another. The lock guards no data and is never held across waiting for a
//! child, so a panic cannot leave state to repair and poisoning is ignored.
//!
//! Do not nest [`spawn`] (or any shared guard), and do not spawn while holding
//! [`writing`]: a queued writer would then deadlock the nested reader.
//!
//! `clippy.toml` forbids `Command::{spawn, output, status}` and raw forks
//! everywhere else. Code that runs only in an execution worker, a test or a
//! developer tool opts out with a scoped `allow` stating why. The worker is
//! exempt because it writes its one executable copy (runtime extraction)
//! before it starts any machine, and so before any spawn.
use std::{
    io,
    process::{Child, Command},
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError},
};

static SCOPE: RwLock<()> = RwLock::new(());

/// Exclusive: no child process can be forked while this is held.
#[must_use = "the guard excludes forks only while it is alive"]
pub struct Writing(#[allow(dead_code)] RwLockWriteGuard<'static, ()>);

/// Shared: no executable is being written while this is held.
#[must_use = "the guard excludes writers only while it is alive"]
pub struct Spawning(#[allow(dead_code)] RwLockReadGuard<'static, ()>);

/// Wait until no spawn is between fork and exec, then exclude new ones.
pub fn writing() -> Writing {
    Writing(SCOPE.write().unwrap_or_else(PoisonError::into_inner))
}

/// Non-blocking [`writing`]; `None` while any spawn or writer is active.
pub fn try_writing() -> Option<Writing> {
    match SCOPE.try_write() {
        Ok(guard) => Some(Writing(guard)),
        Err(TryLockError::Poisoned(error)) => Some(Writing(error.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Non-blocking shared guard; `None` while a writer is active (or queued).
pub fn try_spawning() -> Option<Spawning> {
    match SCOPE.try_read() {
        Ok(guard) => Some(Spawning(guard)),
        Err(TryLockError::Poisoned(error)) => Some(Spawning(error.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// `Command::spawn` with the fork-to-exec window covered. Every production
/// spawn in a process that also writes executables goes through here; the
/// guard is released as soon as the child has exec'd, not when it exits.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    let _spawning = Spawning(SCOPE.read().unwrap_or_else(PoisonError::into_inner));
    #[allow(
        clippy::disallowed_methods,
        reason = "the guarded spawn that every other site must use"
    )]
    let child = command.spawn();
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the scope is process-wide, so concurrent unit tests that
    // inspect it would observe each other's guards.
    #[test]
    fn spawners_share_and_writers_exclude() {
        let first = try_spawning().expect("idle");
        let second = try_spawning().expect("spawners share");
        assert!(try_writing().is_none(), "a spawn excludes writers");
        drop((first, second));
        let writer = try_writing().expect("idle after spawns");
        assert!(try_spawning().is_none(), "a writer excludes spawns");
        assert!(try_writing().is_none(), "writers are exclusive");
        drop(writer);
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        assert!(spawn(&mut command).unwrap().wait().unwrap().success());
        assert!(try_writing().is_some(), "spawn released its guard at exec");
    }
}
