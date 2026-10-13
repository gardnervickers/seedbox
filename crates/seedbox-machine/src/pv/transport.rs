//! Private byte transport; write boundaries are not application messages.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LifecycleWrite {
    pub(crate) time: u64,
    pub(crate) bytes: Vec<u8>,
}

/// One actual guest emission. Vector order is authoritative for equal times
/// across UART and lifecycle device writes; channels must not be merged later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GuestWrite {
    Console(LifecycleWrite),
    Lifecycle(LifecycleWrite),
}
impl GuestWrite {
    pub(crate) fn time(&self) -> u64 {
        match self {
            Self::Console(write) | Self::Lifecycle(write) => write.time,
        }
    }
}

/// Pending output belongs to snapshots until transferred at a stopped boundary.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LifecycleOutput(Vec<LifecycleWrite>);
impl LifecycleOutput {
    pub(crate) fn push(&mut self, time: u64, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.0.push(LifecycleWrite { time, bytes });
        }
    }
    pub(crate) fn extend(&mut self, writes: Vec<LifecycleWrite>) {
        self.0.extend(writes);
    }
    pub(crate) fn drain(&mut self) -> Vec<LifecycleWrite> {
        std::mem::take(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragments_and_same_time_writes_survive_restore_and_drain_once() {
        let mut output = LifecycleOutput::default();
        output.push(7, b"rea".to_vec());
        output.push(7, b"dy\nnext\n".to_vec());
        output.push(9, b"tail".to_vec());
        let snapshot = serde_json::to_vec(&output).unwrap();
        let mut restored: LifecycleOutput = serde_json::from_slice(&snapshot).unwrap();
        let writes = restored.drain();
        assert_eq!(writes.iter().map(|w| w.time).collect::<Vec<_>>(), [7, 7, 9]);
        assert_eq!(
            writes
                .iter()
                .flat_map(|w| w.bytes.iter().copied())
                .collect::<Vec<_>>(),
            b"ready\nnext\ntail"
        );
        assert!(restored.drain().is_empty());
        let after = serde_json::to_vec(&restored).unwrap();
        let mut restored: LifecycleOutput = serde_json::from_slice(&after).unwrap();
        assert!(restored.drain().is_empty());
        assert_eq!(output.drain(), writes);
    }
}
