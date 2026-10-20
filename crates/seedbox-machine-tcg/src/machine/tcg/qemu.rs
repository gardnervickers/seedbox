//! Private QEMU process lifecycle and QMP transport. Execution uses QMP only;
//! test builds additionally open a GDB socket for independent state inspection.

use super::{checked_scratch_root, path_text, Boot, Error, Manifest, Timestamp};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod state;
pub(super) use state::TimerSetup;

// Host transport/startup limits; these do not advance simulation time.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Test builds only: the longest host time one bounded run may take before
/// the adapter kills QEMU and fails the run. Production waits without a host
/// limit (see [`Process::execute`]). A run normally takes well under a second;
/// the slowest legitimate one known, a 10 ms quantum executed entirely from
/// outside RAM, takes about 30 s, so 600 s only catches a QEMU that never
/// stops, for example a livelock in its run loop.
#[cfg(any(test, feature = "test-support"))]
const TEST_RUN_WATCHDOG: Duration = Duration::from_secs(600);
// Match the private worker bound; byte arrays expand when encoded as JSON.
const MAX_PACKET: usize = 8 * 1024 * 1024;

#[cfg(any(test, feature = "test-support"))]
mod inspector;
#[cfg(any(test, feature = "test-support"))]
use inspector::{decode_hex, Gdb};

/// Host inputs of the Seedbox block device for one QEMU process.
pub(super) struct DiskInput {
    pub(super) sectors: u64,
    /// Base image file; absent for a zeroed disk.
    pub(super) base: Option<PathBuf>,
    /// SBXSTOR1 overlays for a new boot; absent when migration restores them.
    pub(super) overlay: Option<PathBuf>,
}

/// Frames per `seedbox-net-receive` command, the device's limit.
const RECEIVE_BATCH: usize = 64;
/// Frames per `seedbox-net-drain` chunk, at most.
const DRAIN_BATCH: usize = 256;

/// Lowercase hexadecimal is the private QMP frame encoding.
pub(super) use seedbox_machine::machine::hex;

pub(super) fn unhex(text: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let text = text.as_bytes();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| Some(digit(high)? << 4 | digit(low)?))
        .collect()
}

/// Append one validated drain chunk to `frames`; returns its `more` flag.
/// Each frame must be in Ethernet bounds, lowercase hex, and no earlier than
/// the frame before it.
pub(super) fn drain_chunk(
    chunk: &Value,
    frames: &mut Vec<crate::pv::transport::LifecycleWrite>,
) -> Result<bool, Error> {
    let invalid = || Error(format!("invalid seedbox-net-drain chunk: {chunk}"));
    let items = chunk["frames"].as_array().ok_or_else(invalid)?;
    let more = chunk["more"].as_bool().ok_or_else(invalid)?;
    if items.len() > DRAIN_BATCH || (more && items.is_empty()) {
        return Err(invalid());
    }
    for item in items {
        let time = item["time"].as_u64().ok_or_else(invalid)?;
        let bytes = item["bytes"].as_str().and_then(unhex).ok_or_else(invalid)?;
        if !crate::pv::frame_in_bounds(&bytes)
            || frames.last().is_some_and(|previous| previous.time > time)
        {
            return Err(invalid());
        }
        frames.push(crate::pv::transport::LifecycleWrite { time, bytes });
    }
    Ok(more)
}

/// QEMU `-device` option value: a comma is escaped by doubling it.
fn option_path(path: &Path) -> Result<String, Error> {
    Ok(path_text(path, "device option path")?.replace(',', ",,"))
}

pub(super) struct Process {
    child: Child,
    directory: TempDir,
    qmp: Qmp,
    // Tests inspect register/RAM state independently; GDB never drives execution.
    #[cfg(any(test, feature = "test-support"))]
    gdb: Gdb,
    /// Host bound of one bounded run in test builds; see [`TEST_RUN_WATCHDOG`].
    #[cfg(any(test, feature = "test-support"))]
    watchdog: Duration,
}

impl Process {
    /// Execute one bounded run and return its stopped result.
    ///
    /// The run ends after its simulated budget however long that takes on the
    /// host: QEMU translates code fetched outside RAM one instruction at a
    /// time, thousands of times slower than cached code. Waiting for the run
    /// therefore has no host read timeout, so its result never depends on
    /// host speed. A dead QEMU still ends the wait: its socket reads as EOF.
    /// Test builds kill QEMU after [`TEST_RUN_WATCHDOG`] instead of waiting
    /// forever, so a livelocked run fails its test rather than hanging it.
    pub(super) fn execute(
        &mut self,
        start: Timestamp,
        upto: Timestamp,
        execution: crate::execution::ExecutionConfig,
    ) -> Result<Value, Error> {
        #[cfg(any(test, feature = "test-support"))]
        {
            let limit = self.watchdog;
            let watchdog = Watchdog::arm(self.qmp.stream.get_ref(), limit)?;
            let result = self.execute_unwatched(start, upto, execution);
            if watchdog.disarm() {
                let _ = self.terminate();
                return Err(Error(format!(
                    "test run watchdog: QEMU did not stop a bounded run within {limit:?}; killed it"
                )));
            }
            result
        }
        #[cfg(not(any(test, feature = "test-support")))]
        self.execute_unwatched(start, upto, execution)
    }

    fn execute_unwatched(
        &mut self,
        start: Timestamp,
        upto: Timestamp,
        execution: crate::execution::ExecutionConfig,
    ) -> Result<Value, Error> {
        self.qmp.execute_bounded(start, upto, execution)
    }

    /// Replace the test watchdog bound of later bounded runs.
    #[cfg(test)]
    pub(super) fn set_run_watchdog(&mut self, limit: Duration) {
        self.watchdog = limit;
    }

    /// QEMU's process ID, for tests that stop it from outside.
    #[cfg(test)]
    pub(super) fn pid(&self) -> u32 {
        self.child.id()
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
                crate::pv::transport::GuestWrite::Console(_)
                | crate::pv::transport::GuestWrite::Frame(_) => None,
            })
            .collect())
    }

    /// Export the stopped Seedbox block device's overlays. Exporting never
    /// flushes, discards or executes guest work.
    pub(super) fn export_disk(&mut self) -> Result<Vec<u8>, Error> {
        let export = tempfile::Builder::new()
            .prefix("disk-export-")
            .tempdir_in(self.directory.path())?;
        let path = export.path().join("overlays");
        self.qmp.command(
            "seedbox-disk-export",
            json!({"path": path_text(&path, "disk export path")?}),
        )?;
        let bytes = fs::read(&path)?;
        Ok(bytes)
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

    /// Offer received frames, in order, to the stopped NIC in QMP batches of
    /// [`RECEIVE_BATCH`]; returns whether a CPU is runnable afterwards. A
    /// device failure, including a malformed receive ring, is an error.
    pub(super) fn net_receive(&mut self, frames: &[Vec<u8>]) -> Result<bool, Error> {
        let mut runnable = false;
        for batch in frames.chunks(RECEIVE_BATCH) {
            let hex: Vec<String> = batch.iter().map(|frame| hex(frame)).collect();
            runnable = self
                .qmp
                .command("seedbox-net-receive", json!({"frames": hex}))?["runnable"]
                .as_bool()
                .ok_or_else(|| Error("invalid seedbox-net-receive response".into()))?;
        }
        Ok(runnable)
    }

    /// Drain every frame the stopped NIC transmitted, oldest first, in as
    /// many QMP chunks as it takes; there is no overall count limit.
    pub(super) fn net_drain(&mut self) -> Result<Vec<crate::pv::transport::LifecycleWrite>, Error> {
        let mut frames = Vec::new();
        loop {
            let chunk = self.qmp.command("seedbox-net-drain", json!({}))?;
            let more = drain_chunk(&chunk, &mut frames)?;
            if !more {
                return Ok(frames);
            }
        }
    }

    pub(super) fn console(&self) -> Result<Vec<u8>, Error> {
        Ok(fs::read(self.directory.path().join("console"))?)
    }

    pub(super) fn terminate(&mut self) -> Result<(), Error> {
        self.child.kill()?;
        self.child.wait()?;
        Ok(())
    }

    /// Read guest memory at a CPU virtual address (physical while paging is off).
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
        disk: Option<&DiskInput>,
    ) -> Result<Self, Error> {
        let directory = Self::boot_directory(None)?;
        Self::start_in_directory(qemu, manifest, image, seed, disk, directory, None)
    }

    pub(super) fn start_with_scratch(
        qemu: &Path,
        manifest: &Manifest,
        image: Option<&[u8]>,
        seed: u64,
        disk: Option<&DiskInput>,
        scratch_root: Option<&Path>,
    ) -> Result<Self, Error> {
        match scratch_root {
            None => Self::start(qemu, manifest, image, seed, disk),
            Some(root) => {
                let directory = Self::boot_directory(Some(root))?;
                Self::start_in_directory(qemu, manifest, image, seed, disk, directory, None)
            }
        }
    }

    /// Supply timer authority to the same owned setup before incoming restore.
    /// The caller owns boot/domain selection and exact factory-policy validation.
    pub(super) fn start_with_timer_policy(
        qemu: &Path,
        manifest: &Manifest,
        image: Option<&[u8]>,
        seed: u64,
        disk: Option<&DiskInput>,
        scratch_root: Option<&Path>,
        policy: &TimerSetup,
    ) -> Result<Self, Error> {
        let directory = Self::boot_directory(scratch_root)?;
        Self::start_in_directory(qemu, manifest, image, seed, disk, directory, Some(policy))
    }

    fn boot_directory(scratch_root: Option<&Path>) -> Result<TempDir, Error> {
        match scratch_root {
            None => {
                // Preserve the standalone default and its existing error path.
                let root = checked_scratch_root(Path::new("/tmp"))?;
                Ok(tempfile::Builder::new()
                    .prefix("seedbox-tcg-")
                    .tempdir_in(root)?)
            }
            Some(root) => {
                let root = checked_scratch_root(root)?;
                tempfile::Builder::new()
                    .prefix("boot-")
                    .tempdir_in(&root)
                    .map_err(|e| Error(format!("TCG boot directory in {root:?}: {e}")))
            }
        }
    }

    fn start_in_directory(
        qemu: &Path,
        manifest: &Manifest,
        image: Option<&[u8]>,
        seed: u64,
        disk: Option<&DiskInput>,
        directory: TempDir,
        timer_setup: Option<&TimerSetup>,
    ) -> Result<Self, Error> {
        let stderr = fs::File::create(directory.path().join("stderr"))?;
        fs::write(directory.path().join("console"), [])?;
        let (qmp_stream, qmp_child) = management_pair()?;
        #[cfg(any(test, feature = "test-support"))]
        let (gdb_stream, gdb_child) = management_pair()?;
        let mut command = command(
            qemu,
            manifest,
            directory.path(),
            image.is_some(),
            seed,
            disk,
        )?;
        command
            .stdin(qmp_child)
            .stdout(Stdio::null())
            .stderr(stderr);
        #[cfg(any(test, feature = "test-support"))]
        command.stdout(gdb_child);
        if seedbox_machine::process_lease::inherit(&mut command)? {
            // QEMU owns this fdset until exit. Unregistered inherited fds are
            // not a lifetime guarantee: QEMU may close them during startup.
            command.args(["-add-fd", "fd=198,set=198,opaque=seedbox-execution-lease"]);
        }
        // Through the process-wide exec-file scope: a test or a server in this
        // process may be writing an executable when this forks.
        let spawned = seedbox_exec::spawn(&mut command);
        // Command owns copies of the child socket endpoints even after spawn.
        // Release them before any read, including on spawn failure: otherwise
        // a dead child looks like a silent live peer until TIMEOUT instead of EOF.
        drop(command);
        let mut child =
            SetupChild(Some(spawned.map_err(|error| {
                Error(format!("start {}: {error}", qemu.display()))
            })?));
        let setup = (|| {
            let mut qmp = Qmp {
                stream: BufReader::new(qmp_stream),
                stopped: false,
            };
            if qmp.read()?.get("QMP").is_none() {
                return Err(Error("missing QMP greeting".into()));
            }
            qmp.command("qmp_capabilities", json!({}))?;
            if let Some(setup) = timer_setup {
                state::configure(&mut qmp, setup)?;
            }
            qmp.command("query-seedbox-run", json!({}))?;
            if let Some(image) = image {
                let incoming = tempfile::Builder::new()
                    .prefix("incoming-")
                    .tempdir_in(directory.path())?;
                let path = incoming.path().join("image");
                fs::write(&path, image)?;
                qmp.command("migrate-incoming", migration_file(&path)?)?;
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
                // Stopped restore has consumed the complete body, including on
                // subsequent setup failure. No source path survives in the VM.
            }
            #[cfg(any(test, feature = "test-support"))]
            let gdb = Gdb::attach(gdb_stream)?;
            #[cfg(not(any(test, feature = "test-support")))]
            let gdb = ();
            Ok((qmp, gdb))
        })();
        match setup {
            Ok((qmp, _gdb)) => Ok(Self {
                child: child.0.take().expect("owned setup child"),
                directory,
                qmp,
                #[cfg(any(test, feature = "test-support"))]
                gdb: _gdb,
                #[cfg(any(test, feature = "test-support"))]
                watchdog: TEST_RUN_WATCHDOG,
            }),
            Err(error) => {
                child.reap();
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

    /// RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, R8..R15 and RIP, read-only.
    /// Outside long mode QEMU reports the 32-bit registers zero-extended, R8..R15
    /// as zero, and EIP without the CS base.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn registers(&mut self) -> Result<[u64; 17], Error> {
        // x86-64 GDB `g` begins with these, each eight little-endian bytes,
        // then 32-bit EFLAGS and segment selectors and further state.
        let bytes = decode_hex(&self.gdb.request("g")?)?;
        if bytes.len() < 17 * 8 + 4 * 7 {
            return Err(Error("unexpected x86-64 register layout".into()));
        }
        let mut registers = [0; 17];
        for (register, bytes) in registers.iter_mut().zip(bytes.as_chunks::<8>().0) {
            *register = u64::from_le_bytes(*bytes);
        }
        Ok(registers)
    }

    pub(super) fn snapshot(&mut self) -> Result<Vec<u8>, Error> {
        let capture = tempfile::Builder::new()
            .prefix("capture-")
            .tempdir_in(self.directory.path())?;
        let path = capture.path().join("image");
        self.qmp.command("migrate", migration_file(&path)?)?;
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
/// The complete QEMU command line of one process, writing its boot input
/// files into `directory`. Every board choice is fixed here.
fn command(
    qemu: &Path,
    manifest: &Manifest,
    directory: &Path,
    incoming: bool,
    seed: u64,
    disk: Option<&DiskInput>,
) -> Result<Command, Error> {
    let mut command = Command::new(qemu);
    command.args([
        // Every optional board device is fixed rather than left to QEMU's
        // `auto` defaults. ACPI provides power-off; option ROMs are off so
        // only owned firmware runs, and QEMU appends no kernel arguments.
        "-machine",
        "microvm,acpi=on,pic=on,pit=on,rtc=on,isa-serial=on,pcie=off,ioapic2=off,\
             usb=off,x-option-roms=off,auto-kernel-cmdline=off",
        "-accel",
        "tcg,thread=single",
        "-cpu",
        // TCG's LAPIC timer runs through CPU idle. Without ARAT Linux needs
        // a broadcast clockevent microvm does not have, disabling NOHZ/highres.
        "qemu64,arat=on",
        "-smp",
        "1",
        "-nodefaults",
        "-display",
        "none",
        "-monitor",
        "none",
        "-S",           // Start stopped; only seedbox-run may execute code.
        "-no-shutdown", // Retain the stopped guest so it can be inspected/snapshotted.
        // A guest reset (deliberate reboot or a panic restart) stops like a
        // shutdown; the bounded result reports its cause as `reset`. The
        // machine never reboots itself behind the caller's lifecycle.
        "-action",
        "reboot=shutdown",
        "-icount",
        "shift=0,align=off,sleep=off", // Nominal icount base; bounded execution applies fractional work pricing.
        "-global",
        "migration.store-global-state=off", // Lifecycle is saved separately.
        "-global",
        "virtio-mmio.ioeventfd=off", // Queue kicks execute at their guest MMIO instruction.
        "-global",
        // Pin QEMU's default legacy (version 1) transport, whose 10-byte
        // virtio-net header the NIC expects; disk and console use it too.
        "virtio-mmio.force-legacy=true",
        "-global",
        // Only the ISA COM1 UART, which embeds this serial core, opts
        // into canonical console capture.
        "serial.seedbox-console=on",
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
    command.arg("-serial").arg(format!(
        "file:{}",
        path_text(&directory.join("console"), "serial console path")?
    ));
    // Optional devices are attached only when the definition requests them.
    if let Some(mac) = manifest.network {
        let mac: Vec<String> = mac.iter().map(|byte| format!("{byte:02x}")).collect();
        command
            .arg("-device")
            .arg(format!("seedbox-net-device,mac={}", mac.join(":")));
    }
    if let Some(disk) = disk {
        let mut device = format!("seedbox-blk-device,sectors={}", disk.sectors);
        if let Some(base) = &disk.base {
            device += &format!(",image={}", option_path(base)?);
        }
        if let Some(overlay) = &disk.overlay {
            device += &format!(",overlay={}", option_path(overlay)?);
        }
        command.arg("-device").arg(device);
    }
    match &manifest.boot {
        #[cfg(any(test, feature = "test-support"))]
        Boot::Program(program) => {
            command.args(["-rtc", "base=1970-01-01T00:00:00,clock=vm"]);
            // A test BIOS image starts at the CPU's reset vector; QEMU's
            // generic loader would only set EIP under the reset CS base.
            let path = directory.join("program");
            fs::write(&path, program)?;
            command.arg("-bios").arg(path);
        }
        Boot::Linux {
            kernel,
            initramfs,
            firmware,
            command_line,
            epoch_seconds,
        } => {
            // The CMOS RTC reads epoch plus the virtual clock, matching
            // the realtime the supervisor sets; no host wall clock is read.
            // The BIOS loads the kernel, initramfs and exact command line
            // from fw_cfg.
            command
                .arg("-rtc")
                .arg(format!("base={},clock=vm", utc(*epoch_seconds)));
            for (name, bytes, flag) in [
                ("kernel", kernel, "-kernel"),
                ("initramfs", initramfs, "-initrd"),
                ("firmware", firmware, "-bios"),
            ] {
                let path = directory.join(name);
                fs::write(&path, bytes)?;
                command.arg(flag).arg(path);
            }
            command.arg("-append").arg(command_line);
        }
    }
    // Connected anonymous sockets supplied through stdio. No pathname limit,
    // listeners, reconnect loop or deprecated -mon warning. No default monitor
    // or serial writer may compete for these descriptors.
    command.args([
        "-chardev",
        "socket,id=seedbox-qmp,fd=0",
        "-qmp",
        "chardev:seedbox-qmp",
    ]);
    #[cfg(any(test, feature = "test-support"))]
    command.args([
        "-chardev",
        "socket,id=seedbox-gdb,fd=1",
        "-gdb",
        "chardev:seedbox-gdb",
    ]);
    if incoming {
        command.args(["-incoming", "defer"]);
    }
    Ok(command)
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Guard the interval before Process owns the child, including unwinding.
struct SetupChild(Option<Child>);
impl SetupChild {
    fn reap(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for SetupChild {
    fn drop(&mut self) {
        self.reap();
    }
}

fn management_pair() -> Result<(UnixStream, Stdio), Error> {
    let (parent, child) = UnixStream::pair()?;
    parent.set_read_timeout(Some(TIMEOUT))?;
    parent.set_write_timeout(Some(TIMEOUT))?;
    Ok((parent, Stdio::from(std::os::fd::OwnedFd::from(child))))
}

/// QEMU 11.1 qapi/migration.json: MigrationChannel -> MigrationAddress(file),
/// with FileMigrationArgs { filename, offset }. URI parsing treats ,offset= as
/// syntax, so keep caller pathnames in a structured field rather than a URI.
fn migration_file(path: &Path) -> Result<Value, Error> {
    let filename = path_text(path, "migration path")?;
    Ok(json!({"channels": [{"channel-type": "main", "addr": {
        "transport": "file", "filename": filename, "offset": 0
    }}]}))
}

struct Qmp {
    stream: BufReader<UnixStream>,
    stopped: bool,
}
impl Qmp {
    // The actual execution transport, shared with controlled-peer host tests.
    // Guest execution waits without a host read deadline; ordinary queries keep it.
    fn execute_bounded(
        &mut self,
        start: Timestamp,
        upto: Timestamp,
        execution: crate::execution::ExecutionConfig,
    ) -> Result<Value, Error> {
        self.stopped = false;
        self.without_read_timeout(|qmp| {
            qmp.command(
                "seedbox-run",
                json!({
                    "time": start.as_nanos(), "budget": upto.as_nanos() - start.as_nanos(),
                    "factor": execution.factor_permille(),
                }),
            )?;
            // STOP acknowledges the CPU is stopped. It may precede the command's
            // reply, so Qmp also remembers it while processing ordinary replies.
            while !qmp.stopped {
                qmp.read()?;
            }
            Ok(())
        })?;
        self.command("query-seedbox-run", json!({}))
    }

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
    /// Run `f` with no read timeout, restoring the previous one afterwards,
    /// also when `f` fails or panics.
    fn without_read_timeout<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let restore = RestoreReadTimeout {
            previous: self.stream.get_ref().read_timeout()?,
            // The socket option is shared by every descriptor of the socket.
            stream: self.stream.get_ref().try_clone()?,
        };
        self.stream.get_ref().set_read_timeout(None)?;
        let result = f(self);
        // Restore explicitly on the normal path so a failure is reported;
        // the guard repeats it harmlessly and covers unwinding.
        let restored = restore.restore();
        let value = result?;
        restored?;
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

/// Test builds: ends a bounded run whose QEMU does not stop in time. Firing
/// shuts the QMP socket down, which wakes the blocked reader with EOF; the
/// caller then kills QEMU. Disarming reports whether it fired.
#[cfg(any(test, feature = "test-support"))]
struct Watchdog {
    cancel: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<bool>,
}
#[cfg(any(test, feature = "test-support"))]
impl Watchdog {
    fn arm(stream: &UnixStream, limit: Duration) -> Result<Self, Error> {
        let socket = stream.try_clone()?;
        let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || match cancelled.recv_timeout(limit) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = socket.shutdown(std::net::Shutdown::Both);
                true
            }
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
        });
        Ok(Self { cancel, thread })
    }
    fn disarm(self) -> bool {
        let _ = self.cancel.send(());
        self.thread.join().unwrap_or(true)
    }
}

/// Restores a socket's read timeout when dropped, including during unwinding.
struct RestoreReadTimeout {
    stream: UnixStream,
    previous: Option<Duration>,
}
impl RestoreReadTimeout {
    fn restore(&self) -> std::io::Result<()> {
        self.stream.set_read_timeout(self.previous)
    }
}
impl Drop for RestoreReadTimeout {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// QEMU `-rtc base=` form of whole seconds since the Unix epoch, in UTC.
fn utc(seconds: u32) -> String {
    let days = i64::from(seconds / 86_400);
    let time = seconds % 86_400;
    // Proleptic Gregorian civil date from days since 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        time / 3_600,
        time / 60 % 60,
        time % 60
    )
}

#[cfg(test)]
mod command_tests;

#[cfg(test)]
mod transport_tests;

#[cfg(test)]
mod utc_tests;

impl Process {
    pub(super) fn start_native_witness(
        &mut self,
        chain: &str,
        boot: u64,
        forced: bool,
    ) -> Result<(), Error> {
        self.qmp.command(
            "seedbox-witness-start",
            json!({"chain":chain,"boot":boot,"forced":forced}),
        )?;
        Ok(())
    }
    pub(super) fn native_witness(
        &mut self,
        network: bool,
    ) -> Result<(String, String, Option<String>), Error> {
        let chain = self.qmp.command("seedbox-witness-chain", json!({}))?;
        let lifecycle = self.qmp.command("seedbox-lifecycle-pending", json!({}))?;
        let frames = if network {
            Some(serde_json::from_value(
                self.qmp.command("seedbox-net-pending", json!({}))?["bytes"].clone(),
            )?)
        } else {
            None
        };
        Ok((
            serde_json::from_value(chain["chain"].clone())?,
            serde_json::from_value(lifecycle["bytes"].clone())?,
            frames,
        ))
    }
}
