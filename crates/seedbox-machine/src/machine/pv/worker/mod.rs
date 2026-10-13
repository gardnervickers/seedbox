//! Private stopped native machine. The calling Driver alone advances shared
//! time. Device service and timer delivery happen at deterministic guest exits.
//!
//! This module owns the architecture-neutral PV time page, event registers,
//! lifecycle console and snapshot files. The private `arch` module supplies
//! boot, interrupt controller, platform devices and the native backend.
#[cfg(target_arch = "aarch64")]
#[path = "aarch64.rs"]
mod arch;
mod console;
mod memory;
use super::{
    boot,
    protocol::{self, Request, Response, Stop},
};
use arch::{Architecture, Backend, Devices, Exit, Native};
use memory::Memory;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    rc::Rc,
};
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
const EXCLUSIVE_EPOCH: usize = 40;
const SYSCALL_COST: usize = 48;
const EVENT_MMIO: u64 = 0x0a01_0000;
const EVENT_END: u64 = EVENT_MMIO + PAGE_BYTES as u64 - 1;
const LIFECYCLE_MMIO: u64 = 0x0b01_0000;
const LIFECYCLE_END: u64 = LIFECYCLE_MMIO + PAGE_BYTES as u64 - 1;
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    now: u64,
    deadline: Option<u64>,
    timer: bool,
    ready: bool,
    shutdown: bool,
    idle: bool,
    devices: Devices,
    console: console::Console,
    writes: Vec<crate::pv::transport::GuestWrite>,
}
#[derive(Serialize, Deserialize)]
struct Capture {
    state: State,
    architecture: Architecture,
}
/// Result of one architecture-specific exit handled at a stopped rendezvous.
#[derive(Default)]
struct Effect {
    response: Option<u64>,
    idle: bool,
    off: bool,
}
struct Machine<B: Backend = Native> {
    native: B,
    ram: Rc<Memory>,
    page: Rc<Memory>,
    state: State,
    output: Vec<u8>,
}
impl<B: Backend> Machine<B> {
    fn boot(kernel: &[u8], initrd: &[u8], seed: u64, command_line: &str) -> Result<Self> {
        boot::arguments(command_line)?;
        let mut ram = Memory::new(boot::RAM_BYTES)?;
        let page = Rc::new(Memory::new(PAGE_BYTES)?);
        let entry = arch::load(&mut ram, kernel, initrd, seed, command_line)?;
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
            state: State::default(),
            output: Vec::new(),
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
                .map(crate::pv::transport::GuestWrite::Lifecycle),
        );
    }
    /// Record one guest console byte at the current stopped time.
    fn console_byte(&mut self, byte: u8) {
        self.output.push(byte);
        self.state
            .writes
            .push(crate::pv::transport::GuestWrite::Console(
                crate::pv::transport::LifecycleWrite {
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
    ) -> Result<Stop> {
        if now > upto || upto == u64::MAX || syscall_cost_ns == 0 {
            return Err("PV invalid run interval".into());
        }
        // Guest syscall progress samples this word once into its remaining
        // charge. Existing partial work keeps its old price in captured CPU/RAM.
        // Republish even after restore and on zero-budget adoption; the caller,
        // not the snapshot's cached publication, owns future execution pricing.
        self.page.set_word(SYSCALL_COST, syscall_cost_ns);
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
                        0 => self.state.deadline = if value == 0 { None } else { Some(value) },
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
                        offset => effect.off = self.event_register(offset, value)?,
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
                exit => effect = self.platform(exit)?,
            }
            self.interrupts()?;
            self.native.complete(effect.response)?;
            self.collect_lifecycle();
            self.publish(upto);
            if effect.off {
                if !self.state.ready {
                    return Err("PV poweroff before handshake".into());
                }
                return Ok(Stop::PoweredOff);
            }
            if self.state.now == upto {
                return Ok(Stop::Horizon);
            }
            if effect.idle && self.quiet() {
                self.state.idle = true;
                return Ok(Stop::Blocked(self.state.deadline));
            }
        }
    }
    fn capture(&mut self, path: &Path) -> Result<()> {
        let state = Capture {
            state: self.state.clone(),
            architecture: self.native.capture()?,
        };
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("PV snapshot create: {e}"))?;
        protocol::send(&mut file, &state)?;
        unsafe {
            file.write_all(self.page.bytes())
                .and_then(|_| file.write_all(self.ram.bytes()))
        }
        .map_err(|e| format!("PV snapshot RAM: {e}"))
    }
}
fn read_snapshot(path: &Path) -> Result<(Capture, Vec<u8>, Vec<u8>)> {
    let mut file = File::open(path).map_err(|e| format!("PV snapshot open: {e}"))?;
    let capture = protocol::receive(&mut file)?.ok_or("PV snapshot header absent")?;
    let mut page = vec![0; PAGE_BYTES];
    let mut ram = vec![0; boot::RAM_BYTES];
    file.read_exact(&mut page)
        .and_then(|_| file.read_exact(&mut ram))
        .map_err(|e| format!("PV snapshot RAM read: {e}"))?;
    if file
        .read(&mut [0])
        .map_err(|e| format!("PV snapshot trailing read: {e}"))?
        != 0
    {
        return Err("PV snapshot trailing bytes".into());
    }
    Ok((capture, page, ram))
}
pub(crate) fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut machine: Option<Machine> = None;
    let mut boot_inputs: Option<(Vec<u8>, Vec<u8>, u64, String)> = None;
    while let Some(request) = protocol::receive(&mut input)? {
        let result: Result<Response> = (|| match request {
            Request::Boot {
                kernel,
                initrd,
                command_line,
                seed,
                now,
            } => {
                if machine.is_some() {
                    return Err("PV repeated boot".into());
                }
                let kernel = std::fs::read(kernel).map_err(|e| format!("PV kernel read: {e}"))?;
                let initrd = std::fs::read(initrd).map_err(|e| format!("PV initrd read: {e}"))?;
                let mut vm = Machine::<Native>::boot(&kernel, &initrd, seed, &command_line)?;
                vm.adopt(now)?;
                boot_inputs = Some((kernel, initrd, seed, command_line));
                machine = Some(vm);
                Ok(Response::Ready)
            }
            Request::Run {
                now,
                upto,
                syscall_cost_ns,
                shutdown,
                lifecycle,
            } => {
                let vm = machine.as_mut().ok_or("PV run before boot")?;
                let stop = vm.run(now, upto, syscall_cost_ns, shutdown, &lifecycle)?;
                vm.output.clear();
                Ok(Response::Stopped {
                    time: vm.state.now,
                    stop,
                    writes: std::mem::take(&mut vm.state.writes),
                })
            }
            Request::Capture(path) => {
                machine
                    .as_mut()
                    .ok_or("PV capture before boot")?
                    .capture(&path)?;
                Ok(Response::Captured)
            }
            Request::Restore(path) => {
                let (capture, page, ram) = read_snapshot(&path)?;
                let (kernel, initrd, seed, command_line) =
                    boot_inputs.as_ref().ok_or("PV restore before boot")?;
                drop(machine.take());
                let mut vm = Machine::<Native>::boot(kernel, initrd, *seed, command_line)?;
                unsafe {
                    vm.ram.modify(|out| out.copy_from_slice(&ram));
                    vm.page.modify(|out| out.copy_from_slice(&page));
                }
                vm.native.restore(&capture.architecture)?;
                vm.state = capture.state;
                machine = Some(vm);
                Ok(Response::Ready)
            }
        })();
        let response = match result {
            Ok(answer) => answer,
            Err(error) => Response::Error(error),
        };
        protocol::send(&mut output, &response)?;
    }
    Ok(())
}
