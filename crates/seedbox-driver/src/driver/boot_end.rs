//! Boot-end payload identity is separate from the generic output carrier.
#[cfg(test)]
mod driver_tests;
use super::*;
use crate::pv::Output;
use seedbox_machine::machine::EvidenceDisposition;

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn shape(response: &Response) -> Result<Option<u64>, &'static str> {
    let Response::BootEnded {
        ended_boot,
        disposition,
        native_booted,
        disk_lost_sectors,
        surviving_disk,
        retired_state_digest,
    } = response
    else {
        return Ok(None);
    };
    if *disposition == EvidenceDisposition::Live {
        return Err("boot-end disposition is live");
    }
    if !native_booted
        && (retired_state_digest.is_some() || *disposition != EvidenceDisposition::Crashed)
    {
        return Err("never-booted retirement cannot claim RAM or natural termination");
    }
    if surviving_disk.is_none() && *disk_lost_sectors != 0 {
        return Err("lost sectors without surviving disk evidence");
    }
    if surviving_disk
        .iter()
        .chain(retired_state_digest)
        .any(|hash| !digest(hash))
    {
        return Err("boot-end component is not lowercase SHA-256");
    }
    Ok(Some(*ended_boot))
}

fn ended(reason: ExitReason) -> EvidenceDisposition {
    match reason {
        ExitReason::Shutdown => EvidenceDisposition::Shutdown,
        ExitReason::Crashed => EvidenceDisposition::Crashed,
        ExitReason::ProgramExit { code } => EvidenceDisposition::Exited { code },
    }
}

/// Validate on a local cursor before any accepted batch state is mutated.
/// The returned cursor covers accepted staged reports as well as published ones.
pub(super) fn validate(
    outputs: &[Output],
    committed_boot: u64,
    last: Option<u64>,
    previous: StopReason,
    stopped: StopReason,
    admitted_end: bool,
) -> Result<Option<u64>, &'static str> {
    let mut next = last;
    for output in outputs {
        let Some(boot) = shape(&output.payload)? else {
            continue;
        };
        if boot != committed_boot {
            return Err("ended boot differs from committed lifecycle ordinal");
        }
        if next.is_some_and(|previous| boot <= previous) {
            return Err("boot end was already reported");
        }
        if !admitted_end && !matches!(stopped, StopReason::Exited { .. }) {
            return Err("boot-end report without an exited stop or accepted crash/reboot");
        }
        let Response::BootEnded { disposition, .. } = &output.payload else {
            unreachable!()
        };
        let expected = if admitted_end {
            // An already-exited slot may still retain its first pending report.
            // Never reclassify that natural exit as a later forced teardown.
            match previous {
                StopReason::Exited { reason, .. } => ended(reason),
                _ => EvidenceDisposition::Crashed,
            }
        } else {
            match stopped {
                StopReason::Exited { reason, .. } => ended(reason),
                _ => unreachable!(),
            }
        };
        if *disposition != expected {
            return Err("boot-end disposition differs from the ended lifecycle");
        }
        next = Some(boot);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output(boot: u64) -> Output {
        Output {
            at: Timestamp::from_nanos(0),
            payload: Response::BootEnded {
                ended_boot: boot,
                disposition: EvidenceDisposition::Crashed,
                native_booted: false,
                disk_lost_sectors: 0,
                surviving_disk: None,
                retired_state_digest: None,
            },
        }
    }
    fn crashed() -> StopReason {
        StopReason::Exited {
            reached: Timestamp::from_nanos(0),
            reason: ExitReason::Crashed,
        }
    }
    #[test]
    fn logical_zero_is_valid_and_no_end_or_arbitrary_identity_is_rejected() {
        assert_eq!(
            validate(
                &[output(0)],
                0,
                None,
                StopReason::BoundaryReached,
                crashed(),
                true
            ),
            Ok(Some(0))
        );
        assert!(validate(
            &[output(1)],
            0,
            None,
            StopReason::BoundaryReached,
            crashed(),
            true
        )
        .is_err());
        assert!(validate(
            &[output(0)],
            1,
            None,
            StopReason::BoundaryReached,
            crashed(),
            true
        )
        .is_err());
        assert!(validate(
            &[output(0)],
            0,
            None,
            StopReason::BoundaryReached,
            StopReason::BoundaryReached,
            false
        )
        .is_err());
    }
    #[test]
    fn duplicate_rejection_does_not_change_the_callers_cursor() {
        let last = None;
        assert!(validate(
            &[output(0), output(0)],
            0,
            last,
            StopReason::BoundaryReached,
            crashed(),
            true
        )
        .is_err());
        assert_eq!(last, None);
        assert!(validate(&[output(0)], 0, Some(0), crashed(), crashed(), true).is_err());
    }
    #[test]
    fn never_booted_cannot_claim_retired_ram_or_a_natural_exit() {
        let mut out = output(0);
        if let Response::BootEnded {
            retired_state_digest,
            ..
        } = &mut out.payload
        {
            *retired_state_digest = Some("a".repeat(64));
        }
        assert!(shape(&out.payload).is_err());
        if let Response::BootEnded {
            retired_state_digest,
            disposition,
            ..
        } = &mut out.payload
        {
            *retired_state_digest = None;
            *disposition = EvidenceDisposition::Shutdown;
        }
        assert!(shape(&out.payload).is_err());
    }
}
