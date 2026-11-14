use super::protocol::{receive, send, Request, Response, Result, Stop};
use super::{shm, wire};
use seedbox_machine::pv::transport::GuestWrite;
use std::{
    io::BufReader,
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard},
};
use tempfile::TempDir;

/// The attached region, buffered for reads; writes go to the stream inside.
type Stream = BufReader<shm::Stream<ChildStdout, ChildStdin>>;

const LOST: &str = "PV worker is gone";

pub(super) struct Worker {
    child: Child,
    stream: Stream,
    /// An exchange failed part-way, so the stream may hold half of one. The
    /// worker was terminated and every later request fails.
    lost: bool,
}
impl Worker {
    pub(super) fn spawn(executable: &Path) -> Result<Self> {
        let (region, descriptor) = shm::Region::create(shm::CAPACITY)?;
        let mut command = Command::new(executable);
        if seedbox_machine::process_lease::inherit(&mut command)
            .map_err(|error| format!("PV worker execution lease: {error}"))?
        {
            command.arg(seedbox_machine::process_lease::ENTRY);
        }
        shm::inherit(descriptor, &mut command)?;
        command
            .arg("--seedbox-pv-worker-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // Through the process-wide exec-file scope: a test or a server in this
        // process may be writing an executable when this forks.
        let spawned = seedbox_exec::spawn(&mut command);
        // Child-only descriptor copies must not survive in this Command.
        drop(command);
        let mut child = spawned.map_err(|e| format!("PV worker {}: {e}", executable.display()))?;
        match Self::attach(&mut child, region) {
            Ok(stream) => Ok(Self {
                child,
                stream,
                lost: false,
            }),
            Err(error) => {
                // Not yet a Worker, so Drop would not reap it.
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }
    /// Start-up on the pipes: attach the shared region, which then carries
    /// every message while the pipes carry wakeups.
    fn attach(child: &mut Child, region: shm::Region) -> Result<Stream> {
        let mut input = child.stdin.take().ok_or("PV worker stdin unavailable")?;
        let mut output = child.stdout.take().ok_or("PV worker stdout unavailable")?;
        send(
            &mut input,
            &Request::Attach {
                capacity: shm::CAPACITY,
            },
        )?;
        match receive(&mut output)?.ok_or("PV worker closed its protocol")? {
            Response::Attached => Ok(BufReader::new(shm::Stream::client(region, output, input))),
            Response::Error(error) => Err(error),
            _ => Err("PV worker returned unexpected attach response".into()),
        }
    }
    /// One request and its whole answer. If the exchange itself fails the
    /// protocol's position is unknown and the worker is lost. An error the
    /// worker answers with is a complete exchange and comes back as a value.
    fn exchange<T>(&mut self, exchange: impl FnOnce(&mut Stream) -> Result<T>) -> Result<T> {
        if self.lost {
            return Err(LOST.into());
        }
        let result = exchange(&mut self.stream);
        if result.is_err() {
            self.lose();
        }
        result
    }
    fn lose(&mut self) {
        self.lost = true;
        if let Err(error) = self.terminate() {
            eprintln!("{error}");
        }
    }
    /// One request without trailers; `Run` goes through [`Worker::run`].
    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        if matches!(request, Request::Run { .. }) {
            return Err("PV run requires its frame trailer".into());
        }
        let response = self.exchange(|stream| {
            send(stream.get_mut(), &request)?;
            match answer(stream)? {
                Response::Stopped { .. } => Err("PV unexpected run response".into()),
                other => Ok(other),
            }
        })?;
        match response {
            Response::Error(error) => Err(error),
            other => Ok(other),
        }
    }
    /// Send a `Run` with its host frames, then receive the stop and the
    /// guest's writes in emission order.
    pub(super) fn run(
        &mut self,
        mut request: Request,
        frames: &[Vec<u8>],
    ) -> Result<(u64, Stop, Vec<GuestWrite>)> {
        let Request::Run { frames: count, .. } = &mut request else {
            return Err("PV run requires a Run request".into());
        };
        *count = frames.len() as u64;
        self.exchange(|stream| {
            send(stream.get_mut(), &request)?;
            wire::send_frames(stream.get_mut(), frames)?;
            match answer(stream)? {
                Response::Stopped { time, stop, writes } => {
                    Ok(Ok((time, stop, wire::receive_writes(stream, writes)?)))
                }
                Response::Error(error) => Ok(Err(error)),
                _ => Err("PV worker returned unexpected run response".into()),
            }
        })?
    }
    pub(super) fn terminate(&mut self) -> Result<()> {
        if self
            .child
            .try_wait()
            .map_err(|e| format!("PV worker status: {e}"))?
            .is_none()
        {
            self.child
                .kill()
                .map_err(|e| format!("PV worker terminate: {e}"))?;
        }
        self.child
            .wait()
            .map_err(|e| format!("PV worker reap: {e}"))?;
        Ok(())
    }
}
fn answer(stream: &mut Stream) -> Result<Response> {
    receive(stream)?.ok_or_else(|| "PV worker closed its protocol".into())
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("{error}");
        }
    }
}

/// A worker, shared by the machine running in it and the retained snapshots
/// it holds. It exits when the last of them is dropped, so a worker whose
/// machine has left stays, parked, for as long as a snapshot can rewind it.
///
/// One request is in flight at a time. Whoever needs the worker while a
/// machine runs in it (a snapshot being restored, exported or dropped) waits
/// for that run to stop.
struct Slot {
    seat: Mutex<Seat>,
}
struct Seat {
    worker: Worker,
    /// A machine is running in the worker; a parked worker has none.
    occupied: bool,
}
impl Slot {
    fn seat(&self) -> MutexGuard<'_, Seat> {
        self.seat.lock().unwrap_or_else(|poisoned| {
            // A panic inside an exchange left the protocol anywhere.
            let mut seat = poisoned.into_inner();
            seat.worker.lose();
            seat
        })
    }
}

/// The machine's hold on the worker it runs in. Dropping it leaves the
/// worker, which then exits unless a retained snapshot refers to it.
pub(super) struct Occupant {
    slot: Arc<Slot>,
    /// The machine's scratch, where the snapshots it retains are exported.
    /// A machine rewound into a parked worker brings its own: the worker
    /// reads nothing beneath any scratch after it has booted.
    directory: Arc<TempDir>,
}
impl Occupant {
    /// `directory` is the machine's scratch.
    pub(super) fn spawn(executable: &Path, directory: &Arc<TempDir>) -> Result<Self> {
        Ok(Self {
            slot: Arc::new(Slot {
                seat: Mutex::new(Seat {
                    worker: Worker::spawn(executable)?,
                    occupied: true,
                }),
            }),
            directory: directory.clone(),
        })
    }
    /// The worker's process.
    // Only the tests of a backend that retains ask.
    #[cfg(all(test, target_arch = "aarch64", target_os = "macos"))]
    pub(super) fn process(&self) -> u32 {
        self.slot.seat().worker.child.id()
    }
    /// As [`Worker::call`].
    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        self.slot.seat().worker.call(request)
    }
    /// As [`Worker::run`].
    pub(super) fn run(
        &mut self,
        request: Request,
        frames: &[Vec<u8>],
    ) -> Result<(u64, Stop, Vec<GuestWrite>)> {
        self.slot.seat().worker.run(request, frames)
    }
    /// Keep the stopped machine in the worker as a retained snapshot. `Err`
    /// carries the bytes of RAM pages the worker's snapshots hold when
    /// another would take them over `limit`; nothing was kept.
    pub(super) fn snapshot(
        &mut self,
        limit: u64,
        verify: bool,
    ) -> Result<std::result::Result<Taken, u64>> {
        let response = self.call(Request::Snapshot { limit, verify })?;
        match response {
            Response::Snapshotted { id, pages, .. } => Ok(Ok(Taken {
                version: Version {
                    slot: self.slot.clone(),
                    directory: self.directory.clone(),
                    id,
                },
                pages,
            })),
            Response::Full { held } => Ok(Err(held)),
            _ => Err("PV unexpected retained snapshot response".into()),
        }
    }
    /// Leave the worker and, unless a retained snapshot refers to it, see it
    /// exit.
    pub(super) fn leave(self) -> Result<()> {
        let slot = self.slot.clone();
        drop(self);
        match Arc::into_inner(slot) {
            Some(slot) => slot
                .seat
                .into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .worker
                .terminate(),
            None => Ok(()),
        }
    }
}
impl Drop for Occupant {
    fn drop(&mut self) {
        self.slot.seat().occupied = false;
    }
}
/// A retained snapshot that was taken.
pub(super) struct Taken {
    pub(super) version: Version,
    /// RAM pages it had to copy.
    pub(super) pages: u64,
}

/// A retained snapshot in its worker. Dropping it frees, there, the pages no
/// other snapshot shares.
pub(super) struct Version {
    slot: Arc<Slot>,
    /// Scratch of the machine that took the snapshot, kept for its exports.
    directory: Arc<TempDir>,
    id: u64,
}
impl Version {
    /// Occupy the parked worker and make its machine the one this snapshot
    /// was taken from, returning the RAM pages rewritten. `None`, and nothing
    /// done, if a machine is running in the worker.
    ///
    /// `directory` is the scratch of the machine that continues in the
    /// worker. A failed rewind leaves the worker parked and able to be
    /// rewound again.
    pub(super) fn rewind(
        &self,
        verify: bool,
        directory: &Arc<TempDir>,
    ) -> Result<Option<(Occupant, u64)>> {
        let mut seat = self.slot.seat();
        if seat.occupied {
            return Ok(None);
        }
        let request = Request::Rewind {
            id: self.id,
            verify,
        };
        match seat.worker.call(request)? {
            Response::Rewound { pages } => {
                seat.occupied = true;
                let occupant = Occupant {
                    slot: self.slot.clone(),
                    directory: directory.clone(),
                };
                Ok(Some((occupant, pages)))
            }
            _ => Err("PV unexpected rewind response".into()),
        }
    }
    /// Write the snapshot to a new file at `path`, byte for byte the image a
    /// capture of the machine would have written when it was taken.
    pub(super) fn export(&self, path: &Path) -> Result<()> {
        let request = Request::Export {
            id: self.id,
            path: path.to_owned(),
        };
        match self.slot.seat().worker.call(request)? {
            Response::Exported => Ok(()),
            _ => Err("PV unexpected export response".into()),
        }
    }
    /// Where a caller with no scratch of its own exports this snapshot: the
    /// scratch of the machine that took it.
    pub(super) fn scratch(&self) -> &Path {
        self.directory.path()
    }
}
impl Drop for Version {
    fn drop(&mut self) {
        // The last reference: the worker exits with its slot instead.
        if Arc::strong_count(&self.slot) == 1 {
            return;
        }
        let mut seat = self.slot.seat();
        if seat.worker.lost {
            return;
        }
        if let Err(error) = seat.worker.call(Request::Release(self.id)) {
            eprintln!("PV retained snapshot release: {error}");
        }
    }
}

/// Spawn `executable` as a worker and return its errors for requests that
/// need a boot, all through the shared region.
#[cfg(feature = "test-support")]
pub(crate) fn preboot_errors(executable: &Path) -> Result<Vec<Option<String>>> {
    let mut worker = Worker::spawn(executable)?;
    let unused = std::path::PathBuf::from("/nonexistent");
    let run = Request::Run {
        now: 0,
        upto: 1,
        syscall_cost_ns: 1,
        shutdown: false,
        lifecycle: Vec::new(),
        frames: 0,
        breaks: Vec::new(),
        resume: false,
    };
    Ok(vec![
        worker.call(Request::Capture(unused.clone())).err(),
        worker.call(Request::ExportDisk(unused)).err(),
        worker.run(run, &[vec![0; 60]]).err(),
    ])
}
