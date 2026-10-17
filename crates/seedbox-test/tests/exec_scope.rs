//! Server-level descriptor-inheritance probes for bd-cfd. A server execution
//! writes its retained engine with `ContentStore::materialize` while another
//! execution thread may fork the monitor. Nothing here sleeps or retries: the
//! store read is paused on a FIFO (so the destination descriptor is provably
//! open) and the fork child is held in `pre_exec` (so it provably has not
//! exec'd) until the test explicitly releases it.
//!
//! The probes assert Linux semantics: exec of a file still open for writing
//! fails with ETXTBSY. Other Unix hosts permit it, so the file is Linux-only.
#![cfg(target_os = "linux")]
use seedbox_artifacts::{
    exec_files,
    store::{ContentDigest, ContentStore},
};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    thread::{self, JoinHandle},
};

/// The exec-file scope is process-wide, so these tests (which hold it across
/// paused steps and assert on it) must not interleave with one another.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A shell-script "engine" larger than a pipe buffer. The materializer opens
/// the FIFO source twice (verify, then copy); writing more than the pipe holds
/// blocks until the second reader is open and draining, so closing the writer
/// afterwards can never strand it.
fn engine_bytes() -> Vec<u8> {
    let mut bytes = b"#!/bin/sh\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'#', 256 * 1024));
    bytes.extend_from_slice(b"\nexit 0\n");
    bytes
}

fn pipe() -> (fs::File, fs::File) {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    unsafe { (fs::File::from_raw_fd(fds[0]), fs::File::from_raw_fd(fds[1])) }
}

/// A store whose object for `ENGINE` is a FIFO: a read of it blocks until the
/// test opens the write end, exactly while `materialize` holds its destination.
struct PausedStore {
    _root: tempfile::TempDir,
    store: ContentStore,
    digest: ContentDigest,
    fifo: PathBuf,
    bytes: Vec<u8>,
}
impl PausedStore {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = ContentStore::new(root.path());
        let bytes = engine_bytes();
        let digest = store.put(&bytes).unwrap();
        let fifo = root.path().join("sha256").join(digest.hex());
        fs::remove_file(&fifo).unwrap();
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        Self {
            _root: root,
            store,
            digest,
            fifo,
            bytes,
        }
    }
    /// Returns once the materializer has created its writable destination and
    /// opened the source: the blocking open completes only then.
    fn source_is_blocked_with_destination_open(&self) -> fs::File {
        fs::OpenOptions::new().write(true).open(&self.fifo).unwrap()
    }
}

/// A child forked from this process that has not exec'd. `spawn_returned`
/// completes only after the child execs, so joining it proves release.
struct Parked {
    ready: fs::File,
    release: fs::File,
    spawn_returned: JoinHandle<io::Result<Child>>,
}
impl Parked {
    /// `drop_in_child` is test plumbing only: the FIFO writer must not be kept
    /// open by the held child, or the store read would never reach EOF.
    fn start(spawn: fn(&mut Command) -> io::Result<Child>, drop_in_child: RawFd) -> Self {
        let (ready, ready_write) = pipe();
        let (release_read, release) = pipe();
        let (ready_fd, release_fd) = (ready_write.as_raw_fd(), release_read.as_raw_fd());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        // Child only: async-signal-safe write/read between fork and exec.
        unsafe {
            command.pre_exec(move || {
                libc::close(drop_in_child);
                let byte = 1u8;
                if libc::write(ready_fd, std::ptr::addr_of!(byte).cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                let mut released = 0u8;
                loop {
                    match libc::read(release_fd, std::ptr::addr_of_mut!(released).cast(), 1) {
                        1 => return Ok(()),
                        -1 if *libc::__errno_location() == libc::EINTR => continue,
                        _ => return Err(io::Error::from_raw_os_error(libc::EIO)),
                    }
                }
            });
        }
        let spawn_returned = thread::spawn(move || {
            let _keep = (ready_write, release_read);
            spawn(&mut command)
        });
        Self {
            ready,
            release,
            spawn_returned,
        }
    }
    fn wait_until_forked(&mut self) {
        let mut byte = [0];
        self.ready.read_exact(&mut byte).unwrap();
    }
    fn release_and_reap(mut self) {
        self.release.write_all(&[1]).unwrap();
        let mut child = self.spawn_returned.join().unwrap().unwrap();
        assert!(child.wait().unwrap().success());
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "deliberately unguarded: reproduces the race the exec-file scope prevents"
)]
fn plain_spawn(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

fn scoped_spawn(command: &mut Command) -> io::Result<Child> {
    exec_files::spawn(command)
}

type Materialize = fn(&ContentStore, &ContentDigest, &Path) -> seedbox_artifacts::Result<PathBuf>;
const PLAIN: Materialize = |store, digest, directory| store.materialize(digest, directory);
const EXECUTABLE: Materialize =
    |store, digest, directory| store.materialize_executable(digest, directory);
const LINKED: Materialize = |store, digest, directory| {
    store.materialize_linked_executable(digest, &directory.join("shared"), &directory.join("run"))
};

fn publish(store: &PausedStore, directory: &Path, materialize: Materialize) -> JoinHandle<PathBuf> {
    let (store, digest, directory) = (
        store.store.clone(),
        store.digest.clone(),
        directory.to_owned(),
    );
    thread::spawn(move || {
        let path = materialize(&store, &digest, &directory).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
        path
    })
}

#[allow(
    clippy::disallowed_methods,
    reason = "deliberately unguarded: reproduces the race the exec-file scope prevents"
)]
#[test]
fn unscoped_materialize_leaves_fork_copy_that_makes_published_engine_etxtbsy() {
    let _serial = serial();
    let paused = PausedStore::new();
    let directory = tempfile::tempdir().unwrap();
    let writer = publish(&paused, directory.path(), PLAIN);
    let mut source = paused.source_is_blocked_with_destination_open();
    // A second execution forks its monitor now: the child copies the open
    // writable destination descriptor and is held before exec.
    let mut monitor = Parked::start(plain_spawn, source.as_raw_fd());
    monitor.wait_until_forked();
    source.write_all(&paused.bytes).unwrap();
    drop(source);
    let engine = writer.join().unwrap();
    let error = Command::new(&engine).status().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY), "{error}");
    eprintln!(
        "server materialize pre-exec ETXTBSY: errno={:?}",
        error.raw_os_error()
    );
    monitor.release_and_reap();
    assert!(Command::new(&engine).status().unwrap().success());
}

fn run(engine: &Path) {
    // Through the same guard every production spawn uses.
    let mut command = Command::new(engine);
    assert!(exec_files::spawn(&mut command)
        .unwrap()
        .wait()
        .unwrap()
        .success());
}

#[test]
fn executable_materialize_excludes_every_fork_until_its_descriptors_close() {
    let _serial = serial();
    for (name, materialize) in [("direct", EXECUTABLE), ("shared copy", LINKED)] {
        let paused = PausedStore::new();
        let directory = tempfile::tempdir().unwrap();
        let writer = publish(&paused, directory.path(), materialize);
        let mut source = paused.source_is_blocked_with_destination_open();
        assert!(
            exec_files::try_spawning().is_none(),
            "{name}: a fork could now copy the open destination"
        );
        // Another execution's monitor spawn queues behind the write.
        let mut monitor = Parked::start(scoped_spawn, -1);
        source.write_all(&paused.bytes).unwrap();
        drop(source);
        let engine = writer.join().unwrap();
        monitor.wait_until_forked(); // forked only after every writer closed
        run(&engine);
        monitor.release_and_reap();
    }
}

#[test]
fn a_spawn_between_fork_and_exec_excludes_executable_writes() {
    let _serial = serial();
    let mut monitor = Parked::start(scoped_spawn, -1);
    monitor.wait_until_forked();
    assert!(
        exec_files::try_writing().is_none(),
        "an executable could be opened for write while a fork is pre-exec"
    );
    let paused = PausedStore::new();
    let directory = tempfile::tempdir().unwrap();
    let writer = publish(&paused, directory.path(), EXECUTABLE);
    monitor.release_and_reap();
    let mut source = paused.source_is_blocked_with_destination_open();
    source.write_all(&paused.bytes).unwrap();
    drop(source);
    run(&writer.join().unwrap());
    assert!(exec_files::try_writing().is_some());
}
