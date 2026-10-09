//! Direct KVM primitives for the shared world; no simulation loop lives here.
use crate::machine::{ExitKind, Machine, MachineSnapshot};
use crate::{boot_machine, BootInputs, Clock};
use seedbox_engine_core::{
    rng::Rng,
    world::{Reseed, SliceRequest, Stop, WorldMachine},
};
impl WorldMachine for Machine {
    type Factory = kvm_ioctls::Kvm;
    type Boot = BootInputs;
    type Clock = Clock;
    type Snapshot = MachineSnapshot;
    fn boot(
        factory: &Self::Factory,
        name: &str,
        boot: &Self::Boot,
        barrier: bool,
        entropy: [u8; 32],
    ) -> Result<Self, String> {
        boot_machine(factory, name, boot, barrier, entropy)
    }
    fn has_pending_irq(&self) -> bool {
        Machine::has_pending_irq(self)
    }
    fn next_deadline(&self) -> u64 {
        Machine::next_deadline(self)
    }
    fn owed_io(&self) -> Option<u64> {
        self.pending_io.as_ref().map(|p| p.remaining_ns)
    }
    fn has_control(&self) -> bool {
        self.channel.is_some()
    }
    fn deliver_control(&mut self, bytes: &[u8]) -> Result<(), String> {
        Machine::deliver_control(self, bytes)
    }
    fn deliver_serial(&mut self, bytes: &[u8]) -> Result<(), String> {
        Machine::deliver_serial(self, bytes);
        Ok(())
    }
    fn deliver_frame(&mut self, bytes: &[u8]) -> Result<bool, String> {
        Ok(Machine::deliver_frame(self, bytes)?.accepted)
    }
    fn pause(&mut self) -> Result<(), String> {
        Machine::pause(self)
    }
    fn resume(&mut self, at: u64) -> Result<(), String> {
        Machine::resume(self, at)
    }
    fn boot_at(&mut self, at: u64) -> Result<(), String> {
        Machine::boot_at(self, at)
    }
    fn succeed(&mut self, old: &mut Self, at: u64, seed: u64) -> Result<Option<u64>, String> {
        Machine::succeed(self, old, at, seed, None)
    }
    fn succeed_branch(
        &mut self,
        old: &mut Self,
        at: u64,
        seed: u64,
        identity: Option<&str>,
    ) -> Result<Option<u64>, String> {
        Machine::succeed(self, old, at, seed, identity)
    }
    fn crash_observations(&self) -> serde_json::Value {
        serde_json::json!({"pio_total":self.stats.kvm_n[ExitKind::Pio as usize],"net":self.net.as_ref().map(|n|n.report())})
    }
    /// Clocks hold no checkpoint state: PV probes are per machine, and
    /// `Machine::snapshot` refuses one inside a probe.
    fn checkpoint_boundary(_: &Self::Clock) -> Result<(), String> {
        Ok(())
    }
    fn restore_boundary(clock: &mut Self::Clock) -> Result<(), String> {
        if let Clock::Pv(clock) = clock {
            clock.restore_boundary()?;
        }
        Ok(())
    }
    fn reseed_guest(&mut self, entropy: [u32; 8]) -> Result<(), String> {
        self.reseed_time_page(entropy)
    }
    fn park(&mut self) -> Result<(), String> {
        Machine::park(self)
    }
    fn wake(&mut self, at: u64) -> Result<(), String> {
        Machine::wake(self, at);
        Ok(())
    }
    fn release(&mut self, at: u64, entropy: [u32; 8]) -> Result<Reseed, String> {
        Machine::release(self, at, entropy)
    }
    fn abi(&self) -> u64 {
        Machine::abi(self)
    }
    fn seed_timer(&mut self, seed: u64, domain: &str) -> Result<(), String> {
        self.timer_rng = Some(Rng::new(seed, domain));
        Ok(())
    }
    fn seed_timer_branch(
        &mut self,
        seed: u64,
        identity: Option<&str>,
        domain: &str,
    ) -> Result<(), String> {
        self.timer_rng = Some(Rng::branch(seed, identity, domain));
        Ok(())
    }
    fn supports_speed_reseed(&self) -> bool {
        true
    }
    fn seed_speed(&mut self, seed: u64, domain: &str) -> Result<(), String> {
        self.syscall_ns = crate::machine::pv::syscall_ns(&mut Rng::new(seed, domain));
        Ok(())
    }
    fn supports_throttle(&self) -> bool {
        self.pv_clock
    }
    fn throttle(&mut self, factor_permille: u64) -> Result<(), String> {
        if !self.pv_clock {
            return Err("throttle needs the pv timebase".into());
        }
        self.throttle_permille = Some(factor_permille);
        Ok(())
    }
    fn branches(&self) -> u64 {
        self.branches
    }
    fn exits(&self) -> u64 {
        self.stats.exits
    }
    fn exit_witness(&mut self) -> Result<String, String> {
        Ok(Machine::exit_witness(self))
    }
    fn terminate_worker(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, String> {
        Machine::snapshot(self)
    }
    fn restore(&mut self, snapshot: &Self::Snapshot) -> Result<u64, String> {
        Machine::restore(self, snapshot)
    }
    fn advance(
        &mut self,
        clock: &mut Self::Clock,
        request: SliceRequest,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        match clock {
            Clock::Counter(c) => self.run_until(c, request.hard_limit, request.early_slack, frames),
            Clock::Pv(p) => self.run_until(p, request.hard_limit, request.early_slack, frames),
        }
    }
}
