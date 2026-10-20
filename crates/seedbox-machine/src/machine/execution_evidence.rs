//! Lightweight native interaction witness, independent of optional RAM capture.
//!
//! Framing (comparable only within one backend):
//! `h0=SHA256(b"seedbox-execution-witness-v1\0")`;
//! record `SHA256(h || b"rec\0" || kind:u8 || size:u8 || time:u64LE ||
//! a:u64LE || b:u64LE)`; observation `SHA256(h || b"read\0" || len:u64LE ||
//! canonical_pending_io)`, never folded into h. Successor is
//! `SHA256(h0 || b"boot\0" || old_witness32 || mark:u8 || time:u64LE)`.
//! Persist h and canonical pending state, not a live hasher. Getters never
//! execute, settle I/O, drain, adopt time, draw RNG or change durability.
//!
//! Native family contract / backend hook accounting (implementation followups
//! must wire these sites; this API leaf alone does not claim backend parity):
//!
//! | kind | size / a / b | PV native site | x86 TCG native site |
//! |---|---|---|---|
//! | I | 0 / vector / 0 | actual Backend::inject/irq delivery | x86 actual interrupt vector delivery |
//! | J | 0 / fixed kind / vector | TCG-only native extension | actual SMI=1, INIT=2, SIPI=3 delivery; b is delivered vector |
//! | G | 0 / fixed cause / 0 | TCG-only native extension | actual guest triple fault=1 or ACPI GED guest reset=2; no host reset |
//! | O | width / port / write value | worker Port write | guest port write dispatch |
//! | i | width / port / 0 | worker Port read | guest port read dispatch |
//! | w | width / GPA / write value | worker MMIO write | guest MMIO write dispatch |
//! | r | width / GPA / 0 | worker MMIO read | guest MMIO read dispatch |
//! | d | 0 / sector / full kind:u32 low32, status:u8 bits32..39 | block completion in completion order | Seedbox block completion order |
//! | H | 0 / 0 / 0 | actual Halt/Wait | actual guest halt |
//! | X | width / 0xf4 / code | no debug-exit device in Linux PV board; floating/ignored ports are ordinary O/i | only if debug-exit device exists; production nodefault board has none |
//! | E | width / 0xf5 / code | no init-error port; supervisor errors remain protocol errors | production board has no init-error device |
//! | N | 0 / physical boot ordinal / 0 | physical boot, never restore | physical boot, never migration restore |
//! | R | 0 / 0 / 0 | absent: current Machine contract has no release input | absent: current Machine contract has no release input |
//! | S | 0 / entropy generation / 0 | actual entropy-generation change | none: production qemu64 board has no guest entropy delivery/reseed device |
//! | B | 0 / 0 / 0 | only explicit initial launch barrier | production TCG has no initial launch parking |
//! | P | 0 / 0 / 0 | no native fork-park operation in Machine contract | no native fork-park operation; snapshot is observation |
//! | Z,U | 0 / 0 / 0 | only native machine pause/resume if supported; app freeze is guest IO | no native machine pause inputs; app freeze is guest IO |
//! | Y | 0 / soft bound / 0 | actual PV soft-bound rendezvous | no PV soft-bound ABI in TCG |
//! | C | 0 / 0 / 0 | forced replacement after predecessor fold | forced replacement after predecessor fold |
//!
//! Console is witnessed by O/native console MMIO; network in-flight policy is
//! a separate Driver component. Guest TX/RX device effects remain actual native
//! I/O/completion records. Host interruption/watchdog, QMP run chunks, TB/cache
//! maintenance, source/build bytes and Driver/report Event vectors are NOT
//! native records. J/G are actual delivered guest/native events, never asserted
//! lines, QMP host reset commands, migration, or startup configuration. Fixed
//! codes are retained semantics, not QEMU enum values or bit masks. Unused
//! fields are zero; b for reads is always zero.
//!
//! Marks: 1 accepted Crash predecessor, 2 live forced Reboot predecessor,
//! 3 naturally ended predecessor then Reboot. Preserve retirement cause in the
//! backend codec; never reclassify the cached ended disposition. The backend
//! folds old observation before loss/close and starts a successor only at a
//! real new physical boot. Restoring h/pending/cache never appends a record.
use super::EvidenceDisposition;
use sha2::{Digest, Sha256};

/// Actual native execution evidence at a stopped boundary, even with full-state
/// diagnostics disabled. It contains no RAM regions, page hashes or snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionEvidence {
    /// Actual live or last actual ended physical owner, never a successor label.
    pub boot: u64,
    /// Captured natural-end disposition remains unchanged by later teardown.
    pub disposition: EvidenceDisposition,
    /// Canonical native interaction-chain observation (including pending I/O).
    pub witness: String,
    /// CURRENT attached disk's surviving/live bytes and selection digest.
    /// After loss this is post-loss, independently of cached pre-loss native data.
    pub disk: Option<String>,
}
impl ExecutionEvidence {
    /// Validate the specific SHA representation; this does not authenticate
    /// imported native state or certify backend capture semantics.
    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !valid(&self.witness) || self.disk.as_deref().is_some_and(|d| !valid(d)) {
            return Err("execution evidence: invalid SHA256");
        }
        Ok(())
    }
}
/// Canonical constant-space native witness mechanics shared by Rust backends.
/// QEMU's C implementation must pass these same golden vectors.
pub mod chain {
    use super::*;
    /// A native successor following an accepted Crash (RAM was destroyed).
    pub const CRASH: u8 = 1;
    /// Forced teardown of a live physical boot by Reboot.
    pub const REBOOT: u8 = 2;
    /// Reboot after an already natural terminal physical boot.
    pub const NATURAL_REBOOT: u8 = 3;
    /// Initial native chain; configuration does not supply execution records.
    pub fn initial() -> [u8; 32] {
        Sha256::digest(b"seedbox-execution-witness-v1\0").into()
    }
    /// Append exactly one canonical actual-native record.
    pub fn record(h: &[u8; 32], kind: u8, size: u8, time: u64, a: u64, b: u64) -> [u8; 32] {
        let mut s = Sha256::new();
        s.update(h);
        s.update(b"rec\0");
        s.update([kind, size]);
        s.update(time.to_le_bytes());
        s.update(a.to_le_bytes());
        s.update(b.to_le_bytes());
        s.finalize().into()
    }
    /// Read-only observation: canonical pending bytes are never folded into h.
    pub fn observe(h: &[u8; 32], pending: &[u8]) -> [u8; 32] {
        let mut s = Sha256::new();
        s.update(h);
        s.update(b"read\0");
        s.update((pending.len() as u64).to_le_bytes());
        s.update(pending);
        s.finalize().into()
    }
    /// Establish a real successor from its predecessor's pre-loss observation.
    pub fn successor(old: &[u8; 32], mark: u8, time: u64) -> Result<[u8; 32], &'static str> {
        if !matches!(mark, CRASH | REBOOT | NATURAL_REBOOT) {
            return Err("execution witness: invalid successor mark");
        }
        let mut s = Sha256::new();
        s.update(initial());
        s.update(b"boot\0");
        s.update(old);
        s.update([mark]);
        s.update(time.to_le_bytes());
        Ok(s.finalize().into())
    }
}
#[cfg(test)]
mod tests;
