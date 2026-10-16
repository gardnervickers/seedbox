//! Commit-time acceptance of property frames. The coordinator calls this in
//! its fixed absorb order; the reducer itself lives in the protocol crate so
//! offline reporting applies identical semantics.
use seedbox_engine_protocol::property::{decode, Ledger};

/// Reduce one committed `SBX_PROP1` line; `None` is an oversized line.
pub fn commit(
    ledger: &mut Ledger,
    machine: &str,
    incarnation: u32,
    at: u64,
    line: Option<Vec<u8>>,
) {
    let Some(line) = line else {
        return ledger.malformed_frame(machine, incarnation, at, "frame too large");
    };
    match decode(&line) {
        Ok(Some(frame)) => ledger.apply(&frame, machine, incarnation, at),
        Ok(None) => ledger.malformed_frame(machine, incarnation, at, "not a property frame"),
        Err(error) => ledger.malformed_frame(machine, incarnation, at, error),
    }
}
