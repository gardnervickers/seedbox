use crate::{
    backend::{Backend, CoreReg, Exit, SysReg},
    devices::{pl011::Pl011, ClockEvent, Effect, MmioDevice, TimePage},
    fdt,
    memory::Memory,
    pv::PvClock,
    timebase::Timebase,
    witness::{self, Record, Report},
    *,
};
use std::{collections::BTreeSet, rc::Rc, time::Instant};

/// A completed semantic stop, with the vCPU left stopped for the world owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MachineStop {
    Horizon { time_ns: u64 },
    PoweredOff,
    Device { time_ns: u64 },
    ApplicationExit { code: u8, setup_error: bool },
    Barrier,
}

/// Memory/device ownership and semantic decisions live here, never in a backend.
/// Backend drops first and also retains its own references to every mapping.
pub struct Machine<B: Backend> {
    backend: B,
    ram: Rc<Memory>,
    page: Rc<Memory>,
    uart: Pl011,
    uart_irq: bool,
    deferred_spis: BTreeSet<u32>,
    // Keep only one source asserted across guest entry; others wait for a rendezvous.
    asserted_spis: BTreeSet<u32>,
    deferred_spi_events: u64,
    deferred_clock_read_spis: u64,
    event: ClockEvent,
    clock: PvClock,
    ready: bool,
    trace: Vec<Record>,
    seed: u64,
    kernel_hash: String,
    initrd_hash: String,
    raw_exits: u64,
    powered_off: bool,
    application: Option<application::Devices>,
    boot_identity: application::BootIdentity,
    timer_policy: Option<(application::TimerPolicy, crate::rng::Rng)>,
}
impl<B: Backend> Machine<B> {
    pub fn new(kernel: &[u8], initrd: &[u8], seed: u64) -> Result<Self> {
        Self::from_spec(boot::BootSpec::fixture(kernel, initrd, seed))
    }

    /// Construct a machine from checked boot inputs. Placement and generated
    /// FDT size are validated before allocating RAM or creating a backend VM.
    pub fn from_spec(spec: boot::BootSpec<'_>) -> Result<Self> {
        Self::construct(spec, None)
    }

    /// Construct the Compose personality with network, management and an
    /// optional disk. The fixture constructor never exposes these devices.
    pub fn for_application(
        spec: boot::BootSpec<'_>,
        mac: [u8; 6],
        disk: Option<crate::disk::Disk>,
    ) -> Result<Self> {
        Self::construct(spec, Some(application::Devices::new(mac, disk)))
    }

    fn construct(
        spec: boot::BootSpec<'_>,
        application: Option<application::Devices>,
    ) -> Result<Self> {
        let layout = spec.validate()?;
        let boot_identity = application::BootIdentity::from_layout(&layout);
        let dtb = match &application {
            Some(devices) => fdt::build_for_application(&layout, devices.block.is_some())?,
            None => fdt::build_for_layout(&layout)?,
        };
        let entry = layout.kernel_range().start;
        let boot::BootSpec {
            kernel,
            initrd,
            seed,
            ..
        } = spec;
        let ram = Rc::new(Memory::new(layout.memory_bytes())?);
        let page = Rc::new(Memory::new(PAGE_SIZE)?);
        // No VM exists yet, so none of these copies race with a vCPU.
        unsafe {
            ram.write(layout.ram_offset(entry, kernel.len())?, kernel)?;
            ram.write(
                layout.ram_offset(layout.initrd_range().start, initrd.len())?,
                initrd,
            )?;
            ram.write(
                layout.ram_offset(layout.dtb_range().start, dtb.len())?,
                &dtb,
            )?;
            page.set_word(16, u64::MAX)?;
            page.set_word(24, ABI)?;
        }
        let mut backend = B::create()?;
        backend.map_memory(0, RAM_BASE, Rc::clone(&ram))?;
        backend.map_memory(1, TIME_ADDR, Rc::clone(&page))?;
        backend.create_vcpu()?;
        configure_cpu_features(&mut backend)?;
        for n in 0..31 {
            backend.set_reg(CoreReg::X(n), 0)?;
        }
        backend.set_reg(CoreReg::Sp, 0)?;
        backend.set_reg(CoreReg::X(0), layout.dtb_range().start)?;
        backend.set_reg(CoreReg::Pc, entry)?;
        backend.set_reg(CoreReg::Pstate, 0x3c5)?; // EL1h, DAIF masked
        backend.set_sys_reg(SysReg::SCTLR, 0x30d0_0800)?; // architectural RES1, MMU/caches off
        backend.set_sys_reg(SysReg::CNTKCTL, 0)?;
        backend.set_sys_reg(SysReg::CNTV_CTL, 0)?;
        Ok(Self {
            backend,
            ram,
            page,
            uart: Pl011::default(),
            uart_irq: false,
            deferred_spis: BTreeSet::new(),
            asserted_spis: BTreeSet::new(),
            deferred_spi_events: 0,
            deferred_clock_read_spis: 0,
            event: ClockEvent,
            clock: PvClock::default(),
            ready: false,
            trace: vec![],
            seed,
            kernel_hash: witness::hash(kernel),
            initrd_hash: witness::hash(initrd),
            raw_exits: 0,
            powered_off: false,
            application,
            boot_identity,
            timer_policy: None,
        })
    }
    /// `before_run` is a performance-only hook for acceptance jitter. It cannot
    /// access guest state. Host duration is report metadata, never witness input.
    pub fn run(self, before_run: impl FnMut()) -> Result<Report> {
        self.run_with_diagnostics(before_run, None)
    }
    /// Optionally write page hashes after successful power-off, with the vCPU
    /// stopped and the final PV publication complete. Never part of the witness.
    pub fn run_with_diagnostics(
        mut self,
        before_run: impl FnMut(),
        ram_pages: Option<&mut dyn std::io::Write>,
    ) -> Result<Report> {
        let start = Instant::now();
        let report = self.run_inner(before_run).map_err(|error| {
            format!(
                "{error}\nexits: {}\nwall_seconds: {:.6}\ndeferred_spi_events: {}\ndeferred_clock_read_spis: {}\nlast exit: {:?}\nconsole:\n{}",
                self.trace.len(),
                start.elapsed().as_secs_f64(),
                self.deferred_spi_events,
                self.deferred_clock_read_spis,
                self.trace.last(),
                String::from_utf8_lossy(&self.uart.console)
            )
        })?;
        if let Some(output) = ram_pages {
            // run_inner returned at SYSTEM_OFF; no further vCPU entry occurs.
            unsafe {
                crate::diagnostics::write_ram_pages(
                    output,
                    &report,
                    self.ram.bytes(),
                    self.page.bytes(),
                )?;
            }
        }
        Ok(report)
    }
    /// Advance a stopped machine to a finite world boundary. The horizon is
    /// published before entry, including boot/handshake reconciliation. A timer
    /// beyond the boundary remains armed. The raw-exit budget spans all slices.
    pub fn run_until(&mut self, horizon: u64, before_run: impl FnMut()) -> Result<MachineStop> {
        if self.powered_off {
            return Ok(MachineStop::PoweredOff);
        }
        self.clock.set_horizon(Some(horizon))?;
        self.advance(before_run)
    }

    pub fn time_ns(&self) -> u64 {
        self.clock.now()
    }

    /// Resume after a world pause, or boot a new incarnation at a lifecycle
    /// boundary. Expired timers coalesce at this deterministic time.
    pub fn resume_at(&mut self, time_ns: u64) -> Result<()> {
        if time_ns < self.clock.now() {
            return Err("resume precedes machine clock".into());
        }
        self.clock.set_horizon(None)?;
        self.clock.read(time_ns)?;
        if self.clock.judge(false)? {
            self.inject_spi(TIMER_SPI, true)?;
        }
        self.publish()
    }

    pub fn boot_identity(&self) -> &application::BootIdentity {
        &self.boot_identity
    }

    pub fn console(&self) -> &[u8] {
        &self.uart.console
    }

    pub fn with_timer_policy(mut self, policy: application::TimerPolicy) -> Result<Self> {
        if self.application.is_none()
            || self.timer_policy.is_some()
            || self.raw_exits != 0
            || policy.jitter_ns > 3_600_000_000_000
            || policy.domain.is_empty()
        {
            return Err("invalid application timer policy".into());
        }
        let rng = crate::rng::Rng::new(policy.seed, &policy.domain);
        self.timer_policy = Some((policy, rng));
        Ok(self)
    }

    /// A stopped application boundary. The world records its finite horizons
    /// separately; this inventory covers every machine-owned continuation.
    pub fn application_evidence(&self, full_state: bool) -> Result<application::Evidence> {
        use sha2::{Digest, Sha256};
        use std::collections::BTreeMap;
        let devices = self
            .application
            .as_ref()
            .ok_or("fixture has no application evidence")?;
        let mut trace = Sha256::new();
        trace.update(b"seedbox-arm64-application-trace-v1\0");
        for record in &self.trace {
            trace.update(record.encode());
        }
        let trace_hash = format!("{:x}", trace.finalize());
        let console_hash = witness::hash(&self.uart.console);
        let timer_policy = self.timer_policy.as_ref().map(|(policy, _)| policy.clone());
        let disk_digest = devices
            .block
            .as_ref()
            .and_then(|device| device.disk())
            .map(crate::disk::Disk::digest);
        let identity = serde_json::to_vec(&(
            &self.boot_identity,
            &self.kernel_hash,
            &self.initrd_hash,
            &timer_policy,
        ))
        .map_err(|error| error.to_string())?;
        let parts = if full_state {
            let mut ram = Sha256::new();
            unsafe {
                ram.update((self.ram.bytes().len() as u64).to_le_bytes());
                ram.update(self.ram.bytes());
                ram.update(self.page.bytes());
            }
            let runtime = serde_json::to_vec(&(
                &self.clock,
                self.ready,
                self.powered_off,
                self.uart_irq,
                &self.deferred_spis,
                &self.asserted_spis,
                self.timer_policy
                    .as_ref()
                    .map(|(policy, rng)| (policy, rng.counter())),
            ))
            .map_err(|error| error.to_string())?;
            let cpu =
                serde_json::to_vec(&self.backend.state()?).map_err(|error| error.to_string())?;
            Some(BTreeMap::from([
                ("boot".into(), witness::hash(&identity)),
                ("ram".into(), format!("{:x}", ram.finalize())),
                ("cpu".into(), witness::hash(&cpu)),
                ("devices".into(), devices.state_digest()?),
                ("uart".into(), self.uart.state_digest()),
                ("clock".into(), witness::hash(&runtime)),
            ]))
        } else {
            None
        };
        let state_digest = parts
            .as_ref()
            .map(|parts| {
                serde_json::to_vec(parts)
                    .map(|bytes| witness::hash(&bytes))
                    .map_err(|e| e.to_string())
            })
            .transpose()?;
        let mut hash = Sha256::new();
        hash.update(b"seedbox-arm64-application-witness-v1\0");
        hash.update((identity.len() as u64).to_le_bytes());
        hash.update(identity);
        hash.update((self.trace.len() as u64).to_le_bytes());
        hash.update(trace_hash.as_bytes());
        hash.update((self.uart.console.len() as u64).to_le_bytes());
        hash.update(console_hash.as_bytes());
        hash.update(self.clock.now().to_le_bytes());
        if let Some(disk) = &disk_digest {
            hash.update(disk.as_bytes());
        }
        Ok(application::Evidence {
            format: 1,
            boot: self.boot_identity.clone(),
            kernel_sha256: self.kernel_hash.clone(),
            initrd_sha256: self.initrd_hash.clone(),
            timer_policy,
            disk_digest,
            control_channel: devices.console.control_channel(),
            time_ns: self.clock.now(),
            exits: self.trace.len() as u64,
            trace_hash,
            console_hash,
            witness: format!("{:x}", hash.finalize()),
            state_digest,
            state_digest_parts: parts,
        })
    }

    pub fn crashed_disk(
        &self,
        seed: u64,
        name: &str,
        incarnation: u32,
    ) -> Option<(crate::disk::Disk, u64)> {
        self.application
            .as_ref()?
            .block
            .as_ref()?
            .disk()
            .map(|disk| disk.crashed_seeded(seed, name, incarnation))
    }

    pub fn take_events(&mut self) -> Vec<application::Event> {
        self.application
            .as_mut()
            .map(|d| std::mem::take(&mut d.events))
            .unwrap_or_default()
    }

    /// Deliver only at a stopped world boundary. Acceptance and interrupt
    /// assertion are distinct, so bounded network backlogs cannot lose an ACK.
    pub fn receive_frame(&mut self, bytes: &[u8]) -> Result<bool> {
        let devices = self
            .application
            .as_mut()
            .ok_or("fixture has no network device")?;
        let accepted = unsafe {
            self.ram
                .with_bytes_mut(|ram| devices.net.receive(ram, bytes))
        }?;
        self.sync_application_irqs()?;
        self.publish()?;
        Ok(accepted)
    }

    pub fn receive_control(&mut self, bytes: &[u8]) -> Result<()> {
        let devices = self
            .application
            .as_mut()
            .ok_or("fixture has no control device")?;
        unsafe {
            self.ram
                .with_bytes_mut(|ram| devices.console.receive(ram, bytes))
        }?;
        self.sync_application_irqs()?;
        self.publish()
    }

    pub fn release_barrier(&mut self, words: [u32; 8]) -> Result<()> {
        let devices = self
            .application
            .as_mut()
            .ok_or("fixture has no launch barrier")?;
        if !devices.barrier || !devices.entropy.is_empty() {
            return Err("machine is not at a fresh launch barrier".into());
        }
        devices.entropy.extend(words);
        devices.barrier = false;
        Ok(())
    }

    fn run_inner(&mut self, before_run: impl FnMut()) -> Result<Report> {
        if self.application.is_some() {
            return Err("application machines require the versioned world report".into());
        }
        let start = Instant::now();
        self.clock.set_horizon(None)?;
        if self.advance(before_run)? != MachineStop::PoweredOff {
            return Err("fixture stopped before power-off".into());
        }
        let console = String::from_utf8_lossy(&self.uart.console).into_owned();
        if !console
            .lines()
            .any(|line| line.trim_end() == "SEEDBOX_INIT_PASS")
        {
            return Err("guest powered off without acceptance marker".into());
        }
        let (witness, trace_hash, console_hash, ram_hash) = unsafe {
            witness::finish(
                &self.trace,
                &self.uart.console,
                self.ram.bytes(),
                self.page.bytes(),
            )
        };
        Ok(Report {
            backend: B::NAME.into(),
            arch: "arm64".into(),
            timebase: "pv".into(),
            format: 1,
            seed: self.seed,
            witness,
            trace_hash,
            console_hash,
            ram_hash,
            kernel_sha256: self.kernel_hash.clone(),
            initrd_sha256: self.initrd_hash.clone(),
            exits: self.trace.len(),
            virtual_ns: self.clock.now(),
            wall_seconds: start.elapsed().as_secs_f64(),
            deferred_spi_events: self.deferred_spi_events,
            deferred_clock_read_spis: self.deferred_clock_read_spis,
            console,
            trace: std::mem::take(&mut self.trace),
        })
    }

    fn advance(&mut self, mut before_run: impl FnMut()) -> Result<MachineStop> {
        if self.powered_off {
            return Ok(MachineStop::PoweredOff);
        }
        if let Some(devices) = &self.application {
            if let Some((code, setup_error)) = devices.exit {
                return Ok(MachineStop::ApplicationExit { code, setup_error });
            }
            if devices.barrier {
                return Ok(MachineStop::Barrier);
            }
        }
        self.publish()?;
        if self.clock.at_horizon() {
            return Ok(MachineStop::Horizon {
                time_ns: self.clock.now(),
            });
        }
        while self.raw_exits < 5_000_000 {
            before_run();
            let stop = self.backend.run()?;
            self.raw_exits += 1;
            if self.backend.get_sys_reg(SysReg::CNTV_CTL)? & 1 != 0 {
                return Err("architectural virtual timer enabled: PV timer leak".into());
            }
            if self.ready {
                let control = self.backend.get_sys_reg(SysReg::CNTKCTL)?;
                if control != 0 {
                    return Err(format!(
                        "CNTKCTL_EL1={control:#x} after PV handshake: hardware counter access or event stream enabled; rebuild kernel-arm64 with the exec/context-switch counter-access fix"
                    ));
                }
            }
            // These writes preserve our initially unlocked debug state. KVM
            // consumes them in-kernel; HVF exposes them. They are completion
            // work, not architectural scheduling points: do not reconcile or
            // publish the PV page, judge timers, or add a witness record here.
            // Keep the leak checks above and the raw-exit budget for all stops.
            if matches!(stop.exit, Exit::SysReg { reg, write: true, data: 0, .. }
                if matches!(SysReg(reg), SysReg::OSDLR | SysReg::OSLAR))
            {
                self.backend.complete(None)?;
                continue;
            }
            // All guest memory accesses happen after the synchronous exit.
            self.clock.read(unsafe { self.page.word(8)? })?;
            let mut response = None;
            let mut off = false;
            let mut idle = false;
            match stop.exit {
                Exit::Mmio {
                    addr,
                    len,
                    write,
                    data,
                    ..
                } => {
                    let effect = if (UART_ADDR..UART_ADDR + 0x1000).contains(&addr) {
                        if write {
                            self.uart.write(addr - UART_ADDR, len, data)?
                        } else {
                            response = Some(self.uart.read(addr - UART_ADDR, len)?);
                            Effect::None
                        }
                    } else if (EVENT_ADDR..EVENT_ADDR + PAGE_SIZE as u64).contains(&addr) {
                        if !write {
                            return Err("clockevent read is not part of PV ABI".into());
                        }
                        self.event.write(addr - EVENT_ADDR, len, data)?
                    } else if self.application.is_some() {
                        response = self.application_mmio(addr, len, write, data)?;
                        Effect::None
                    } else {
                        return Err(format!("unmapped MMIO {addr:#x} at PC {:#x}", stop.pc));
                    };
                    match effect {
                        Effect::None => (),
                        Effect::Arm(mut d) => {
                            if d != 0 {
                                if let Some((policy, rng)) = &mut self.timer_policy {
                                    if policy.jitter_ns > 0 {
                                        d = d
                                            .checked_add(rng.below(policy.jitter_ns + 1))
                                            .filter(|d| *d != u64::MAX)
                                            .ok_or("application timer jitter overflow")?;
                                    }
                                }
                            }
                            self.clock.arm(d)?;
                        }
                        Effect::Idle => {
                            if !self.ready {
                                return Err("idle before PV handshake".into());
                            }
                            idle = true;
                        }
                        Effect::Ack => {
                            self.clock.ack();
                            self.inject_spi(TIMER_SPI, false)?;
                        }
                        Effect::Yield => {
                            // A deferred IRQ requests a rendezvous independently
                            // of the deadline. Do not advance to a future timer
                            // when the guest is yielding just to deliver it.
                            if self.deferred_spis.is_empty() {
                                self.clock.at_bound()?;
                            }
                        }
                        Effect::Ready(v) => {
                            if v != ABI || self.ready {
                                return Err(format!("invalid PV handshake {v}"));
                            }
                            self.ready = true;
                        }
                    }
                }
                Exit::Wfi => {
                    if !self.ready {
                        return Err("WFI before PV handshake".into());
                    }
                    idle = true;
                }
                Exit::Hvc { imm: 0 } => {
                    let function = self.backend.get_reg(CoreReg::X(0))?;
                    let value = match function {
                        0x8400_0000 => 2, // PSCI 0.2: avoid discovering host SMCCC services
                        0x8400_0006 => 2, // MIGRATE_INFO_TYPE: no trusted OS
                        0x8400_0008 => {
                            off = true;
                            0
                        }
                        _ => u64::MAX, // NOT_SUPPORTED
                    };
                    if !off {
                        self.backend.set_reg(CoreReg::X(0), value)?;
                        for n in 1..4 {
                            self.backend.set_reg(CoreReg::X(n), 0)?;
                        }
                    }
                }
                ref other => {
                    return Err(format!("unsupported exit {other:?} at PC {:#x}", stop.pc))
                }
            }
            let uart_irq = self.uart.irq_level();
            if uart_irq != self.uart_irq {
                self.inject_spi(UART_SPI, uart_irq)?;
                self.uart_irq = uart_irq;
            }
            if self.clock.judge(idle)? {
                self.inject_spi(TIMER_SPI, true)?;
                // A timer expiry at the guest clock-read rendezvous with
                // PSTATE.I set proves the masked-deadline path was exercised.
                if self.deferred_spis.contains(&TIMER_SPI)
                    && matches!(stop.exit, Exit::Mmio { addr, write: true, .. }
                        if addr == EVENT_ADDR + 24)
                {
                    self.deferred_clock_read_spis += 1;
                }
            }
            self.flush_spis()?;
            self.trace
                .push(Record::new(&stop.exit, self.clock.now(), stop.pc, response));
            if off {
                if !self.ready {
                    return Err("power-off before PV handshake".into());
                }
                self.publish()?;
                self.powered_off = true;
                return Ok(MachineStop::PoweredOff);
            }
            self.backend.complete(response)?;
            self.publish()?;
            if let Some(devices) = &self.application {
                if let Some((code, setup_error)) = devices.exit {
                    return Ok(MachineStop::ApplicationExit { code, setup_error });
                }
                if devices.barrier {
                    return Ok(MachineStop::Barrier);
                }
                if !devices.events.is_empty() {
                    return Ok(MachineStop::Device {
                        time_ns: self.clock.now(),
                    });
                }
            }
            if self.clock.at_horizon() {
                return Ok(MachineStop::Horizon {
                    time_ns: self.clock.now(),
                });
            }
        }
        Err("exit budget exhausted (5,000,000); no successful power-off".into())
    }

    fn application_mmio(
        &mut self,
        addr: u64,
        width: u8,
        write: bool,
        value: u64,
    ) -> Result<Option<u64>> {
        use application::*;
        let devices = self
            .application
            .as_mut()
            .ok_or("application devices unavailable")?;
        if (SUPERVISOR_MMIO..SUPERVISOR_MMIO + PAGE_SIZE as u64).contains(&addr) {
            let offset = addr - SUPERVISOR_MMIO;
            if write {
                let event = supervisor_write(offset, width, value)?;
                match event {
                    Event::Exit { code, setup_error } => devices.exit = Some((code, setup_error)),
                    Event::Barrier => {
                        if devices.barrier || !devices.entropy.is_empty() {
                            return Err("repeated launch barrier".into());
                        }
                        devices.barrier = true;
                    }
                    _ => {}
                }
                devices.events.push(event);
                return Ok(None);
            }
            if offset != 0xf8 * 8 || width != 4 || devices.barrier {
                return Err("invalid supervisor entropy read".into());
            }
            return devices
                .entropy
                .pop_front()
                .map(|word| Some(word as u64))
                .ok_or_else(|| "launch barrier entropy exhausted".into());
        }
        let (device, base) = if (NET_MMIO..NET_MMIO + PAGE_SIZE as u64).contains(&addr) {
            (&mut devices.net, NET_MMIO)
        } else if (CONSOLE_MMIO..CONSOLE_MMIO + PAGE_SIZE as u64).contains(&addr) {
            (&mut devices.console, CONSOLE_MMIO)
        } else if (BLOCK_MMIO..BLOCK_MMIO + PAGE_SIZE as u64).contains(&addr) {
            (
                devices.block.as_mut().ok_or("block MMIO without a disk")?,
                BLOCK_MMIO,
            )
        } else {
            return Err(format!("unmapped application MMIO {addr:#x}"));
        };
        let response = if write {
            let events = unsafe {
                self.ram
                    .with_bytes_mut(|ram| device.write(ram, addr - base, width, value))
            }?;
            devices.events.extend(events);
            None
        } else {
            Some(device.read(addr - base, width)?)
        };
        self.sync_application_irqs()?;
        Ok(response)
    }

    fn sync_application_irqs(&mut self) -> Result<()> {
        if let Some(devices) = &self.application {
            let levels = [
                (application::NET_SPI, devices.net.irq()),
                (application::CONSOLE_SPI, devices.console.irq()),
                (
                    application::BLOCK_SPI,
                    devices.block.as_ref().is_some_and(|d| d.irq()),
                ),
            ];
            for (irq, level) in levels {
                self.inject_spi(irq, level)?;
            }
        }
        Ok(())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        if !level {
            self.deferred_spis.remove(&intid);
            self.asserted_spis.remove(&intid);
            return self.backend.inject_spi(intid, false);
        }
        if self.asserted_spis.contains(&intid) {
            return Ok(());
        }
        // Multiple queued hardware sources can leave a second IRQ pending after
        // the first handler unmasks. Its interrupted PC then skids, contaminating
        // guest state (including Linux's interrupt entropy pool). Admit one line
        // only, and wait for its deassertion plus an unmasked semantic exit.
        if !self.asserted_spis.is_empty() || self.backend.get_reg(CoreReg::Pstate)? & (1 << 7) != 0
        {
            if self.deferred_spis.insert(intid) {
                self.deferred_spi_events += 1;
            }
            return Ok(());
        }
        self.deferred_spis.remove(&intid);
        self.backend.inject_spi(intid, true)?;
        self.asserted_spis.insert(intid);
        Ok(())
    }
    fn flush_spis(&mut self) -> Result<()> {
        if self.asserted_spis.is_empty()
            && !self.deferred_spis.is_empty()
            && self.backend.get_reg(CoreReg::Pstate)? & (1 << 7) == 0
        {
            let intid = self
                .deferred_spis
                .pop_first()
                .expect("nonempty deferred SPIs");
            self.backend.inject_spi(intid, true)?;
            self.asserted_spis.insert(intid);
        }
        Ok(())
    }
    fn publish(&mut self) -> Result<()> {
        let last = unsafe { self.page.word(8)? };
        let mut page = TimePage::from_clock(&self.clock, last, !self.deferred_spis.is_empty());
        for offset in [0, 16, 24, 32] {
            unsafe {
                self.page.set_word(offset as usize, page.read(offset, 8)?)?;
            }
        }
        Ok(())
    }
}
/// This EL1-only machine does not expose EL2 Fine Grained Traps. Host defaults
/// disagree here: feature detection itself calls printk -> sched_clock -> PV
/// read, even when loglevel suppresses console output. Define the architectural
/// input before boot, never compensate for the resulting read in the clock.
/// This is a partial feature policy, not yet a complete portable CPU model.
pub fn configure_cpu_features(backend: &mut impl Backend) -> Result<()> {
    const FGT: u64 = 0xf << 56; // ID_AA64MMFR0_EL1[59:56]
    let initial = backend.get_sys_reg(SysReg::ID_AA64MMFR0)?;
    let expected = initial & !FGT;
    // Avoid unnecessary writes on KVM versions which already hide this feature.
    if initial != expected {
        backend.set_sys_reg(SysReg::ID_AA64MMFR0, expected)?;
    }
    let actual = backend.get_sys_reg(SysReg::ID_AA64MMFR0)?;
    if actual != expected {
        return Err(format!(
            "ID_AA64MMFR0_EL1 feature policy readback: expected {expected:#018x}, got {actual:#018x}; backend must hide FGT and preserve all other fields"
        ));
    }
    // One CPU, affinity 0, architectural RES1 bit. Never inherit host topology.
    exact_register(backend, SysReg::MPIDR, 0x8000_0000, "MPIDR_EL1")?;
    // This machine has no PMU device or PMU interrupt. KVM never enables
    // KVM_ARM_VCPU_PMU_V3; make the advertised feature agree on both hosts.
    let dfr0 = backend.get_sys_reg(SysReg::ID_AA64DFR0)?;
    exact_register(
        backend,
        SysReg::ID_AA64DFR0,
        dfr0 & !(0xf << 8),
        "ID_AA64DFR0_EL1",
    )?;
    Ok(())
}

fn exact_register(
    backend: &mut impl Backend,
    reg: SysReg,
    expected: u64,
    name: &str,
) -> Result<()> {
    if backend.get_sys_reg(reg)? != expected {
        backend.set_sys_reg(reg, expected)?;
    }
    let actual = backend.get_sys_reg(reg)?;
    if actual != expected {
        return Err(format!(
            "{name} feature policy readback: expected {expected:#018x}, got {actual:#018x}"
        ));
    }
    Ok(())
}

pub fn image_layout(kernel: &[u8]) -> Result<(u64, u64)> {
    if kernel.len() < 64 || &kernel[56..60] != b"ARM\x64" {
        return Err("--kernel: expected an uncompressed arm64 Linux Image".into());
    }
    let word = |n| u64::from_le_bytes(kernel[n..n + 8].try_into().expect("checked header size"));
    let offset = word(8);
    let size = word(16);
    let flags = word(24);
    if flags & 1 != 0 || size == 0 || offset > 0x20_0000 {
        return Err("unsupported Image endianness, legacy size, or text offset".into());
    }
    let size = size.max(kernel.len() as u64);
    Ok((RAM_BASE + offset, size))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_images() {
        assert!(image_layout(b"bad").is_err());
        let mut k = [0; 64];
        k[56..60].copy_from_slice(b"ARM\x64");
        assert!(image_layout(&k).is_err());
        k[16..24].copy_from_slice(&4096u64.to_le_bytes());
        assert_eq!(image_layout(&k).unwrap(), (RAM_BASE, 4096));
    }
}

#[cfg(test)]
mod spi_regression {
    use super::*;
    use crate::backend::{core_index, DirtyPages, Stop};
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Fake {
        regs: BTreeMap<u32, u64>,
        sys: BTreeMap<u16, u64>,
        injected: Vec<(u32, bool)>,
    }
    impl Backend for Fake {
        fn create() -> Result<Self> {
            Ok(Self::default())
        }
        fn map_memory(&mut self, _: u32, _: u64, _: Rc<Memory>) -> Result<()> {
            Ok(())
        }
        fn unmap_memory(&mut self, _: u32) -> Result<()> {
            Ok(())
        }
        fn create_vcpu(&mut self) -> Result<()> {
            Ok(())
        }
        fn get_reg(&self, r: CoreReg) -> Result<u64> {
            Ok(*self.regs.get(&core_index(r)?).unwrap_or(&0))
        }
        fn set_reg(&mut self, r: CoreReg, v: u64) -> Result<()> {
            self.regs.insert(core_index(r)?, v);
            Ok(())
        }
        fn get_sys_reg(&self, r: SysReg) -> Result<u64> {
            Ok(*self.sys.get(&r.0).unwrap_or(&0))
        }
        fn set_sys_reg(&mut self, r: SysReg, v: u64) -> Result<()> {
            self.sys.insert(r.0, v);
            Ok(())
        }
        fn run(&mut self) -> Result<Stop> {
            Err("unexpected guest entry".into())
        }
        fn complete(&mut self, _: Option<u64>) -> Result<()> {
            Ok(())
        }
        fn inject_spi(&mut self, id: u32, level: bool) -> Result<()> {
            self.injected.push((id, level));
            Ok(())
        }
        fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
            Ok(DirtyPages::Unsupported)
        }
    }
    fn machine() -> Machine<Fake> {
        let mut kernel = [0; 64];
        kernel[56..60].copy_from_slice(b"ARM\x64");
        kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
        Machine::for_application(boot::BootSpec::fixture(&kernel, b"initrd", 1), [0; 6], None)
            .unwrap()
    }
    #[test]
    fn overlapping_sources_cannot_queue_a_second_hardware_irq() {
        let mut m = machine();
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        m.inject_spi(TIMER_SPI, true).unwrap();
        m.inject_spi(application::NET_SPI, true).unwrap();
        m.inject_spi(application::CONSOLE_SPI, true).unwrap();
        m.inject_spi(TIMER_SPI, true).unwrap();
        m.flush_spis().unwrap();
        assert_eq!(m.backend.injected, vec![(TIMER_SPI, true)]);
        assert_eq!(m.asserted_spis, BTreeSet::from([TIMER_SPI]));
        m.backend.set_reg(CoreReg::Pstate, 1 << 7).unwrap();
        m.inject_spi(TIMER_SPI, false).unwrap();
        m.flush_spis().unwrap();
        assert!(m.asserted_spis.is_empty());
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        m.flush_spis().unwrap();
        assert_eq!(m.asserted_spis, BTreeSet::from([application::NET_SPI]));
        assert_eq!(m.deferred_spis, BTreeSet::from([application::CONSOLE_SPI]));
        m.flush_spis().unwrap();
        assert_eq!(
            m.backend.injected,
            vec![
                (TIMER_SPI, true),
                (TIMER_SPI, false),
                (application::NET_SPI, true)
            ]
        );
        m.inject_spi(application::NET_SPI, false).unwrap();
        m.flush_spis().unwrap();
        assert_eq!(m.asserted_spis, BTreeSet::from([application::CONSOLE_SPI]));
    }
    #[test]
    fn cancelled_deferred_sources_never_reach_hardware_and_sync_keeps_active_timer() {
        let mut m = machine();
        m.inject_spi(application::NET_SPI, true).unwrap();
        m.inject_spi(application::NET_SPI, true).unwrap();
        assert_eq!(m.deferred_spi_events, 1);
        m.inject_spi(application::NET_SPI, false).unwrap();
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        m.flush_spis().unwrap();
        assert!(m.backend.injected.iter().all(|(_, level)| !level));
        m.inject_spi(TIMER_SPI, true).unwrap();
        m.sync_application_irqs().unwrap();
        m.sync_application_irqs().unwrap();
        assert_eq!(m.asserted_spis, BTreeSet::from([TIMER_SPI]));
        assert_eq!(
            m.backend
                .injected
                .iter()
                .filter(|(_, level)| *level)
                .count(),
            1
        );
    }
}
