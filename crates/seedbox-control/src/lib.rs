//! Version-one language-neutral guest control schema. No transport or scheduler.
pub const PROCESS_EFFECT_TIMEOUT_NS: u64 = 5_000_000_000;
/// Largest wall-clock skew, fixed or seeded, in either direction: one year.
pub const MAX_CLOCK_OFFSET_NS: u64 = 365 * 86_400 * 1_000_000_000;
/// Probabilities are in parts per million: this is certainty.
pub const MAX_PPM: u64 = 1_000_000;
/// Largest total delay a link policy or shape may add to a frame: one hour.
pub const MAX_LINK_DELAY_NS: u64 = 3_600_000_000_000;
/// Largest clock rate error, fixed or seeded, in either direction: 10% (in
/// parts per billion). Linux slews at most about 10% through `adjtimex`.
pub const MAX_CLOCK_RATE_PPB: u64 = 100_000_000;

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, num::NonZeroU32};

/// Opt-in send-time impairments, carried by a healable directed shape layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkFaults {
    pub reorder_ppm: u64,
    pub reorder_delay_ns: u64,
    pub duplicate_ppm: u64,
    pub burst_loss_ppm: u64,
    pub burst_length: u64,
    pub latency_spike_ppm: u64,
    pub latency_spike_ns: u64,
    pub tcp_reset_ppm: u64,
}
impl NetworkFaults {
    pub fn is_off(&self) -> bool {
        self.reorder_ppm == 0
            && self.duplicate_ppm == 0
            && self.burst_loss_ppm == 0
            && self.latency_spike_ppm == 0
            && self.tcp_reset_ppm == 0
    }
    pub fn validate(&self, latency_ns: u64, jitter_ns: u64) -> Result<(), String> {
        for (name, ppm) in [
            ("reorder_ppm", self.reorder_ppm),
            ("duplicate_ppm", self.duplicate_ppm),
            ("burst_loss_ppm", self.burst_loss_ppm),
            ("latency_spike_ppm", self.latency_spike_ppm),
            ("tcp_reset_ppm", self.tcp_reset_ppm),
        ] {
            if ppm > MAX_PPM {
                return Err(format!("{name} exceeds {MAX_PPM}"));
            }
        }
        for (name, ppm, amount) in [
            ("reorder_delay_ns", self.reorder_ppm, self.reorder_delay_ns),
            ("burst_length", self.burst_loss_ppm, self.burst_length),
            (
                "latency_spike_ns",
                self.latency_spike_ppm,
                self.latency_spike_ns,
            ),
        ] {
            if ppm != 0 && amount == 0 {
                return Err(format!("{name} must be positive when its fault is active"));
            }
        }
        let reorder = if self.reorder_ppm == 0 {
            0
        } else {
            self.reorder_delay_ns
        };
        let spike = if self.latency_spike_ppm == 0 {
            0
        } else {
            self.latency_spike_ns
        };
        if latency_ns
            .checked_add(jitter_ns)
            .and_then(|n| n.checked_add(reorder))
            .and_then(|n| n.checked_add(spike))
            .is_none_or(|n| n > MAX_LINK_DELAY_NS)
        {
            return Err("latency, jitter and fault delays exceed one hour".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Service(String),
    Machine(String),
}

/// One direction of an observed IPv4 TCP incarnation. Names are prepared machines.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub from: String,
    pub to: String,
    pub src_addr: std::net::Ipv4Addr,
    pub dst_addr: std::net::Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub incarnation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Now {},
    Inspect {},
    Connections {
        from: Target,
        to: Target,
        #[serde(default)]
        after: u64,
    },
    DropConnection {
        connection: Connection,
    },
    Sleep {
        duration_ns: u64,
    },
    /// Start no generated fault for this long. Teardowns of generated faults
    /// already active still run, so the system can recover.
    Quiet {
        duration_ns: u64,
    },
    Stop {
        target: Target,
    },
    Restart {
        target: Target,
    },
    PauseProcess {
        target: Target,
    },
    ResumeProcess {
        target: Target,
    },
    Pause {
        target: Target,
    },
    Resume {
        target: Target,
    },
    Crash {
        target: Target,
    },
    Reboot {
        target: Target,
    },
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
    Shape {
        from: Vec<Target>,
        to: Vec<Target>,
        latency_ns: u64,
        jitter_ns: u64,
        loss_ppm: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bandwidth_bps: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_bound_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "NetworkFaults::is_off")]
        network_faults: NetworkFaults,
    },
    /// Step one machine's CLOCK_REALTIME by a fixed or seeded signed offset.
    ClockSkew {
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset_ns: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_offset_ns: Option<u64>,
    },
    /// Make one machine's clock run fast or slow by a fixed or seeded rate.
    ClockDrift {
        target: Target,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_ppb: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rate_ppb: Option<u64>,
    },
    /// Run the machine's CPU at `speed_hz` instead of the machine's configured CPU
    /// speed until healed: every charge for CPU work scales by the ratio.
    Throttle {
        target: Target,
        speed_hz: u64,
    },
    Heal {
        handle: String,
    },
    ArmTrigger {
        source: Target,
        marker: Vec<u8>,
        match_timeout_ns: u64,
        fault: TriggerFault,
    },
    CancelTrigger {
        trigger_id: u32,
    },
}
pub const MAX_TRIGGERS: usize = 64;
pub const MAX_TRIGGER_MARKER: usize = 256;

/// A trigger marker holds 1..=[`MAX_TRIGGER_MARKER`] bytes.
pub fn valid_trigger_marker(marker: &[u8]) -> bool {
    !marker.is_empty() && marker.len() <= MAX_TRIGGER_MARKER
}
/// Refuse another trigger once [`MAX_TRIGGERS`] have been armed in a run.
pub fn check_trigger_budget(armed: usize) -> Result<(), String> {
    if armed >= MAX_TRIGGERS {
        return Err(format!("trigger budget exhausted ({MAX_TRIGGERS} per run)"));
    }
    Ok(())
}

/// The initial trigger surface intentionally has only process stop and partition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerFault {
    Stop {
        target: Target,
    },
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
}
impl TriggerFault {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Stop { target } => Operation::Stop {
                target: target.clone(),
            },
            Self::Partition { left, right } => Operation::Partition {
                left: left.clone(),
                right: right.clone(),
            },
        }
    }
}

/// Canonical matcher and scheduled-effect state, retained only when used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRecord {
    pub id: u32,
    pub source: String,
    pub marker: Vec<u8>,
    pub armed_at_ns: u64,
    pub match_deadline_ns: u64,
    pub action: Action,
    pub prefix: usize,
    pub seen: u64,
    pub resets: u64,
    pub matched_at_ns: Option<u64>,
    pub effect_at_ns: Option<u64>,
    pub fault_id: Option<u32>,
    pub cancelled_at_ns: Option<u64>,
    pub cut_off_at_ns: Option<u64>,
    pub status: TriggerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
}

/// Where a trigger is in its life, serialized as its snake_case name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerStatus {
    /// Watching its source's console for the marker.
    Armed,
    /// Cancelled before it matched.
    Cancelled,
    /// The marker was seen; the effect waits for its time.
    Matched,
    /// The match deadline passed without the marker.
    Expired,
    /// The effect was refused at its time.
    Rejected,
    /// The effect was appended as a fault.
    Fired,
    /// The run ended while it was armed or matched.
    CutOff,
}
impl TriggerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Cancelled => "cancelled",
            Self::Matched => "matched",
            Self::Expired => "expired",
            Self::Rejected => "rejected",
            Self::Fired => "fired",
            Self::CutOff => "cut_off",
        }
    }
}
/// Compares with the serialized name, as checks of recorded evidence do.
impl PartialEq<&str> for TriggerStatus {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub id: NonZeroU32,
    #[serde(flatten)]
    pub operation: Operation,
}
/// Parse identity first: malformed identity is a protocol failure; valid identity
/// with invalid operation arguments can receive a correlated operation error.
#[derive(Deserialize)]
pub struct Header {
    pub version: u32,
    pub id: NonZeroU32,
    #[serde(flatten)]
    pub arguments: BTreeMap<String, serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    DropConnection {
        handle: String,
        connection: Connection,
    },
    Stop {
        machine: String,
    },
    Restart {
        machine: String,
    },
    /// SIGSTOP the application's process group; its kernel keeps running.
    PauseProcess {
        machine: String,
    },
    /// SIGCONT a process group frozen by `PauseProcess`.
    ResumeProcess {
        machine: String,
    },
    Partition {
        handle: String,
        left: Vec<String>,
        right: Vec<String>,
    },
    Shape {
        handle: String,
        from: Vec<String>,
        to: Vec<String>,
        latency_ns: u64,
        jitter_ns: u64,
        loss_ppm: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bandwidth_bps: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_bound_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "NetworkFaults::is_off")]
        network_faults: NetworkFaults,
    },
    Heal {
        handle: String,
    },
    /// Step the machine's wall clock (CLOCK_REALTIME) by `offset_ns`, or by an
    /// offset drawn uniformly from [-max_offset_ns, max_offset_ns] on the
    /// machine's `clock/<machine>` stream. Monotonic time is untouched; heal
    /// steps the clock back by the same offset.
    ClockSkew {
        handle: String,
        machine: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset_ns: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_offset_ns: Option<u64>,
    },
    /// Run the machine's system clock `rate_ppb` parts per billion fast (or
    /// slow if negative), or at a rate drawn uniformly from
    /// [-max_rate_ppb, max_rate_ppb] on the machine's `clock/<machine>`
    /// stream. Realtime and monotonic time drift together, as with a real
    /// oscillator error; the raw monotonic clock and the virtual TSC keep the
    /// simulated rate. Heal restores the nominal rate.
    ClockDrift {
        handle: String,
        machine: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_ppb: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rate_ppb: Option<u64>,
    },
    /// Run the machine's CPU at `speed_hz` until the handle is healed.
    Throttle {
        handle: String,
        machine: String,
        speed_hz: u64,
    },
    /// Fail the machine's disk requests of kind `op` with an I/O error, each
    /// with probability `ppm` per million drawn on a seeded stream, until the
    /// handle is healed. A failed write changes nothing on disk and a failed
    /// flush makes nothing durable.
    DiskError {
        handle: String,
        machine: String,
        op: DiskOp,
        #[serde(default = "every_request")]
        ppm: u32,
    },
    /// Freeze the machine's CPU and devices; the world goes on.
    Pause {
        machine: String,
    },
    /// Continue a paused machine, catching up its timers and held ingress.
    Resume {
        machine: String,
    },
    /// Destroy the machine's VM: RAM, devices, queued I/O and sockets.
    Crash {
        machine: String,
    },
    /// Boot a crashed machine again as a new incarnation.
    Reboot {
        machine: String,
    },
}
impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::DropConnection { .. } => "drop_connection",
            Self::Stop { .. } => "stop",
            Self::Restart { .. } => "restart",
            Self::PauseProcess { .. } => "pause_process",
            Self::ResumeProcess { .. } => "resume_process",
            Self::Partition { .. } => "partition",
            Self::Shape { .. } => "shape",
            Self::Heal { .. } => "heal",
            Self::ClockSkew { .. } => "clock_skew",
            Self::ClockDrift { .. } => "clock_drift",
            Self::Throttle { .. } => "throttle",
            Self::DiskError { .. } => "disk_error",
            Self::Pause { .. } => "pause",
            Self::Resume { .. } => "resume",
            Self::Crash { .. } => "crash",
            Self::Reboot { .. } => "reboot",
        }
    }
    pub fn machine(&self) -> Option<&str> {
        match self {
            Self::Stop { machine }
            | Self::Restart { machine }
            | Self::PauseProcess { machine }
            | Self::ResumeProcess { machine }
            | Self::Pause { machine }
            | Self::Resume { machine }
            | Self::Crash { machine }
            | Self::Reboot { machine }
            | Self::ClockSkew { machine, .. }
            | Self::ClockDrift { machine, .. }
            | Self::Throttle { machine, .. }
            | Self::DiskError { machine, .. } => Some(machine),
            _ => None,
        }
    }
    /// Effects the target's supervisor carries out and acknowledges.
    pub fn is_process(&self) -> bool {
        matches!(
            self,
            Self::Stop { .. }
                | Self::Restart { .. }
                | Self::PauseProcess { .. }
                | Self::ResumeProcess { .. }
        )
    }
    /// Whole-machine lifecycle effects, which the world applies at admission.
    pub fn is_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::Pause { .. } | Self::Resume { .. } | Self::Crash { .. } | Self::Reboot { .. }
        )
    }
}
/// Exactly one of a fixed nonzero offset or a seeded positive bound, each
/// within [`MAX_CLOCK_OFFSET_NS`].
pub fn validate_clock_skew(
    offset_ns: Option<i64>,
    max_offset_ns: Option<u64>,
) -> Result<(), String> {
    match (offset_ns, max_offset_ns) {
        (Some(o), None) if o != 0 && o.unsigned_abs() <= MAX_CLOCK_OFFSET_NS => Ok(()),
        (None, Some(m)) if m != 0 && m <= MAX_CLOCK_OFFSET_NS => Ok(()),
        _ => Err(format!(
            "clock_skew needs exactly one of a nonzero offset_ns or a positive max_offset_ns, each at most {MAX_CLOCK_OFFSET_NS}"
        )),
    }
}
/// Exactly one of a fixed nonzero rate or a seeded positive bound, each
/// within [`MAX_CLOCK_RATE_PPB`].
pub fn validate_clock_drift(
    rate_ppb: Option<i64>,
    max_rate_ppb: Option<u64>,
) -> Result<(), String> {
    match (rate_ppb, max_rate_ppb) {
        (Some(r), None) if r != 0 && r.unsigned_abs() <= MAX_CLOCK_RATE_PPB => Ok(()),
        (None, Some(m)) if m != 0 && m <= MAX_CLOCK_RATE_PPB => Ok(()),
        _ => Err(format!(
            "clock_drift needs exactly one of a nonzero rate_ppb or a positive max_rate_ppb, each at most {MAX_CLOCK_RATE_PPB}"
        )),
    }
}

/// Which disk requests a `disk_error` fault fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskOp {
    Read,
    Write,
    Flush,
    /// Reads, writes and flushes.
    Any,
}
/// Disk error probabilities, in parts per million: from rare to every request.
pub const DISK_ERROR_PPM: std::ops::RangeInclusive<u32> = 1..=1_000_000;
fn every_request() -> u32 {
    *DISK_ERROR_PPM.end()
}

/// Throttled CPU speeds, in Hz: from a thousandth of the slowest configured CPU
/// (1 MHz) to the fastest (1 THz). How far a given machine can slow follows
/// from its own speed; see `throttle_permille`.
pub const THROTTLE_SPEED_HZ: std::ops::RangeInclusive<u64> = 1_000..=1_000_000_000_000;
/// Throttle factors a backend charges, in thousandths: from unchanged to a
/// thousand times slower.
pub const THROTTLE_PERMILLE: std::ops::RangeInclusive<u64> = 1_000..=1_000_000;

/// The factor, in thousandths, a backend charges CPU work to run a
/// `speed_hz` CPU at `throttled_hz`: rounded up, so a throttle never speeds a
/// machine up, and clamped to `THROTTLE_PERMILLE`, so a speed at or above the
/// machine's runs it at full speed and one below a thousandth of it at a
/// thousandth.
pub fn throttle_permille(speed_hz: u64, throttled_hz: u64) -> u64 {
    if throttled_hz == 0 {
        return *THROTTLE_PERMILLE.end();
    }
    let permille = (u128::from(speed_hz) * 1000).div_ceil(u128::from(throttled_hz));
    u64::try_from(permille)
        .unwrap_or(u64::MAX)
        .clamp(*THROTTLE_PERMILLE.start(), *THROTTLE_PERMILLE.end())
}

/// Nominal CPU speed actually applied after factor rounding and saturation.
pub fn effective_throttle_speed(speed_hz: u64, factor_permille: u64) -> u64 {
    (u128::from(speed_hz) * 1000 / u128::from(factor_permille)) as u64
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: u32,
    pub at_ns: u64,
    pub deadline_ns: u64,
    #[serde(flatten)]
    pub action: Action,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_factor_rounds_up_and_clamps_to_the_backend_range() {
        assert_eq!(throttle_permille(2_000_000_000, 500_000_000), 4000);
        assert_eq!(throttle_permille(2_000_000_000, 2_000_000_000), 1000);
        assert_eq!(throttle_permille(2_000_000_000, 333_000_000), 6007);
        assert_eq!(throttle_permille(2_000_000_000, 3_000_000_000), 1000);
        assert_eq!(throttle_permille(2_000_000_000, 1_000_000), 1_000_000);
        assert_eq!(throttle_permille(2_000_000_000, 1_000), 1_000_000);
        assert_eq!(throttle_permille(2_000_000_000, 0), 1_000_000);
        assert_eq!(throttle_permille(0, 500_000_000), 1000);
        for (requested, effective) in [
            (500_000_000, 500_000_000),
            (3_000_000_000, 2_000_000_000),
            (1_000, 2_000_000),
            (333_000_000, 332_944_897),
        ] {
            let factor = throttle_permille(2_000_000_000, requested);
            assert_eq!(effective_throttle_speed(2_000_000_000, factor), effective);
        }
    }
    #[test]
    fn strict_wire_rejects_unknown_fields_bad_targets_and_duplicate_identity() {
        for bad in [
            r#"{"version":1,"id":1,"op":"now","extra":1}"#,
            r#"{"version":1,"id":1,"op":"stop","target":{"service":"s","machine":"m"}}"#,
            r#"{"version":1,"id":1,"op":"sleep","duration_ns":-1}"#,
            r#"{"version":1,"id":1,"op":"now","id":2}"#,
        ] {
            assert!(
                serde_json::from_str::<Request>(bad).is_err(),
                "accepted {bad}"
            );
        }
        let r:Request=serde_json::from_str(r#"{"version":1,"id":1,"op":"partition","left":[{"service":"c"}],"right":[{"machine":"s"}]}"#).unwrap();
        assert!(matches!(r.operation, Operation::Partition { .. }));
        for op in [
            "pause",
            "resume",
            "crash",
            "reboot",
            "pause_process",
            "resume_process",
        ] {
            for target in [Target::Service("s".into()), Target::Machine("m".into())] {
                let wire = serde_json::json!({"version":1,"id":1,"op":op,"target":target});
                let request: Request = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(serde_json::to_value(request).unwrap(), wire);
            }
            for arguments in [
                serde_json::json!({}),
                serde_json::json!({"target":"s"}),
                serde_json::json!({"target":{"service":"s","machine":"m"}}),
                serde_json::json!({"target":{"service":"s"},"extra":1}),
            ] {
                let mut wire = serde_json::json!({"version":1,"id":1,"op":op});
                wire.as_object_mut()
                    .unwrap()
                    .extend(arguments.as_object().unwrap().clone());
                assert!(serde_json::from_value::<Request>(wire).is_err());
            }
        }
    }
}
