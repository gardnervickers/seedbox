use super::protocol::{receive, send, Request, Response, Result, Stop};
use super::{shm, wire};
use seedbox_machine::pv::transport::GuestWrite;
use std::{
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

pub(super) struct Worker {
    child: Child,
    stream: shm::Stream<ChildStdout, ChildStdin>,
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
            Ok(stream) => Ok(Self { child, stream }),
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
    fn attach(
        child: &mut Child,
        region: shm::Region,
    ) -> Result<shm::Stream<ChildStdout, ChildStdin>> {
        let mut input = child.stdin.take().ok_or("PV worker stdin unavailable")?;
        let mut output = child.stdout.take().ok_or("PV worker stdout unavailable")?;
        send(
            &mut input,
            &Request::Attach {
                capacity: shm::CAPACITY,
            },
        )?;
        match receive(&mut output)?.ok_or("PV worker closed its protocol")? {
            Response::Attached => Ok(shm::Stream::client(region, output, input)),
            Response::Error(error) => Err(error),
            _ => Err("PV worker returned unexpected attach response".into()),
        }
    }
    /// One request without trailers; `Run` goes through [`Worker::run`].
    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        if matches!(request, Request::Run { .. }) {
            return Err("PV run requires its frame trailer".into());
        }
        send(&mut self.stream, &request)?;
        match self.answer()? {
            Response::Stopped { .. } => Err("PV unexpected run response".into()),
            other => Ok(other),
        }
    }
    fn answer(&mut self) -> Result<Response> {
        match receive(&mut self.stream)?.ok_or("PV worker closed its protocol")? {
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
        send(&mut self.stream, &request)?;
        wire::send_frames(&mut self.stream, frames)?;
        match self.answer()? {
            Response::Stopped { time, stop, writes } => {
                Ok((time, stop, wire::receive_writes(&mut self.stream, writes)?))
            }
            _ => Err("PV worker returned unexpected run response".into()),
        }
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
impl Drop for Worker {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            eprintln!("{error}");
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
