//! Private stopped native machine. The calling Driver alone advances shared
//! time. Device service and timer delivery happen at deterministic guest exits.
//!
//! This module owns the architecture-neutral PV time page, event registers,
//! lifecycle console, the optional block and network devices and snapshot files. The private `arch` module supplies
//! boot, interrupt controller, platform devices and the native backend.
#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "aarch64")]
use aarch64 as arch;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "x86_64")]
use x86_64 as arch;
mod block;
mod console;
mod evidence;
mod memory;
mod net;
pub(super) mod snapshot;
mod virtio;
mod witness;
use super::{
    boot,
    protocol::{self, BootDisk, Request, Response, Stop},
};
use arch::{Architecture, Backend, Devices, Exit, Native};
use memory::Memory;
use seedbox_machine::machine::disk::Store;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    rc::Rc,
};
#[cfg(test)]
mod resource_tests;

type Result<T> = std::result::Result<T, String>;
// Seedbox PV v3 shared-page ABI. Guest advances LAST while executing; a stopped
// forward adoption aligns it to canonical time without consuming pending work.
// Guest may also advance the exclusive epoch on exception return; all other
// publication words are host-owned.
const ABI: u64 = 3;
const PAGE_BYTES: usize = 16384;
const NOW: usize = 0;
const LAST: usize = 8;
const BOUND: usize = 16;
const VERSION: usize = 24;
const IRQ_DEFERRED: usize = 32;
// Word 40 is the ARM exclusive-reservation epoch (see aarch64.rs).
const SYSCALL_COST: usize = 48;
const EVENT_MMIO: u64 = 0x0a01_0000;
const EVENT_END: u64 = EVENT_MMIO + PAGE_BYTES as u64 - 1;
const LIFECYCLE_MMIO: u64 = 0x0b01_0000;
const LIFECYCLE_END: u64 = LIFECYCLE_MMIO + PAGE_BYTES as u64 - 1;
/// The block device window, mapped only when the boot has a disk; the guest
/// learns of it only then, so diskless boots are unchanged.
const DISK_MMIO: u64 = 0x0b02_0000;
const DISK_END: u64 = DISK_MMIO + PAGE_BYTES as u64 - 1;
/// The NIC window, mapped and announced only when the boot has a NIC.
const NET_MMIO: u64 = 0x0b03_0000;
const NET_END: u64 = NET_MMIO + 0xfff;
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    /// Persistable native interaction chain, not serialized hasher internals.
    witness: [u8; 32],
    witness_boot: u64,
    witness_origin: [u8; 32],
    now: u64,
    deadline: Option<u64>,
    timer: bool,
    ready: bool,
    shutdown: bool,
    idle: bool,
    devices: Devices,
    console: console::Console,
    /// The boot's NIC, including its backlog and untaken transmissions.
    net: Option<net::Net>,
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    #[serde(default)]
    policy: Option<super::policy::Runtime>,
}
/// Snapshot header. With a disk, the file ends with the store's encoded
/// overlays after RAM; its base stays with the boot inputs.
#[derive(Serialize, Deserialize)]
struct Capture {
    memory_bytes: u64,
    layout_version: u32,
    page_bytes: u64,
    segments: Vec<(u64, u64)>,
    state: State,
    architecture: Architecture,
    block: Option<block::Transport>,
}
/// Result of one architecture-specific exit handled at a stopped rendezvous.
#[derive(Default)]
struct Effect {
    response: Option<u64>,
    idle: bool,
    /// A guest terminal request: [`Stop::PoweredOff`] or [`Stop::Reset`].
    terminal: Option<Stop>,
}
struct Machine<B: Backend = Native> {
    native: B,
    ram: Rc<Memory>,
    page: Rc<Memory>,
    state: State,
    /// The boot's block device; the worker owns its store until the client
    /// exports it at boot end.
    block: Option<block::Block>,
    output: Vec<u8>,
    /// The guest ended this boot; it must never be entered again.
    ended: bool,
}
impl<B: Backend> Machine<B> {
    #[cfg(test)]
    fn boot(
        kernel: &[u8],
        initrd: &[u8],
        seed: u64,
        command_line: &str,
        disk: Option<Store>,
        network: Option<[u8; 6]>,
    ) -> Result<Self> {
        Self::boot_with_memory(
            kernel,
            initrd,
            seed,
            command_line,
            disk,
            network,
            boot::RAM_BYTES as u64,
        )
    }
    fn boot_with_memory(
        kernel: &[u8],
        initrd: &[u8],
        seed: u64,
        command_line: &str,
        disk: Option<Store>,
        network: Option<[u8; 6]>,
        memory_bytes: u64,
    ) -> Result<Self> {
        boot::arguments(command_line)?;
        let layout = boot::RamLayout::native(memory_bytes)?;
        boot::layout_for(&layout, kernel, initrd)?;
        let mut ram = Memory::ram(layout)?;
        let page = Rc::new(Memory::new(PAGE_BYTES)?);
        let entry = arch::load(
            &mut ram,
            kernel,
            initrd,
            seed,
            command_line,
            disk.is_some(),
            network.is_some(),
        )?;
        page.set_word(BOUND, u64::MAX);
        page.set_word(VERSION, ABI);
        page.set_word(SYSCALL_COST, 1000);
        let ram = Rc::new(ram);
        let mut native = B::create(ram.clone(), page.clone())?;
        arch::reset(&mut native, &entry)?;
        Ok(Self {
            native,
            ram,
            page,
            state: State {
                witness: seedbox_machine::machine::execution_evidence::chain::initial(),
                witness_origin: seedbox_machine::machine::execution_evidence::chain::initial(),
                net: network.map(net::Net::new),
                ..State::default()
            },
            block: disk.map(block::Block::new),
            output: Vec::new(),
            ended: false,
        })
    }
    fn publish(&self, upto: u64) {
        self.page.set_word(NOW, self.state.now);
        self.page
            .set_word(BOUND, self.state.deadline.map_or(upto, |d| d.min(upto)));
        self.page.set_word(VERSION, ABI);
        self.page
            .set_word(IRQ_DEFERRED, self.state.devices.deferred() as u64);
    }
    fn adopt(&mut self, now: u64) -> Result<()> {
        if now < self.state.now || now == u64::MAX {
            return Err("PV time adoption backwards or infinite".into());
        }
        if now > self.state.now {
            // Advancing idle/paused time must not pay for the first pending
            // progress read. Its next successful read starts after this point.
            self.page.set_word(LAST, self.page.word(LAST).max(now));
        }
        self.state.now = now;
        self.interrupts()?;
        Ok(())
    }
    fn collect_lifecycle(&mut self) {
        self.state.writes.extend(
            self.state
                .console
                .output
                .drain()
                .into_iter()
                .map(seedbox_machine::pv::transport::GuestWrite::Lifecycle),
        );
    }
    /// Move NIC transmissions, already stamped, into the ordered write stream.
    fn collect_network(&mut self) {
        if let Some(net) = &mut self.state.net {
            self.state
                .writes
                .extend(net.take_transmitted().into_iter().map(|frame| {
                    seedbox_machine::pv::transport::GuestWrite::Frame(
                        seedbox_machine::pv::transport::LifecycleWrite {
                            time: frame.time,
                            bytes: frame.bytes,
                        },
                    )
                }));
        }
    }
    /// Admit host frames to the NIC in order at the current stopped time.
    /// Each is consumed; delivery, backlog or drop is the NIC's decision.
    fn receive_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        if self.state.net.is_none() {
            return Err("PV frames for a boot without a NIC".into());
        }
        self.boundary()?;
        let net = self.state.net.as_mut().expect("checked NIC");
        for frame in frames {
            unsafe { self.ram.modify(|ram| net.receive(ram, frame)) }?;
        }
        self.interrupts()
    }
    /// Record one guest console byte at the current stopped time.
    fn console_byte(&mut self, byte: u8) {
        self.output.push(byte);
        self.state
            .writes
            .push(seedbox_machine::pv::transport::GuestWrite::Console(
                seedbox_machine::pv::transport::LifecycleWrite {
                    time: self.state.now,
                    bytes: vec![byte],
                },
            ));
    }
    fn quiet(&self) -> bool {
        !self.state.timer && self.state.devices.quiet()
    }
    fn run(
        &mut self,
        now: u64,
        upto: u64,
        syscall_cost_ns: u64,
        shutdown: bool,
        input: &[u8],
        frames: Vec<Vec<u8>>,
    ) -> Result<Stop> {
        if now > upto || upto == u64::MAX || syscall_cost_ns == 0 {
            return Err("PV invalid run interval".into());
        }
        if self.ended {
            return Err("PV run after the guest ended its boot".into());
        }
        // Guest syscall progress samples this word once into its remaining
        // charge. Existing partial work keeps its old price in captured CPU/RAM.
        // Republish even after restore and on zero-budget adoption; the caller,
        // not the snapshot's cached publication, owns future execution pricing.
        let cost = match &self.state.policy {
            Some(p) => p.charge(syscall_cost_ns)?,
            None => syscall_cost_ns,
        };
        self.page.set_word(SYSCALL_COST, cost);
        self.adopt(now)?;
        if !input.is_empty() {
            self.boundary()?;
            self.state.console.input(input);
            unsafe {
                self.ram
                    .modify(|ram| self.state.console.process(ram, self.state.now))
            }?;
            self.interrupts()?;
        }
        self.receive_frames(frames)?;
        self.collect_lifecycle();
        if shutdown && !self.state.shutdown {
            self.state.shutdown = true;
            self.boundary()?;
            self.state.console.shutdown();
            unsafe {
                self.ram
                    .modify(|ram| self.state.console.process(ram, self.state.now))
            }?;
            self.interrupts()?;
        }
        self.collect_lifecycle();
        self.publish(upto);
        if self.state.idle && self.quiet() {
            return Ok(Stop::Blocked(self.state.deadline));
        }
        if self.state.now == upto {
            return Ok(Stop::Horizon);
        }
        self.state.idle = false;
        self.prepare()?;
        loop {
            let exit = self.native.enter()?;
            // Host preemption/cancellation is invisible to simulated state.
            if matches!(exit, Exit::Interrupted) {
                continue;
            }
            // Backend maintenance exits are not guest rendezvous either.
            if self.maintenance(&exit)? {
                continue;
            }
            // Only a real semantic guest rendezvous invalidates reservations.
            // Maintenance exits above and host interruptions do not.
            self.boundary()?;
            let last = self.page.word(LAST);
            if last == u64::MAX || last > upto {
                return Err("PV guest crossed accepted horizon".into());
            }
            self.state.now = self.state.now.max(last);
            self.record_exit(&exit)?;
            let mut effect = Effect::default();
            match exit {
                Exit::Mmio {
                    address,
                    width,
                    write,
                    value,
                } if (EVENT_MMIO..=EVENT_END).contains(&address) => {
                    if width != 8 || !write {
                        return Err("PV clockevent access".into());
                    }
                    match address - EVENT_MMIO {
                        0 => {
                            self.state.deadline = match &mut self.state.policy {
                                Some(policy) => policy.arm(value)?,
                                None => {
                                    if value == 0 {
                                        None
                                    } else {
                                        Some(value)
                                    }
                                }
                            }
                        }
                        8 => {
                            self.state.timer = false;
                            self.lower(arch::TIMER)?;
                        }
                        16 => {
                            if !self.state.ready {
                                return Err("PV idle before handshake".into());
                            }
                            effect.idle = true;
                        }
                        24 => {
                            self.record_native(b'Y', 0, upto, 0);
                            if !self.state.devices.deferred() {
                                self.state.now = self
                                    .state
                                    .deadline
                                    .map_or(upto, |d| d.min(upto))
                                    .max(self.state.now);
                            }
                        }
                        32 => {
                            if value != ABI || self.state.ready {
                                return Err("PV invalid ready handshake".into());
                            }
                            self.state.ready = true;
                        }
                        offset => {
                            if self.event_register(offset, value)? {
                                effect.terminal = Some(Stop::PoweredOff);
                            }
                        }
                    }
                }
                Exit::Mmio {
                    address,
                    width,
                    write,
                    value,
                } if (LIFECYCLE_MMIO..=LIFECYCLE_END).contains(&address) => {
                    if width != 4 {
                        return Err("virtio MMIO requires 32 bits".into());
                    }
                    let offset = address - LIFECYCLE_MMIO;
                    if write {
                        unsafe {
                            self.ram.modify(|ram| {
                                self.state
                                    .console
                                    .write(ram, offset, value as u32, self.state.now)
                            })
                        }?;
                        self.collect_lifecycle();
                    } else {
                        effect.response = Some(self.state.console.read(offset)? as u64);
                    }
                }
                Exit::Mmio {
                    address,
                    width,
                    write,
                    value,
                } if self.block.is_some() && (DISK_MMIO..=DISK_END).contains(&address) => {
                    if width != 4 {
                        return Err("virtio MMIO requires 32 bits".into());
                    }
                    let offset = address - DISK_MMIO;
                    let block = self.block.as_mut().expect("disk window mapped");
                    if write {
                        let h = &mut self.state.witness;
                        let now = self.state.now;
                        unsafe {
                            self.ram.modify(|ram| block.write_traced(ram, offset, value as u32, |kind, status, sector| {
                                *h = seedbox_machine::machine::execution_evidence::chain::record(h, b'd', 0, now, sector, kind as u64 | ((status as u64) << 32));
                            }))
                        }?;
                    } else {
                        effect.response = Some(block.read(offset)? as u64);
                    }
                }
                Exit::Mmio {
                    address,
                    width,
                    write,
                    value,
                } if self.state.net.is_some() && (NET_MMIO..=NET_END).contains(&address) => {
                    let offset = address - NET_MMIO;
                    let net = self.state.net.as_mut().expect("NIC window mapped");
                    if write {
                        let now = self.state.now;
                        unsafe {
                            self.ram
                                .modify(|ram| net.write(ram, offset, width, value as u32, now))
                        }?;
                        self.collect_network();
                    } else {
                        effect.response = Some(net.read(offset, width)? as u64);
                    }
                }
                exit => effect = self.platform(exit)?,
            }
            self.interrupts()?;
            self.native.complete(effect.response)?;
            self.collect_lifecycle();
            self.publish(upto);
            // A guest terminal request ends the boot at this rendezvous. Before
            // the ready handshake it is a setup failure, not a guest outcome.
            if let Some(stop) = effect.terminal {
                if !self.state.ready {
                    return Err(format!("PV guest terminal {stop:?} before handshake"));
                }
                self.finish_stop()?;
                self.ended = true;
                return Ok(stop);
            }
            if self.state.now == upto {
                self.finish_stop()?;
                return Ok(Stop::Horizon);
            }
            if effect.idle && self.quiet() {
                self.state.idle = true;
                self.finish_stop()?;
                return Ok(Stop::Blocked(self.state.deadline));
            }
            // Host interruptions re-enter above without preparing anything new.
            self.prepare()?;
        }
    }
    fn capture(&mut self, path: &Path) -> Result<()> {
        // The client owns an ended boot's exit state without a live worker.
        if self.ended {
            return Err("PV capture after the guest ended its boot".into());
        }
        let layout = self.ram.layout()?;
        let state = Capture {
            memory_bytes: layout.capacity,
            layout_version: boot::LAYOUT_VERSION,
            page_bytes: PAGE_BYTES as u64,
            segments: layout
                .segments
                .iter()
                .map(|r| (r.start, r.end - r.start))
                .collect(),
            state: self.state.clone(),
            architecture: self.native.capture()?,
            block: self.block.as_ref().map(|block| block.transport().clone()),
        };
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("PV snapshot create: {e}"))?;
        protocol::send(&mut file, &state)?;
        unsafe { file.write_all(self.page.bytes()) }
            .map_err(|e| format!("PV snapshot page: {e}"))?;
        for region in &layout.segments {
            let offset = layout.offset(region.start, (region.end - region.start) as usize)?;
            let bytes =
                unsafe { &self.ram.bytes()[offset..offset + (region.end - region.start) as usize] };
            file.write_all(bytes)
                .map_err(|e| format!("PV snapshot RAM: {e}"))?;
        }
        if let Some(block) = &self.block {
            file.write_all(&block.store().encode_overlays())
                .map_err(|e| format!("PV snapshot disk: {e}"))?;
        }
        Ok(())
    }
    /// Write the disk's encoded overlays, unchanged, to a new file. Unlike a
    /// capture this is allowed after the guest ended its boot.
    fn export_disk(&self, path: &Path) -> Result<()> {
        let block = self.block.as_ref().ok_or("PV disk export without a disk")?;
        File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut file| file.write_all(&block.store().encode_overlays()))
            .map_err(|e| format!("PV disk export: {e}"))
    }
}
/// The boot's disk with the client's overlays applied, plus its base alone
/// for later restores.
fn open_disk(disk: &BootDisk) -> Result<(Store, Store)> {
    let base = match &disk.base {
        Some(path) => {
            let image = std::fs::read(path).map_err(|e| format!("PV disk base read: {e}"))?;
            if image.len() as u64 != disk.sectors.saturating_mul(512) {
                return Err("PV disk base length differs from its capacity".into());
            }
            Store::from_image(image)
        }
        None => Store::zeroed(disk.sectors),
    }
    .map_err(|e| format!("PV disk base: {e:?}"))?;
    let mut store = base.clone();
    if let Some(path) = &disk.overlay {
        let bytes = std::fs::read(path).map_err(|e| format!("PV disk overlay read: {e}"))?;
        store
            .replace_overlays(&bytes)
            .map_err(|e| format!("PV disk overlay: {e:?}"))?;
    }
    Ok((store, base))
}
/// A capture file's parts; `overlays` is empty without a disk.
struct Saved {
    capture: Capture,
    file: File,
    data_start: u64,
    overlays: Vec<u8>,
}
/// Owned inputs for initial Boot or its staged native restoration.
struct BootInputs {
    witness: protocol::BootWitness,
    memory_bytes: u64,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    seed: u64,
    command_line: String,
    /// The disk base with empty overlays.
    disk: Option<Store>,
    /// The NIC's MAC address, if the boot has a NIC.
    network: Option<[u8; 6]>,
    policy: Option<protocol::BootPolicy>,
}
fn read_snapshot(path: &Path, inputs: &BootInputs) -> Result<Saved> {
    let mut file = File::open(path).map_err(|e| format!("PV snapshot open: {e}"))?;
    let capture: Capture = protocol::receive(&mut file)?.ok_or("PV snapshot header absent")?;
    if capture.state.witness_boot != inputs.witness.boot
        || capture.state.witness_origin != inputs.witness.initial
    {
        return Err("PV saved execution witness owner/origin".into());
    }
    if capture.state.policy.as_ref().map(|p| &p.setup) != inputs.policy.as_ref() {
        return Err("PV saved worker policy differs from boot authority".into());
    }
    if let Some(p) = &capture.state.policy {
        p.validate()?;
    }
    // Pure header checks precede overlay maps, native mmap/VM creation and
    // full page/RAM reads. Private JSON remains an internal worker protocol.
    let layout = capture.validate_snapshot(
        inputs.memory_bytes,
        inputs.disk.is_some(),
        inputs.network,
        None,
    )?;
    let data_start = file
        .stream_position()
        .map_err(|e| format!("PV snapshot position: {e}"))?;
    let end = data_start
        .checked_add(PAGE_BYTES as u64)
        .and_then(|v| v.checked_add(layout.capacity))
        .ok_or("PV snapshot length overflow")?;
    let length = file
        .metadata()
        .map_err(|e| format!("PV snapshot size: {e}"))?
        .len();
    let tail = length.checked_sub(end).ok_or("PV snapshot truncated RAM")?;
    match &inputs.disk {
        None if tail != 0 => return Err("PV snapshot trailing bytes".into()),
        Some(base)
            if u128::from(tail)
                > seedbox_machine::machine::disk::codec::encoded_limit(base.capacity()) =>
        {
            return Err("PV snapshot disk exceeds capacity".into())
        }
        _ => (),
    }
    file.seek(SeekFrom::Start(end))
        .map_err(|e| format!("PV snapshot disk position: {e}"))?;
    let count = usize::try_from(tail).map_err(|_| "PV snapshot disk length overflow")?;
    let mut overlays = Vec::new();
    overlays
        .try_reserve_exact(count)
        .map_err(|e| format!("PV snapshot disk allocation: {e}"))?;
    (&mut file)
        .take(tail)
        .read_to_end(&mut overlays)
        .map_err(|e| format!("PV snapshot disk read: {e}"))?;
    if overlays.len() != count {
        return Err("PV snapshot truncated disk".into());
    }
    if let Some(base) = &inputs.disk {
        seedbox_machine::machine::disk::codec::validate_overlays(&overlays, base.capacity())
            .map_err(|e| format!("PV snapshot disk: {e:?}"))?;
    }
    Ok(Saved {
        capture,
        file,
        data_start,
        overlays,
    })
}

/// Restore into an unpublished target, reading mapped segments directly into
/// native backing. Failure drops the complete temporary VM; it is not resumable.
// Reading segments straight from the file avoids a multi-GiB intermediate RAM
// copy and keeps resource/layout validation ahead of native allocation.
fn restore_machine(inputs: &BootInputs, mut saved: Saved) -> Result<Machine> {
    let block = match (saved.capture.block.take(), &inputs.disk) {
        (Some(transport), Some(base)) => {
            let mut store = base.clone();
            store
                .replace_overlays(&saved.overlays)
                .map_err(|e| format!("PV snapshot disk: {e:?}"))?;
            Some(block::Block::restore(transport, store))
        }
        (None, None) => None,
        _ => return Err("PV snapshot disk differs from boot disk".into()),
    };
    let mut vm = Machine::<Native>::boot_with_memory(
        &inputs.kernel,
        &inputs.initrd,
        inputs.seed,
        &inputs.command_line,
        inputs.disk.clone(),
        inputs.network,
        inputs.memory_bytes,
    )?;
    saved
        .file
        .seek(SeekFrom::Start(saved.data_start))
        .map_err(|e| format!("PV snapshot RAM position: {e}"))?;
    unsafe { vm.page.modify(|bytes| saved.file.read_exact(bytes)) }
        .map_err(|e| format!("PV snapshot page read: {e}"))?;
    let layout = vm.ram.layout()?.clone();
    for segment in &layout.segments {
        let count = (segment.end - segment.start) as usize;
        let offset = layout.offset(segment.start, count)?;
        unsafe {
            vm.ram
                .modify(|bytes| saved.file.read_exact(&mut bytes[offset..offset + count]))
        }
        .map_err(|e| format!("PV snapshot RAM read: {e}"))?;
    }
    vm.native.restore(&saved.capture.architecture)?;
    vm.state = saved.capture.state;
    vm.block = block;
    Ok(vm)
}
pub(crate) fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut machine: Option<Machine> = None;
    while let Some(request) = protocol::receive(&mut input)? {
        // A Run's frames follow it on the pipe; read them before running so
        // a failed run cannot desynchronize the protocol.
        let frames = match &request {
            Request::Run { frames, .. } => super::wire::receive_frames(&mut input, *frames)?,
            _ => Vec::new(),
        };
        let mut writes = None;
        let result: Result<Response> = (|| match request {
            Request::Boot {
                witness,
                memory_bytes,
                layout_version,
                restore,
                kernel,
                initrd,
                command_line,
                seed,
                now,
                disk,
                network,
                policy,
            } => {
                let policy = *policy;
                if machine.is_some() {
                    return Err("PV repeated boot".into());
                }
                if layout_version != boot::LAYOUT_VERSION {
                    return Err("PV boot layout version".into());
                }
                boot::RamLayout::native(memory_bytes)?;
                let kernel = std::fs::read(kernel).map_err(|e| format!("PV kernel read: {e}"))?;
                let initrd = std::fs::read(initrd).map_err(|e| format!("PV initrd read: {e}"))?;
                let (store, base) = disk.as_ref().map(open_disk).transpose()?.unzip();
                let inputs = BootInputs {
                    witness: (*witness).clone(),
                    memory_bytes,
                    kernel,
                    initrd,
                    seed,
                    command_line,
                    disk: base,
                    network,
                    policy: policy.clone(),
                };
                let vm = if let Some(path) = restore {
                    let saved = read_snapshot(&path, &inputs)?;
                    restore_machine(&inputs, saved)?
                } else {
                    let mut vm = Machine::<Native>::boot_with_memory(
                        &inputs.kernel,
                        &inputs.initrd,
                        seed,
                        &inputs.command_line,
                        store,
                        network,
                        memory_bytes,
                    )?;
                    vm.state.witness = witness.initial;
                    vm.state.witness_origin = witness.initial;
                    vm.state.witness_boot = witness.boot;
                    vm.configure_policy(policy.as_ref())?;
                    vm.adopt(now)?;
                    let ordinal = witness.boot;
                    if matches!(witness.mark, Some(1 | 2)) {
                        vm.record_native(b'C', 0, 0, 0);
                    }
                    vm.record_native(b'N', 0, ordinal, 0);
                    vm.record_native(b'S', 0, ordinal, 0);
                    vm
                };
                let installed_witness = protocol::BootWitness {
                    boot: vm.state.witness_boot,
                    initial: vm.state.witness_origin,
                    mark: witness.mark,
                };
                let installed = vm.policy_setup().cloned();
                machine = Some(vm);
                Ok(Response::Ready {
                    witness: installed_witness,
                    memory_bytes,
                    layout_version: boot::LAYOUT_VERSION,
                    policy: installed,
                })
            }
            Request::Run {
                now,
                upto,
                syscall_cost_ns,
                shutdown,
                lifecycle,
                frames: _,
            } => {
                let vm = machine.as_mut().ok_or("PV run before boot")?;
                let stop = vm.run(now, upto, syscall_cost_ns, shutdown, &lifecycle, frames)?;
                vm.output.clear();
                let taken = std::mem::take(&mut vm.state.writes);
                let count = taken.len() as u64;
                writes = Some(taken);
                Ok(Response::Stopped {
                    time: vm.state.now,
                    stop,
                    writes: count,
                })
            }
            Request::ExecutionEvidence => Ok(Response::ExecutionEvidence(
                machine
                    .as_ref()
                    .ok_or("PV execution witness before boot")?
                    .native_execution()?,
            )),
            Request::Evidence { pages } => Ok(Response::Evidence(
                machine
                    .as_mut()
                    .ok_or("PV evidence before boot")?
                    .stopped_evidence(&pages)?,
            )),
            Request::Capture(path) => {
                machine
                    .as_mut()
                    .ok_or("PV capture before boot")?
                    .capture(&path)?;
                Ok(Response::Captured)
            }
            Request::ExportDisk(path) => {
                machine
                    .as_ref()
                    .ok_or("PV disk export before boot")?
                    .export_disk(&path)?;
                Ok(Response::Exported)
            }
        })();
        let response = match result {
            Ok(answer) => answer,
            Err(error) => Response::Error(error),
        };
        protocol::send(&mut output, &response)?;
        if let Some(writes) = writes {
            super::wire::send_writes(&mut output, &writes)?;
        }
    }
    Ok(())
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for State {
    const MIN: u64 = <[u8; 32] as seedbox_machine::snapshot::Record>::MIN
        + <u64 as seedbox_machine::snapshot::Record>::MIN
        + 40
        + <Option<u64> as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <Devices as seedbox_machine::snapshot::Record>::MIN
        + <console::Console as seedbox_machine::snapshot::Record>::MIN
        + <Option<net::Net> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<seedbox_machine::pv::transport::GuestWrite> as seedbox_machine::snapshot::Record>::MIN
        + <Option<super::policy::Runtime> as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            witness,
            witness_boot,
            witness_origin,
            now,
            deadline,
            timer,
            ready,
            shutdown,
            idle,
            devices,
            console,
            net,
            writes,
            policy,
        } = self;
        witness.encode(w)?;
        witness_boot.encode(w)?;
        witness_origin.encode(w)?;
        now.encode(w)?;
        deadline.encode(w)?;
        timer.encode(w)?;
        ready.encode(w)?;
        shutdown.encode(w)?;
        idle.encode(w)?;
        devices.encode(w)?;
        console.encode(w)?;
        net.encode(w)?;
        writes.encode(w)?;
        policy.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            witness: seedbox_machine::snapshot::Record::decode(r)?,
            witness_boot: seedbox_machine::snapshot::Record::decode(r)?,
            witness_origin: seedbox_machine::snapshot::Record::decode(r)?,
            now: seedbox_machine::snapshot::Record::decode(r)?,
            deadline: seedbox_machine::snapshot::Record::decode(r)?,
            timer: seedbox_machine::snapshot::Record::decode(r)?,
            ready: seedbox_machine::snapshot::Record::decode(r)?,
            shutdown: seedbox_machine::snapshot::Record::decode(r)?,
            idle: seedbox_machine::snapshot::Record::decode(r)?,
            devices: seedbox_machine::snapshot::Record::decode(r)?,
            console: seedbox_machine::snapshot::Record::decode(r)?,
            net: seedbox_machine::snapshot::Record::decode(r)?,
            writes: seedbox_machine::snapshot::Record::decode(r)?,
            policy: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Capture {
    const MIN: u64 = <u64 as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <u64 as seedbox_machine::snapshot::Record>::MIN
        + <Vec<(u64, u64)> as seedbox_machine::snapshot::Record>::MIN
        + <State as seedbox_machine::snapshot::Record>::MIN
        + <Architecture as seedbox_machine::snapshot::Record>::MIN
        + <Option<block::Transport> as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            memory_bytes,
            layout_version,
            page_bytes,
            segments,
            state,
            architecture,
            block,
        } = self;
        memory_bytes.encode(w)?;
        layout_version.encode(w)?;
        page_bytes.encode(w)?;
        segments.encode(w)?;
        state.encode(w)?;
        architecture.encode(w)?;
        block.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            memory_bytes: seedbox_machine::snapshot::Record::decode(r)?,
            layout_version: seedbox_machine::snapshot::Record::decode(r)?,
            page_bytes: seedbox_machine::snapshot::Record::decode(r)?,
            segments: seedbox_machine::snapshot::Record::decode(r)?,
            state: seedbox_machine::snapshot::Record::decode(r)?,
            architecture: seedbox_machine::snapshot::Record::decode(r)?,
            block: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

impl Capture {
    fn validate_snapshot(
        &self,
        memory_bytes: u64,
        disk: bool,
        network: Option<[u8; 6]>,
        reached: Option<u64>,
    ) -> Result<boot::RamLayout> {
        let layout = boot::RamLayout::native(memory_bytes)?;
        if self.memory_bytes != memory_bytes
            || self.layout_version != boot::LAYOUT_VERSION
            || self.page_bytes != PAGE_BYTES as u64
            || self.segments.len() != layout.segments.len()
            || self
                .segments
                .iter()
                .zip(&layout.segments)
                .any(|(&(start, len), s)| start != s.start || len != s.end - s.start)
        {
            return Err("PV snapshot RAM layout differs from boot".into());
        }
        if self.block.is_some() != disk {
            return Err("PV snapshot disk differs from boot disk".into());
        }
        if self.state.net.as_ref().map(net::Net::mac) != network {
            return Err("PV snapshot NIC differs from boot NIC".into());
        }
        if reached.is_some_and(|t| self.state.now != t) || !self.state.writes.is_empty() {
            return Err("PV snapshot native time or pending output".into());
        }
        if let Some(p) = &self.state.policy {
            p.validate()?;
        }
        self.architecture.validate_snapshot()?;
        self.state.devices.validate_snapshot()?;
        self.state.console.validate_snapshot()?;
        if let Some(net) = &self.state.net {
            net.validate_snapshot()?;
        }
        if let Some(block) = &self.block {
            block.validate_snapshot()?;
        }
        Ok(layout)
    }
}
