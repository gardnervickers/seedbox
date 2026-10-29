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
/// The Driver's link policies and oracle fault profiles check links with this.
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
            | Self::Throttle { machine, .. } => Some(machine),
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

/// Throttle factors, in thousandths: from unchanged to a thousand times slower.
pub const THROTTLE_PERMILLE: std::ops::RangeInclusive<u64> = 1_000..=1_000_000;
/// A throttle factor within [`THROTTLE_PERMILLE`].
pub fn validate_throttle(factor_permille: u64) -> Result<(), String> {
    if THROTTLE_PERMILLE.contains(&factor_permille) {
        return Ok(());
    }
    Err(format!(
        "factor_permille must be in [{}, {}]",
        THROTTLE_PERMILLE.start(),
        THROTTLE_PERMILLE.end()
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: u32,
    pub at_ns: u64,
    pub deadline_ns: u64,
    #[serde(flatten)]
    pub action: Action,
}
