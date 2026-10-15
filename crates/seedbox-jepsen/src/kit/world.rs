//! The fault controller an oracle drives: Seedbox's World, or a native shim
//! over a Docker-compatible engine socket so the same oracle can run against
//! plain containers as a control outside Seedbox. [`Ctl`] also remembers
//! which faults are in force, so the final-read discipline can undo them all.
use super::{evidence, http, Error, Result};
use seedbox_control_client::{Partition, Trigger, World};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, os::unix::net::UnixStream, path::PathBuf, time::Duration};

/// The native shim: container kill/start/pause/unpause through the engine API.
/// It has no partitions, triggers or simulated time.
#[derive(Debug, Clone)]
pub struct Native {
    pub socket: PathBuf,
}

impl Native {
    fn post(&self, path: &str) -> Result<()> {
        let stream = UnixStream::connect(&self.socket)
            .map_err(|e| Error(format!("engine {}: {e}", self.socket.display())))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        let (status, body) = http::exchange(stream, "POST", "localhost", path, None)?;
        if status >= 300 && status != 304 {
            let text = String::from_utf8_lossy(&body[..body.len().min(200)]).to_string();
            return Err(Error(format!("engine {path}: {status} {text}")));
        }
        Ok(())
    }
}

enum Backend {
    Seedbox(World),
    Native(Native),
}

/// A cut this controller made, healable once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    pub partition: Partition,
    pub left: Vec<String>,
    pub right: Vec<String>,
}

/// The fault state [`Ctl::heal_everything`] undoes.
#[derive(Debug, Clone, Default)]
pub struct InForce {
    pub paused_processes: BTreeSet<String>,
    pub paused_machines: BTreeSet<String>,
    pub stopped: BTreeSet<String>,
    pub crashed: BTreeSet<String>,
    pub cuts: Vec<Cut>,
}

/// The oracle's fault controller.
pub struct Ctl {
    backend: Backend,
    pub in_force: InForce,
}

/// Connect to the guest controller, or to the engine socket when `native` is set.
pub fn connect_world(native: Option<PathBuf>) -> Result<Ctl> {
    let backend = match native {
        Some(socket) => Backend::Native(Native { socket }),
        None => Backend::Seedbox(
            World::connect_default().map_err(|e| Error(format!("control socket: {e}")))?,
        ),
    };
    Ok(Ctl {
        backend,
        in_force: InForce::default(),
    })
}

impl Ctl {
    pub fn from_world(world: World) -> Self {
        Self {
            backend: Backend::Seedbox(world),
            in_force: InForce::default(),
        }
    }

    pub fn is_native(&self) -> bool {
        matches!(self.backend, Backend::Native(_))
    }

    /// The Seedbox World, when there is one.
    pub fn world(&mut self) -> Option<&mut World> {
        match &mut self.backend {
            Backend::Seedbox(world) => Some(world),
            Backend::Native(_) => None,
        }
    }

    fn seedbox(&mut self, what: &str) -> Result<&mut World> {
        match &mut self.backend {
            Backend::Seedbox(world) => Ok(world),
            Backend::Native(_) => Err(Error(format!("native mode has no {what}"))),
        }
    }

    /// Wait in simulated time through the controller (wall time natively).
    pub fn sleep(&mut self, seconds: f64) -> Result<()> {
        let duration = Duration::from_millis((seconds.max(0.0) * 1000.0).round() as u64);
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.sleep(duration)?;
            }
            Backend::Native(_) => std::thread::sleep(duration),
        }
        Ok(())
    }

    /// SIGKILL the application (natively: kill the container).
    pub fn stop(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.stop(target)?;
            }
            Backend::Native(native) => {
                native.post(&format!("/containers/{target}/kill?signal=KILL"))?
            }
        }
        self.in_force.stopped.insert(target.into());
        Ok(())
    }

    pub fn restart(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.restart(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/start"))?,
        }
        self.in_force.stopped.remove(target);
        Ok(())
    }

    pub fn pause_process(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.pause_process(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/pause"))?,
        }
        self.in_force.paused_processes.insert(target.into());
        Ok(())
    }

    pub fn resume_process(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.resume_process(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/unpause"))?,
        }
        self.in_force.paused_processes.remove(target);
        Ok(())
    }

    /// Freeze the whole machine (natively the same as a process pause).
    pub fn pause(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.pause(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/pause"))?,
        }
        self.in_force.paused_machines.insert(target.into());
        Ok(())
    }

    pub fn resume(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.resume(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/unpause"))?,
        }
        self.in_force.paused_machines.remove(target);
        Ok(())
    }

    /// Power-fail the machine: page cache and unflushed sectors are lost
    /// (natively a SIGKILL, which keeps the page cache).
    pub fn crash(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.crash(target)?;
            }
            Backend::Native(native) => {
                native.post(&format!("/containers/{target}/kill?signal=KILL"))?
            }
        }
        self.in_force.crashed.insert(target.into());
        self.in_force.paused_machines.remove(target);
        self.in_force.paused_processes.remove(target);
        self.in_force.stopped.remove(target);
        Ok(())
    }

    pub fn reboot(&mut self, target: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Seedbox(world) => {
                world.reboot(target)?;
            }
            Backend::Native(native) => native.post(&format!("/containers/{target}/start"))?,
        }
        self.in_force.crashed.remove(target);
        Ok(())
    }

    pub fn partition(
        &mut self,
        left: &[impl AsRef<str>],
        right: &[impl AsRef<str>],
    ) -> Result<Cut> {
        let left: Vec<String> = left.iter().map(|s| s.as_ref().to_string()).collect();
        let right: Vec<String> = right.iter().map(|s| s.as_ref().to_string()).collect();
        let partition = self.seedbox("partitions")?.partition(&left, &right)?;
        let cut = Cut {
            partition,
            left,
            right,
        };
        self.in_force.cuts.push(cut.clone());
        Ok(cut)
    }

    pub fn heal(&mut self, cut: &Cut) -> Result<()> {
        self.heal_partition(&cut.partition)
    }

    /// Heal a partition handle, including one a console trigger created.
    pub fn heal_partition(&mut self, partition: &Partition) -> Result<()> {
        self.seedbox("partitions")?.heal(partition)?;
        self.in_force.cuts.retain(|c| &c.partition != partition);
        Ok(())
    }

    /// The simulator's world document; `None` natively.
    pub fn inspect(&mut self) -> Result<Option<Map<String, Value>>> {
        match &mut self.backend {
            Backend::Seedbox(world) => Ok(Some(world.inspect()?)),
            Backend::Native(_) => Ok(None),
        }
    }

    pub fn arm_console_partition(
        &mut self,
        source: &str,
        marker: &[u8],
        left: &[impl AsRef<str>],
        right: &[impl AsRef<str>],
        window: f64,
    ) -> Result<Trigger> {
        let left: Vec<String> = left.iter().map(|s| s.as_ref().to_string()).collect();
        let right: Vec<String> = right.iter().map(|s| s.as_ref().to_string()).collect();
        let timeout = Duration::from_millis((window.max(0.001) * 1000.0).round() as u64);
        Ok(self
            .seedbox("console triggers")?
            .arm_console_partition(source, marker, &left, &right, timeout)?)
    }

    pub fn cancel_trigger(&mut self, trigger: &Trigger) -> Result<()> {
        self.seedbox("console triggers")?.cancel_trigger(trigger)?;
        Ok(())
    }

    /// A trigger's record in `inspect()["triggers"]`.
    pub fn trigger_status(&mut self, trigger: &Trigger) -> Result<Value> {
        let state = self.inspect()?.unwrap_or_default();
        state
            .get("triggers")
            .and_then(|t| t.get(trigger.id as usize - 1))
            .cloned()
            .ok_or_else(|| Error(format!("trigger {} missing from inspect", trigger.id)))
    }

    /// Undo every fault this controller still has in force: heal cuts,
    /// resume paused processes and machines, reboot crashed machines and
    /// restart stopped applications. Each step is recorded as evidence;
    /// a failed step is recorded and the rest still run.
    pub fn heal_everything(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        let mut record = |what: &str, target: &str, result: Result<()>| match result {
            Ok(()) => evidence(
                "fault",
                json!({"action": what, "targets": [target], "final": true}),
            ),
            Err(error) => {
                evidence(
                    "heal-failed",
                    json!({"action": what, "target": target, "error": error.0}),
                );
                failures.push(format!("{what} {target}: {error}"));
            }
        };
        for cut in std::mem::take(&mut self.in_force.cuts) {
            let result = self.heal(&cut);
            record("heal", &format!("{:?}|{:?}", cut.left, cut.right), result);
        }
        for target in self.in_force.paused_processes.clone() {
            let result = self.resume_process(&target);
            record("resume_process", &target, result);
        }
        for target in self.in_force.paused_machines.clone() {
            let result = self.resume(&target);
            record("resume", &target, result);
        }
        for target in self.in_force.crashed.clone() {
            let result = self.reboot(&target);
            record("reboot", &target, result);
        }
        for target in self.in_force.stopped.clone() {
            let result = self.restart(&target);
            record("restart", &target, result);
        }
        failures
    }
}
