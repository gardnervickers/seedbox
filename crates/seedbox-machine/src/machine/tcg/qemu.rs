//! Private QEMU process lifecycle and QMP transport. Execution uses QMP only;
//! test builds additionally open a GDB socket for independent state inspection.

use super::{Boot, Error, Manifest, Timestamp};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// QEMU's RISC-V virt board maps main RAM here.
const RAM_BASE: u64 = 0x8000_0000;
// Host transport/startup limits; these do not advance simulation time.
const TIMEOUT: Duration = Duration::from_secs(30);
// Match the private worker bound; byte arrays expand when encoded as JSON.
const MAX_PACKET: usize = 8 * 1024 * 1024;

#[cfg(test)]
#[path = "inspector.rs"]
mod inspector;
#[cfg(test)]
use inspector::{decode_hex, Gdb};

pub(super) struct Process {
    child: Child,
    directory: TempDir,
    qmp: Qmp,
    // Tests inspect register/RAM state independently; GDB never drives execution.
    #[cfg(test)]
    gdb: Gdb,
}

impl Process {
    pub(super) fn execute(
        &mut self,
        start: Timestamp,
        upto: Timestamp,
        execution: crate::execution::ExecutionConfig,
    ) -> Result<Value, Error> {
        self.qmp.stopped = false;
        self.qmp.command(
            "seedbox-run",
            json!({
                "time": start.as_nanos(), "budget": upto.as_nanos() - start.as_nanos(),
                "factor": execution.factor_permille(),
            }),
        )?;
        // STOP acknowledges the CPU is stopped. It may precede the command's
        // reply, so Qmp also remembers it while processing ordinary replies.
        while !self.qmp.stopped {
            self.qmp.read()?;
        }
        self.qmp.command("query-seedbox-run", json!({}))
    }

    pub(super) fn drain_guest(&mut self) -> Result<Vec<crate::pv::transport::GuestWrite>, Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Kind {
            Console,
            Lifecycle,
        }
        #[derive(serde::Deserialize)]
        struct Write {
            kind: Kind,
            time: u64,
            bytes: Vec<u8>,
        }
        let writes: Vec<Write> =
            serde_json::from_value(self.qmp.command("seedbox-drain-lifecycle", json!({}))?)?;
        use crate::pv::transport::{GuestWrite, LifecycleWrite};
        Ok(writes
            .into_iter()
            .map(|write| {
                let payload = LifecycleWrite {
                    time: write.time,
                    bytes: write.bytes,
                };
                match write.kind {
                    Kind::Console => GuestWrite::Console(payload),
                    Kind::Lifecycle => GuestWrite::Lifecycle(payload),
                }
            })
            .collect())
    }

    // Existing device-focused tests intentionally inspect lifecycle bytes only.
    #[cfg(test)]
    pub(super) fn drain_lifecycle(
        &mut self,
    ) -> Result<Vec<crate::pv::transport::LifecycleWrite>, Error> {
        Ok(self
            .drain_guest()?
            .into_iter()
            .filter_map(|write| match write {
                crate::pv::transport::GuestWrite::Lifecycle(write) => Some(write),
                crate::pv::transport::GuestWrite::Console(_) => None,
            })
            .collect())
    }

    pub(super) fn advance(&mut self, at: Timestamp) -> Result<(), Error> {
        self.qmp
            .command("seedbox-advance", json!({"time": at.as_nanos()}))?;
        Ok(())
    }

    pub(super) fn input(&mut self, bytes: &[u8]) -> Result<bool, Error> {
        self.qmp
            .command("seedbox-input-lifecycle", json!({"bytes": bytes}))?["runnable"]
            .as_bool()
            .ok_or_else(|| Error("invalid lifecycle input response".into()))
    }

    pub(super) fn console(&self) -> Result<Vec<u8>, Error> {
        Ok(fs::read(self.directory.path().join("console"))?)
    }

    pub(super) fn terminate(&mut self) -> Result<(), Error> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>, Error> {
        // GDB mADDR,LENGTH reads hex-encoded memory; both arguments are hex.
        decode_hex(&self.gdb.request(&format!("m{address:x},{length:x}"))?)
    }

    pub(super) fn start(
        qemu: &Path,
        manifest: &Manifest,
        image: Option<&[u8]>,
        seed: u64,
    ) -> Result<Self, Error> {
        // macOS has short Unix socket paths, even if TMPDIR itself is long.
        let directory = tempfile::Builder::new()
            .prefix("seedbox-tcg-")
            .tempdir_in("/tmp")?;
        let qmp_path = directory.path().join("qmp");
        let stderr = fs::File::create(directory.path().join("stderr"))?;
        let console = directory.path().join("console");
        fs::write(&console, [])?;
        let mut command = Command::new(qemu);
        command.args([
            "-machine",
            "virt",
            "-accel",
            "tcg,thread=single",
            "-cpu",
            "rv64",
            "-smp",
            "1",
            "-nodefaults",
            "-display",
            "none",
            "-monitor",
            "none",
            "-S",           // Start stopped; only seedbox-run may execute code.
            "-no-shutdown", // Retain the stopped guest so it can be inspected/snapshotted.
            "-rtc",
            "base=2000-01-01T00:00:00,clock=vm", // No host wall-clock input.
            "-icount",
            "shift=0,align=off,sleep=off", // Nominal icount base; bounded execution applies fractional work pricing.
            "-global",
            "migration.store-global-state=off", // Lifecycle is saved separately.
            "-global",
            "virtio-mmio.ioeventfd=off", // Queue kicks execute at their guest MMIO instruction.
            "-global",
            "serial.seedbox-console=on", // Only the reference UART opts into canonical capture.
        ]);
        command.arg("-seed").arg(seed.to_string());
        command.arg("-m").arg(format!("{}M", manifest.memory_mib));
        // A dedicated standard virtio-serial port keeps lifecycle requests out
        // of the guest's interactive console. Its private backend owns the
        // early-boot backlog and migrates it with the rest of the machine.
        command.args([
            "-device",
            "virtio-serial-device,id=lifecycle-bus",
            "-device",
            "seedbox-lifecycle-port,bus=lifecycle-bus.0,name=seedbox-lifecycle",
        ]);
        command
            .arg("-serial")
            .arg(format!("file:{}", console.display()));
        match &manifest.boot {
            #[cfg(test)]
            Boot::Program(program) => {
                let path = directory.path().join("program");
                fs::write(&path, program)?;
                command.args(["-bios", "none"]).arg("-device").arg(format!(
                    "loader,file={},addr={RAM_BASE:#x},cpu-num=0",
                    path.display()
                ));
            }
            Boot::Linux {
                kernel,
                initramfs,
                firmware,
                command_line,
            } => {
                for (name, bytes, flag) in [
                    ("kernel", kernel, "-kernel"),
                    ("initramfs", initramfs, "-initrd"),
                    ("firmware", firmware, "-bios"),
                ] {
                    let path = directory.path().join(name);
                    fs::write(&path, bytes)?;
                    command.arg(flag).arg(path);
                }
                command.arg("-append").arg(command_line);
            }
        }
        command
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp_path.display()));
        #[cfg(test)]
        command.arg("-gdb").arg(format!(
            "unix:{},server=on,wait=off",
            directory.path().join("gdb").display()
        ));
        if image.is_some() {
            command.args(["-incoming", "defer"]);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|error| Error(format!("start {}: {error}", qemu.display())))?;
        let setup = (|| {
            let mut qmp = Qmp {
                stream: BufReader::new(connect(&qmp_path, &mut child)?),
                stopped: false,
            };
            if qmp.read()?.get("QMP").is_none() {
                return Err(Error("missing QMP greeting".into()));
            }
            qmp.command("qmp_capabilities", json!({}))?;
            // Fail during setup if the caller supplied ordinary, unpatched QEMU.
            qmp.command("query-seedbox-run", json!({}))?;
            if let Some(image) = image {
                let path = directory.path().join("incoming");
                fs::write(&path, image)?;
                qmp.command(
                    "migrate-incoming",
                    json!({"uri": format!("file:{}", path.display())}),
                )?;
                let started = Instant::now();
                loop {
                    let status = qmp.command("query-status", json!({}))?;
                    match status["status"].as_str() {
                        Some("paused") => break,
                        Some("inmigrate") if started.elapsed() < TIMEOUT => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        _ => {
                            return Err(Error(format!(
                                "restore did not reach a stopped state: {status}"
                            )));
                        }
                    }
                }
            }
            Ok(qmp)
        })();
        match setup {
            Ok(qmp) => {
                // Tests use a read-only GDB inspector; failed setup reaps QEMU.
                #[cfg(test)]
                let gdb = match connect(&directory.path().join("gdb"), &mut child) {
                    Ok(socket) => Gdb(socket),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                };
                Ok(Self {
                    child,
                    directory,
                    qmp,
                    #[cfg(test)]
                    gdb,
                })
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let diagnostics =
                    fs::read_to_string(directory.path().join("stderr")).unwrap_or_default();
                Err(Error(format!(
                    "{error}; {}: {}",
                    qemu.display(),
                    diagnostics.trim()
                )))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn stopped_result(&mut self) -> Result<Value, Error> {
        self.qmp.command("query-seedbox-run", json!({}))
    }

    #[cfg(test)]
    pub(super) fn registers(&mut self) -> Result<[u64; 33], Error> {
        // GDB core registers are x0..x31 then PC, each eight little-endian bytes.
        let bytes = decode_hex(&self.gdb.request("g")?)?;
        if bytes.len() != 33 * 8 {
            return Err(Error("unexpected RV64 register layout".into()));
        }
        let mut registers = [0; 33];
        for (register, bytes) in registers.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *register = u64::from_le_bytes(*bytes);
        }
        Ok(registers)
    }

    pub(super) fn snapshot(&mut self) -> Result<Vec<u8>, Error> {
        let path = self.directory.path().join("snapshot");
        self.qmp.command(
            "migrate",
            json!({"uri": format!("file:{}", path.display())}),
        )?;
        let started = Instant::now();
        loop {
            let status = self.qmp.command("query-migrate", json!({}))?;
            match status["status"].as_str() {
                Some("completed") => {
                    // Migration's worker can publish completion before its
                    // main-thread cleanup places the source in POSTMIGRATE.
                    let state = self.qmp.command("query-status", json!({}))?;
                    if state["status"] != "postmigrate" {
                        if started.elapsed() < TIMEOUT {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        return Err(Error(format!("snapshot source did not stop: {state}")));
                    }
                    let image = fs::read(path).map_err(Error::from);
                    // Clear completed-migration bookkeeping while remaining
                    // stopped, even if reading the snapshot failed. No guest
                    // instruction or clock advancement is needed for recapture.
                    self.qmp.command("seedbox-capture-complete", json!({}))?;
                    return image;
                }
                Some("failed" | "cancelled") => {
                    return Err(Error(format!("snapshot failed: {status}")));
                }
                _ if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    self.qmp.command("migrate_cancel", json!({}))?;
                    return Err(Error("snapshot timed out".into()));
                }
            }
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn connect(path: &Path, child: &mut Child) -> Result<UnixStream, Error> {
    let started = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                stream.set_read_timeout(Some(TIMEOUT))?;
                stream.set_write_timeout(Some(TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) => {
                if child.try_wait()?.is_some() || started.elapsed() >= TIMEOUT {
                    return Err(Error(format!("connect {}: {error}", path.display())));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

struct Qmp {
    stream: BufReader<UnixStream>,
    stopped: bool,
}
impl Qmp {
    fn read(&mut self) -> Result<Value, Error> {
        let mut line = String::new();
        self.stream
            .by_ref()
            .take(MAX_PACKET as u64)
            .read_line(&mut line)?;
        if !line.ends_with('\n') {
            return Err(Error("missing or oversized QMP response".into()));
        }
        let value: Value = serde_json::from_str(&line)?;
        self.stopped |= value["event"] == "STOP";
        Ok(value)
    }
    fn command(&mut self, name: &str, arguments: Value) -> Result<Value, Error> {
        writeln!(
            self.stream.get_mut(),
            "{}",
            json!({"execute": name, "arguments": arguments})
        )?;
        loop {
            let response = self.read()?;
            if response.get("event").is_some() {
                continue;
            }
            if let Some(value) = response.get("return") {
                return Ok(value.clone());
            }
            return Err(Error(format!("QMP {name}: {response}")));
        }
    }
}
