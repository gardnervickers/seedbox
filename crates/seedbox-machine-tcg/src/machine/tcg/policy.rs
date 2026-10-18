//! Factory policy identity and dedicated atomic disk-loss streams.
use super::{qemu::TimerSetup, Error};
use crate::{
    definition::BackendPolicy,
    machine::disk::{Store, SECTOR},
};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PolicyState {
    pub name: String,
    pub configuration: BackendPolicy,
}
impl PolicyState {
    pub fn new(name: &str, configuration: &BackendPolicy) -> Result<Self, Error> {
        if name.contains('\0') {
            return Err(Error(
                "TCG backend policy machine name contains NUL; supply a name representable by QMP"
                    .into(),
            ));
        }
        configuration
            .validate()
            .map_err(|e| Error(format!("TCG backend policy: {e}")))?;
        Ok(Self {
            name: name.to_owned(),
            configuration: configuration.clone(),
        })
    }
    pub fn timer_setup(&self, boot: u64) -> TimerSetup {
        TimerSetup {
            seed: self.configuration.seed,
            jitter: self.configuration.timer_jitter_ns,
            domain: if boot == 0 {
                format!("timer-jitter/{}", self.name)
            } else {
                format!("timer-jitter/{}/{boot}", self.name)
            },
        }
    }
}
pub(super) fn validate_authority(
    retained: Option<&PolicyState>,
    name: &str,
    expected: Option<&BackendPolicy>,
) -> Result<(), Error> {
    match (retained, expected) {
        (None, None) => Ok(()),
        (Some(p), Some(e)) if p.name == name && p.configuration == *e => Ok(()),
        _ => Err(Error("TCG snapshot backend policy differs from retained factory authority; restore with the original named configured policy (absence and an explicit default are distinct)".into())),
    }
}
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|v| format!("{v:02x}")).collect()
}
pub(super) fn disk_digest(store: &Store, torn: bool) -> Result<String, Error> {
    // Actual stable bytes and ordered latest volatile sectors, not codec bytes.
    let mut volatile = Sha256::new();
    let mut count = 0u64;
    let (stable, _) = store.crashed_keeping(|sector, bytes| {
        volatile.update(sector.to_le_bytes());
        volatile.update(bytes);
        count += 1;
        false
    });
    let mut all = Sha256::new();
    all.update(b"seedbox-disk-state-v1");
    all.update(store.capacity().to_le_bytes());
    all.update([u8::from(torn)]);
    let mut sector = [0u8; SECTOR];
    for index in 0..stable.capacity() {
        stable
            .read(index, &mut sector)
            .map_err(|e| Error(format!("TCG disk evidence: {e:?}")))?;
        all.update(sector);
    }
    all.update(count.to_le_bytes());
    all.update(volatile.finalize());
    Ok(hex(&all.finalize()))
}
pub(super) fn lose(
    store: &Store,
    policy: Option<&PolicyState>,
    boot: u64,
) -> Result<(Store, u64), Error> {
    let Some(p) = policy.filter(|p| p.configuration.torn_writes) else {
        return Ok(store.crashed_keeping(|_, _| false)); // exactly zero random draws
    };
    let mut key = Sha256::new();
    key.update(b"seedbox-v1");
    key.update(p.configuration.seed.to_le_bytes());
    key.update(format!("disk-loss/{}/{boot}", p.name).as_bytes());
    let key = key.finalize();
    let mut counter = 0u64;
    let mut failed = false;
    let result = store.crashed_keeping(|_, _| {
        let Some(next) = counter.checked_add(1) else {
            failed = true;
            return false;
        };
        let mut draw = Sha256::new();
        draw.update(key);
        draw.update(counter.to_le_bytes());
        counter = next;
        let bytes = draw.finalize();
        let draw = u64::from_le_bytes(bytes[..8].try_into().expect("eight digest bytes"));
        // Match legacy floating-point threshold, including rounding near 0.5.
        (draw as f64 / (u64::MAX as f64 + 1.0)) < 0.5
    });
    if failed {
        return Err(Error("TCG disk-loss counter overflow".into()));
    }
    Ok(result)
}
