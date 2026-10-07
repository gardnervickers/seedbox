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
use std::{rc::Rc, time::Instant};

/// Memory/device ownership and semantic decisions live here, never in a backend.
/// Backend drops first and also retains its own references to every mapping.
pub struct Machine<B: Backend> {
    backend: B,
    ram: Rc<Memory>,
    page: Rc<Memory>,
    uart: Pl011,
    uart_irq: bool,
    event: ClockEvent,
    clock: PvClock,
    ready: bool,
    trace: Vec<Record>,
    seed: u64,
    kernel_hash: String,
    initrd_hash: String,
}
impl<B: Backend> Machine<B> {
    pub fn new(kernel: &[u8], initrd: &[u8], seed: u64) -> Result<Self> {
        let (entry, image_size) = image_layout(kernel)?;
        if entry
            .checked_add(image_size)
            .is_none_or(|end| end > INITRD_ADDR)
        {
            return Err("kernel overlaps initrd".into());
        }
        let dtb = fdt::build(seed, initrd.len())?;
        let ram = Rc::new(Memory::new(RAM_SIZE)?);
        let page = Rc::new(Memory::new(PAGE_SIZE)?);
        // No VM exists yet, so none of these copies race with a vCPU.
        unsafe {
            ram.write((entry - RAM_BASE) as usize, kernel)?;
            ram.write((INITRD_ADDR - RAM_BASE) as usize, initrd)?;
            ram.write((DTB_ADDR - RAM_BASE) as usize, &dtb)?;
            page.set_word(16, u64::MAX)?;
            page.set_word(24, ABI)?;
        }
        let mut backend = B::create()?;
        backend.map_memory(0, RAM_BASE, Rc::clone(&ram))?;
        backend.map_memory(1, TIME_ADDR, Rc::clone(&page))?;
        backend.create_vcpu()?;
        for n in 0..31 {
            backend.set_reg(CoreReg::X(n), 0)?;
        }
        backend.set_reg(CoreReg::Sp, 0)?;
        backend.set_reg(CoreReg::X(0), DTB_ADDR)?;
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
            event: ClockEvent,
            clock: PvClock::default(),
            ready: false,
            trace: vec![],
            seed,
            kernel_hash: witness::hash(kernel),
            initrd_hash: witness::hash(initrd),
        })
    }
    /// `before_run` is a performance-only hook for acceptance jitter. It cannot
    /// access guest state. Host duration is report metadata, never witness input.
    pub fn run(mut self, before_run: impl FnMut()) -> Result<Report> {
        let start = Instant::now();
        self.run_inner(before_run).map_err(|error| {
            format!(
                "{error}\nexits: {}\nwall_seconds: {:.6}\nlast exit: {:?}\nconsole:\n{}",
                self.trace.len(),
                start.elapsed().as_secs_f64(),
                self.trace.last(),
                String::from_utf8_lossy(&self.uart.console)
            )
        })
    }
    fn run_inner(&mut self, mut before_run: impl FnMut()) -> Result<Report> {
        let start = Instant::now();
        self.publish()?;
        for _ in 0..5_000_000 {
            before_run();
            let stop = self.backend.run()?;
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
                    } else {
                        return Err(format!("unmapped MMIO {addr:#x} at PC {:#x}", stop.pc));
                    };
                    match effect {
                        Effect::None => (),
                        Effect::Arm(d) => self.clock.arm(d)?,
                        Effect::Idle => {
                            if !self.ready {
                                return Err("idle before PV handshake".into());
                            }
                            idle = true;
                        }
                        Effect::Ack => {
                            self.clock.ack();
                            self.backend.inject_spi(TIMER_SPI, false)?;
                        }
                        Effect::Yield => self.clock.at_bound()?,
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
            }
            self.trace
                .push(Record::new(&stop.exit, self.clock.now(), stop.pc, response));
            if off {
                if !self.ready {
                    return Err("power-off before PV handshake".into());
                }
                let console = String::from_utf8_lossy(&self.uart.console).into_owned();
                if !console
                    .lines()
                    .any(|line| line.trim_end() == "SEEDBOX_INIT_PASS")
                {
                    return Err("guest powered off without acceptance marker".into());
                }
                self.publish()?;
                let (witness, trace_hash, console_hash, ram_hash) = unsafe {
                    witness::finish(
                        &self.trace,
                        &self.uart.console,
                        self.ram.bytes(),
                        self.page.bytes(),
                    )
                };
                return Ok(Report {
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
                    console,
                    trace: std::mem::take(&mut self.trace),
                });
            }
            self.backend.complete(response)?;
            self.publish()?;
        }
        Err("exit budget exhausted (5,000,000); no successful power-off".into())
    }
    fn inject_spi(&mut self, intid: u32, level: bool) -> Result<()> {
        // A pending hardware IRQ can skid across instructions when the guest
        // later unmasks DAIF.I. Linux records the interrupted PC in its stack
        // and interrupt entropy pool. Require an unmasked admission boundary;
        // never clear the guest's mask here or normalize the resulting RAM.
        if level && self.backend.get_reg(CoreReg::Pstate)? & (1 << 7) != 0 {
            return Err(format!(
                "SPI {intid} asserted with PSTATE.I masked: nondeterministic interrupt admission; rebuild kernel-arm64 with the unmasked PV idle rendezvous"
            ));
        }
        self.backend.inject_spi(intid, level)
    }
    fn publish(&mut self) -> Result<()> {
        let last = unsafe { self.page.word(8)? };
        let mut page = TimePage::from_clock(&self.clock, last);
        for offset in [0, 16, 24] {
            unsafe {
                self.page.set_word(offset as usize, page.read(offset, 8)?)?;
            }
        }
        Ok(())
    }
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
