//! Typed triggers: committed occurrences that admit a fault.
//!
//! The oracle's `arm_trigger` call already arms console-marker triggers whose
//! match stops an application or partitions machines. A [`TriggerSpec`]
//! states the same rule in general: when its [`EventSelector`] has matched
//! `nth` times, its `then` action is admitted as an ordinary fault, with its
//! own fault ID, at the committed barrier that found the match plus
//! `delay_ns`. Oracle-armed and program-armed triggers share one ID space,
//! one budget and one [`TriggerStatus`] lifecycle, and both publish a
//! [`TriggerReceipt`] as they change.
use super::{Action, DiskOp, PROCESS_EFFECT_TIMEOUT_NS};
use serde::{Deserialize, Serialize};

/// Trigger IDs a run may allocate, oracle-armed and program-armed together.
/// IDs start at 1 and are never reused.
pub const MAX_TRIGGERS: usize = 64;
/// Bytes in a console marker.
pub const MAX_TRIGGER_MARKER: usize = 256;
/// Selector names reserved for SDK workload evidence (events, assertions and
/// setup markers), which arrive in a later change. They parse as unknown
/// today.
#[cfg(test)]
pub(crate) const RESERVED_SELECTORS: [&str; 3] = ["event", "assertion", "setup_complete"];

/// Once `when` has matched `nth` times, `then` is admitted as a fault at the
/// committed barrier that found the match plus `delay_ns`, where it meets the
/// same state checks as a timed input. Occurrences at or after `within_ns`
/// from arming never match, and the trigger expires; without it the run's
/// end cuts it off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSpec {
    pub when: EventSelector,
    #[serde(
        default = "first_occurrence",
        skip_serializing_if = "is_first_occurrence"
    )]
    pub nth: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ns: Option<u64>,
    pub then: Action,
}
fn first_occurrence() -> u64 {
    1
}
fn is_first_occurrence(value: &u64) -> bool {
    *value == 1
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}
impl TriggerSpec {
    /// Shape checks that need no world: names, counts, the effect's own
    /// arguments, and pairing a held block request with its crash.
    pub fn validate(&self) -> Result<(), String> {
        if self.nth == 0 {
            return Err("trigger nth counts from 1".into());
        }
        if self.within_ns == Some(0) {
            return Err("trigger within_ns must be positive".into());
        }
        self.when.validate()?;
        self.then.validate()?;
        if let EventSelector::BlockRequest { machine, .. } = &self.when {
            // The request is held at the device before it has any effect, so
            // the only consistent outcome is that machine losing power there.
            if self.then
                != (Action::Crash {
                    machine: machine.clone(),
                })
                || self.delay_ns != 0
            {
                return Err(
                    "a block_request trigger's only effect is an undelayed crash of its machine"
                        .into(),
                );
            }
        }
        Ok(())
    }
    /// Check deadline arithmetic for a trigger armed at `armed_at_ns`, as
    /// the oracle's `arm_trigger` does for its match timeout.
    pub fn validate_at(&self, armed_at_ns: u64) -> Result<(), String> {
        self.validate()?;
        let latest_match = match self.within_ns {
            Some(within) => armed_at_ns
                .checked_add(within)
                .ok_or("trigger match deadline overflows")?,
            None => armed_at_ns,
        };
        latest_match
            .checked_add(self.delay_ns)
            .and_then(|effect| effect.checked_add(PROCESS_EFFECT_TIMEOUT_NS))
            .ok_or("trigger effect deadline overflows")?;
        Ok(())
    }
}

/// Committed evidence a trigger matches. Omitted machines match any machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
// The tag is `kind`: YAML 1.1 parsers read a bare `on` key as a boolean,
// and no selector has a field of that name.
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventSelector {
    /// `marker` in the machine's committed output: stdout, stderr and raw
    /// console bytes in observation order, matched across chunk boundaries.
    /// A reboot of the machine clears a partial match, not the trigger. This
    /// is what the oracle's `arm_trigger` arms.
    Output { machine: String, marker: Marker },
    /// A machine lifecycle transition, as the Driver commits it.
    Lifecycle {
        machine: String,
        transition: Transition,
    },
    /// An observed IPv4 TCP connection opening: its first SYN sent between
    /// the machines, whatever the frame's fate.
    ConnectionOpen {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
    },
    /// A frame sent from one machine towards another, whatever its fate.
    Frame {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    /// A guest disk request of kind `op`, held at the device on arrival,
    /// before it has any effect. Its only effect is the machine's crash.
    BlockRequest { machine: String, op: DiskOp },
}
impl EventSelector {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Output { .. } => "output",
            Self::Lifecycle { .. } => "lifecycle",
            Self::ConnectionOpen { .. } => "connection_open",
            Self::Frame { .. } => "frame",
            Self::BlockRequest { .. } => "block_request",
        }
    }
    /// Every machine the selector names, in field order.
    pub fn machines(&self) -> Vec<&str> {
        match self {
            Self::Output { machine, .. }
            | Self::Lifecycle { machine, .. }
            | Self::BlockRequest { machine, .. } => vec![machine],
            Self::ConnectionOpen { from, to, .. } | Self::Frame { from, to } => {
                from.iter().chain(to).map(String::as_str).collect()
            }
        }
    }
    fn validate(&self) -> Result<(), String> {
        if let Self::Output { marker, .. } = self {
            if marker.0.is_empty() || marker.0.len() > MAX_TRIGGER_MARKER {
                return Err(format!(
                    "output selector markers need 1..={MAX_TRIGGER_MARKER} bytes"
                ));
            }
        }
        if let Self::ConnectionOpen { port: Some(0), .. } = self {
            return Err("connection_open selector port must be positive".into());
        }
        if self.machines().iter().any(|machine| machine.is_empty()) {
            return Err(format!("{} selector machine is empty", self.name()));
        }
        Ok(())
    }
}

/// Output marker bytes. The wire form is a UTF-8 string, or `{hex: "..."}`
/// for bytes that are not UTF-8; text is written as a string either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker(pub Vec<u8>);
impl Serialize for Marker {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(&self.0) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => {
                use serde::ser::SerializeMap;
                let hex: String = self.0.iter().map(|b| format!("{b:02x}")).collect();
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("hex", &hex)?;
                map.end()
            }
        }
    }
}
impl<'de> Deserialize<'de> for Marker {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Text(String),
            Bytes(Hex),
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Hex {
            hex: String,
        }
        match Wire::deserialize(deserializer)
            .map_err(|_| D::Error::custom("marker must be a string or {hex: \"...\"}"))?
        {
            Wire::Text(text) => Ok(Self(text.into_bytes())),
            Wire::Bytes(Hex { hex }) => {
                let digits = hex.as_bytes();
                if digits.len() % 2 != 0 {
                    return Err(D::Error::custom("marker hex needs whole bytes"));
                }
                digits
                    .chunks(2)
                    .map(|pair| {
                        std::str::from_utf8(pair)
                            .ok()
                            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                            .ok_or_else(|| D::Error::custom("marker hex has a non-hex digit"))
                    })
                    .collect::<Result<_, _>>()
                    .map(Self)
            }
        }
    }
}

/// Machine lifecycle transitions a trigger can watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    /// The supervisor reported the machine ready.
    Ready,
    /// The machine's application exited.
    Exited,
    Crashed,
    Rebooted,
    Paused,
    Resumed,
}

/// Where a trigger is in its life. The Driver's oracle triggers already use
/// these states; `Fired` proves admission of its fault, not its outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerStatus {
    /// Watching for its occurrence.
    Armed,
    /// Matched; its effect waits for its time.
    Matched,
    /// Its effect was admitted as a fault.
    Fired,
    /// Its effect was refused when due.
    Rejected,
    /// Cancelled before it matched.
    Cancelled,
    /// No match before its deadline.
    Expired,
    /// The run ended while it was armed or matched.
    CutOff,
}
impl TriggerStatus {
    /// A final trigger never changes again.
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Armed | Self::Matched)
    }
}

/// A trigger's receipt, published whenever it changes. Times are simulation
/// time. A fired trigger names the fault it admitted, whose own status says
/// whether the effect applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerReceipt {
    pub id: u32,
    /// The program input that armed it; absent for an oracle-armed trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_key: Option<String>,
    pub when: EventSelector,
    pub status: TriggerStatus,
    pub armed_at_ns: u64,
    /// Occurrences at or after this time never match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_deadline_ns: Option<u64>,
    /// Matching occurrences seen so far.
    pub seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_id: Option<u32>,
    /// Why a rejected trigger's effect was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
