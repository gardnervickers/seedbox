use super::protocol::{receive, send, Request, Response, Result, Stop};
use super::wire;
use crate::pv::transport::GuestWrite;
use std::{
    io::{BufReader, BufWriter},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

pub(super) struct Worker {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Worker {
    pub(super) fn spawn(executable: &Path) -> Result<Self> {
        let mut command = Command::new(executable);
        if seedbox_machine::process_lease::inherit(&mut command)
            .map_err(|error| format!("PV worker execution lease: {error}"))?
        {
            command.arg(seedbox_machine::process_lease::ENTRY);
        }
        command
            .arg("--seedbox-pv-worker-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[allow(
            clippy::disallowed_methods,
            reason = "execution-worker process only; it writes its one executable copy (runtime extraction) before any machine or spawn exists"
        )]
        let spawned = command.spawn();
        // Child-only descriptor copies must not survive in this Command.
        drop(command);
        let mut child = spawned.map_err(|e| format!("PV worker {}: {e}", executable.display()))?;
        let input = BufWriter::new(child.stdin.take().ok_or("PV worker stdin unavailable")?);
        let output = BufReader::new(child.stdout.take().ok_or("PV worker stdout unavailable")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }
    /// One request without trailers; `Run` goes through [`Worker::run`].
    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        if matches!(request, Request::Run { .. }) {
            return Err("PV run requires its frame trailer".into());
        }
        send(&mut self.input, &request)?;
        match self.answer()? {
            Response::Stopped { .. } => Err("PV unexpected run response".into()),
            other => Ok(other),
        }
    }
    fn answer(&mut self) -> Result<Response> {
        match receive(&mut self.output)?.ok_or("PV worker closed its protocol")? {
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
        send(&mut self.input, &request)?;
        wire::send_frames(&mut self.input, frames)?;
        match self.answer()? {
            Response::Stopped { time, stop, writes } => {
                Ok((time, stop, wire::receive_writes(&mut self.output, writes)?))
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
