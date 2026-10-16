//! Native checks of the unwired `seedbox-net-device` in the patched QEMU.
//!
//! An x86 BIOS program finds the device's legacy virtio-mmio transport, routes
//! the board's virtio interrupts through the IOAPIC, and interprets a command
//! list preloaded in RAM: register stores, ring index updates, interrupt
//! waits, delays and register reads. Rings and buffers are preloaded too. QMP
//! drives bounded execution, delivers and drains frames, reads RAM and
//! migrates.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
use seedbox_machine_tcg::machine::tcg::test_support::raw_program::{lapic, Asm, Cond, Reg};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const COMMANDS: u64 = 0x0002_0000;
/// One preloaded blob: both rings, results and buffers.
const REGION: u64 = 0x0010_0000;
const REGION_SIZE: usize = 0x10_0000;
const RX_RING: u64 = REGION;
const TX_RING: u64 = REGION + 0x1_0000;
const RESULTS: u64 = REGION + 0x2_0000;
const RX_BUFFERS: u64 = REGION + 0x4_0000;
const TX_BUFFERS: u64 = REGION + 0xc_0000;
const QUEUE: u16 = 256;
const RX_STRIDE: u64 = 1536;
const HEADER: usize = 10;
const MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];

// Legacy split ring with QueueNum 256 and 4 KiB alignment.
fn available(ring: u64) -> u64 {
    ring + 16 * QUEUE as u64
}
fn used_ring(ring: u64) -> u64 {
    ring + 0x2000
}

/// The microvm board's first virtio-mmio transport, and its IOAPIC GSI.
const VIRTIO_MMIO: u32 = 0xFEB0_0000;
const VIRTIO_GSI: u32 = 16;
const IOAPIC: u32 = 0xFEC0_0000;

/// The command interpreter. See [`Commands`] for the operations.
fn program() -> Vec<u8> {
    use Reg::{Eax, Ecx, Edi, Edx, Esi};
    let mut a = Asm::default();
    // Stop the board timers, then outlast the PIT's last transition, so an
    // idle CPU has no pending deadline.
    a.untimed();
    a.mov_imm(Esi, VIRTIO_MMIO)
        .label("scan")
        .load_at(Eax, Esi, 8) // DeviceID
        .cmp_imm(Eax, 1) // VIRTIO_ID_NET
        .jcc(Cond::Equal, "found")
        .add_imm(Esi, 0x200)
        .jmp("scan")
        .label("found");
    // Software-enable the LAPIC, then route every virtio-mmio GSI, edge
    // triggered, to vector 0x21 on APIC 0. HLT wakes only with IF set.
    a.store_imm(lapic::SPURIOUS, 0x1FF);
    for pin in VIRTIO_GSI..VIRTIO_GSI + 8 {
        a.store_imm(IOAPIC, 0x10 + 2 * pin)
            .store_imm(IOAPIC + 0x10, 0x21)
            .store_imm(IOAPIC, 0x11 + 2 * pin)
            .store_imm(IOAPIC + 0x10, 0);
    }
    a.mov_imm(Edi, COMMANDS as u32)
        .label("loop")
        .load_at(Eax, Edi, 0)
        .load_at(Ecx, Edi, 4)
        .load_at(Edx, Edi, 8)
        .add_imm(Edi, 16)
        .cmp_imm(Eax, 0)
        .jcc(Cond::Equal, "end");
    for (op, label) in [
        (1, "store32"),
        (2, "store16"),
        (3, "register"),
        (5, "read"),
        (6, "delay"),
    ] {
        a.cmp_imm(Eax, op).jcc(Cond::Equal, label);
    }
    // 4: wait for an interrupt (the handler writes the EOI), then
    // acknowledge the device's interrupt status.
    a.sti()
        .hlt()
        .cli()
        .load_at(Eax, Esi, 0x60)
        .store_at(Esi, 0x64, Eax)
        .jmp("loop");
    a.label("store32").store_at(Edx, 0, Ecx).jmp("loop");
    a.label("store16").store16_at(Edx, 0, Ecx).jmp("loop");
    a.label("register")
        .add(Edx, Esi)
        .store_at(Edx, 0, Ecx)
        .jmp("loop");
    a.label("read")
        .add(Ecx, Esi)
        .load_at(Eax, Ecx, 0)
        .store_at(Edx, 0, Eax)
        .jmp("loop");
    a.label("delay")
        .dec(Ecx)
        .jcc(Cond::NotEqual, "delay")
        .jmp("loop");
    // 0: stay idle with interrupts disabled, so the machine blocks.
    a.label("end").hlt().jmp("end");
    a.image()
}

/// Commands interpreted in order by [`program`].
#[derive(Default, Clone)]
struct Commands(Vec<u8>);

impl Commands {
    fn op(mut self, op: u32, value: u32, address: u64) -> Self {
        self.0.extend(op.to_le_bytes());
        self.0.extend(value.to_le_bytes());
        self.0.extend(address.to_le_bytes());
        self
    }
    fn register(self, offset: u64, value: u32) -> Self {
        self.op(3, value, offset)
    }
    fn store16(self, address: u64, value: u16) -> Self {
        self.op(2, value.into(), address)
    }
    fn read(self, offset: u32, into: u64) -> Self {
        self.op(5, offset, into)
    }
    fn wait(self) -> Self {
        self.op(4, 0, 0)
    }
    fn delay(self, iterations: u32) -> Self {
        self.op(6, iterations, 0)
    }
    /// Negotiate MAC and MTU and configure both queues, stopping before
    /// DRIVER_OK when `ready` is false.
    fn setup(self, ready: bool) -> Self {
        let mut commands = self
            .register(0x70, 0)
            .register(0x70, 3) // ACKNOWLEDGE | DRIVER
            .register(0x24, 0)
            .register(0x20, 0x28) // MAC | MTU
            .register(0x28, 4096);
        for (queue, ring) in [(0, RX_RING), (1, TX_RING)] {
            commands = commands
                .register(0x30, queue)
                .register(0x38, QUEUE.into())
                .register(0x3c, 4096)
                .register(0x40, (ring >> 12) as u32);
        }
        if ready {
            commands.register(0x70, 7)
        } else {
            commands
        }
    }
    /// Publish ring entries up to `index` and notify `queue`.
    fn publish(self, queue: u32, index: u16) -> Self {
        let ring = if queue == 0 { RX_RING } else { TX_RING };
        self.store16(available(ring) + 2, index)
            .register(0x50, queue)
    }
    fn end(self) -> Vec<u8> {
        self.op(0, 0, 0).0
    }
}

/// Preloaded rings and buffers.
struct Memory {
    bytes: Vec<u8>,
    tx_descriptors: u16,
    tx_next: u64,
    /// Transmit chain heads in ring-slot order.
    published: Vec<u16>,
}

enum Part {
    Read(Vec<u8>),
    Write(u32),
    Outside,
}

impl Memory {
    fn new() -> Self {
        let mut memory = Self {
            bytes: vec![0; REGION_SIZE],
            tx_descriptors: 0,
            tx_next: TX_BUFFERS,
            published: Vec::new(),
        };
        // Every ring slot names the descriptor of the same index.
        for ring in [RX_RING, TX_RING] {
            for slot in 0..QUEUE {
                memory.put(available(ring) + 4 + 2 * slot as u64, &slot.to_le_bytes());
            }
        }
        memory
    }
    fn put(&mut self, at: u64, bytes: &[u8]) {
        let at = (at - REGION) as usize;
        self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
    }
    fn descriptor(&mut self, ring: u64, index: u16, address: u64, length: u32, flags: u16) {
        let mut bytes = address.to_le_bytes().to_vec();
        bytes.extend(length.to_le_bytes());
        bytes.extend(flags.to_le_bytes());
        bytes.extend((index + 1).to_le_bytes());
        self.put(ring + 16 * index as u64, &bytes);
    }
    /// One writable receive buffer per descriptor, filled with 0xcc.
    fn receive_buffers(&mut self, length: u32) {
        for index in 0..QUEUE {
            let address = RX_BUFFERS + index as u64 * RX_STRIDE;
            self.put(address, &vec![0xcc; RX_STRIDE as usize]);
            self.descriptor(RX_RING, index, address, length, 2);
        }
    }
    /// Add one transmit chain and publish its head in the next ring slot.
    fn transmit(&mut self, parts: &[Part]) {
        let head = self.tx_descriptors;
        for (index, part) in parts.iter().enumerate() {
            let (address, length, flags) = match part {
                Part::Read(bytes) => {
                    let at = self.tx_next;
                    self.put(at, bytes);
                    self.tx_next += (bytes.len() as u64).div_ceil(64) * 64;
                    (at, bytes.len() as u32, 0)
                }
                Part::Write(length) => {
                    let at = self.tx_next;
                    self.tx_next += u64::from(*length).div_ceil(64) * 64;
                    (at, *length, 2)
                }
                Part::Outside => (1 << 40, 64, 0),
            };
            let more = u16::from(index + 1 < parts.len());
            self.descriptor(TX_RING, head + index as u16, address, length, flags | more);
        }
        self.tx_descriptors += parts.len() as u16;
        let slot = self.published.len() as u64;
        self.put(available(TX_RING) + 4 + 2 * slot, &head.to_le_bytes());
        self.published.push(head);
    }
}

/// A distinguishable frame addressed to the device.
fn frame(index: u16, length: usize) -> Vec<u8> {
    let mut bytes = MAC.to_vec();
    bytes.extend([0x52, 0x54, 0, 0, 0, 2, 0x88, 0xb5]);
    bytes.resize(length, 0);
    if length >= 16 {
        bytes[14..16].copy_from_slice(&index.to_be_bytes());
    }
    for (i, byte) in bytes.iter_mut().enumerate().skip(16) {
        *byte = (i as u16 ^ index) as u8;
    }
    bytes
}

fn header(flags: u8, gso: u8) -> Vec<u8> {
    let mut bytes = vec![0; HEADER];
    bytes[0] = flags;
    bytes[1] = gso;
    bytes
}

struct Qmp {
    stream: BufReader<UnixStream>,
    stopped: bool,
}

impl Qmp {
    fn connect(path: &Path, child: &mut Child) -> Self {
        let started = Instant::now();
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(error) => {
                    assert!(child.try_wait().unwrap().is_none(), "QEMU exited: {error}");
                    assert!(started.elapsed() < Duration::from_secs(30), "{error}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
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
        let request = json!({"execute": name, "arguments": arguments});
        writeln!(self.stream.get_mut(), "{request}").unwrap();
        loop {
            let response = self.read();
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
    fn receive(&mut self, frames: &[Vec<u8>]) -> Result<bool, Value> {
        let frames: Vec<String> = frames.iter().map(hex::encode).collect();
        let result = self.command("seedbox-net-receive", json!({"frames": frames}))?;
        Ok(result["runnable"].as_bool().unwrap())
    }
    /// Deliver in chunks of at most `chunk`; returns the last runnable state.
    fn receive_all(&mut self, frames: &[Vec<u8>], chunk: usize) -> bool {
        let mut runnable = false;
        for part in frames.chunks(chunk) {
            runnable = self.receive(part).unwrap();
        }
        runnable
    }
    /// Drain every pending transmission, returning the chunk sizes too.
    fn drain(&mut self) -> (Vec<(u64, Vec<u8>)>, Vec<usize>) {
        let mut frames = Vec::new();
        let mut chunks = Vec::new();
        loop {
            let result = self.command("seedbox-net-drain", json!({})).unwrap();
            let chunk = result["frames"].as_array().unwrap();
            chunks.push(chunk.len());
            for item in chunk {
                frames.push((
                    item["time"].as_u64().unwrap(),
                    hex::decode(item["bytes"].as_str().unwrap()).unwrap(),
                ));
            }
            if !result["more"].as_bool().unwrap() {
                return (frames, chunks);
            }
        }
    }
    fn memory(&mut self, directory: &Path, address: u64, size: usize) -> Vec<u8> {
        let path = directory.join(format!("memory-{address:x}-{size}"));
        let arguments = json!({"val": address, "size": size, "filename": path});
        self.command("pmemsave", arguments).unwrap();
        std::fs::read(path).unwrap()
    }
    fn migrate(&mut self, path: &Path) {
        let uri = format!("file:{}", path.display());
        self.command("migrate", json!({"uri": uri})).unwrap();
        let started = Instant::now();
        while self.command("query-migrate", json!({})).unwrap()["status"] != "completed" {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(5));
        }
        // Return the source to stopped control, as Process::snapshot does.
        let started = Instant::now();
        while self.command("query-status", json!({})).unwrap()["status"] != "postmigrate" {
            assert!(started.elapsed() < Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(5));
        }
        self.command("seedbox-capture-complete", json!({})).unwrap();
    }
}

/// Owns the spawned QEMU from creation, so a failed connection or restore
/// still kills and reaps it. A panicking test reports its stderr.
struct Process {
    child: Child,
    stderr: PathBuf,
}

impl Drop for Process {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let status = self.child.try_wait();
            let stderr = std::fs::read_to_string(&self.stderr).unwrap_or_default();
            eprintln!("{} {status:?}: {stderr}", self.stderr.display());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Qemu {
    qmp: Qmp,
    directory: PathBuf,
    // Dropped after the QMP connection.
    _process: Process,
}

impl Qemu {
    /// Start stopped QEMU with the interpreter, `commands` and `memory`; or
    /// restore it from `incoming`.
    fn start(
        directory: &Path,
        name: &str,
        commands: &[u8],
        memory: &[u8],
        incoming: Option<&Path>,
    ) -> Self {
        let qemu = PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").expect("SEEDBOX_TCG_QEMU"));
        let file = |suffix: &str, bytes: &[u8]| {
            let path = directory.join(format!("{name}-{suffix}"));
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let program = file("program", &program());
        let commands = file("commands", commands);
        let memory = file("memory", memory);
        let qmp = directory.join(format!("{name}-qmp"));
        let mut command = Command::new(qemu);
        // The TCG machine's own board options (`qemu.rs`), without its
        // console, lifecycle port or inspection sockets.
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
            "-device",
            "seedbox-net-device,mac=52:54:00:00:00:01",
        ]);
        command.arg("-bios").arg(&program);
        for (path, address) in [(&commands, COMMANDS), (&memory, REGION)] {
            command
                .arg("-device")
                .arg(format!("loader,file={},addr={address:#x}", path.display()));
        }
        command
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp.display()));
        if incoming.is_some() {
            command.args(["-incoming", "defer"]);
        }
        let stderr = directory.join(format!("{name}-stderr"));
        let mut process = Process {
            child: command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
            stderr,
        };
        let mut qmp = Qmp::connect(&qmp, &mut process.child);
        if let Some(path) = incoming {
            let uri = format!("file:{}", path.display());
            qmp.command("migrate-incoming", json!({"uri": uri}))
                .unwrap();
            let started = Instant::now();
            while qmp.command("query-status", json!({})).unwrap()["status"] != "paused" {
                assert!(started.elapsed() < Duration::from_secs(30));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Self {
            qmp,
            directory: directory.to_owned(),
            _process: process,
        }
    }
    fn memory(&mut self, address: u64, size: usize) -> Vec<u8> {
        let directory = self.directory.clone();
        self.qmp.memory(&directory, address, size)
    }
    /// The used ring of `ring`: (descriptor, length) pairs.
    fn used(&mut self, ring: u64) -> Vec<(u32, u32)> {
        let bytes = self.memory(used_ring(ring), 4 + 8 * QUEUE as usize);
        let index = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
        (0..index)
            .map(|i| {
                let slot = 4 + (i % QUEUE as usize) * 8;
                let entry = &bytes[slot..slot + 8];
                (
                    u32::from_le_bytes(entry[..4].try_into().unwrap()),
                    u32::from_le_bytes(entry[4..].try_into().unwrap()),
                )
            })
            .collect()
    }
    fn used_index(&mut self, ring: u64) -> u16 {
        let bytes = self.memory(used_ring(ring) + 2, 2);
        u16::from_le_bytes([bytes[0], bytes[1]])
    }
    fn rx_buffer(&mut self, index: u64, length: usize) -> Vec<u8> {
        self.memory(RX_BUFFERS + index * RX_STRIDE, length)
    }
    /// A stopped run must report the given reason.
    fn run_until(&mut self, time: u64, budget: u64, reason: &str) -> Value {
        let stop = self.qmp.run(time, budget);
        assert_eq!(stop["reason"], reason, "{stop}");
        stop
    }
}

const BUDGET: u64 = 50_000_000;

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_offers_exactly_mac_and_mtu() {
    let directory = tempfile::tempdir().unwrap();
    let commands = Commands::default()
        .register(0x14, 0)
        .read(0x10, RESULTS)
        .register(0x14, 1)
        .read(0x10, RESULTS + 4)
        .read(0x100, RESULTS + 8)
        .read(0x104, RESULTS + 12)
        .read(0x108, RESULTS + 16)
        .read(0x04, RESULTS + 20)
        .end();
    let mut qemu = Qemu::start(
        directory.path(),
        "features",
        &commands,
        &Memory::new().bytes,
        None,
    );
    qemu.run_until(0, BUDGET, "blocked");
    let results = qemu.memory(RESULTS, 24);
    assert_eq!(&results[..4], 0x28u32.to_le_bytes(), "MAC | MTU only");
    assert_eq!(&results[4..8], [0; 4], "no high features");
    assert_eq!(&results[8..14], MAC);
    assert_eq!(&results[14..18], [0; 4], "status and queue pairs unused");
    assert_eq!(&results[18..20], 1500u16.to_le_bytes());
    assert_eq!(&results[20..24], 1u32.to_le_bytes(), "legacy virtio-mmio");
}

/// Valid frames at indices 0, 6 and 7; the rest are dropped.
fn transmit_memory() -> Memory {
    let mut memory = Memory::new();
    memory.receive_buffers(RX_STRIDE as u32);
    let payload = |index, length| frame(index, length);
    memory.transmit(&[Part::Read(header(0, 0)), Part::Read(payload(0, 60))]);
    memory.transmit(&[Part::Read(header(1, 0)), Part::Read(payload(1, 60))]);
    memory.transmit(&[Part::Read(header(0, 1)), Part::Read(payload(2, 60))]);
    memory.transmit(&[Part::Read(header(0, 0)), Part::Read(payload(3, 13))]);
    memory.transmit(&[Part::Read(header(0, 0)), Part::Read(payload(4, 1515))]);
    memory.transmit(&[Part::Read(vec![0; 9])]);
    let large = payload(6, 1514);
    let mut first = header(0, 0);
    first.extend(&large[..4]);
    memory.transmit(&[
        Part::Read(first),
        Part::Read(large[4..700].to_vec()),
        Part::Read(large[700..].to_vec()),
    ]);
    let mut single = header(0, 0);
    single.extend(payload(7, 14));
    memory.transmit(&[Part::Read(single)]);
    memory
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_transmits_at_the_kick_and_drops_invalid_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    let memory = transmit_memory();
    let heads: Vec<u32> = memory.published.iter().map(|&h| h.into()).collect();
    let commands = Commands::default().setup(true).publish(1, 8).end();
    let mut results = Vec::new();
    for name in ["first", "second"] {
        let mut qemu = Qemu::start(directory, name, &commands, &memory.bytes, None);
        let stop = qemu.run_until(0, BUDGET, "blocked");
        let (frames, _) = qemu.qmp.drain();
        let expected = [frame(0, 60), frame(6, 1514), frame(7, 14)];
        assert_eq!(
            frames.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
            expected
        );
        let time = frames[0].0;
        assert!(frames.iter().all(|(t, _)| *t == time), "one kick");
        assert!(time > 0 && time <= stop["time"].as_u64().unwrap());
        // Every chain completes, including dropped ones, with length zero.
        let used: Vec<(u32, u32)> = heads.iter().map(|&h| (h, 0)).collect();
        assert_eq!(qemu.used(TX_RING), used);
        // Drained exactly once.
        assert_eq!(qemu.qmp.drain().0, []);
        results.push((stop, frames));
    }
    assert_eq!(results[0], results[1], "repeatable");
    // The stamp is the kick instruction's completion: one nanosecond less
    // stops before it, and exactly that budget includes it.
    let time = results[0].1[0].0;
    let mut before = Qemu::start(directory, "before", &commands, &memory.bytes, None);
    before.run_until(0, time - 1, "boundary");
    assert_eq!(before.qmp.drain().0, []);
    let mut at = Qemu::start(directory, "at", &commands, &memory.bytes, None);
    at.run_until(0, time, "boundary");
    assert_eq!(at.qmp.drain().0, results[0].1);
    // Before DRIVER_OK a kick is ignored; the chains stay pending.
    let early = Commands::default().setup(false).publish(1, 8).end();
    let mut ignored = Qemu::start(directory, "early", &early, &memory.bytes, None);
    ignored.run_until(0, BUDGET, "blocked");
    assert_eq!(ignored.qmp.drain().0, []);
    assert_eq!(ignored.used_index(TX_RING), 0);
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_malformed_transmit_rings_fail_the_machine() {
    type Case = (&'static str, fn(&mut Memory), u16);
    let cases: [Case; 3] = [
        (
            "writable descriptor",
            |memory| {
                memory.transmit(&[Part::Read(header(0, 0)), Part::Write(64)]);
            },
            2,
        ),
        (
            "descriptor outside RAM",
            |memory| {
                memory.transmit(&[Part::Read(header(0, 0)), Part::Outside]);
            },
            2,
        ),
        ("available index beyond the queue", |_| {}, 1 + QUEUE + 1),
    ];
    for (name, bad, published) in cases {
        let directory = tempfile::tempdir().unwrap();
        let mut memory = Memory::new();
        memory.transmit(&[Part::Read(header(0, 0)), Part::Read(frame(0, 60))]);
        bad(&mut memory);
        let commands = Commands::default().setup(true).publish(1, published).end();
        let mut qemu = Qemu::start(directory.path(), "error", &commands, &memory.bytes, None);
        let stop = qemu.run_until(0, BUDGET, "error");
        let frames = qemu.qmp.drain().0;
        if published == 2 {
            assert_eq!(frames, [(frames[0].0, frame(0, 60))], "{name}: prefix");
            assert_eq!(qemu.used(TX_RING), [(0, 0)], "{name}");
        }
        let again = qemu.qmp.command(
            "seedbox-run",
            json!({"time": stop["time"], "budget": 10, "factor": 1000}),
        );
        let error = again.unwrap_err().to_string();
        assert!(error.contains("failed seedbox device"), "{name}: {error}");
    }
}

/// Deliver, backlog, tail-drop, refill and short-buffer behavior, from one
/// boot or from a restored migration with a full backlog.
fn receive_scenario(directory: &Path, name: &str, chunk: usize, migrate: bool) -> Vec<u8> {
    let mut memory = Memory::new();
    memory.receive_buffers(RX_STRIDE as u32);
    // Descriptor 255 is structurally valid but too short for any frame.
    memory.descriptor(RX_RING, 255, RX_BUFFERS + 255 * RX_STRIDE, 20, 2);
    let commands = Commands::default()
        .setup(true)
        .publish(0, 2) // buffers 0 and 1
        .wait()
        .delay(1_000_000) // execution continues while the backlog fills
        .publish(0, 256) // buffers 2..=255
        .wait()
        .publish(0, 258) // buffers 0 and 1 again
        .wait()
        .publish(0, 259) // buffer 2 again: the backlog is empty
        .end();
    let mut qemu = Qemu::start(directory, name, &commands, &memory.bytes, None);
    // Before the driver is ready, frames are dropped.
    assert!(qemu.qmp.receive(&[frame(999, 60)]).unwrap());
    let stop = qemu.run_until(0, BUDGET, "blocked");
    assert!(stop.get("deadline").is_none());
    // An empty delivery leaves the idle CPU blocked.
    assert!(!qemu.qmp.receive(&[]).unwrap());
    let a = frame(1000, 60);
    let b = frame(1001, 1514);
    let c = frame(1002, 14);
    // a and b fill the posted buffers; c waits. The interrupt wakes the CPU.
    assert!(qemu
        .qmp
        .receive(&[a.clone(), b.clone(), c.clone()])
        .unwrap());
    assert_eq!(qemu.used(RX_RING), [(0, 70), (1, 1524)]);
    let mut expected = vec![0; HEADER];
    expected.extend(&a);
    assert_eq!(qemu.rx_buffer(0, 70), expected);
    let time = stop["time"].as_u64().unwrap();
    qemu.run_until(time, 100_000, "boundary");
    let time = time + 100_000;
    // c plus d0..=d254 fill the backlog; d255.. are dropped. The CPU is
    // still executing its delay loop.
    let d: Vec<Vec<u8>> = (0..300).map(|i| frame(i, 60 + i as usize)).collect();
    assert!(qemu.qmp.receive_all(&d, chunk));
    let mut qemu = if migrate {
        let image = directory.join(format!("{name}-migration"));
        qemu.qmp.migrate(&image);
        let mut restored = Qemu::start(
            directory,
            &format!("{name}-restored"),
            &commands,
            &memory.bytes,
            Some(&image),
        );
        // The source keeps its own backlog after capture.
        qemu.run_until(time, BUDGET, "blocked");
        assert_eq!(qemu.used_index(RX_RING), 258);
        restored.run_until(time, BUDGET, "blocked");
        restored
    } else {
        qemu.run_until(time, BUDGET, "blocked");
        qemu
    };
    // Refills drain in FIFO order: c, d0..=d251 into buffers 2..=254; d252
    // meets the short buffer 255 and is dropped; d253, d254 reuse 0 and 1.
    let used = qemu.used(RX_RING);
    assert_eq!(used.len(), 258, "later frames were tail-dropped");
    assert_eq!(used[2], (2, 24));
    for (i, entry) in used[3..255].iter().enumerate() {
        assert_eq!(*entry, (3 + i as u32, (HEADER + 60 + i) as u32));
    }
    assert_eq!(used[255], (255, 0), "short buffer consumed without data");
    assert_eq!(qemu.rx_buffer(255, 20), [0xcc; 20]);
    assert_eq!(used[256], (0, (HEADER + 60 + 253) as u32));
    assert_eq!(used[257], (1, (HEADER + 60 + 254) as u32));
    let mut expected = vec![0; HEADER];
    expected.extend(&d[254]);
    assert_eq!(qemu.rx_buffer(1, expected.len()), expected);
    let mut expected = vec![0; HEADER];
    expected.extend(&c);
    assert_eq!(qemu.rx_buffer(2, expected.len()), expected);
    qemu.memory(RX_BUFFERS, QUEUE as usize * RX_STRIDE as usize)
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_receives_backlogs_and_refills_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    let whole = receive_scenario(directory, "chunk64", 64, false);
    // Transport chunking is not a simulated limit.
    assert_eq!(receive_scenario(directory, "chunk7", 7, false), whole);
    // A migrated backlog continues identically.
    assert_eq!(receive_scenario(directory, "migrated", 64, true), whole);
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_reset_clears_the_backlog() {
    // A status reset, or disabling the legacy receive queue (QueuePFN 0,
    // which resets the device), discards backlogged frames; with neither,
    // the refill delivers all three in order.
    for reset in ["none", "status", "receive queue"] {
        let directory = tempfile::tempdir().unwrap();
        let mut memory = Memory::new();
        memory.receive_buffers(RX_STRIDE as u32);
        let mut commands = Commands::default().setup(true).delay(1_000_000);
        commands = match reset {
            "status" => commands.setup(true),
            "receive queue" => commands.register(0x30, 0).register(0x40, 0).setup(true),
            _ => commands,
        };
        let commands = commands.publish(0, 4).end();
        let mut qemu = Qemu::start(directory.path(), "reset", &commands, &memory.bytes, None);
        qemu.run_until(0, 100_000, "boundary");
        let frames: Vec<Vec<u8>> = (0..3).map(|i| frame(i, 60)).collect();
        assert!(qemu.qmp.receive(&frames).unwrap(), "still executing");
        qemu.run_until(100_000, BUDGET, "blocked");
        if reset == "none" {
            let used: Vec<(u32, u32)> = (0..3).map(|i| (i, 70)).collect();
            assert_eq!(qemu.used(RX_RING), used);
            for i in 0..3u16 {
                let mut expected = vec![0; HEADER];
                expected.extend(frame(i, 60));
                assert_eq!(qemu.rx_buffer(i.into(), 70), expected);
            }
        } else {
            assert_eq!(qemu.used_index(RX_RING), 0, "{reset}");
        }
    }
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_rejects_bad_requests_and_malformed_receive_rings() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    let mut memory = Memory::new();
    memory.receive_buffers(RX_STRIDE as u32);
    let commands = Commands::default().setup(true).publish(0, 1).wait().end();
    let mut qemu = Qemu::start(directory, "requests", &commands, &memory.bytes, None);
    let stop = qemu.run_until(0, BUDGET, "blocked");
    // Whole requests are validated before any frame is accepted.
    let many: Vec<Vec<u8>> = (0..65).map(|i| frame(i, 60)).collect();
    assert!(qemu.qmp.receive(&many).is_err());
    for length in [13, 1515] {
        assert!(qemu.qmp.receive(&[frame(0, 60), frame(1, length)]).is_err());
    }
    for bad in ["zz", "ABCDEF0123456789ABCDEF0123456789", "123"] {
        let result = qemu
            .qmp
            .command("seedbox-net-receive", json!({"frames": [bad]}));
        assert!(result.is_err(), "{bad}");
    }
    assert_eq!(qemu.used_index(RX_RING), 0);
    assert!(qemu.qmp.receive(&[frame(7, 60)]).unwrap());
    assert_eq!(qemu.used(RX_RING), [(0, 70)]);
    qemu.run_until(stop["time"].as_u64().unwrap(), BUDGET, "blocked");

    for (name, flags, address) in [
        ("readable receive descriptor", 0, RX_BUFFERS),
        ("receive descriptor outside RAM", 2, 1 << 40),
    ] {
        let mut memory = Memory::new();
        memory.receive_buffers(RX_STRIDE as u32);
        memory.descriptor(RX_RING, 0, address, RX_STRIDE as u32, flags);
        let mut qemu = Qemu::start(directory, "malformed", &commands, &memory.bytes, None);
        let stop = qemu.run_until(0, BUDGET, "blocked");
        let error = qemu.qmp.receive(&[frame(0, 60)]).unwrap_err().to_string();
        assert!(error.contains("malformed receive ring"), "{name}: {error}");
        // The failure is sticky: neither execution nor delivery resumes.
        let again = qemu.qmp.command(
            "seedbox-run",
            json!({"time": stop["time"], "budget": 10, "factor": 1000}),
        );
        let error = again.unwrap_err().to_string();
        assert!(error.contains("failed seedbox device"), "{name}: {error}");
        assert!(qemu.qmp.receive(&[frame(0, 60)]).is_err(), "{name}");
        assert_eq!(qemu.used_index(RX_RING), 0, "{name}");
    }
}

#[test]
#[ignore = "requires patched QEMU (SEEDBOX_TCG_QEMU)"]
fn tcg_network_device_migrates_undrained_transmissions_in_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let directory = directory.path();
    let mut memory = Memory::new();
    for index in 0..QUEUE {
        let mut chain = header(0, 0);
        chain.extend(frame(index, 60));
        memory.transmit(&[Part::Read(chain)]);
    }
    // Ring slots name heads modulo the queue size, so the 256 chains are
    // reused: 600 frames from three kicks of 200 in one run.
    let commands = Commands::default()
        .setup(true)
        .publish(1, 200)
        .publish(1, 400)
        .publish(1, 600)
        .end();
    let mut qemu = Qemu::start(directory, "source", &commands, &memory.bytes, None);
    qemu.run_until(0, BUDGET, "blocked");
    let image = directory.join("migration");
    qemu.qmp.migrate(&image);
    let mut restored = Qemu::start(
        directory,
        "restored",
        &commands,
        &memory.bytes,
        Some(&image),
    );
    let (frames, chunks) = restored.qmp.drain();
    assert_eq!(chunks, [256, 256, 88]);
    assert_eq!(frames.len(), 600);
    for (index, (_, bytes)) in frames.iter().enumerate() {
        assert_eq!(*bytes, frame((index % QUEUE as usize) as u16, 60));
    }
    assert!(frames.windows(2).all(|w| w[0].0 <= w[1].0));
    let kicks: std::collections::BTreeSet<u64> = frames.iter().map(|(t, _)| *t).collect();
    assert_eq!(kicks.len(), 3, "one stamp per kick");
    assert_eq!(qemu.qmp.drain(), (frames, chunks), "source keeps its own");
}
