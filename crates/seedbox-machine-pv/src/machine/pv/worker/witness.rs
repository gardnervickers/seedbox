//! Native evidence families and where each is recorded:
//! I: x86::Machine::prepare after successful Native::inject(vector); ARM irq/
//! interrupts after successful previously-unasserted Native::irq(vector,true).
//! O/i/w/r: Machine::run -> record_exit after Interrupted/maintenance exclusions
//! and accepted LAST; writes include value, reads use zero. H: Halt/Wait.
//! d: Block::write_traced -> process callback AFTER each successful used-ring
//! completion, preserving partial-prefix errors and actual completion order.
//! N: fresh Boot dispatch after arch::load/reset/adopt, never restore.
//! S: PV boot entropy-generation installation (arch::load wrote the boot seed);
//! generation is physical boot ordinal. No runtime page-reseed API exists.
//! C: fresh Boot after predecessor successor() fold, for forced marks1/2 only.
//! Y: semantic soft-bound yield after its actual MMIO or port write. Console bytes are O or PL011 w.
//! X/E: this Linux PV board maps no 0xf4/0xf5 debug-exit/init-error devices:
//! those ports float/ignore, and are ordinary O/i, not terminal requests.
//! Native outcomes use PV power-off MMIO, PSCI or architectural reset.
//! R/B: this Machine ABI has no release/parked-launch input or barrier port:
//! fresh boot begins at caller's now; READY is kernel ABI, not a launch
//! barrier. P: no native fork-park API; snapshot must be
//! an observation, never a synthetic P. Z/U: no native pause/resume API; Driver
//! gating is separate logical-world witness, app freezing is actual guest IO.
//! These absence reasons describe the current native contract, not a claim
//! that unfinished service launch/fork behavior has been implemented.
//! New PV-specific families: G architectural reset (size/a/b zero); t/v
//! trapped system-register read/write (size8,a=register,b=write value else0);
//! k hypercall (size0,a=function in x0,b0); j actual IRQ deassert(size0,a=vector,b0).
//! d completion packs full kind:u32 in b low32 and status:u8 in bits32..39;
//! a=sector,size0, ordering is actual used-ring completion order.
//! q: consumed x86 shared PIC or virtio ACK publication (size8,a=page offset,b=observed word).
//! Native interaction records only; host landings and inspection are invisible.
use super::*;
use seedbox_machine::machine::execution_evidence::chain;
impl<B: Backend> Machine<B> {
    pub(super) fn native_execution(&self) -> Result<protocol::NativeExecution> {
        let native = self.native.canonical_pending_io()?;
        // Only actual ordered untaken native guest IO, not the worker snapshot
        // or Driver/report events. GuestWrite has byte vectors and ordered
        // scalar/Vec payloads; opaque lifecycle bytes are never normalized.
        let writes =
            serde_json::to_vec(&self.state.writes).map_err(|e| format!("PV pending IO: {e}"))?;
        let mut pending = b"pv-pending-io-v1\0".to_vec();
        pending.extend_from_slice(&(native.len() as u64).to_le_bytes());
        pending.extend_from_slice(&native);
        pending.extend_from_slice(&(writes.len() as u64).to_le_bytes());
        pending.extend_from_slice(&writes);
        Ok(protocol::NativeExecution {
            chain: self.state.witness,
            pending,
        })
    }
    pub(super) fn record_native(&mut self, kind: u8, size: u8, a: u64, b: u64) {
        self.state.witness = chain::record(&self.state.witness, kind, size, self.state.now, a, b);
    }
    /// The run loop calls this only after excluding Interrupted and backend
    /// maintenance, and after accepting the actual guest's LAST publication.
    pub(super) fn record_exit(&mut self, exit: &Exit) -> Result<()> {
        match *exit {
            Exit::Mmio {
                address,
                width,
                write,
                value,
            } => self.record_native(
                if write { b'w' } else { b'r' },
                width,
                address,
                if write { value } else { 0 },
            ),
            #[cfg(target_arch = "x86_64")]
            Exit::Port {
                port,
                width,
                write,
                value,
            } => self.record_native(
                if write { b'O' } else { b'i' },
                width,
                port.into(),
                if write { value } else { 0 },
            ),
            #[cfg(target_arch = "x86_64")]
            Exit::Halt { .. } => self.record_native(b'H', 0, 0, 0),
            #[cfg(all(target_arch = "aarch64", any(target_os = "macos", test)))]
            Exit::Wait { .. } => self.record_native(b'H', 0, 0, 0),
            #[cfg(target_arch = "x86_64")]
            Exit::Shutdown => self.record_native(b'G', 0, 0, 0),
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            Exit::System {
                register,
                write,
                value,
            } => self.record_native(
                if write { b'v' } else { b't' },
                8,
                register.into(),
                if write { value } else { 0 },
            ),
            #[cfg(target_arch = "aarch64")]
            Exit::Call => {
                let function = self.native.core(0)?;
                self.record_native(b'k', 0, function, 0);
            }
            // Interrupt-window openings are native delivery mechanics, not an
            // extra guest IO record. Actual injection is recorded in prepare.
            _ => {}
        }
        Ok(())
    }
}
