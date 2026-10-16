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

/// The SDK occurrence a committed line carries for typed triggers. The
/// supervisor's own records and malformed lines carry none.
pub fn observed(machine: &str, line: &[u8]) -> Option<crate::triggers::Observed> {
    use seedbox_engine_protocol::property::{Record, SUPERVISOR_SESSION};
    let frame = decode(line).ok()??;
    if frame.session == SUPERVISOR_SESSION {
        return None;
    }
    crate::triggers::Observed::from_record(machine, &Record::parse(&frame.payload).ok()?)
}

/// A committed choice request: its session, site and alternatives. Records
/// the ledger refuses (bad alternatives, the supervisor's session) are none.
pub fn choice(line: &[u8]) -> Option<(u32, String, Vec<String>)> {
    use seedbox_engine_protocol::property::{Record, SUPERVISOR_SESSION};
    let frame = decode(line).ok()??;
    if frame.session == SUPERVISOR_SESSION {
        return None;
    }
    match Record::parse(&frame.payload).ok()? {
        Record::Choice { site, alternatives } => Some((frame.session, site, alternatives)),
        _ => None,
    }
}
