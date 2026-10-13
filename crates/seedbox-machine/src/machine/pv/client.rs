use super::protocol::{receive, send, Request, Response, Result};
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
        let mut child = Command::new(executable)
            .arg("--seedbox-pv-worker-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("PV worker {}: {e}", executable.display()))?;
        let input = BufWriter::new(child.stdin.take().ok_or("PV worker stdin unavailable")?);
        let output = BufReader::new(child.stdout.take().ok_or("PV worker stdout unavailable")?);
        Ok(Self {
            child,
            input,
            output,
        })
    }
    pub(super) fn call(&mut self, request: Request) -> Result<Response> {
        send(&mut self.input, &request)?;
        let answer = receive(&mut self.output)?.ok_or("PV worker closed its protocol")?;
        match answer {
            Response::Error(error) => Err(error),
            other => Ok(other),
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
