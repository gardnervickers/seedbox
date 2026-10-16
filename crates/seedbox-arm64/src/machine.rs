mod snapshot;
pub use snapshot::Snapshot;

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

const EXCLUSIVE_EPOCH_OFFSET: usize = 40;
const SYSCALL_NS_OFFSET: usize = 48;
use seedbox_engine_core::cpu_charge::{throttled_ns, SYSCALL_NS};
/// A progress call's nominal charge, jitter bound and stream seed
/// (kernel-arm64/seedbox.c).
const PROGRESS_NS_OFFSET: usize = 56;
const PROGRESS_JITTER_NS_OFFSET: usize = 64;
const PROGRESS_JITTER_SEED_OFFSET: usize = 72;
/// Branch entropy: a generation, then 32 bytes (kernel-arm64/seedbox.c).
const ENTROPY_GEN_OFFSET: usize = 80;
const ENTROPY_OFFSET: usize = 88;

/// A completed semantic stop, with the vCPU left stopped for the world owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MachineStop {
    Horizon {
        time_ns: u64,
    },
    PoweredOff,
    Device {
        time_ns: u64,
    },
    /// The block device held a selected request: terminal until a crash.
    Held {
        time_ns: u64,
        trigger: u32,
    },
    Console {
        time_ns: u64,
    },
    ApplicationExit {
        code: u8,
        setup_error: bool,
    },
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
    // Observation only: excluded from every machine/device digest.
    log_capture: application::log_capture::Pending,
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
            page.set_word(SYSCALL_NS_OFFSET, SYSCALL_NS)?;
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
            log_capture: Default::default(),
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
        self.advance(before_run, false)
    }

    /// Slack permits an earlier stop; this cooperative backend uses the exact limit.
    pub fn run_slice(
        &mut self,
        hard_limit: u64,
        _early_slack: u64,
        observe_console: bool,
    ) -> Result<MachineStop> {
        if self.held().is_some() {
            return Err("machine holds a block request until its crash".into());
        }
        self.clock.set_horizon(Some(hard_limit))?;
        self.advance(|| {}, observe_console)
    }
    /// The trigger whose block request hold suspended this machine.
    pub fn held(&self) -> Option<u32> {
        self.application
            .as_ref()
            .and_then(|d| d.block.as_ref())
            .and_then(|b| b.held())
    }
    pub fn stopped_observation(&self) -> application::worker::StoppedObservation {
        application::worker::StoppedObservation {
            time_ns: self.clock.now(),
            pending_irq: self.clock.is_pending()
                || !self.deferred_spis.is_empty()
                || !self.asserted_spis.is_empty(),
            next_deadline: self.clock.next_event().unwrap_or(u64::MAX),
            owed_io: None,
            abi: if self.ready { ABI } else { 0 },
            exits: self.trace.len() as u64,
            powered_off: self.powered_off,
            exit: self.application.as_ref().and_then(|d| d.exit),
            barrier: self.application.as_ref().is_some_and(|d| d.barrier),
            has_control: self.application.is_some(),
        }
    }
    pub fn park(&self) -> Result<()> {
        if self.powered_off
            || self
                .application
                .as_ref()
                .is_some_and(|d| !d.events.is_empty())
        {
            return Err("machine cannot park with terminal or undrained physical work".into());
        }
        // Every nonterminal return has completed its held backend operation.
        Ok(())
    }
    /// Publish the next syscall's CPU charge while the vCPU is stopped. An
    /// in-flight charge lives in guest registers/stack and keeps its old cost.
    /// The page is included in snapshots; crash succession reapplies active
    /// faults through the shared world coordinator.
    pub fn throttle(&mut self, factor_permille: u64) -> Result<()> {
        if !seedbox_control::THROTTLE_PERMILLE.contains(&factor_permille) {
            return Err("invalid ARM throttle factor".into());
        }
        unsafe {
            self.page
                .set_word(SYSCALL_NS_OFFSET, throttled_ns(SYSCALL_NS, factor_permille))
        }
    }

    /// Restart the progress jitter stream from `seed` while the vCPU is
    /// stopped; the guest kernel picks it up at its next progress call.
    pub fn seed_progress_jitter(&mut self, seed: u64) -> Result<()> {
        unsafe { self.page.set_word(PROGRESS_JITTER_SEED_OFFSET, seed) }
    }

    /// Give a stopped guest new entropy for a branch. The kernel mixes it
    /// into its pool and reseeds at its next random draw or tick; the page is
    /// part of snapshots, so a restored capture keeps the pending entropy.
    pub fn reseed_guest(&mut self, entropy: [u32; 8]) -> Result<()> {
        for (i, pair) in entropy.as_chunks::<2>().0.iter().enumerate() {
            let word = u64::from(pair[0]) | (u64::from(pair[1]) << 32);
            unsafe { self.page.set_word(ENTROPY_OFFSET + 8 * i, word)? };
        }
        let generation = unsafe { self.page.word(ENTROPY_GEN_OFFSET)? }
            .checked_add(1)
            .ok_or("entropy generation overflow")?;
        unsafe { self.page.set_word(ENTROPY_GEN_OFFSET, generation) }
    }

    pub fn seed_timer(&mut self, seed: u64, domain: &str) -> Result<()> {
        if domain.is_empty() {
            return Err("empty application timer domain".into());
        }
        let (policy, rng) = self
            .timer_policy
            .as_mut()
            .ok_or("application timer policy unavailable")?;
        policy.seed = seed;
        policy.domain = domain.into();
        *rng = crate::rng::Rng::new(seed, domain);
        Ok(())
    }
    pub fn install_crash_disk(
        &mut self,
        input: &mut impl std::io::Read,
        digest: Option<&str>,
    ) -> Result<()> {
        if self.raw_exits != 0 {
            return Err("successor guest has already entered".into());
        }
        let devices = self
            .application
            .as_mut()
            .ok_or("fixture has no application disk")?;
        let disk = match (devices.block.as_ref().and_then(|d| d.disk()), digest) {
            (Some(base), Some(digest)) => {
                let disk = crate::disk::Disk::read_crash_transfer(base.clone(), &mut *input)?;
                if disk.digest() != digest {
                    return Err("successor disk digest mismatch".into());
                }
                Some(disk)
            }
            (None, None) => {
                let mut byte = [0];
                if input
                    .read(&mut byte)
                    .map_err(|e| format!("disk transfer: {e}"))?
                    != 0
                {
                    return Err("unexpected diskless transfer payload".into());
                }
                None
            }
            _ => return Err("successor disk topology mismatch".into()),
        };
        devices.block = disk.map(application::virtio::Virtio::block);
        Ok(())
    }
    pub fn time_ns(&self) -> u64 {
        self.clock.now()
    }
    /// Install or remove a block request hold on the retained disk.
    pub fn hold_block(
        &mut self,
        hold: Option<seedbox_engine_core::virtio_block::Hold>,
    ) -> Result<()> {
        self.application
            .as_mut()
            .and_then(|devices| devices.block.as_mut())
            .ok_or("no retained disk for a block request hold")?
            .set_hold(hold)
    }
    /// Start or end injected errors on the retained disk, from its next request.
    pub fn disk_errors(
        &mut self,
        errors: Option<seedbox_engine_core::virtio_block::InjectedErrors>,
    ) -> Result<Option<seedbox_engine_core::virtio_block::ErrorCounts>> {
        self.application
            .as_mut()
            .and_then(|devices| devices.block.as_mut())
            .ok_or("no retained disk for disk errors")?
            .set_disk_errors(errors)
    }
    /// Start or end injected latency on the retained disk, from its next
    /// request. Requests already pending keep their due times.
    pub fn disk_latency(
        &mut self,
        latency: Option<seedbox_engine_core::virtio_block::InjectedLatency>,
    ) -> Result<Option<seedbox_engine_core::virtio_block::LatencyCounts>> {
        self.application
            .as_mut()
            .and_then(|devices| devices.block.as_mut())
            .ok_or("no retained disk for disk latency")?
            .set_disk_latency(latency)
    }
    /// Complete delayed disk requests due by now, then publish the next one
    /// as the clock's device deadline.
    fn complete_device_work(&mut self) -> Result<()> {
        let now = self.clock.now();
        let Some(block) = self
            .application
            .as_mut()
            .and_then(|devices| devices.block.as_mut())
        else {
            return Ok(());
        };
        if block.next_due().is_some_and(|due| due <= now) {
            unsafe { self.ram.with_bytes_mut(|ram| block.complete_due(ram, now)) }?;
            let next = block.next_due();
            self.clock.set_device(next);
            return self.sync_application_irqs();
        }
        let next = block.next_due();
        self.clock.set_device(next);
        Ok(())
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

    /// Publish what a progress call costs, its jitter bound and the seed of
    /// the stream the guest kernel draws the jitter from, before the first
    /// instruction. The page is part of every snapshot, so a branch keeps its
    /// own stream.
    pub fn with_progress(self, progress: &application::Progress) -> Result<Self> {
        if self.raw_exits != 0 {
            return Err("the progress charge must be set before the machine runs".into());
        }
        unsafe {
            self.page.set_word(PROGRESS_NS_OFFSET, progress.ns)?;
            self.page
                .set_word(PROGRESS_JITTER_NS_OFFSET, progress.jitter_ns)?;
            self.page
                .set_word(PROGRESS_JITTER_SEED_OFFSET, progress.seed)?;
        }
        Ok(self)
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

    /// Stopped-memory diagnostics with the same compact page format as KVM.
    /// These hashes are not included in execution witnesses.
    pub fn write_application_page_hashes(&self, output: &mut dyn std::io::Write) -> Result<()> {
        use sha2::{Digest, Sha256};
        let zero = [0u8; 4096];
        let zero_hash = Sha256::digest(zero);
        for page in unsafe { self.ram.bytes() }.chunks(4096) {
            let digest = if page == zero {
                zero_hash
            } else {
                Sha256::digest(page)
            };
            output.write_all(&digest[..8]).map_err(|e| e.to_string())?;
        }
        Ok(())
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
        let trace_hash = hex::encode(trace.finalize());
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
                ("ram".into(), hex::encode(ram.finalize())),
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
            witness: hex::encode(hash.finalize()),
            state_digest,
            state_digest_parts: parts,
        })
    }

    /// The next incarnation's disk. A branch keys unflushed-sector loss on
    /// its identity, as x86 does; a continuation keeps the ordinary stream.
    pub fn crashed_disk(
        &self,
        seed: u64,
        identity: Option<&str>,
        name: &str,
        incarnation: u32,
    ) -> Option<(crate::disk::Disk, u64)> {
        self.application
            .as_ref()?
            .block
            .as_ref()?
            .disk()
            .map(|disk| disk.crashed_branch(seed, identity, name, incarnation))
    }

    pub fn take_log_chunks(&mut self) -> Vec<application::log_capture::Chunk> {
        self.log_capture.take()
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
        self.application
            .as_ref()
            .ok_or("fixture has no network device")?;
        self.invalidate_exclusive_reservation()?;
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
        self.application
            .as_ref()
            .ok_or("fixture has no control device")?;
        self.invalidate_exclusive_reservation()?;
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
        if self.advance(before_run, false)? != MachineStop::PoweredOff {
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

    fn advance(
        &mut self,
        mut before_run: impl FnMut(),
        observe_console: bool,
    ) -> Result<MachineStop> {
        let console_start = self.uart.console.len();
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
            let stop = match self.backend.run()? {
                backend::RunStop::Guest(stop) => stop,
                // Host stops create no Pending operation. Re-enter through
                // before_run so cancellation/liveness remains observable, but
                // do not read/publish PV time, deliver IRQs, complete an exit,
                // spend a semantic budget or append a witness record.
                backend::RunStop::Interrupted => continue,
            };
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
            self.invalidate_exclusive_reservation()?;
            // All guest memory accesses happen after the synchronous exit.
            self.clock.read(unsafe { self.page.word(8)? })?;
            let console_before = self.uart.console.len();
            let events_before = self.application.as_ref().map_or(0, |d| d.events.len());
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
                            // when the guest is yielding just to deliver it. A
                            // masked guest cannot take it and yields only at its
                            // bound, which its unpublished read step crosses.
                            if self.deferred_spis.is_empty()
                                || self.backend.get_reg(CoreReg::Pstate)? & (1 << 7) != 0
                            {
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
                // A host interruption wakes WFE, so letting the guest wait would
                // make spin-loop iteration counts depend on host timing. The
                // translator already rewrites application WFE/WFI to NOP; this
                // covers kernel WFE and untranslated guest processes on HVF.
                Exit::Wfe => (),
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
            self.complete_device_work()?;
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
            if let Some(devices) = &self.application {
                use application::log_capture::Stream;
                // Append only after the guest write has successfully committed.
                // A single exit writes either PL011 or virtio management output.
                self.log_capture.append(
                    Stream::Console,
                    self.clock.now(),
                    self.uart.console.len() - console_before,
                );
                for event in &devices.events[events_before..] {
                    if let application::Event::Control(bytes) = event {
                        self.log_capture
                            .append(Stream::Management, self.clock.now(), bytes.len());
                    }
                }
            }
            self.publish()?;
            if let Some(devices) = &self.application {
                if let Some((code, setup_error)) = devices.exit {
                    return Ok(MachineStop::ApplicationExit { code, setup_error });
                }
                if devices.barrier {
                    return Ok(MachineStop::Barrier);
                }
                // The MMIO write completed, but the held request did not.
                if let Some(trigger) = devices.block.as_ref().and_then(|b| b.held()) {
                    return Ok(MachineStop::Held {
                        time_ns: self.clock.now(),
                        trigger,
                    });
                }
                if !devices.events.is_empty() {
                    return Ok(MachineStop::Device {
                        time_ns: self.clock.now(),
                    });
                }
            }
            if observe_console && self.uart.console.len() > console_start {
                return Ok(MachineStop::Console {
                    time_ns: self.clock.now(),
                });
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
        let now = self.clock.now();
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
                    .with_bytes_mut(|ram| device.write(ram, addr - base, width, value, now))
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
    fn invalidate_exclusive_reservation(&mut self) -> Result<()> {
        // The kernel advances this same word on EL0 exception return. Read it
        // rather than retaining a host copy, and invalidate before device DMA
        // can partially mutate RAM, even if delivery subsequently fails.
        let epoch = unsafe { self.page.word(EXCLUSIVE_EPOCH_OFFSET)? }
            .checked_add(1)
            .ok_or("exclusive reservation epoch overflow")?;
        unsafe { self.page.set_word(EXCLUSIVE_EPOCH_OFFSET, epoch)? };
        // EL1 entry code uses TPIDRRO_EL0 as scratch. Only an already stopped
        // EL0 needs its visible epoch updated here; EL1 publishes on return.
        if self.backend.get_reg(CoreReg::Pstate)? & 0xf == 0 {
            self.backend.set_sys_reg(SysReg::TPIDRRO_EL0, epoch)?;
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
    use crate::backend::{core_index, DirtyPages, RunStop};
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Fake {
        regs: BTreeMap<u32, u64>,
        sys: BTreeMap<u16, u64>,
        injected: Vec<(u32, bool)>,
        stops: std::collections::VecDeque<RunStop>,
        fail_completion: bool,
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
        fn run(&mut self) -> Result<RunStop> {
            self.stops
                .pop_front()
                .ok_or("unexpected guest entry".into())
        }
        fn complete(&mut self, _: Option<u64>) -> Result<()> {
            if self.fail_completion {
                Err("injected completion failure".into())
            } else {
                Ok(())
            }
        }
        fn inject_spi(&mut self, id: u32, level: bool) -> Result<()> {
            self.injected.push((id, level));
            Ok(())
        }
        fn dirty_pages(&self, _: u32) -> Result<DirtyPages> {
            Ok(DirtyPages::Unsupported)
        }
        fn snapshot_state(&self) -> Result<backend::BackendState> {
            self.state()
        }
        fn restore_state(&mut self, state: &backend::BackendState) -> Result<()> {
            self.regs = state
                .core
                .iter()
                .enumerate()
                .map(|(i, &v)| (i as u32, v))
                .collect();
            self.sys = state.system.clone();
            Ok(())
        }
        fn state(&self) -> Result<backend::BackendState> {
            Ok(backend::BackendState {
                core: (0..34).map(|i| *self.regs.get(&i).unwrap_or(&0)).collect(),
                system: self.sys.clone(),
                simd: Vec::new(),
                gic_cpu: BTreeMap::new(),
                gic: Vec::new(),
            })
        }
    }
    #[test]
    fn machine_snapshot_preserves_stopped_state_and_isolates_later_mutations() {
        let fresh = || {
            let mut machine = machine();
            machine.application.as_mut().unwrap().block = Some(application::virtio::Virtio::block(
                crate::disk::Disk::new(None, Some(8)).unwrap(),
            ));
            machine
        };
        let mut original = fresh();
        original.backend.set_reg(CoreReg::X(7), 123).unwrap();
        original.clock.arm(500).unwrap();
        original.ready = true;
        original.throttle(4_000).unwrap();
        original.uart.console.extend_from_slice(b"abc");
        original.uart_irq = true;
        original.deferred_spis.insert(application::NET_SPI);
        original.asserted_spis.insert(application::CONSOLE_SPI);
        original.timer_policy = Some((
            application::TimerPolicy {
                seed: 9,
                domain: "timer/test".into(),
                jitter_ns: 10,
            },
            crate::rng::Rng::new(9, "timer/test"),
        ));
        let next_random = original.timer_policy.as_ref().unwrap().1.clone().next();
        original
            .log_capture
            .append(application::log_capture::Stream::Console, 0, 3);
        unsafe {
            original.ram.write(0x1000, b"before checkpoint").unwrap();
        }
        let devices = original.application.as_mut().unwrap();
        let mut disk = crate::disk::Disk::new(None, Some(8)).unwrap();
        disk.write(1, &[3; 512]).unwrap();
        disk.flush();
        disk.write(1, &[4; 512]).unwrap();
        devices.block = Some(application::virtio::Virtio::block(disk));
        devices.entropy.extend([1, 2, 3]);
        devices
            .console
            .receive(&mut [], b"pending control")
            .unwrap();
        let expected = original.application_evidence(true).unwrap();
        let snapshot = original.snapshot().unwrap();
        original.throttle(1_000).unwrap();
        unsafe {
            original.ram.write(0x1000, b"after checkpoint!").unwrap();
        }
        original.application.as_mut().unwrap().entropy.clear();
        original.uart.console.clear();
        drop(original);
        let mut restored = fresh().restore_snapshot(&snapshot).unwrap();
        assert_eq!(
            unsafe { restored.page.word(SYSCALL_NS_OFFSET).unwrap() },
            4_000
        );
        assert_eq!(restored.application_evidence(true).unwrap(), expected);
        assert_eq!(
            restored.timer_policy.as_mut().unwrap().1.next(),
            next_random
        );
        assert_eq!(restored.log_capture.take()[0].length, 3);
        assert_eq!(
            restored.deferred_spis,
            BTreeSet::from([application::NET_SPI])
        );
        assert_eq!(
            restored.asserted_spis,
            BTreeSet::from([application::CONSOLE_SPI])
        );
        // A second branch receives the unchanged snapshot, not the first branch's RNG.
        let mut checkpoint = Vec::new();
        snapshot.write_checkpoint(&mut checkpoint).unwrap();
        let mut branch = fresh().restore_checkpoint(checkpoint.as_slice()).unwrap();
        assert_eq!(branch.application_evidence(true).unwrap(), expected);
        assert_eq!(branch.timer_policy.as_mut().unwrap().1.next(), next_random);
        // RAM corruption must fail before a restored machine can escape.
        let metadata_len = u64::from_le_bytes(checkpoint[8..16].try_into().unwrap()) as usize;
        let last_ram_byte = 16 + metadata_len + 32 + 8 + branch.ram.len() - 1;
        checkpoint[last_ram_byte] ^= 1;
        assert!(fresh().restore_checkpoint(checkpoint.as_slice()).is_err());
    }

    #[test]
    fn throttle_changes_only_future_syscall_cost_and_survives_publication() {
        let mut m = machine();
        assert_eq!(unsafe { m.page.word(SYSCALL_NS_OFFSET).unwrap() }, 1_000);
        m.clock.arm(500).unwrap();
        let before = m.stopped_observation();
        m.throttle(4_000).unwrap();
        m.publish().unwrap();
        assert_eq!(unsafe { m.page.word(SYSCALL_NS_OFFSET).unwrap() }, 4_000);
        assert_eq!(m.stopped_observation(), before);
        assert!(m.throttle(999).is_err());
        assert!(m.throttle(1_000_001).is_err());
        assert_eq!(unsafe { m.page.word(SYSCALL_NS_OFFSET).unwrap() }, 4_000);
        m.throttle(1_000).unwrap();
        assert_eq!(unsafe { m.page.word(SYSCALL_NS_OFFSET).unwrap() }, 1_000);
    }

    fn machine() -> Machine<Fake> {
        let mut kernel = [0; 64];
        kernel[56..60].copy_from_slice(b"ARM\x64");
        kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
        Machine::for_application(boot::BootSpec::fixture(&kernel, b"initrd", 1), [0; 6], None)
            .unwrap()
    }
    #[test]
    fn host_interruptions_do_not_exhaust_last_semantic_exit() {
        let mut m = machine();
        m.ready = true;
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        // Simulate an epoch already advanced by the guest kernel.
        unsafe { m.page.set_word(EXCLUSIVE_EPOCH_OFFSET, 41).unwrap() };
        m.raw_exits = 4_999_999;
        m.backend.stops.extend([
            RunStop::Interrupted,
            RunStop::Interrupted,
            backend::Stop {
                pc: RAM_BASE,
                exit: Exit::Wfi,
            }
            .into(),
        ]);
        let mut entries = 0;
        assert_eq!(
            m.run_until(10, || entries += 1).unwrap(),
            MachineStop::Horizon { time_ns: 10 }
        );
        assert_eq!(entries, 3);
        assert_eq!(m.raw_exits, 5_000_000);
        assert_eq!(m.trace.len(), 1);
        assert!(m.backend.injected.is_empty());
        assert_eq!(unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() }, 42);
        assert_eq!(m.backend.get_sys_reg(SysReg::TPIDRRO_EL0).unwrap(), 42);
    }
    #[test]
    fn host_interruptions_and_publication_preserve_exclusive_epoch() {
        let mut m = machine();
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        unsafe { m.page.set_word(EXCLUSIVE_EPOCH_OFFSET, 41).unwrap() };
        m.backend.set_sys_reg(SysReg::TPIDRRO_EL0, 41).unwrap();
        m.backend
            .stops
            .extend([RunStop::Interrupted, RunStop::Interrupted]);
        assert!(m
            .run_until(10, || {})
            .unwrap_err()
            .contains("unexpected guest entry"));
        m.publish().unwrap();
        assert_eq!(unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() }, 41);
        assert_eq!(m.backend.get_sys_reg(SysReg::TPIDRRO_EL0).unwrap(), 41);
    }
    #[test]
    fn delivery_invalidates_exclusive_epoch_and_preserves_el1_scratch() {
        let mut m = machine();
        m.backend.set_reg(CoreReg::Pstate, 0).unwrap();
        // A ready receive queue with no available descriptors accepts a frame
        // into the network backlog without requiring an IRQ or guest entry.
        unsafe {
            m.ram.with_bytes_mut(|ram| {
                let net = &mut m.application.as_mut().unwrap().net;
                for (offset, value) in [
                    (0x30, 0),
                    (0x38, 8),
                    (0x80, RAM_BASE),
                    (0x90, RAM_BASE + 0x1000),
                    (0xa0, RAM_BASE + 0x1100),
                    (0x44, 1),
                ] {
                    net.write(ram, offset, 4, value, 0).unwrap();
                }
            });
        }
        assert!(m.receive_frame(b"frame").unwrap());
        assert_eq!(unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() }, 1);
        assert_eq!(m.backend.get_sys_reg(SysReg::TPIDRRO_EL0).unwrap(), 1);
        m.receive_control(b"control").unwrap();
        assert_eq!(unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() }, 2);
        assert_eq!(m.backend.get_sys_reg(SysReg::TPIDRRO_EL0).unwrap(), 2);
        m.backend.set_reg(CoreReg::Pstate, 0x3c5).unwrap();
        m.backend.set_sys_reg(SysReg::TPIDRRO_EL0, 0xfeed).unwrap();
        unsafe { m.page.set_word(EXCLUSIVE_EPOCH_OFFSET, 100).unwrap() };
        m.receive_control(b"more").unwrap();
        assert_eq!(unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() }, 101);
        assert_eq!(m.backend.get_sys_reg(SysReg::TPIDRRO_EL0).unwrap(), 0xfeed);
    }
    #[test]
    fn exclusive_epoch_overflow_does_not_wrap() {
        let mut m = machine();
        unsafe { m.page.set_word(EXCLUSIVE_EPOCH_OFFSET, u64::MAX).unwrap() };
        assert!(m
            .invalidate_exclusive_reservation()
            .unwrap_err()
            .contains("epoch overflow"));
        assert_eq!(
            unsafe { m.page.word(EXCLUSIVE_EPOCH_OFFSET).unwrap() },
            u64::MAX
        );
    }
    #[test]
    fn capture_requires_completed_write_and_is_excluded_from_state_evidence() {
        let mut m = machine();
        m.ready = true;
        let write = backend::Stop {
            pc: RAM_BASE,
            exit: Exit::Mmio {
                addr: UART_ADDR,
                len: 1,
                write: true,
                data: b'a' as u64,
                reg: None,
            },
        };
        m.backend.stops.push_back(write.clone().into());
        m.run_slice(10, 0, true).unwrap();
        let before = m.application_evidence(true).unwrap();
        assert_eq!(m.take_log_chunks()[0].length, 1);
        assert_eq!(m.application_evidence(true).unwrap(), before);
        m.backend.stops.push_back(write.into());
        m.backend.fail_completion = true;
        assert!(m
            .run_slice(10, 0, true)
            .unwrap_err()
            .contains("completion failure"));
        assert!(m.take_log_chunks().is_empty());
    }

    #[test]
    fn observed_console_stops_on_first_committed_byte_before_next_entry() {
        let mut m = machine();
        m.ready = true;
        for byte in *b"ab" {
            m.backend.stops.push_back(
                backend::Stop {
                    pc: RAM_BASE,
                    exit: Exit::Mmio {
                        addr: UART_ADDR,
                        len: 1,
                        write: true,
                        data: byte as u64,
                        reg: None,
                    },
                }
                .into(),
            );
        }
        assert_eq!(
            m.run_slice(10, 9, true).unwrap(),
            MachineStop::Console { time_ns: 0 }
        );
        assert_eq!(m.console(), b"a");
        assert_eq!(m.backend.stops.len(), 1);
        assert_eq!(m.stopped_observation().exits, 1);
        assert_eq!(
            m.run_slice(10, 0, true).unwrap(),
            MachineStop::Console { time_ns: 0 }
        );
        assert_eq!(m.console(), b"ab");
    }
    #[test]
    fn stopped_observation_keeps_timer_deadline_beyond_exact_horizon() {
        let mut m = machine();
        m.ready = true;
        m.clock.arm(30).unwrap();
        m.backend.stops.push_back(
            backend::Stop {
                pc: RAM_BASE,
                exit: Exit::Wfi,
            }
            .into(),
        );
        assert_eq!(
            m.run_slice(20, 19, false).unwrap(),
            MachineStop::Horizon { time_ns: 20 }
        );
        let observation = m.stopped_observation();
        assert_eq!((observation.time_ns, observation.next_deadline), (20, 30));
        assert!(!observation.pending_irq);
        assert_eq!(observation.owed_io, None);
        m.park().unwrap();
        m.resume_at(30).unwrap();
        assert!(m.stopped_observation().pending_irq);
        assert_eq!(m.stopped_observation().next_deadline, u64::MAX);
    }
    #[test]
    fn trapped_wfe_is_skipped_without_idling() {
        let mut m = machine();
        m.ready = true;
        m.clock.arm(30).unwrap();
        m.backend.stops.extend([
            backend::Stop {
                pc: RAM_BASE,
                exit: Exit::Wfe,
            }
            .into(),
            backend::Stop {
                pc: RAM_BASE + 4,
                exit: Exit::Wfi,
            }
            .into(),
        ]);
        // WFE leaves time at zero and continues; only the WFI idles to the horizon.
        assert_eq!(
            m.run_slice(20, 0, false).unwrap(),
            MachineStop::Horizon { time_ns: 20 }
        );
        assert_eq!(m.trace.len(), 2);
        assert_eq!(
            (m.trace[0].kind, m.trace[0].time, m.trace[0].pc),
            (5, 0, RAM_BASE)
        );
        assert_eq!((m.trace[1].kind, m.trace[1].time), (3, 20));
        assert!(m.backend.injected.is_empty());
        assert_eq!(m.stopped_observation().next_deadline, 30);
    }
    #[test]
    fn disk_install_rejects_entered_guest_before_consuming_input() {
        let mut m = machine();
        m.raw_exits = 1;
        let mut input = b"unread".as_slice();
        assert!(m
            .install_crash_disk(&mut input, None)
            .unwrap_err()
            .contains("already entered"));
        assert_eq!(input, b"unread");
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
