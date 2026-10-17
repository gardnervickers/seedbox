//! A fault profile asks the server to generate faults for one submission. It is
//! a request option, not part of a simulation definition: the server samples it
//! once, with the simulation's seed, into ordinary keyed program inputs. Rerun
//! and replay execute those inputs and never sample the profile again.
use serde::{Deserialize, Serialize};
use std::fmt;

/// Generated faults never exceed this many, so their start and teardown inputs
/// stay well inside the input program limit.
pub const MAX_GENERATED_FAULTS: u32 = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultProfile {
    /// No generated fault starts before this simulated time.
    #[serde(default = "Duration::zero")]
    pub start: Duration,
    /// No generated fault starts at or after this simulated time.
    pub end: Duration,
    /// Mean simulated time between fault starts; gaps are drawn uniformly
    /// from half to one and a half times this.
    pub interval: Duration,
    /// At most this many faults.
    #[serde(default = "max_faults")]
    pub max_faults: u32,
    /// Machines faults may target; empty means every machine but the oracle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// The kinds of fault to draw from, by weight.
    pub faults: Vec<FaultKind>,
}

fn max_faults() -> u32 {
    64
}

impl FaultProfile {
    pub fn validate(&self) -> Result<(), String> {
        if self.end.0 <= self.start.0 {
            return Err("fault profile end must be after its start".into());
        }
        if self.interval.0 == 0 {
            return Err("fault profile interval must be positive".into());
        }
        if self.max_faults == 0 || self.max_faults > MAX_GENERATED_FAULTS {
            return Err(format!(
                "fault profile maxFaults must be 1 to {MAX_GENERATED_FAULTS}"
            ));
        }
        if self.faults.is_empty() || self.faults.iter().all(|f| f.weight() == 0) {
            return Err("fault profile needs at least one fault kind with positive weight".into());
        }
        for fault in &self.faults {
            fault.validate()?;
        }
        Ok(())
    }
}

/// An inclusive range of durations, sampled uniformly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurationRange {
    pub min: Duration,
    pub max: Duration,
}

impl DurationRange {
    fn validate(&self, what: &str) -> Result<(), String> {
        if self.min.0 == 0 || self.max.0 < self.min.0 {
            return Err(format!("fault profile {what} needs 0 < min <= max"));
        }
        Ok(())
    }
}

fn weight() -> u32 {
    1
}

fn every_request() -> u32 {
    1_000_000
}
fn any_request() -> DiskOp {
    DiskOp::Any
}
/// The engine's disk latency bounds, in nanoseconds (`seedbox_control::DISK_LATENCY_NS`).
const DISK_LATENCY_NS: (u64, u64) = (1_000, 1_000_000_000);

/// Which disk requests a generated disk error fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskOp {
    Read,
    Write,
    Flush,
    Any,
}

/// What a generated disk error does to a request it selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskEffect {
    /// Fail it with an I/O error.
    #[default]
    Error,
    /// Complete it with one sector of seeded garbage.
    Corrupt,
}

impl DiskEffect {
    pub fn is_error(&self) -> bool {
        *self == Self::Error
    }
}

/// Which sectors a generated disk error may select.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskScope {
    /// Any request of the selected kind.
    #[default]
    Disk,
    /// Only reads and writes clear of the filesystem's static metadata.
    Data,
}

impl DiskScope {
    pub fn is_disk(&self) -> bool {
        *self == Self::Disk
    }
}

/// One kind of generated fault. Each fault with a duration is followed by
/// its teardown: a heal, resume, reboot or restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FaultKind {
    /// Cut one target off from the machines in `from`, or from every other
    /// machine, including the oracle, when `from` is empty.
    Partition {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        from: Vec<String>,
    },
    /// Delay and drop traffic leaving one target.
    #[serde(rename_all = "camelCase")]
    Shape {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        latency: DurationRange,
        #[serde(default)]
        loss_ppm: u64,
    },
    /// Destroy one target's VM, then boot it again.
    Crash {
        #[serde(default = "weight")]
        weight: u32,
        downtime: DurationRange,
    },
    /// Freeze one target's whole VM, then resume it.
    Pause {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
    },
    /// SIGSTOP one target's application, then continue it.
    PauseProcess {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
    },
    /// Kill one target's application, then start it again.
    Stop {
        #[serde(default = "weight")]
        weight: u32,
        downtime: DurationRange,
    },
    /// Fail one target's disk requests of kind `op`, each with probability
    /// `ppm` per million, then heal; with `effect: corrupt`, complete them
    /// with one sector of garbage instead; with `scope: data`, select only
    /// requests clear of the filesystem's static metadata. Only machines with
    /// a retained disk can take it; `targets` narrows the profile's targets
    /// to those.
    DiskError {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        op: DiskOp,
        #[serde(default = "every_request")]
        ppm: u32,
        #[serde(default, skip_serializing_if = "DiskEffect::is_error")]
        effect: DiskEffect,
        #[serde(default, skip_serializing_if = "DiskScope::is_disk")]
        scope: DiskScope,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        targets: Vec<String>,
    },
    /// Complete one target's disk requests of kind `op` late, by a latency
    /// drawn from `latency` plus a seeded draw up to `jitter` per request,
    /// then heal. Only machines with a retained disk can take it; `targets`
    /// narrows the profile's targets to those.
    DiskLatency {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        latency: DurationRange,
        #[serde(default = "any_request")]
        op: DiskOp,
        #[serde(default = "Duration::zero")]
        jitter: Duration,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        targets: Vec<String>,
    },
    /// Perturb one target's thread scheduling: its guest yields the CPU at
    /// `ppm` per million syscalls and progress calls, then heal.
    Preempt {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        ppm: u32,
    },
    /// Step one target's wall clock by up to `maxOffset` either way.
    #[serde(rename_all = "camelCase")]
    ClockSkew {
        #[serde(default = "weight")]
        weight: u32,
        duration: DurationRange,
        max_offset: Duration,
    },
}

impl FaultKind {
    pub fn weight(&self) -> u32 {
        match self {
            Self::Partition { weight, .. }
            | Self::Shape { weight, .. }
            | Self::Crash { weight, .. }
            | Self::Pause { weight, .. }
            | Self::PauseProcess { weight, .. }
            | Self::Stop { weight, .. }
            | Self::DiskError { weight, .. }
            | Self::DiskLatency { weight, .. }
            | Self::Preempt { weight, .. }
            | Self::ClockSkew { weight, .. } => *weight,
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Partition { duration, .. }
            | Self::Pause { duration, .. }
            | Self::PauseProcess { duration, .. } => duration.validate("duration"),
            Self::Shape {
                duration,
                latency,
                loss_ppm,
                ..
            } => {
                duration.validate("duration")?;
                latency.validate("latency")?;
                if *loss_ppm > 1_000_000 {
                    return Err("fault profile lossPpm must be at most 1000000".into());
                }
                Ok(())
            }
            Self::Crash { downtime, .. } | Self::Stop { downtime, .. } => {
                downtime.validate("downtime")
            }
            Self::DiskError {
                duration,
                ppm,
                op,
                effect,
                scope,
                ..
            } => {
                duration.validate("duration")?;
                if *ppm == 0 || *ppm > 1_000_000 {
                    return Err("fault profile disk_error ppm must be 1 to 1000000".into());
                }
                if *op == DiskOp::Flush
                    && (*effect == DiskEffect::Corrupt || *scope == DiskScope::Data)
                {
                    return Err(
                        "fault profile disk_error cannot corrupt or data-scope only flushes".into(),
                    );
                }
                Ok(())
            }
            Self::DiskLatency {
                duration,
                latency,
                jitter,
                ..
            } => {
                duration.validate("duration")?;
                latency.validate("latency")?;
                let bounds = DISK_LATENCY_NS;
                if latency.min.0 < bounds.0 || latency.max.0 > bounds.1 || jitter.0 > bounds.1 {
                    return Err(format!(
                        "fault profile disk_latency latency must be {}ns to {}ns, jitter at most {}ns",
                        bounds.0, bounds.1, bounds.1
                    ));
                }
                Ok(())
            }
            Self::Preempt { duration, ppm, .. } => {
                duration.validate("duration")?;
                if *ppm == 0 || *ppm > 1_000_000 {
                    return Err("fault profile preempt ppm must be 1 to 1000000".into());
                }
                Ok(())
            }
            Self::ClockSkew {
                duration,
                max_offset,
                ..
            } => {
                duration.validate("duration")?;
                if max_offset.0 == 0 || max_offset.0 > i64::MAX as u64 {
                    return Err("fault profile maxOffset must be positive".into());
                }
                Ok(())
            }
        }
    }
}

/// A whole number of nanoseconds, written with a unit: `250ms`, `2s`, `1m`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration(pub u64);

impl Duration {
    pub fn zero() -> Self {
        Self(0)
    }
}

impl std::str::FromStr for Duration {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        let digits = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(digits);
        let scale: u64 = match unit {
            "ns" => 1,
            "us" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => {
                return Err(format!(
                    "duration {text:?} needs a unit: ns, us, ms, s, m or h"
                ))
            }
        };
        number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .map(Self)
            .ok_or_else(|| format!("invalid duration {text:?}"))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (unit, scale) in [
            ("h", 3_600_000_000_000),
            ("m", 60_000_000_000),
            ("s", 1_000_000_000),
            ("ms", 1_000_000),
            ("us", 1_000),
        ] {
            if self.0 != 0 && self.0.is_multiple_of(scale) {
                return write!(f, "{}{unit}", self.0 / scale);
            }
        }
        write!(f, "{}ns", self.0)
    }
}

impl Serialize for Duration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_units_and_round_trip() {
        for (text, ns) in [
            ("0s", 0),
            ("250ms", 250_000_000),
            ("2s", 2_000_000_000),
            ("7ns", 7),
        ] {
            let duration: Duration = text.parse().unwrap();
            assert_eq!(duration.0, ns);
            assert_eq!(duration.to_string().parse::<Duration>().unwrap(), duration);
        }
        for bad in ["", "5", "1.5s", "-1s", "3 days", "99999999999999999999h"] {
            assert!(bad.parse::<Duration>().is_err(), "{bad}");
        }
    }

    #[test]
    fn profiles_parse_from_yaml_and_validate() {
        let profile: FaultProfile = serde_yaml_ng::from_str(
            r#"
start: 2s
end: 60s
interval: 5s
faults:
  - kind: partition
    weight: 3
    duration: {min: 1s, max: 10s}
  - kind: crash
    downtime: {min: 500ms, max: 3s}
  - kind: shape
    duration: {min: 1s, max: 2s}
    latency: {min: 10ms, max: 200ms}
    lossPpm: 10000
  - kind: clock_skew
    duration: {min: 1s, max: 5s}
    maxOffset: 2s
"#,
        )
        .unwrap();
        profile.validate().unwrap();
        assert_eq!(profile.max_faults, 64);
        assert_eq!(profile.faults[1].weight(), 1);
        let mut invalid = profile.clone();
        invalid.end = invalid.start;
        assert!(invalid.validate().is_err());
        let mut invalid = profile.clone();
        invalid.faults = vec![FaultKind::Pause {
            weight: 1,
            duration: DurationRange {
                min: Duration(5),
                max: Duration(1),
            },
        }];
        assert!(invalid.validate().is_err());
        assert!(serde_yaml_ng::from_str::<FaultProfile>(
            "end: 1s\ninterval: 1s\nfaults: []\nextra: 1"
        )
        .is_err());
    }
}
