//! Private stopped native machine. The calling Driver alone advances shared
//! time. Device service and timer delivery happen at deterministic guest exits.
mod backend;
mod console;
mod memory;
mod tree;
use super::{
    boot,
    protocol::{self, Request, Response, Stop},
};
use backend::{Architecture, Backend, Exit, Native};
use memory::Memory;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::Path,
    rc::Rc,
};
type Result<T> = std::result::Result<T, String>;
const TIMER: u32 = 40;
const UART: u32 = 33;
const CONSOLE: u32 = 42;
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
const PSTATE: u32 = 33;
const PSTATE_I: u64 = 1 << 7;
const CNTV_CTL: u16 = 0xdf19;
const CNTKCTL: u16 = 0xc708;
const TPIDRRO: u16 = 0xde83;
const UART_MMIO: u64 = 0x0900_0000;
const UART_END: u64 = UART_MMIO + 0xfff;
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
    deferred: BTreeSet<u32>,
    asserted: BTreeSet<u32>,
    uart: [u32; 16],
    console: console::Console,
    writes: Vec<crate::pv::transport::GuestWrite>,
}
#[derive(Serialize, Deserialize)]
struct Capture {
    state: State,
    architecture: Architecture,
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
        let (entry, initrd_address) = boot::layout(kernel, initrd)?;
        let mut ram = Memory::new(boot::RAM_BYTES)?;
        let page = Rc::new(Memory::new(PAGE_BYTES)?);
        ram.write((entry - boot::RAM_BASE) as usize, kernel)?;
        ram.write((initrd_address - boot::RAM_BASE) as usize, initrd)?;
        ram.write(
            (boot::DTB - boot::RAM_BASE) as usize,
            &tree::build(initrd_address, initrd.len(), seed, command_line)?,
        )?;
        page.set_word(BOUND, u64::MAX);
        page.set_word(VERSION, ABI);
        page.set_word(SYSCALL_COST, 1000);
        let ram = Rc::new(ram);
        let mut native = B::create(ram.clone(), page.clone())?;
        // No EL2, PMU or host topology belongs in the guest profile.
        for (register, mask, value) in [
            (0xc038, 0xf << 56, None),
            (0xc028, 0xf << 8, None),
            (0xc005, u64::MAX, Some(0x8000_0000)),
        ] {
            let want = value.unwrap_or(native.system(register)? & !mask);
            if native.system(register)? != want {
                native.set_system(register, want)?;
            }
            if native.system(register)? != want {
                return Err(format!("PV CPU feature {register:#x} readback"));
            }
        }
        for register in 0..32 {
            native.set_core(register, 0)?;
        }
        native.set_core(0, boot::DTB)?;
        native.set_core(32, entry)?;
        native.set_core(PSTATE, 0x3c5)?;
        for (register, value) in [(0xc080, 0x30d0_0800), (0xc708, 0), (0xdf19, 0)] {
            native.set_system(register, value)?;
        }
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
            .set_word(IRQ_DEFERRED, (!self.state.deferred.is_empty()) as u64);
    }
    fn epoch(&mut self) -> Result<()> {
        let epoch = self
            .page
            .word(EXCLUSIVE_EPOCH)
            .checked_add(1)
            .ok_or("PV exclusive epoch overflow")?;
        self.page.set_word(EXCLUSIVE_EPOCH, epoch);
        if self.native.core(PSTATE)? & 15 == 0 {
            self.native.set_system(TPIDRRO, epoch)?;
        }
        Ok(())
    }
    fn irq(&mut self, number: u32, level: bool) -> Result<()> {
        if !level {
            self.state.deferred.remove(&number);
            self.state.asserted.remove(&number);
            return self.native.irq(number, false);
        }
        if self.state.asserted.contains(&number) {
            return Ok(());
        }
        // Admit one unmasked IRQ source at a rendezvous; a second host-pending
        // source must not skid into a later guest instruction based on host timing.
        if !self.state.asserted.is_empty() || self.native.core(PSTATE)? & PSTATE_I != 0 {
            self.state.deferred.insert(number);
            return Ok(());
        }
        self.state.deferred.remove(&number);
        self.native.irq(number, true)?;
        self.state.asserted.insert(number);
        Ok(())
    }
    fn interrupts(&mut self) -> Result<()> {
        if self.state.deadline.is_some_and(|d| d <= self.state.now) {
            self.state.deadline = None;
            self.state.timer = true;
            self.irq(TIMER, true)?;
        }
        self.irq(
            UART,
            self.state.uart[0x38 / 4] & 0x20 != 0 && self.state.uart[0x30 / 4] & 0x101 == 0x101,
        )?;
        self.irq(CONSOLE, self.state.console.irq())?;
        if self.state.asserted.is_empty() && self.native.core(PSTATE)? & PSTATE_I == 0 {
            if let Some(number) = self.state.deferred.pop_first() {
                self.native.irq(number, true)?;
                self.state.asserted.insert(number);
            }
        }
        Ok(())
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
    fn uart(&mut self, offset: u64, width: u8, write: bool, value: u64) -> Result<Option<u64>> {
        if !matches!(width, 1 | 2 | 4) || !offset.is_multiple_of(4) {
            return Err("PV PL011 access width/alignment".into());
        }
        if write {
            match offset {
                0 => {
                    self.output.push(value as u8);
                    self.state
                        .writes
                        .push(crate::pv::transport::GuestWrite::Console(
                            crate::pv::transport::LifecycleWrite {
                                time: self.state.now,
                                bytes: vec![value as u8],
                            },
                        ));
                }
                4 | 0x44 | 0x48 => (),
                0x24..=0x38 => self.state.uart[offset as usize / 4] = value as u32,
                _ => return Err(format!("PV PL011 write {offset:#x}")),
            }
            return Ok(None);
        }
        Ok(Some(match offset {
            0 | 4 | 0x44 | 0x48 => 0,
            0x18 => 0x90,
            0x24..=0x38 => self.state.uart[offset as usize / 4] as u64,
            0x3c => 0x20,
            0x40 => {
                if self.state.uart[14] & 0x20 != 0 && self.state.uart[12] & 0x101 == 0x101 {
                    0x20
                } else {
                    0
                }
            }
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xfec => 0,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 5,
            0xffc => 0xb1,
            _ => return Err(format!("PV PL011 read {offset:#x}")),
        }))
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
            self.epoch()?;
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
            self.epoch()?;
            self.state.console.shutdown();
            unsafe {
                self.ram
                    .modify(|ram| self.state.console.process(ram, self.state.now))
            }?;
            self.interrupts()?;
        }
        self.collect_lifecycle();
        self.publish(upto);
        if self.state.idle
            && !self.state.timer
            && self.state.asserted.is_empty()
            && self.state.deferred.is_empty()
        {
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
            if self.native.system(CNTV_CTL)? & 1 != 0
                || self.state.ready && self.native.system(CNTKCTL)? != 0
            {
                return Err("PV guest enabled host counter/timer".into());
            }
            if matches!(
                exit,
                Exit::System {
                    register: 0x809c | 0x8084,
                    write: true,
                    value: 0
                }
            ) {
                self.native.complete(None)?;
                continue;
            }
            // Only a real semantic guest rendezvous invalidates reservations.
            // Maintenance exits above and host interruptions do not.
            self.epoch()?;
            let last = self.page.word(LAST);
            if last == u64::MAX || last > upto {
                return Err("PV guest crossed accepted horizon".into());
            }
            self.state.now = self.state.now.max(last);
            let mut response = None;
            let mut idle = false;
            let mut off = false;
            match exit {
                Exit::Mmio {
                    address,
                    width,
                    write,
                    value,
                } => match address {
                    UART_MMIO..=UART_END => {
                        response = self.uart(address - UART_MMIO, width, write, value)?
                    }
                    EVENT_MMIO..=EVENT_END => {
                        if width != 8 || !write {
                            return Err("PV clockevent access".into());
                        }
                        match address - EVENT_MMIO {
                            0 => self.state.deadline = if value == 0 { None } else { Some(value) },
                            8 => {
                                self.state.timer = false;
                                self.irq(TIMER, false)?;
                            }
                            16 => {
                                if !self.state.ready {
                                    return Err("PV idle before handshake".into());
                                }
                                idle = true;
                            }
                            24 => {
                                if self.state.deferred.is_empty() {
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
                            _ => return Err("PV unknown clockevent register".into()),
                        }
                    }
                    LIFECYCLE_MMIO..=LIFECYCLE_END => {
                        if width != 4 {
                            return Err("virtio MMIO requires 32 bits".into());
                        }
                        let offset = address - LIFECYCLE_MMIO;
                        if write {
                            unsafe {
                                self.ram.modify(|ram| {
                                    self.state.console.write(
                                        ram,
                                        offset,
                                        value as u32,
                                        self.state.now,
                                    )
                                })
                            }?;
                            self.collect_lifecycle();
                        } else {
                            response = Some(self.state.console.read(offset)? as u64);
                        }
                    }
                    _ => return Err(format!("PV unmapped MMIO {address:#x}")),
                },
                Exit::Wait { event } => {
                    if !event {
                        if !self.state.ready {
                            return Err("PV WFI before handshake".into());
                        }
                        idle = true;
                    }
                }
                Exit::Call => {
                    let function = self.native.core(0)?;
                    let value = match function {
                        0x8400_0000 | 0x8400_0006 => 2,
                        0x8400_0008 => {
                            off = true;
                            0
                        }
                        _ => u64::MAX,
                    };
                    self.native.set_core(0, value)?;
                    for register in 1..4 {
                        self.native.set_core(register, 0)?;
                    }
                }
                Exit::System { register, .. } => {
                    return Err(format!("PV unsupported system register {register:#x}"));
                }
                Exit::Interrupted => unreachable!(),
            }
            self.interrupts()?;
            self.native.complete(response)?;
            self.collect_lifecycle();
            self.publish(upto);
            if off {
                if !self.state.ready {
                    return Err("PV poweroff before handshake".into());
                }
                return Ok(Stop::PoweredOff);
            }
            if self.state.now == upto {
                return Ok(Stop::Horizon);
            }
            if idle
                && !self.state.timer
                && self.state.asserted.is_empty()
                && self.state.deferred.is_empty()
            {
                self.state.idle = true;
                return Ok(Stop::Blocked(self.state.deadline));
            }
        }
    }
    fn capture(&self, path: &Path) -> Result<()> {
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
                    .as_ref()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    struct Scripted {
        core: [u64; 34],
        system: BTreeMap<u16, u64>,
        exits: VecDeque<Exit>,
        entries: usize,
        irqs: Vec<(u32, bool)>,
    }
    impl Backend for Scripted {
        fn create(_: Rc<Memory>, _: Rc<Memory>) -> Result<Self> {
            Ok(Self {
                core: [0; 34],
                system: BTreeMap::new(),
                exits: VecDeque::new(),
                entries: 0,
                irqs: Vec::new(),
            })
        }
        fn core(&self, r: u32) -> Result<u64> {
            Ok(self.core[r as usize])
        }
        fn set_core(&mut self, r: u32, v: u64) -> Result<()> {
            self.core[r as usize] = v;
            Ok(())
        }
        fn system(&self, r: u16) -> Result<u64> {
            Ok(*self.system.get(&r).unwrap_or(&0))
        }
        fn set_system(&mut self, r: u16, v: u64) -> Result<()> {
            self.system.insert(r, v);
            Ok(())
        }
        fn enter(&mut self) -> Result<Exit> {
            self.entries += 1;
            self.exits
                .pop_front()
                .ok_or("unexpected scripted entry".into())
        }
        fn complete(&mut self, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        fn irq(&mut self, n: u32, v: bool) -> Result<()> {
            self.irqs.push((n, v));
            Ok(())
        }
        fn capture(&self) -> Result<Architecture> {
            Err("script capture unused".into())
        }
        fn restore(&mut self, _: &Architecture) -> Result<()> {
            Err("script restore unused".into())
        }
    }
    fn machine() -> Machine<Scripted> {
        let mut image = vec![0; 64];
        image[16..24].copy_from_slice(&4096u64.to_le_bytes());
        image[56..60].copy_from_slice(b"ARM\x64");
        Machine::boot(&image, b"initrd", 7, "").unwrap()
    }
    fn ready() -> Exit {
        Exit::Mmio {
            address: 0x0a01_0020,
            width: 8,
            write: true,
            value: 3,
        }
    }
    fn idle() -> Exit {
        Exit::Mmio {
            address: 0x0a01_0010,
            width: 8,
            write: true,
            value: 0,
        }
    }
    #[test]
    fn pending_cross_channel_writes_capture_ties_and_drain_once() {
        use crate::pv::transport::{GuestWrite, LifecycleWrite};
        let mut vm = machine();
        vm.state.now = 3;
        vm.uart(0, 4, true, b'A' as u64).unwrap();
        vm.state.console.output.push(3, b"SBX_READY1\n".to_vec());
        vm.collect_lifecycle();
        vm.uart(0, 4, true, b'B' as u64).unwrap();
        vm.state.now = 10;
        let image = serde_json::to_vec(&vm.state).unwrap();
        let mut restored: State = serde_json::from_slice(&image).unwrap();
        assert_eq!(
            std::mem::take(&mut restored.writes),
            [
                GuestWrite::Console(LifecycleWrite {
                    time: 3,
                    bytes: b"A".to_vec()
                }),
                GuestWrite::Lifecycle(LifecycleWrite {
                    time: 3,
                    bytes: b"SBX_READY1\n".to_vec()
                }),
                GuestWrite::Console(LifecycleWrite {
                    time: 3,
                    bytes: b"B".to_vec()
                }),
            ]
        );
        assert!(restored.writes.is_empty());
        assert!(restored.console.output.drain().is_empty());
        assert_eq!(vm.output, b"AB");
    }

    #[test]
    fn host_interruptions_neither_consume_time_nor_invalidate_exclusives() {
        let mut vm = machine();
        vm.native
            .exits
            .extend([Exit::Interrupted, Exit::Interrupted, ready(), idle()]);
        assert!(matches!(
            vm.run(0, 10, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.state.now, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
        assert_eq!(vm.native.entries, 4);
        let entries = vm.native.entries;
        assert!(matches!(
            vm.run(25, 25, 1000, false, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 2);
        assert_eq!(vm.native.entries, entries);
        assert_eq!(vm.page.word(NOW), 25);
    }
    #[test]
    fn zero_budget_adoption_coalesces_due_timer_without_guest_entry() {
        let mut vm = machine();
        vm.state.ready = true;
        vm.state.idle = true;
        vm.state.deadline = Some(5);
        assert!(matches!(
            vm.run(20, 20, 1000, false, &[]).unwrap(),
            Stop::Horizon
        ));
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 0);
        assert_eq!(vm.state.now, 20);
        assert_eq!(vm.state.deadline, None);
        assert!(vm.state.timer);
        assert!(vm.state.deferred.contains(&TIMER));
        assert_eq!(vm.page.word(IRQ_DEFERRED), 1);
    }
    #[test]
    fn early_shutdown_buffers_once_and_does_not_invent_wakeup() {
        let mut vm = machine();
        vm.state.ready = true;
        vm.state.idle = true;
        assert!(matches!(
            vm.run(10, 10, 1000, true, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        let state = serde_json::to_vec(&vm.state).unwrap();
        let epoch = vm.page.word(EXCLUSIVE_EPOCH);
        assert!(matches!(
            vm.run(10, 11, 1000, true, &[]).unwrap(),
            Stop::Blocked(None)
        ));
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), epoch);
        assert_eq!(serde_json::to_vec(&vm.state).unwrap(), state);
    }
    #[test]
    fn zero_budget_republishes_next_price_without_guest_entry_or_progress() {
        let mut vm = machine();
        vm.page.set_word(SYSCALL_COST, 4000);
        assert!(matches!(
            vm.run(0, 0, 1001, false, &[]).unwrap(),
            Stop::Horizon
        ));
        assert_eq!(vm.page.word(SYSCALL_COST), 1001);
        assert_eq!(vm.page.word(LAST), 0);
        assert_eq!(vm.page.word(EXCLUSIVE_EPOCH), 0);
        assert_eq!(vm.native.entries, 0);
        assert_eq!(vm.state.now, 0);
        vm.state.idle = true;
        vm.state.deadline = Some(500);
        assert!(matches!(
            vm.run(100, 100, 1_000_000, false, &[]).unwrap(),
            Stop::Blocked(Some(500))
        ));
        assert_eq!(vm.page.word(SYSCALL_COST), 1_000_000);
        assert_eq!(vm.state.deadline, Some(500));
        assert_eq!(vm.state.now, 100);
        assert_eq!(vm.page.word(LAST), 100);
        assert_eq!(vm.native.entries, 0);
    }
}
