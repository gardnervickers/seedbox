//! Execution-profile parameters recorded in a run's identity.

use super::*;

pub fn recorded_checkpoint(profile: &ExecutionProfile) -> Result<Option<(u64, u64)>> {
    let parameters = &profile.semantic_parameters;
    match (
        parameters.get("checkpoint_at_ns"),
        parameters.get("checkpoint_branch_seed"),
    ) {
        (None, None) => Ok(None),
        (Some(at), Some(seed)) => {
            let invalid = || Error::Integrity("invalid checkpoint semantic parameters".into());
            let t: u64 = at.parse().map_err(|_| invalid())?;
            let b: u64 = seed.parse().map_err(|_| invalid())?;
            if at != &t.to_string() || seed != &b.to_string() {
                return Err(invalid());
            }
            Ok(Some((t, b)))
        }
        _ => Err(Error::Integrity(
            "incomplete checkpoint semantic parameters".into(),
        )),
    }
}

pub(super) const COUNTER: &str = "counter";
pub(super) const DISK_LOSS: &str = "disk_loss";
pub(super) const DISK_LOSS_SECTOR_BYTES: &str = "disk_loss_sector_bytes";
pub(super) const DISK_LOSS_SURVIVAL_PROBABILITY: &str = "disk_loss_survival_probability";

/// Recover the disk policy from a retained profile, rejecting unknown semantics.
pub fn recorded_torn_writes(profile: &ExecutionProfile) -> Result<bool> {
    if profile.semantic_parameters.contains_key(DISK_LOSS) {
        return Err(Error::Integrity(
            "legacy disk_loss semantic parameter is not replay-compatible; use the retained previous CLI for historical evidence".into(),
        ));
    }
    let sector_bytes = profile.semantic_parameters.get(DISK_LOSS_SECTOR_BYTES);
    let survival_probability = profile
        .semantic_parameters
        .get(DISK_LOSS_SURVIVAL_PROBABILITY);
    if sector_bytes.is_none() && survival_probability.is_none() {
        return Ok(false);
    }
    match (sector_bytes, survival_probability) {
        (Some(sector_bytes), Some(survival_probability))
            if sector_bytes == "512" && survival_probability == "0.5" =>
        {
            Ok(true)
        }
        _ => Err(Error::Integrity("unknown disk loss parameters".into())),
    }
}

/// The branch counter profile a recorded run was made with.
pub fn recorded_counter(profile: &ExecutionProfile) -> Result<CounterProfile> {
    match profile.semantic_parameters.get(COUNTER) {
        None => Ok(CounterProfile::Zen),
        Some(name) => CounterProfile::from_name(name)
            .ok_or_else(|| Error::Integrity(format!("unknown counter profile {name}"))),
    }
}

pub fn recorded_page_timer(profile: &ExecutionProfile) -> bool {
    profile
        .semantic_parameters
        .get("page_timer")
        .is_some_and(|v| v == "true")
}

/// Older engines and profiles service TX without deferring kicks.
pub fn recorded_deferred_tx(profile: &ExecutionProfile) -> Result<bool> {
    match profile.semantic_parameters.get("deferred_tx").map(String::as_str) {
        None => Ok(false),
        Some("true") => Ok(true),
        Some(value) => Err(Error::Integrity(format!(
            "recorded --deferred-tx {value}: expected true or an absent parameter; use a run recorded with a supported TX mode"
        ))),
    }
}

/// Missing timebase means counter, including records from older engines.
pub fn recorded_timebase(profile: &ExecutionProfile) -> Result<Timebase> {
    match profile.semantic_parameters.get("timebase").map(String::as_str) {
        None | Some("counter") => Ok(Timebase::Counter),
        Some("pv") => Ok(Timebase::Pv),
        Some(value) => Err(Error::Integrity(format!(
            "recorded --timebase {value}: expected counter or pv; use a run recorded with a supported timebase"
        ))),
    }
}
