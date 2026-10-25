//! Language-neutral fault actions, plans and limits. No transport or scheduler.
pub use seedbox_spec::definition::PROCESS_EFFECT_TIMEOUT_NS;
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

mod trigger;
pub use trigger::{
    EventSelector, Marker, Transition, TriggerReceipt, TriggerSpec, TriggerStatus, MAX_TRIGGERS,
    MAX_TRIGGER_MARKER,
};

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

/// A positive bandwidth, and a queue bound only with a bandwidth.
pub fn validate_bandwidth(
    bandwidth_bps: Option<u64>,
    queue_bound_ns: Option<u64>,
) -> Result<(), String> {
    if bandwidth_bps == Some(0) {
        return Err("bandwidth_bps must be positive".into());
    }
    if queue_bound_ns.is_some() && bandwidth_bps.is_none() {
        return Err("queue_bound_ns requires bandwidth_bps".into());
    }
    Ok(())
}
/// The values every link policy and shape layer must have: latency at least
/// the network's global `floor_ns`, jitter below latency, their sum within
/// [`MAX_LINK_DELAY_NS`], loss within [`MAX_PPM`], and a valid bandwidth.
/// The Driver's link policies and fault profiles check links with this.
pub fn validate_link(
    latency_ns: u64,
    jitter_ns: u64,
    loss_ppm: u64,
    bandwidth_bps: Option<u64>,
    queue_bound_ns: Option<u64>,
    floor_ns: u64,
) -> Result<(), String> {
    if latency_ns < floor_ns {
        return Err("latency_ns below global latency".into());
    }
    if jitter_ns >= latency_ns {
        return Err("jitter_ns must be less than latency_ns".into());
    }
    if latency_ns
        .checked_add(jitter_ns)
        .is_none_or(|n| n > MAX_LINK_DELAY_NS)
    {
        return Err("latency_ns + jitter_ns exceeds one hour".into());
    }
    if loss_ppm > MAX_PPM {
        return Err(format!("loss_ppm exceeds {MAX_PPM}"));
    }
    validate_bandwidth(bandwidth_bps, queue_bound_ns)
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
    /// Scale the machine's seeded CPU cost until the handle is healed.
    Throttle {
        handle: String,
        machine: String,
        factor_permille: u64,
    },
    /// Select the machine's disk requests of kind `op`, each with probability
    /// `ppm` per million on its own seeded stream, until the handle is healed.
    /// With `eio` a selected request completes with an I/O error before any
    /// disk effect: a failed write changes nothing, a failed flush makes
    /// nothing durable, a failed read returns no data. With `corrupt` it
    /// succeeds with one sector replaced by seeded bytes: a read returns them
    /// and the disk keeps its data, a write stores them. `scope: data` spares
    /// the filesystem's static metadata (see [`DiskScope::Data`]). The fault survives a guest device
    /// reset and applies to every incarnation of the machine; heal stops new
    /// selections and settles with [`FaultCounts`]. Requests already completed
    /// stay as they completed. See [`validate_disk_error`].
    DiskError {
        handle: String,
        machine: String,
        op: DiskOp,
        ppm: u64,
        #[serde(default, skip_serializing_if = "DiskEffect::is_eio")]
        effect: DiskEffect,
        #[serde(default, skip_serializing_if = "DiskScope::is_all")]
        scope: DiskScope,
    },
    /// Complete the machine's disk requests of kind `op` late, until the
    /// handle is healed: each takes `latency_ns` plus a seeded draw up to
    /// `jitter_ns`, and completes no earlier than the request before it, so
    /// completion order stays submission order. A request has its disk
    /// effect, data, status and interrupt when it completes; a crash or device
    /// reset drops requests still pending, which were never acknowledged.
    /// Heal stops delaying new requests; queued ones complete at their times.
    /// See [`validate_disk_latency`].
    DiskLatency {
        handle: String,
        machine: String,
        op: DiskOp,
        latency_ns: u64,
        #[serde(default, skip_serializing_if = "is_zero")]
        jitter_ns: u64,
    },
    /// Perturb the machine's thread scheduling until the handle is healed:
    /// its guest kernel yields the CPU at `ppm` per million syscalls and
    /// progress charges, drawn on a seeded guest stream, so runnable threads
    /// switch at points a single guest CPU rarely reaches. It charges no time
    /// and applies to every incarnation; heal restores ordinary scheduling.
    /// See [`validate_preempt`].
    Preempt {
        handle: String,
        machine: String,
        ppm: u64,
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
            Self::DiskLatency { .. } => "disk_latency",
            Self::Preempt { .. } => "preempt",
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
            | Self::DiskError { machine, .. }
            | Self::DiskLatency { machine, .. }
            | Self::Preempt { machine, .. } => Some(machine),
            _ => None,
        }
    }
    /// Check the arguments whose bounds this protocol defines: clock, throttle,
    /// disk and scheduler faults. A shape's link policy is checked where links
    /// are compiled, and every action's targets and state at admission.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::ClockSkew {
                offset_ns,
                max_offset_ns,
                ..
            } => validate_clock_skew(*offset_ns, *max_offset_ns),
            Self::ClockDrift {
                rate_ppb,
                max_rate_ppb,
                ..
            } => validate_clock_drift(*rate_ppb, *max_rate_ppb),
            Self::Throttle {
                factor_permille, ..
            } if !THROTTLE_PERMILLE.contains(factor_permille) => Err(format!(
                "throttle factor_permille must be {} to {}",
                THROTTLE_PERMILLE.start(),
                THROTTLE_PERMILLE.end()
            )),
            Self::DiskError {
                op,
                ppm,
                effect,
                scope,
                ..
            } => validate_disk_error(*op, *ppm, *effect, *scope),
            Self::DiskLatency {
                latency_ns,
                jitter_ns,
                ..
            } => validate_disk_latency(*latency_ns, *jitter_ns),
            Self::Preempt { ppm, .. } => validate_preempt(*ppm),
            _ => Ok(()),
        }
    }
    /// Actions this protocol defines but no engine carries out yet. Every
    /// admission path refuses them with [`crate::not_implemented`].
    pub fn is_defined_only(&self) -> bool {
        matches!(
            self,
            Self::DiskError { .. } | Self::DiskLatency { .. } | Self::Preempt { .. }
        )
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

/// Throttle factors, in thousandths: from unchanged to a thousand times slower.
pub const THROTTLE_PERMILLE: std::ops::RangeInclusive<u64> = 1_000..=1_000_000;

/// Which disk requests a disk fault selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskOp {
    Read,
    Write,
    Flush,
    /// Reads, writes and flushes.
    Any,
}
/// What a `disk_error` fault does to a request it selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskEffect {
    /// Complete it with virtio-blk's IOERR status, which the guest sees as EIO.
    #[default]
    Eio,
    /// Complete it successfully with one sector of seeded bytes: silent
    /// corruption. Flushes carry no data and are never selected.
    Corrupt,
}
impl DiskEffect {
    pub fn is_eio(&self) -> bool {
        *self == Self::Eio
    }
}
/// Which sectors a `disk_error` fault may select.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskScope {
    /// Any request of the selected kind.
    #[default]
    All,
    /// Only reads and writes that touch none of the ext4 metadata laid out
    /// when the disk is formatted: superblocks and their backups, group
    /// descriptors and reserved GDT blocks, block and inode bitmaps, inode
    /// tables and the journal. Metadata ext4 allocates later (directory,
    /// extent-tree and xattr blocks) is ordinary data to this scope and can be
    /// selected. Nothing is selected before the disk is formatted, and flushes
    /// never are.
    Data,
}
impl DiskScope {
    pub fn is_all(&self) -> bool {
        *self == Self::All
    }
}
/// Disk error probabilities, in parts per million: from rare to every request.
pub const DISK_ERROR_PPM: std::ops::RangeInclusive<u64> = 1..=MAX_PPM;
/// Disk request latencies, in nanoseconds: from a fast SSD (1 µs) to a
/// stalled device (1 s). Jitter is at most the same upper bound.
pub const DISK_LATENCY_NS: std::ops::RangeInclusive<u64> = 1_000..=1_000_000_000;
/// Scheduler perturbation rates, in parts per million of syscalls and
/// progress charges: from rare to every one.
pub const PREEMPT_PPM: std::ops::RangeInclusive<u64> = 1..=MAX_PPM;
fn is_zero(value: &u64) -> bool {
    *value == 0
}
/// A probability within [`DISK_ERROR_PPM`]. Corruption needs data, so it
/// cannot select only flushes; neither can the data scope.
pub fn validate_disk_error(
    op: DiskOp,
    ppm: u64,
    effect: DiskEffect,
    scope: DiskScope,
) -> Result<(), String> {
    if !DISK_ERROR_PPM.contains(&ppm) {
        return Err(format!(
            "disk_error ppm must be {} to {}",
            DISK_ERROR_PPM.start(),
            DISK_ERROR_PPM.end()
        ));
    }
    if op == DiskOp::Flush && (effect == DiskEffect::Corrupt || scope == DiskScope::Data) {
        return Err(
            "disk_error on flushes takes neither the corrupt effect nor the data scope".into(),
        );
    }
    Ok(())
}
/// A latency within [`DISK_LATENCY_NS`] and a jitter of at most its upper bound.
pub fn validate_disk_latency(latency_ns: u64, jitter_ns: u64) -> Result<(), String> {
    if !DISK_LATENCY_NS.contains(&latency_ns) || jitter_ns > *DISK_LATENCY_NS.end() {
        return Err(format!(
            "disk_latency latency_ns must be {} to {} and jitter_ns at most {}",
            DISK_LATENCY_NS.start(),
            DISK_LATENCY_NS.end(),
            DISK_LATENCY_NS.end()
        ));
    }
    Ok(())
}
/// A rate within [`PREEMPT_PPM`].
pub fn validate_preempt(ppm: u64) -> Result<(), String> {
    if !PREEMPT_PPM.contains(&ppm) {
        return Err(format!(
            "preempt ppm must be {} to {}",
            PREEMPT_PPM.start(),
            PREEMPT_PPM.end()
        ));
    }
    Ok(())
}
/// What a disk fault selected and did, as its heal settles. For
/// `disk_error`, `affected` counts failed or corrupted requests; for
/// `disk_latency`, delayed ones. Faults that count nothing report none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultCounts {
    /// Requests of the selected kind and scope seen while the fault was active.
    pub matched: u64,
    /// Matched requests the fault acted on.
    pub affected: u64,
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
mod tests;
