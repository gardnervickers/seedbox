//! Owned stopped-machine state. Native handles never enter a snapshot; restore
//! consumes a fresh machine so any partial failure destroys the replacement VM.
use super::*;
use std::sync::Arc;
mod codec;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    cpu: backend::BackendState,
    #[serde(skip)]
    ram: Arc<Vec<u8>>,
    page: Vec<u8>,
    uart: Pl011,
    uart_irq: bool,
    deferred_spis: BTreeSet<u32>,
    asserted_spis: BTreeSet<u32>,
    deferred_spi_events: u64,
    deferred_clock_read_spis: u64,
    clock: PvClock,
    ready: bool,
    trace: Vec<Record>,
    seed: u64,
    kernel_hash: String,
    initrd_hash: String,
    raw_exits: u64,
    powered_off: bool,
    #[serde(skip)]
    application: Option<application::Devices>,
    boot_identity: application::BootIdentity,
    timer_policy: Option<(application::TimerPolicy, crate::rng::Rng)>,
    log_capture: application::log_capture::Pending,
}

fn copy_memory(memory: &Memory) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(memory.len())
        .map_err(|e| format!("checkpoint memory allocation: {e}"))?;
    // Machine owns the backend and no guest entry can overlap this borrow.
    bytes.extend_from_slice(unsafe { memory.bytes() });
    Ok(bytes)
}

impl<B: Backend> Machine<B> {
    /// Capture only at a completed semantic stop. Pending native exit state is
    /// rejected by the backend before copying RAM or device policy.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let cpu = self.backend.snapshot_state()?;
        Ok(Snapshot {
            cpu,
            ram: Arc::new(copy_memory(&self.ram)?),
            page: copy_memory(&self.page)?,
            uart: self.uart.clone(),
            uart_irq: self.uart_irq,
            deferred_spis: self.deferred_spis.clone(),
            asserted_spis: self.asserted_spis.clone(),
            deferred_spi_events: self.deferred_spi_events,
            deferred_clock_read_spis: self.deferred_clock_read_spis,
            clock: self.clock.clone(),
            ready: self.ready,
            trace: self.trace.clone(),
            seed: self.seed,
            kernel_hash: self.kernel_hash.clone(),
            initrd_hash: self.initrd_hash.clone(),
            raw_exits: self.raw_exits,
            powered_off: self.powered_off,
            application: self.application.clone(),
            boot_identity: self.boot_identity.clone(),
            timer_policy: self.timer_policy.clone(),
            log_capture: self.log_capture.clone(),
        })
    }

    /// The original VM must already be destroyed on HVF. The replacement has
    /// identical boot inputs and mappings, and has never entered guest code.
    pub fn restore_snapshot(self, snapshot: &Snapshot) -> Result<Self> {
        if self.ram.len() != snapshot.ram.len() {
            return Err("checkpoint does not match replacement machine boot inputs".into());
        }
        self.restore_with(snapshot, true)
    }

    /// As `restore_snapshot` with guest RAM already in place (mapped from
    /// a checkpoint file).
    pub(crate) fn restore_snapshot_without_ram(self, snapshot: &Snapshot) -> Result<Self> {
        self.restore_with(snapshot, false)
    }

    /// Back guest RAM with a private mapping of `file` from `offset`,
    /// unmapping it from the backend and mapping it back around the change.
    pub(crate) fn map_ram_from(&mut self, file: &std::fs::File, offset: u64) -> Result<()> {
        self.backend.unmap_memory(0)?;
        // The vCPU has never entered and the backend no longer maps the RAM.
        unsafe { self.ram.map_file(file, offset)? };
        self.backend.map_memory(0, RAM_BASE, Rc::clone(&self.ram))
    }

    fn restore_with(mut self, snapshot: &Snapshot, copy_ram: bool) -> Result<Self> {
        if !self.boot_identity.same_inputs(&snapshot.boot_identity)
            || self.kernel_hash != snapshot.kernel_hash
            || self.initrd_hash != snapshot.initrd_hash
            || self.page.len() != snapshot.page.len()
            || self.application.is_some() != snapshot.application.is_some()
        {
            return Err("checkpoint does not match replacement machine boot inputs".into());
        }
        // Native restoration validates the fresh-VM requirement and architectural
        // inventory first. Any subsequent error consumes and drops this machine.
        self.backend.restore_state(&snapshot.cpu)?;
        unsafe {
            if copy_ram {
                self.ram.write(0, &snapshot.ram)?;
            }
            self.page.write(0, &snapshot.page)?;
        }
        self.uart = snapshot.uart.clone();
        self.uart_irq = snapshot.uart_irq;
        self.deferred_spis = snapshot.deferred_spis.clone();
        self.asserted_spis = snapshot.asserted_spis.clone();
        self.deferred_spi_events = snapshot.deferred_spi_events;
        self.deferred_clock_read_spis = snapshot.deferred_clock_read_spis;
        self.clock = snapshot.clock.clone();
        self.ready = snapshot.ready;
        self.trace = snapshot.trace.clone();
        self.seed = snapshot.seed;
        self.boot_identity = snapshot.boot_identity.clone();
        self.raw_exits = snapshot.raw_exits;
        self.powered_off = snapshot.powered_off;
        self.application = snapshot.application.clone();
        self.timer_policy = snapshot.timer_policy.clone();
        self.log_capture = snapshot.log_capture.clone();
        Ok(self)
    }
}
