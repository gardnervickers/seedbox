//! Retain real stopped evidence one machine at a time. This is not a snapshot
//! hash or a native witness; the backend owns native evidence production.
mod witness;
use crate::{Error, Result};
use seedbox_artifacts::evidence::{
    RamRegion, StateCapture, StateDisposition, StateReport, STATE_REPORT,
};
use seedbox_driver::driver::{Driver, MachineId};
use seedbox_machine::machine::{EvidenceDisposition, Machine};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, Write},
    path::Path,
};
pub(super) use witness::collect as execution_witness;

/// Retain the real Driver-owned component separately. This is not a native
/// witness and never fills ExecutionReport::witness on its own.
pub(super) fn record_policy<M>(
    driver: &mut Driver<M>,
    output: &Path,
) -> Result<crate::store::ContentDigest>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    use sha2::{Digest, Sha256};
    struct Stream {
        hash: Sha256,
        bytes: u64,
    }
    impl Write for Stream {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.hash.update(bytes);
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| std::io::Error::other("policy evidence byte count overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut stream = Stream {
        hash: Sha256::new(),
        bytes: 0,
    };
    driver
        .with_barrier(|barrier| Ok(barrier.write_policy_evidence(&mut stream)))
        .map_err(|error| Error::Execution(format!("Driver policy evidence: {error}")))?
        .map_err(|error| Error::Integrity(format!("Driver policy evidence: {error}")))?;
    let digest = crate::store::ContentDigest::try_from(hex::encode(stream.hash.finalize()))?;
    let value = serde_json::json!({"version":1,"kind":"driver_policy","sha256":digest,"encoded_bytes":stream.bytes});
    let mut file = tempfile::Builder::new()
        .prefix(".driver-policy-")
        .tempfile_in(output)?;
    serde_json::to_writer(file.as_file_mut(), &value)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(output.join("driver-policy.json"))
        .map_err(|error| {
            Error::Execution(format!("publish Driver policy evidence: {}", error.error))
        })?;
    fs::File::open(output)?.sync_all()?;
    Ok(digest)
}

pub(super) fn disposition(value: EvidenceDisposition) -> StateDisposition {
    match value {
        EvidenceDisposition::Live => StateDisposition::Live,
        EvidenceDisposition::Crashed => StateDisposition::Crashed,
        EvidenceDisposition::Shutdown => StateDisposition::Shutdown,
        EvidenceDisposition::Exited { code } => StateDisposition::Exited { code },
    }
}

pub(super) fn publish_state<M>(
    driver: &mut Driver<M>,
    names: &BTreeMap<MachineId, String>,
    output: &Path,
) -> Result<StateReport>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    let ids = driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .map_err(|error| Error::Execution(error.to_string()))?;
    fs::create_dir_all(output.join("machines"))?;
    let mut machines = BTreeMap::new();
    for (ordinal, id) in ids.into_iter().enumerate() {
        let name = names.get(&id).ok_or_else(|| {
            Error::Integrity("stopped evidence has an unnamed Driver machine".into())
        })?;
        let evidence = driver.with_barrier(|barrier| barrier.state_evidence(id))
            .map_err(|error| Error::Execution(format!("full-state evidence for {name}: {error}")))?
            .ok_or_else(|| Error::Execution(format!("full-state evidence for {name}: backend capture is not configured or supported, or the machine has not booted and has no retained native boot evidence; no state or RAM is fabricated")))?;
        evidence.validate().map_err(|error| {
            Error::Integrity(format!("full-state evidence for {name}: {error}"))
        })?;
        let page_log = format!("machines/state-{ordinal}.pages");
        let capture = StateCapture {
            boot: evidence.boot,
            disposition: disposition(evidence.disposition),
            digest: evidence.digest.try_into()?,
            parts: evidence
                .parts
                .into_iter()
                .map(|(name, digest)| Ok((name, digest.try_into()?)))
                .collect::<Result<_>>()?,
            regions: evidence
                .regions
                .into_iter()
                .map(|region| RamRegion {
                    base: region.base,
                    length: region.length,
                })
                .collect(),
            page_log,
        };
        if capture.page_bytes()? != evidence.page_hashes.len() as u64 {
            return Err(Error::Integrity(format!(
                "full-state page length differs for {name}"
            )));
        }
        let path = output.join(&capture.page_log);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                Error::Execution(format!(
                    "full-state page evidence {}: {error}",
                    path.display()
                ))
            })?;
        file.write_all(&evidence.page_hashes)?;
        file.sync_all()?;
        if machines.insert(name.clone(), capture).is_some() {
            return Err(Error::Integrity(
                "full-state evidence has duplicate machine names".into(),
            ));
        }
        // The native capture/page vector drops here before the next getter.
    }
    let report = StateReport {
        version: 1,
        machines,
    };
    report.validate()?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".stopped-state-")
        .tempfile_in(output)?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(&mut writer, &report)?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(output.join(STATE_REPORT))
        .map_err(|error| {
            Error::Execution(format!("publish stopped-state report: {}", error.error))
        })?;
    fs::File::open(output)?.sync_all()?;
    Ok(report)
}
