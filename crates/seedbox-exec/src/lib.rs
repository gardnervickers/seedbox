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
//! Rules for callers:
//! - **No nesting.** The lock is not reentrant and waiting writers are
//!   preferred, so a thread that takes a second guard while it holds one can
//!   deadlock behind a queued writer. This covers [`spawn`] inside [`spawn`],
//!   [`spawn`] while holding [`writing`], and [`writing`] while holding
//!   either. Debug builds panic on a blocking acquisition by a thread that
//!   already holds a guard.
//! - **Bounded writes.** Hold [`writing`] only for a bounded local copy: never
//!   while waiting on another thread or a child that may itself need to spawn,
//!   such as piping a child's output into the executable.
//! - **Nothing after fork.** `pre_exec` closures run between fork and exec and
//!   may use only async-signal-safe calls; they never touch this lock.
//!
//! `clippy.toml` forbids `Command::{spawn, output, status}` and raw forks
//! outside [`spawn`]. Build scripts, developer tools and deliberately
//! unguarded reproductions opt out with a scoped `allow` stating why.
use std::{
    io,
    process::{Child, Command},
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError},
};

static SCOPE: RwLock<()> = RwLock::new(());

/// Guards held by this thread. Only debug builds track it (rule: no nesting).
mod held {
    #[cfg(debug_assertions)]
    std::thread_local!(static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

    /// Called before a blocking acquisition.
    pub(super) fn assert_none(what: &str) {
        #[cfg(debug_assertions)]
        HELD.with(|held| {
            assert!(
                held.get() == 0,
                "seedbox_exec::{what} while this thread holds an exec-file guard; nesting can deadlock behind a waiting writer"
            )
        });
        #[cfg(not(debug_assertions))]
        let _ = what;
    }
    pub(super) fn acquired() {
        #[cfg(debug_assertions)]
        HELD.with(|held| held.set(held.get() + 1));
    }
    pub(super) fn released() {
        #[cfg(debug_assertions)]
        HELD.with(|held| held.set(held.get() - 1));
    }
}

/// Exclusive: no child process can be forked while this is held.
#[must_use = "the guard excludes forks only while it is alive"]
pub struct Writing(#[allow(dead_code)] RwLockWriteGuard<'static, ()>);

/// Shared: no executable is being written while this is held.
#[must_use = "the guard excludes writers only while it is alive"]
pub struct Spawning(#[allow(dead_code)] RwLockReadGuard<'static, ()>);

impl Writing {
    fn new(guard: RwLockWriteGuard<'static, ()>) -> Self {
        held::acquired();
        Self(guard)
    }
}
impl Spawning {
    fn new(guard: RwLockReadGuard<'static, ()>) -> Self {
        held::acquired();
        Self(guard)
    }
}
impl Drop for Writing {
    fn drop(&mut self) {
        held::released();
    }
}
impl Drop for Spawning {
    fn drop(&mut self) {
        held::released();
    }
}

/// Wait until no spawn is between fork and exec, then exclude new ones.
/// Only for a bounded local copy of a file that will be executed: open it,
/// write it, and close every descriptor to it before the guard drops.
pub fn writing() -> Writing {
    held::assert_none("writing");
    Writing::new(SCOPE.write().unwrap_or_else(PoisonError::into_inner))
}

/// Non-blocking [`writing`]; `None` while any spawn or writer is active.
pub fn try_writing() -> Option<Writing> {
    match SCOPE.try_write() {
        Ok(guard) => Some(Writing::new(guard)),
        Err(TryLockError::Poisoned(error)) => Some(Writing::new(error.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Non-blocking shared guard; `None` while a writer is active (or queued).
pub fn try_spawning() -> Option<Spawning> {
    match SCOPE.try_read() {
        Ok(guard) => Some(Spawning::new(guard)),
        Err(TryLockError::Poisoned(error)) => Some(Spawning::new(error.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// `Command::spawn` with the fork-to-exec window covered. Every spawn in a
/// process that also writes executables goes through here; the guard is
/// released as soon as the child has exec'd, not when it exits.
pub fn spawn(command: &mut Command) -> io::Result<Child> {
    held::assert_none("spawn");
    let _spawning = Spawning::new(SCOPE.read().unwrap_or_else(PoisonError::into_inner));
    #[allow(
        clippy::disallowed_methods,
        reason = "the guarded spawn that every other site must use"
    )]
    let child = command.spawn();
    child
}

/// Run `spawn` with the fork-to-exec window covered, for spawners other than
/// `std::process::Command` (such as `tokio::process::Command`, which forks
/// through std and returns once the child has exec'd). `spawn` must only
/// start the child: it must not wait for it, write an executable, or spawn
/// again through this module.
pub fn spawn_with<T>(spawn: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    held::assert_none("spawn_with");
    let _spawning = Spawning::new(SCOPE.read().unwrap_or_else(PoisonError::into_inner));
    spawn()
}

/// `Command::status` through [`spawn`]. Like std, standard streams are
/// inherited unless the command sets them.
pub fn status(command: &mut Command) -> io::Result<std::process::ExitStatus> {
    spawn(command)?.wait()
}

/// `Command::output` through [`spawn`]. Unlike std, stdin is always null and
/// stdout/stderr are always captured, even if the command set them; use
/// [`spawn`] for other stdio.
pub fn output(command: &mut Command) -> io::Result<std::process::Output> {
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(command)?.wait_with_output()
}

/// [`spawn`], [`status`] and [`output`] as `Command` methods, for builder
/// chains: `Command::new(x).arg(y).guarded_status()`.
pub trait Guarded {
    /// [`spawn`].
    fn guarded_spawn(&mut self) -> io::Result<Child>;
    /// [`status`].
    fn guarded_status(&mut self) -> io::Result<std::process::ExitStatus>;
    /// [`output`]: stdin null, stdout and stderr captured.
    fn guarded_output(&mut self) -> io::Result<std::process::Output>;
}
impl Guarded for Command {
    fn guarded_spawn(&mut self) -> io::Result<Child> {
        spawn(self)
    }
    fn guarded_status(&mut self) -> io::Result<std::process::ExitStatus> {
        status(self)
    }
    fn guarded_output(&mut self) -> io::Result<std::process::Output> {
        output(self)
    }
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
        let excluded = spawn_with(|| Ok(try_writing().is_none())).unwrap();
        assert!(excluded, "spawn_with excludes writers while it runs");
        assert!(try_writing().is_some(), "spawn_with released its guard");
        nesting_panics_before_it_can_deadlock();
    }

    /// Part of the single test above: it must not overlap other lock users.
    fn nesting_panics_before_it_can_deadlock() {
        if !cfg!(debug_assertions) {
            return;
        }
        let nested = |first: fn() -> Box<dyn std::any::Any>, second: fn()| {
            let held = first();
            let result = std::panic::catch_unwind(second);
            drop(held);
            result.expect_err("nested acquisition must panic in debug builds")
        };
        let spawn_shell = || {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 0"]);
            let _ = spawn(&mut command);
        };
        for panic in [
            nested(|| Box::new(writing()), || drop(writing())),
            nested(|| Box::new(writing()), spawn_shell),
            nested(|| Box::new(try_spawning().unwrap()), spawn_shell),
            nested(|| Box::new(try_spawning().unwrap()), || drop(writing())),
        ] {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .unwrap_or_default();
            assert!(message.contains("nesting can deadlock"), "{message}");
        }
        // Unwinding released every guard and the per-thread count.
        drop(writing());
        assert!(try_spawning().is_some());
    }
}
