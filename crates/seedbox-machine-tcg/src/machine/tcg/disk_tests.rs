//! Native checks of the unwired `seedbox-blk-device` in the patched QEMU.
//!
//! An x86 BIOS program copies the preloaded descriptors and buffers from its
//! ROM to RAM, finds the device's legacy virtio-mmio transport, enables one
//! queue at `RING` and notifies it once. QMP drives bounded execution, reads
//! RAM, exports the overlays and migrates; the Rust store decodes the results.
use super::raw_program_data::{Asm, Cond, Reg, GED_RESET, GED_SLEEP, PROGRAM};
use crate::machine::disk::{Store, SECTOR};
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const RING: u64 = 0x0010_0000;
const AVAILABLE: u64 = RING + 16 * 16;
const USED: u64 = RING + 4096;
const BUFFERS: u64 = RING + 0x2000;
const BASE: u8 = 9;
/// The microvm board's first virtio-mmio transport and the transport stride.
const VIRTIO_MMIO: u32 = 0xFEB0_0000;
const VIRTIO_STRIDE: u32 = 0x200;

/// How the program ends after its single notification.
#[derive(Clone, Copy)]
enum Finish {
    /// Halt with interrupts disabled.
    Idle,
    /// ACPI GED sleep control S5: power off.
    PowerOff,
    /// ACPI GED reset register: request a reset.
    Reset,
}

/// Copy `ring` (its nonzero prefix) to `RING`, find the block transport,
/// enable queue 0 there and notify it once, then `finish`.
fn program(ring: &[u8], finish: Finish) -> Vec<u8> {
    let used = ring
        .iter()
        .rposition(|&byte| byte != 0)
        .map_or(0, |at| at + 1);
    let words = used.div_ceil(4);
    let mut code = Asm::default();
    // The template follows the code; its address is patched in below.
    code.mov_imm(Reg::Esi, 0);
    let template_operand = 1;
    code.mov_imm(Reg::Edi, RING as u32)
        .mov_imm(Reg::Ecx, words as u32)
        .rep_movsd()
        .mov_imm(Reg::Esi, VIRTIO_MMIO)
        .label("scan")
        .load_at(Reg::Eax, Reg::Esi, 8) // DeviceID
        .cmp_imm(Reg::Eax, 2) // VIRTIO_ID_BLOCK
        .jcc(Cond::Equal, "found")
        .add_imm(Reg::Esi, VIRTIO_STRIDE)
        .jmp("scan")
        .label("found")
        .store_imm_at(Reg::Esi, 0x70, 3) // ACKNOWLEDGE | DRIVER
        .store_imm_at(Reg::Esi, 0x20, 0x204) // FLUSH | SEG_MAX
        .store_imm_at(Reg::Esi, 0x28, 4096) // GuestPageSize
        .store_imm_at(Reg::Esi, 0x30, 0) // QueueSel
        .store_imm_at(Reg::Esi, 0x38, 16) // QueueNum
        .store_imm_at(Reg::Esi, 0x3c, 4096) // QueueAlign
        .store_imm_at(Reg::Esi, 0x40, (RING >> 12) as u32) // QueuePFN
        .store_imm_at(Reg::Esi, 0x70, 7) // ... | DRIVER_OK
        .store_imm_at(Reg::Esi, 0x50, 0); // QueueNotify
    match finish {
        Finish::Idle => &mut code,
        Finish::PowerOff => code.store_byte(GED_SLEEP, 0x34),
        Finish::Reset => code.store_byte(GED_RESET, 0x42),
    }
    .label("idle")
    .hlt()
    .jmp("idle");
    let mut bytes = code.finish();
    let template = PROGRAM as u32 + bytes.len() as u32;
    bytes[template_operand..template_operand + 4].copy_from_slice(&template.to_le_bytes());
    bytes.extend(&ring[..used]);
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    super::raw_program_data::bios(&bytes)
}

/// Preloaded ring memory: descriptors, available ring and buffers.
struct Ring {
    bytes: Vec<u8>,
    descriptors: u16,
    heads: Vec<u16>,
    next: u64,
}

enum Part {
    Read(Vec<u8>),
    Write(usize),
    Outside,
}

impl Ring {
    fn new() -> Self {
        Self {
            bytes: vec![0; 0x10000],
            descriptors: 0,
            heads: Vec::new(),
            next: BUFFERS,
        }
    }
    fn put(&mut self, at: u64, bytes: &[u8]) {
        let at = (at - RING) as usize;
        self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
    }
    /// Add one chain; returns the guest address of each part.
    fn chain(&mut self, parts: &[Part]) -> Vec<u64> {
        let head = self.descriptors;
        let mut addresses = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let (address, length, flags) = match part {
                Part::Read(bytes) => {
                    let at = self.next;
                    self.put(at, bytes);
                    (at, bytes.len(), 0u16)
                }
                Part::Write(length) => {
                    let at = self.next;
                    self.put(at, &vec![0xcc; *length]);
                    (at, *length, 2)
                }
                Part::Outside => (1 << 40, SECTOR, 0),
            };
            if !matches!(part, Part::Outside) {
                self.next += (length as u64).div_ceil(64) * 64;
            }
            addresses.push(address);
            let more = index + 1 < parts.len();
            let slot = RING + self.descriptors as u64 * 16;
            self.descriptors += 1;
            let mut descriptor = address.to_le_bytes().to_vec();
            descriptor.extend((length as u32).to_le_bytes());
            descriptor.extend((flags | more as u16).to_le_bytes());
            descriptor.extend(self.descriptors.to_le_bytes());
            self.put(slot, &descriptor);
        }
        self.heads.push(head);
        addresses
    }
    fn image(mut self) -> Vec<u8> {
        for (index, head) in self.heads.clone().iter().enumerate() {
            self.put(AVAILABLE + 4 + index as u64 * 2, &head.to_le_bytes());
        }
        let count = self.heads.len() as u16;
        self.put(AVAILABLE + 2, &count.to_le_bytes());
        self.bytes
    }
}

fn header(kind: u32, sector: u64) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0; 4]);
    bytes.extend(sector.to_le_bytes());
    bytes
}

struct Qmp {
    stream: BufReader<UnixStream>,
    stopped: bool,
}

impl Qmp {
    fn connect(path: &Path, child: &mut Child) -> Self {
        let guard = HangGuard::new("QEMU QMP socket readiness", Duration::from_secs(30));
        let stream = guard
            .wait_for(|| match UnixStream::connect(path) {
                Ok(stream) => Some(stream),
                Err(error) => {
                    assert!(child.try_wait().unwrap().is_none(), "QEMU exited: {error}");
                    None
                }
            })
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut qmp = Self {
            stream: BufReader::new(stream),
            stopped: false,
        };
        assert!(qmp.read().get("QMP").is_some());
        qmp.command("qmp_capabilities", json!({})).unwrap();
        qmp
    }
    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stream.read_line(&mut line).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        self.stopped |= value["event"] == "STOP";
        value
    }
    fn command(&mut self, name: &str, arguments: Value) -> Result<Value, Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let timeout = || json!({"error": "QMP command exceeded 60-second deadline"});
        let request = json!({"execute": name, "arguments": arguments});
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(timeout());
        }
        self.stream
            .get_ref()
            .set_write_timeout(Some(remaining))
            .unwrap();
        if let Err(error) = writeln!(self.stream.get_mut(), "{request}") {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                return Err(timeout());
            }
            panic!("QMP command write failed: {error}");
        }
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(timeout());
            }
            self.stream
                .get_ref()
                .set_read_timeout(Some(remaining))
                .unwrap();
            let mut line = String::new();
            match self.stream.read_line(&mut line) {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(timeout())
                }
                Err(error) => panic!("QMP response read failed: {error}"),
            }
            let response: Value = serde_json::from_str(&line).unwrap();
            self.stopped |= response["event"] == "STOP";
            if response.get("event").is_some() {
                continue;
            }
            return match response.get("return") {
                Some(value) => Ok(value.clone()),
                None => Err(response),
            };
        }
    }
    /// Execute a bounded interval and return the stop result.
    fn run(&mut self, time: u64, budget: u64) -> Value {
        self.stopped = false;
        let arguments = json!({"time": time, "budget": budget, "factor": 1000});
        self.command("seedbox-run", arguments).unwrap();
        while !self.stopped {
            self.read();
        }
        self.command("query-seedbox-run", json!({})).unwrap()
    }
    fn memory(&mut self, directory: &Path, address: u64, size: usize) -> Vec<u8> {
        let path = directory.join(format!("memory-{address:x}"));
        let arguments = json!({"val": address, "size": size, "filename": path});
        self.command("pmemsave", arguments).unwrap();
        std::fs::read(path).unwrap()
    }
    fn export(&mut self, directory: &Path, name: &str) -> Vec<u8> {
        let path = directory.join(name);
        self.command("seedbox-disk-export", json!({"path": path}))
            .unwrap();
        std::fs::read(path).unwrap()
    }
}

struct Qemu {
    child: Child,
    qmp: Qmp,
}

impl Drop for Qemu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start stopped QEMU with the driver program carrying `ring`, and a disk of
/// four base sectors plus `overlay`; or restore it from `incoming`. The board
/// options are the TCG machine's own (`qemu.rs`), without its console,
/// lifecycle port or inspection sockets.
fn start(
    directory: &Path,
    name: &str,
    ring: &[u8],
    overlay: &[u8],
    incoming: Option<&Path>,
) -> Qemu {
    let qemu = PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").expect("SEEDBOX_TCG_QEMU"));
    let file = |suffix: &str, bytes: &[u8]| {
        let path = directory.join(format!("{name}-{suffix}"));
        std::fs::write(&path, bytes).unwrap();
        path
    };
    let program = file("program", &program(ring, Finish::Idle));
    let image = file("image", &[BASE; 4 * SECTOR]);
    let overlay = file("overlay", overlay);
    let qmp = directory.join(format!("{name}-qmp"));
    let mut command = Command::new(qemu);
    command.args([
        "-machine",
        "microvm,acpi=on,pic=on,pit=on,rtc=on,isa-serial=on,pcie=off,ioapic2=off,\
             usb=off,x-option-roms=off,auto-kernel-cmdline=off",
        "-accel",
        "tcg,thread=single",
        "-cpu",
        "qemu64,arat=on",
        "-smp",
        "1",
        "-nodefaults",
        "-display",
        "none",
        "-monitor",
        "none",
        "-S",
        "-no-shutdown",
        "-icount",
        "shift=0,align=off,sleep=off",
        "-m",
        "64M",
        "-global",
        "migration.store-global-state=off",
        "-global",
        "virtio-mmio.ioeventfd=off",
        "-global",
        "virtio-mmio.force-legacy=true",
    ]);
    command.arg("-bios").arg(&program);
    command.arg("-device").arg(format!(
        "seedbox-blk-device,sectors=4,image={},overlay={}",
        image.display(),
        overlay.display()
    ));
    command
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp.display()));
    if incoming.is_some() {
        command.args(["-incoming", "defer"]);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .guarded_spawn()
        .unwrap();
    let mut qmp = Qmp::connect(&qmp, &mut child);
    if let Some(path) = incoming {
        let uri = format!("file:{}", path.display());
        qmp.command("migrate-incoming", json!({"uri": uri}))
            .unwrap();
        HangGuard::new("QMP reports paused", Duration::from_secs(30))
            .wait_for(|| {
                (qmp.command("query-status", json!({})).unwrap()["status"] == "paused")
                    .then_some(())
            })
            .unwrap();
    }
    Qemu { child, qmp }
}

/// Each sector's first byte, now and after a power loss.
fn view(overlays: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut store = Store::from_image(vec![BASE; 4 * SECTOR]).unwrap();
    store.replace_overlays(overlays).unwrap();
    let first = |store: &Store| {
        (0..4)
            .map(|s| {
                let mut sector = [0; SECTOR];
                store.read(s, &mut sector).unwrap();
                sector[0]
            })
            .collect::<Vec<_>>()
    };
    let mut lost = store.clone();
    lost.discard_unflushed();
    (first(&store), first(&lost))
}

/// Durable sector 0 = 1 and unflushed sector 1 = 2 over the base.
fn boot_overlay() -> Vec<u8> {
    let mut store = Store::from_image(vec![BASE; 4 * SECTOR]).unwrap();
    store.write(0, &[1; SECTOR]).unwrap();
    store.flush();
    store.write(1, &[2; SECTOR]).unwrap();
    store.encode_overlays()
}

fn used(qemu: &mut Qemu, directory: &Path) -> Vec<(u32, u32)> {
    let ring = qemu.qmp.memory(directory, USED, 4 + 16 * 8);
    let index = u16::from_le_bytes([ring[2], ring[3]]) as usize;
    (0..index)
        .map(|i| {
            let entry = &ring[4 + i * 8..12 + i * 8];
            (
                u32::from_le_bytes(entry[..4].try_into().unwrap()),
                u32::from_le_bytes(entry[4..].try_into().unwrap()),
            )
        })
        .collect()
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_disk_device_completes_requests_and_migrates_its_overlays() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    let mut ring = Ring::new();
    let read = ring.chain(&[
        Part::Read(header(0, 0)),
        Part::Write(2 * SECTOR),
        Part::Write(1),
    ]);
    let write = ring.chain(&[
        Part::Read(header(1, 2)),
        Part::Read(vec![b'W'; SECTOR]),
        Part::Write(1),
    ]);
    let flush = ring.chain(&[Part::Read(header(4, 0)), Part::Write(1)]);
    let late = ring.chain(&[
        Part::Read(header(1, 3)),
        Part::Read(vec![b'U'; SECTOR]),
        Part::Write(1),
    ]);
    let ring = ring.image();
    let overlay = boot_overlay();
    let mut results = Vec::new();
    // Two identical runs must agree exactly.
    for name in ["first", "second"] {
        let mut qemu = start(directory, name, &ring, &overlay, None);
        let stop = qemu.qmp.run(0, 1_000_000);
        assert_eq!(stop["reason"], "blocked", "{stop}");
        assert_eq!(
            used(&mut qemu, directory),
            [(0, 2 * SECTOR as u32 + 1), (3, 1), (6, 1), (8, 1)]
        );
        let data = qemu.qmp.memory(directory, read[1], 2 * SECTOR);
        assert!(
            data[..SECTOR].iter().all(|&b| b == 1),
            "durable boot overlay"
        );
        assert!(
            data[SECTOR..].iter().all(|&b| b == 2),
            "unflushed boot overlay"
        );
        for status in [read[2], write[2], flush[1], late[2]] {
            assert_eq!(qemu.qmp.memory(directory, status, 1), [0]);
        }
        let exported = qemu.qmp.export(directory, &format!("{name}-export"));
        assert_eq!(
            view(&exported),
            (vec![1, 2, b'W', b'U'], vec![1, 2, b'W', BASE]),
            "flush promoted only earlier writes"
        );
        // Exporting twice neither flushes nor changes the disk.
        assert_eq!(
            qemu.qmp.export(directory, &format!("{name}-again")),
            exported
        );
        results.push((stop, exported.clone(), used(&mut qemu, directory)));
        if name == "first" {
            // Migration carries the overlays: a restored device exports the same.
            let image = directory.join("migration");
            let uri = format!("file:{}", image.display());
            qemu.qmp.command("migrate", json!({"uri": uri})).unwrap();
            HangGuard::new("QMP reports completed", Duration::from_secs(30))
                .wait_for(|| {
                    (qemu.qmp.command("query-migrate", json!({})).unwrap()["status"] == "completed")
                        .then_some(())
                })
                .unwrap();
            // A stale boot overlay is replaced entirely by the migrated one.
            let mut restored = start(directory, "restored", &ring, &boot_overlay(), Some(&image));
            assert_eq!(restored.qmp.export(directory, "restored-export"), exported);
            assert_eq!(used(&mut restored, directory), results[0].2);
        }
    }
    assert_eq!(results[0], results[1]);
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_disk_device_errors_stop_execution_after_the_completed_prefix() {
    type Case = (&'static str, fn(&mut Ring));
    let malformed: [Case; 2] = [
        ("request without status", |ring| {
            ring.chain(&[Part::Read(header(1, 1)), Part::Read(vec![b'M'; SECTOR])]);
        }),
        ("descriptor outside RAM", |ring| {
            ring.chain(&[Part::Read(header(1, 1)), Part::Outside, Part::Write(1)]);
        }),
    ];
    for (name, bad) in malformed {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let mut ring = Ring::new();
        let first = ring.chain(&[
            Part::Read(header(1, 2)),
            Part::Read(vec![b'P'; SECTOR]),
            Part::Write(1),
        ]);
        bad(&mut ring);
        ring.chain(&[
            Part::Read(header(1, 3)),
            Part::Read(vec![b'Q'; SECTOR]),
            Part::Write(1),
        ]);
        let mut qemu = start(directory, "error", &ring.image(), &boot_overlay(), None);
        let stop = qemu.qmp.run(0, 1_000_000);
        assert_eq!(stop["reason"], "error", "{name}: {stop}");
        assert_eq!(used(&mut qemu, directory), [(0, 1)], "{name}");
        assert_eq!(qemu.qmp.memory(directory, first[2], 1), [0]);
        let exported = qemu.qmp.export(directory, "export");
        assert_eq!(view(&exported).0, [1, 2, b'P', BASE], "{name}");
        let again = qemu.qmp.command(
            "seedbox-run",
            json!({"time": stop["time"], "budget": 10, "factor": 1000}),
        );
        let error = again.unwrap_err().to_string();
        assert!(error.contains("failed seedbox device"), "{name}: {error}");
    }
}

mod machine {
    //! The same driver program run by `TcgMachine`, which owns the disk
    //! between boots: boot-end export, discard and snapshot ownership.
    use super::*;
    use crate::{
        clock::SimulationClock,
        definition::DiskImage,
        execution::ExecutionConfig,
        machine::{
            tcg::{Boot, Manifest, Snapshot, TcgMachine},
            ExitReason, Machine, RunContext, StopReason, Timestamp,
        },
        pv::{Inbox, Input, Outbox, Request},
        rng::SeededRng,
    };
    use std::collections::VecDeque;

    fn start(ring: Vec<u8>) -> TcgMachine {
        start_then(ring, Finish::Idle)
    }

    fn start_then(ring: Vec<u8>, finish: Finish) -> TcgMachine {
        let qemu = PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").expect("SEEDBOX_TCG_QEMU"));
        let image = DiskImage::from_bytes(vec![BASE; 4 * SECTOR]).unwrap();
        TcgMachine::new_raw(
            &qemu,
            Manifest {
                boot: Boot::Program(program(&ring, finish).into()),
                memory_mib: 16,
                disk: Some(image),
                network: None,
            },
        )
        .unwrap()
    }

    fn drive(machine: &mut TcgMachine, now: u64, end: u64, requests: Vec<Request>) -> StopReason {
        let at = Timestamp::from_nanos;
        let clock = SimulationClock::new(now);
        let mut rng = SeededRng::new(7);
        let mut inbox: VecDeque<_> = requests
            .into_iter()
            .map(|payload| Input {
                at: at(now),
                payload,
            })
            .collect();
        let mut outputs = Vec::new();
        machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    execution: ExecutionConfig::default(),
                    inbox: Inbox::new(&mut inbox, at(now)),
                    outbox: Outbox::new(&mut outputs, at(now), at(end)),
                },
                at(end),
            )
            .unwrap()
    }

    /// The between-boot disk: each sector's first byte.
    fn durable(machine: &TcgMachine) -> Vec<u8> {
        let store = machine
            .disk
            .as_ref()
            .unwrap()
            .store
            .as_ref()
            .expect("no live owner");
        (0..4)
            .map(|s| {
                let mut sector = [0; SECTOR];
                store.read(s, &mut sector).unwrap();
                sector[0]
            })
            .collect()
    }

    #[test]
    #[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
    fn tcg_disk_machine_boot_end_discards_once_and_snapshots_never_promote() {
        let mut ring = Ring::new();
        ring.chain(&[
            Part::Read(header(1, 0)),
            Part::Read(vec![b'A'; SECTOR]),
            Part::Write(1),
        ]);
        ring.chain(&[Part::Read(header(4, 0)), Part::Write(1)]);
        ring.chain(&[
            Part::Read(header(1, 1)),
            Part::Read(vec![b'B'; SECTOR]),
            Part::Write(1),
        ]);
        let mut machine = start(ring.image());
        assert!(matches!(
            drive(&mut machine, 0, 1_000_000, vec![]),
            StopReason::Blocked { .. }
        ));
        // While QEMU owns the disk the machine holds no store copy.
        assert!(machine.disk.as_ref().unwrap().store.is_none());
        let live = machine.snapshot().unwrap();
        assert!(live.image.is_some() && live.disk.is_none());
        // Crash ends the boot: the flushed write survives, the unflushed one
        // is lost, and a repeated Crash discards nothing more.
        drive(&mut machine, 1_000_000, 1_000_000, vec![Request::Crash]);
        assert_eq!(durable(&machine), [b'A', BASE, BASE, BASE]);
        drive(&mut machine, 1_000_000, 1_000_000, vec![Request::Crash]);
        assert_eq!(durable(&machine), [b'A', BASE, BASE, BASE]);
        // The live snapshot neither flushed nor lost the unflushed write:
        // its restored QEMU still reads B, and its own crash discards it.
        let qemu = PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
        let mut restored = TcgMachine::from_snapshot(&qemu, &live).unwrap();
        let exported = restored.process.as_mut().unwrap().export_disk().unwrap();
        assert_eq!(
            view(&exported),
            (vec![b'A', b'B', BASE, BASE], vec![b'A', BASE, BASE, BASE])
        );
        drive(&mut restored, 1_000_000, 1_000_000, vec![Request::Crash]);
        assert_eq!(durable(&restored), [b'A', BASE, BASE, BASE]);
        // A terminal snapshot carries the already-discarded disk.
        let terminal = machine.snapshot().unwrap();
        assert!(terminal.image.is_none() && terminal.disk.is_some());
        let mut again = TcgMachine::from_snapshot(&qemu, &terminal).unwrap();
        assert_eq!(durable(&again), [b'A', BASE, BASE, BASE]);
        // A Reboot boots QEMU over the durable disk and repeats the program.
        let stop = drive(&mut again, 1_000_000, 2_000_000, vec![Request::Reboot]);
        assert!(matches!(stop, StopReason::Blocked { .. }), "{stop:?}");
        let exported = again.process.as_mut().unwrap().export_disk().unwrap();
        assert_eq!(
            view(&exported),
            (vec![b'A', b'B', BASE, BASE], vec![b'A', BASE, BASE, BASE])
        );
    }

    /// Write A, FLUSH, then an unflushed B.
    fn flushed_then_unflushed() -> Vec<u8> {
        let mut ring = Ring::new();
        ring.chain(&[
            Part::Read(header(1, 0)),
            Part::Read(vec![b'A'; SECTOR]),
            Part::Write(1),
        ]);
        ring.chain(&[Part::Read(header(4, 0)), Part::Write(1)]);
        ring.chain(&[
            Part::Read(header(1, 1)),
            Part::Read(vec![b'B'; SECTOR]),
            Part::Write(1),
        ]);
        ring.image()
    }

    fn qemu() -> PathBuf {
        PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").expect("SEEDBOX_TCG_QEMU"))
    }

    #[test]
    #[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
    fn tcg_disk_machine_guest_power_off_and_reset_end_the_disk_boot() {
        for (finish, reason) in [
            (Finish::PowerOff, ExitReason::Shutdown),
            (Finish::Reset, ExitReason::Crashed),
        ] {
            let mut machine = start_then(flushed_then_unflushed(), finish);
            let stop = drive(&mut machine, 0, 1_000_000, vec![]);
            assert!(
                matches!(stop, StopReason::Exited { reason: r, .. } if r == reason),
                "{stop:?}"
            );
            // The guest's own exit ended the boot: QEMU is gone and the
            // unflushed write was discarded once.
            assert!(machine.process.is_none(), "{reason:?}");
            assert_eq!(durable(&machine), [b'A', BASE, BASE, BASE], "{reason:?}");
            drive(&mut machine, 1_000_000, 1_000_000, vec![Request::Crash]);
            assert_eq!(durable(&machine), [b'A', BASE, BASE, BASE], "{reason:?}");
            let terminal = machine.snapshot().unwrap();
            assert!(terminal.image.is_none() && terminal.disk.is_some());
            // A Reboot boots QEMU again over the durable disk.
            let stop = drive(&mut machine, 1_000_000, 1_000_000, vec![Request::Reboot]);
            assert_eq!(stop, StopReason::BoundaryReached);
            let exported = machine.process.as_mut().unwrap().export_disk().unwrap();
            assert_eq!(
                view(&exported),
                (vec![b'A', BASE, BASE, BASE], vec![b'A', BASE, BASE, BASE])
            );
        }
    }

    #[test]
    #[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
    fn tcg_disk_machine_queued_reboots_tear_down_the_live_boot_once() {
        let mut machine = start(flushed_then_unflushed());
        assert!(matches!(
            drive(&mut machine, 0, 1_000_000, vec![]),
            StopReason::Blocked { .. }
        ));
        // Three queued Reboots: the live boot is torn down (exporting and
        // discarding its unflushed write), and two superseded boots start
        // and end before the last one, all at this zero-budget call.
        let stop = drive(
            &mut machine,
            1_000_000,
            1_000_000,
            vec![Request::Reboot, Request::Reboot, Request::Reboot],
        );
        assert_eq!(stop, StopReason::BoundaryReached);
        assert!(machine.process.is_some() && machine.disk_store().is_none());
        let exported = machine.process.as_mut().unwrap().export_disk().unwrap();
        assert_eq!(
            view(&exported),
            (vec![b'A', BASE, BASE, BASE], vec![b'A', BASE, BASE, BASE]),
            "the final boot starts from the durable disk only"
        );
        drive(&mut machine, 1_000_000, 1_000_000, vec![Request::Crash]);
        assert_eq!(durable(&machine), [b'A', BASE, BASE, BASE]);
    }

    #[test]
    #[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
    fn tcg_disk_machine_snapshot_with_its_boot_pending_restores_over_its_disk() {
        // Before its first run the boot is still pending and no QEMU exists.
        let mut source = start(flushed_then_unflushed());
        let pending = source.snapshot().unwrap();
        assert!(pending.image.is_none() && pending.pending_boots == 1);
        let mut restored = TcgMachine::from_snapshot(&qemu(), &pending).unwrap();
        assert!(matches!(
            drive(&mut restored, 0, 1_000_000, vec![]),
            StopReason::Blocked { .. }
        ));
        let exported = restored.process.as_mut().unwrap().export_disk().unwrap();
        assert_eq!(
            view(&exported),
            (vec![b'A', b'B', BASE, BASE], vec![b'A', BASE, BASE, BASE])
        );
        // The source is untouched and still boots independently.
        assert_eq!(durable(&source), [BASE; 4]);
        assert!(matches!(
            drive(&mut source, 0, 1_000_000, vec![]),
            StopReason::Blocked { .. }
        ));
    }

    #[test]
    fn contradictory_snapshot_disk_ownership_is_rejected_before_starting_qemu() {
        let missing = Path::new("/nonexistent/seedbox-qemu");
        let manifest = |disk| Manifest {
            boot: Boot::Program(program(&[], Finish::Idle).into()),
            memory_mib: 16,
            disk,
            network: None,
        };
        let image = DiskImage::from_bytes(vec![BASE; 4 * SECTOR]).unwrap();
        let initial = TcgMachine::new_raw(missing, manifest(Some(image)))
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(initial.image.is_none() && initial.disk.is_some());
        let diskless = TcgMachine::new_raw(missing, manifest(None))
            .unwrap()
            .snapshot()
            .unwrap();
        // Consistent snapshots restore without starting QEMU.
        assert!(TcgMachine::from_snapshot(missing, &initial).is_ok());
        assert!(TcgMachine::from_snapshot(missing, &diskless).is_ok());
        let contradictions = [
            // A live image together with client overlays. The live boot
            // owns the image, so only disk ownership is contradictory.
            Snapshot {
                image: Some(vec![1]),
                active_boot: Some(initial.logical_boot),
                ..initial.clone()
            },
            // A disk with neither a live image nor client overlays.
            Snapshot {
                disk: None,
                ..initial.clone()
            },
            // Client overlays for a diskless manifest.
            Snapshot {
                disk: initial.disk.clone(),
                ..diskless
            },
        ];
        for snapshot in &contradictions {
            let error = TcgMachine::from_snapshot(missing, snapshot).err().unwrap();
            assert!(
                error.to_string().contains("does not match its manifest"),
                "{error}"
            );
        }
    }

    #[test]
    #[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
    fn tcg_disk_machine_malformed_request_is_an_error() {
        let mut ring = Ring::new();
        ring.chain(&[Part::Read(header(1, 1)), Part::Read(vec![b'M'; SECTOR])]);
        let mut machine = start(ring.image());
        let at = Timestamp::from_nanos;
        let clock = SimulationClock::new(0);
        let mut rng = SeededRng::new(7);
        let mut inbox = VecDeque::new();
        let mut outputs = Vec::new();
        let result = machine.run(
            &mut RunContext {
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                execution: ExecutionConfig::default(),
                inbox: Inbox::new(&mut inbox, at(0)),
                outbox: Outbox::new(&mut outputs, at(0), at(1_000_000)),
            },
            at(1_000_000),
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("unexpected QEMU stop") && error.contains("error"),
            "{error}"
        );
        assert!(machine.snapshot().is_err(), "the failed machine is invalid");
    }
}
