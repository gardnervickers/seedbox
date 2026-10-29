//! Private process protocol. No world scheduling or worker-local semantic state.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

pub(super) type Result<T> = std::result::Result<T, String>;
pub(super) const MAX_MESSAGE: usize = 8 << 20;
pub(super) use super::policy::BootPolicy;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeEvidence {
    pub parts: Vec<(String, String)>,
    pub regions: Vec<(u64, u64)>,
    pub page_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BootWitness {
    pub mark: Option<u8>,
    pub boot: u64,
    pub initial: [u8; 32],
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeExecution {
    pub chain: [u8; 32],
    pub pending: Vec<u8>,
}
#[derive(Serialize, Deserialize)]
pub(super) enum Request {
    Boot {
        /// Required structural resource contract; old fixed-size workers cannot acknowledge it.
        witness: Box<BootWitness>,
        memory_bytes: u64,
        layout_version: u32,
        /// Restore into a newly staged worker after validating the native file.
        restore: Option<PathBuf>,
        kernel: PathBuf,
        initrd: PathBuf,
        command_line: String,
        seed: u64,
        now: u64,
        disk: Option<BootDisk>,
        /// The NIC's MAC address; `None` maps no NIC and announces none.
        network: Option<[u8; 6]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Box<Option<BootPolicy>>,
        /// Device faults and holds this boot starts with; ignored on restore,
        /// whose image carries its own.
        #[serde(default, skip_serializing_if = "BootFaults::is_empty")]
        faults: Box<BootFaults>,
    },
    /// Followed by a host frames trailer with `frames` records
    /// ([`super::wire::send_frames`]), admitted in order at `now`.
    Run {
        now: u64,
        upto: u64,
        syscall_cost_ns: u64,
        shutdown: bool,
        lifecycle: Vec<u8>,
        frames: u64,
    },
    ExecutionEvidence,
    Capture(PathBuf),
    /// Read actual stopped CPU/device/RAM state; page records go to this new
    /// caller-owned file, not the bounded JSON response. Never executes/flushes.
    Evidence {
        pages: PathBuf,
    },
    /// Write the stopped disk's encoded overlays (both layers, unchanged) to a
    /// new file. Allowed after the guest ended its boot; it never flushes or
    /// discards, and leaves the boot-end policy to the client.
    ExportDisk(PathBuf),
    /// Change the stopped boot's device faults; answered with [`Response::Faults`].
    Fault(FaultChange),
}
/// The device faults a boot starts with. Streams draw on `boot`, the
/// physical boot ordinal; counts start at zero, holds at their `seen`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BootFaults {
    pub boot: u64,
    pub installed: Vec<InstalledFault>,
    pub holds: Vec<seedbox_machine::machine::disk::fault::Hold>,
}
impl BootFaults {
    pub(super) fn is_empty(&self) -> bool {
        self.installed.is_empty() && self.holds.is_empty()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstalledFault {
    pub id: u64,
    pub seed: u64,
    pub fault: seedbox_machine::pv::DeviceFault,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) enum FaultChange {
    Install(InstalledFault),
    Heal(u64),
    Hold(seedbox_machine::machine::disk::fault::Hold),
    Release(u32),
    /// Report without a change.
    Report,
}
/// This boot's counts for each installed disk fault, the remaining holds,
/// and the counts of a fault just healed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FaultReport {
    pub counts: Vec<(u64, seedbox_machine::pv::FaultCounts)>,
    pub holds: Vec<seedbox_machine::machine::disk::fault::Hold>,
    pub healed: Option<seedbox_machine::pv::FaultCounts>,
}
/// The one block device of a boot, owned by the worker until the boot ends.
// Files keep multi-megabyte disks out of JSON frames; the client keeps the
// base and boot-end policy, so the worker only needs these inputs.
#[derive(Serialize, Deserialize)]
pub(super) struct BootDisk {
    /// Capacity in 512-byte sectors; a base file is exactly this long.
    pub(super) sectors: u64,
    /// Owned immutable base image; `None` is a zero base.
    pub(super) base: Option<PathBuf>,
    /// Encoded overlays over that base; `None` is empty layers.
    pub(super) overlay: Option<PathBuf>,
}
#[derive(Debug, Serialize, Deserialize)]
pub(super) enum Response {
    Ready {
        witness: BootWitness,
        memory_bytes: u64,
        layout_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy: Option<BootPolicy>,
    },
    /// Followed by a guest writes trailer with `writes` records
    /// ([`super::wire::send_writes`]) in emission order.
    Stopped {
        time: u64,
        stop: Stop,
        writes: u64,
    },
    ExecutionEvidence(NativeExecution),
    Captured,
    Exported,
    Evidence(NativeEvidence),
    Faults(FaultReport),
    Error(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Stop {
    Horizon,
    Blocked(Option<u64>),
    /// The guest powered off or permanently halted.
    PoweredOff,
    /// The guest requested an architectural reset after its ready handshake:
    /// a deliberate reboot or a panic restart. The worker never resets itself.
    Reset,
    /// A guest disk request is held for this trigger; the boot can only end.
    Held(u32),
}

/// Accept only a stopped worker that acknowledged this exact resource layout.
// Echoed resource, layout, policy and witness fields detect an incompatible
// worker structurally, without provenance gates.
// Also compiled into the worker, where only the client performs this check.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn ready(
    response: Response,
    expected: u64,
    expected_policy: Option<&BootPolicy>,
    expected_witness: &BootWitness,
) -> Result<()> {
    match response {
        Response::Ready {
            witness,
            memory_bytes,
            layout_version,
            policy,
        } if memory_bytes == expected
            && layout_version == super::boot::LAYOUT_VERSION
            && policy.as_ref() == expected_policy
            && &witness == expected_witness =>
        {
            Ok(())
        }
        _ => Err("PV worker resource acknowledgement mismatch".into()),
    }
}

pub(super) fn send(out: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| format!("PV encode: {e}"))?;
    if bytes.len() > MAX_MESSAGE {
        return Err("PV protocol message too large".into());
    }
    out.write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| out.write_all(&bytes))
        .and_then(|_| out.flush())
        .map_err(|e| format!("PV protocol write: {e}"))
}
pub(super) fn receive<T: DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0; 4];
    match input.read(&mut length[..1]) {
        Ok(0) => return Ok(None),
        Ok(_) => (),
        Err(e) => return Err(format!("PV protocol read: {e}")),
    }
    input
        .read_exact(&mut length[1..])
        .map_err(|e| format!("PV protocol header: {e}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err("PV protocol message too large".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("PV protocol body: {e}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("PV protocol decode: {e}"))
}
#[cfg(test)]
mod tests;
