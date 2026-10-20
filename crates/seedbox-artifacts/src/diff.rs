//! Offline comparison of existing retained evidence; never executes a guest.

pub use crate::comparison::*;
use crate::{
    evidence::{
        RamRegion, StateDigestParts, StateDisposition, StateReport, MACHINE_OUTPUT_DIR,
        STATE_REPORT,
    },
    limits::read_bounded,
    record::{self as orchestration, RunRecord},
    retained,
    store::ContentDigest,
    Error, Result,
};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    Match,
    Different,
}

#[derive(Clone, Debug, Serialize)]
pub struct Difference {
    /// JSON pointer into the compared value. None denotes an absent field.
    pub field: String,
    pub a: Option<Value>,
    pub b: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Witness {
    pub a: Option<ContentDigest>,
    pub b: Option<ContentDigest>,
    /// Unknown if either side did not retain this witness.
    pub matches: Option<bool>,
}
impl Witness {
    fn new(a: Option<ContentDigest>, b: Option<ContentDigest>) -> Self {
        let matches = a.as_ref().zip(b.as_ref()).map(|(a, b)| a == b);
        Self { a, b, matches }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MachineDiff {
    pub machine: String,
    pub witness: Witness,
    pub full_state: Witness,
    pub differences: Vec<Difference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_pages: Option<PageHashDiff>,
}

/// The first machine, by name, whose stopped terminal state differs. A terminal
/// observation cannot locate the actual first exit.
#[derive(Clone, Debug, Serialize)]
pub struct FirstRecord {
    pub machine: String,
    pub a: Option<Value>,
    pub b: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffReport {
    pub status: Status,
    pub world_witness: Witness,
    pub machines: Vec<MachineDiff>,
    pub first_record: Option<FirstRecord>,
    pub identity_differences: Vec<Difference>,
    pub result_differences: Vec<Difference>,
    pub evidence_differences: Vec<Difference>,
    pub limitations: Vec<String>,
}

// Only stopped semantic captures participate; host performance is not evidence.
#[derive(Clone, Debug, Default, Serialize)]
struct Machine {
    state_digest: Option<ContentDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regions: Option<Vec<RamRegion>>,
    #[serde(skip)]
    page_log: Option<String>,
    #[serde(flatten)]
    end: Terminal,
}

/// A machine's end state, which a terminal record compares.
#[derive(Clone, Debug, Default, Serialize)]
struct Terminal {
    state_digest_parts: Option<StateDigestParts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_boot: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_disposition: Option<StateDisposition>,
}

impl Terminal {
    fn is_empty(&self) -> bool {
        self.state_digest_parts.is_none()
            && self.state_boot.is_none()
            && self.state_disposition.is_none()
    }
}

struct Evidence {
    record: RunRecord,
    machines: BTreeMap<String, Machine>,
    world: Option<ContentDigest>,
}

fn bad(path: &Path, error: impl std::fmt::Display) -> Error {
    retained::bad("diff", path, error)
}

/// The engine report is metadata, not an arbitrarily large trace.
const REPORT_LIMIT: u64 = 256 * 1024 * 1024;

fn log(directory: &Path, record: &RunRecord, name: &str) -> Result<Option<File>> {
    crate::retained::log(directory, record, name, "diff")
}

fn state_report_log() -> String {
    format!("{MACHINE_OUTPUT_DIR}/{STATE_REPORT}")
}

fn load(directory: &Path) -> Result<Evidence> {
    let record =
        orchestration::inspect(directory).map_err(|e| bad(&directory.join("manifest.json"), e))?;
    let world = record.report.as_ref().and_then(|r| r.witness.clone());
    // Machine identity is retained even when full-state capture was not requested.
    let mut machines: BTreeMap<_, _> = record
        .identity
        .inputs
        .guests
        .keys()
        .map(|name| (name.clone(), Machine::default()))
        .collect();
    let report_log = state_report_log();
    if let Some(file) = log(directory, &record, &report_log)? {
        let path = directory.join("output").join(&report_log);
        let bytes = read_bounded(file, REPORT_LIMIT, "stopped-state report")
            .map_err(|error| bad(&path, error))?;
        let state: StateReport =
            serde_json::from_slice(&bytes).map_err(|error| bad(&path, error))?;
        state.validate().map_err(|error| bad(&path, error))?;
        if let Some(expected) = record
            .report
            .as_ref()
            .and_then(|report| report.full_state.as_ref())
        {
            if expected.len() != state.machines.len()
                || state
                    .machines
                    .iter()
                    .any(|(name, capture)| expected.get(name) != Some(&capture.digest))
            {
                return Err(bad(
                    &path,
                    "stopped-state inventory/digests disagree with manifest",
                ));
            }
        }
        for (name, capture) in state.machines {
            let machine = machines.entry(name).or_insert_with(Machine::default);
            if machine
                .state_digest
                .as_ref()
                .is_some_and(|digest| digest != &capture.digest)
            {
                return Err(bad(
                    &path,
                    "stopped-state digest disagrees with machine report",
                ));
            }
            machine.state_digest = Some(capture.digest);
            machine.regions = Some(capture.regions);
            // StateReport validates page_log as a relative path containing only
            // normal components. It is relative to the worker output directory,
            // alongside the report, rather than to the attempt output directory.
            machine.page_log = Some(format!("{MACHINE_OUTPUT_DIR}/{}", capture.page_log));
            machine.end.state_boot = Some(capture.boot);
            machine.end.state_disposition = Some(capture.disposition);
            let parts = StateDigestParts::try_from(
                capture
                    .parts
                    .into_iter()
                    .map(|(name, digest)| (name, digest.hex().to_owned()))
                    .collect::<BTreeMap<_, _>>(),
            )?;
            if machine
                .end
                .state_digest_parts
                .as_ref()
                .is_some_and(|old| old != &parts)
            {
                return Err(bad(
                    &path,
                    "stopped-state parts disagree with machine report",
                ));
            }
            machine.end.state_digest_parts = Some(parts);
        }
    }
    // Full state remains available in the manifest when detailed captures were omitted.
    if let Some(states) = record.report.as_ref().and_then(|r| r.full_state.as_ref()) {
        for (name, digest) in states {
            let machine = machines
                .entry(name.clone())
                .or_insert_with(Machine::default);
            if machine.state_digest.as_ref().is_some_and(|d| d != digest) {
                return Err(bad(
                    directory,
                    "machine state digest disagrees with manifest",
                ));
            }
            machine.state_digest = Some(digest.clone());
        }
    }
    Ok(Evidence {
        record,
        machines,
        world,
    })
}

fn differences(path: &str, a: Option<&Value>, b: Option<&Value>, out: &mut Vec<Difference>) {
    if a == b {
        return;
    }
    if let (Some(Value::Object(a)), Some(Value::Object(b))) = (a, b) {
        for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            differences(&format!("{path}/{escaped}"), a.get(key), b.get(key), out);
        }
    } else {
        out.push(Difference {
            field: path.into(),
            a: a.cloned(),
            b: b.cloned(),
        });
    }
}
fn compare_values(a: &impl Serialize, b: &impl Serialize) -> Result<Vec<Difference>> {
    let mut out = vec![];
    differences(
        "",
        Some(&serde_json::to_value(a)?),
        Some(&serde_json::to_value(b)?),
        &mut out,
    );
    Ok(out)
}
fn terminal(machine: &Machine) -> Option<Value> {
    if machine.end.is_empty() {
        return None;
    }
    serde_json::to_value(&machine.end).ok()
}

/// Compare run directories without requiring their original content store. Only
/// selected materialized logs are read; no runtime inputs or host probes are used.
pub fn diff(a: &Path, b: &Path) -> Result<DiffReport> {
    let left = load(a)?;
    let right = load(b)?;
    let mut result = compare(&left, &right)?;
    let pages = ram_pages(a, &left, b, &right)?;
    result.limitations.extend(pages.limitations);
    for machine in &mut result.machines {
        machine.ram_pages = pages.machines.get(&machine.machine).cloned();
    }
    if !result.evidence_differences.is_empty() {
        result.status = Status::Different;
    }
    result.limitations.push("terminal records and final hashes cannot locate an earlier unrecorded divergence; MATCH means equality of compared retained evidence, not replay qualification".into());
    Ok(result)
}

fn compare(a: &Evidence, b: &Evidence) -> Result<DiffReport> {
    let mut machines = Vec::new();
    let mut terminals = Vec::new();
    let mut limitations = Vec::new();
    let names: BTreeSet<_> = a
        .record
        .identity
        .inputs
        .guests
        .keys()
        .chain(b.record.identity.inputs.guests.keys())
        .chain(a.machines.keys())
        .chain(b.machines.keys())
        .collect();
    for name in names {
        let x = a.machines.get(name);
        let y = b.machines.get(name);
        let differences = compare_values(&x, &y)?;
        // Driver publishes a world witness, not per-machine exit witnesses.
        let witness = Witness::new(None, None);
        let full_state = Witness::new(
            x.and_then(|m| m.state_digest.clone()),
            y.and_then(|m| m.state_digest.clone()),
        );
        if witness.matches.is_none() {
            limitations.push(format!(
                "{name}: per-machine exit witness unavailable on one or both sides"
            ));
        }
        let tx = x.and_then(terminal);
        let ty = y.and_then(terminal);
        if tx != ty {
            terminals.push(FirstRecord {
                machine: name.clone(),
                a: tx,
                b: ty,
            });
        }
        machines.push(MachineDiff {
            machine: name.clone(),
            witness,
            full_state,
            differences,
            ram_pages: None,
        });
    }
    let world_witness = Witness::new(a.world.clone(), b.world.clone());
    if world_witness.matches.is_none() {
        limitations.push("world witness unavailable on one or both sides".into());
    }
    let identity_differences = compare_values(&a.record.identity, &b.record.identity)?;
    let result_differences = compare_values(
        &serde_json::json!({"outcome": a.record.outcome, "report": a.record.report, "error": a.record.error}),
        &serde_json::json!({"outcome": b.record.outcome, "report": b.record.report, "error": b.record.error}),
    )?;
    let evidence_differences = compare_values(
        &serde_json::json!({"omitted_logs": a.record.omitted_logs, "state_report_retained": a.record.logs.contains_key(&state_report_log())}),
        &serde_json::json!({"omitted_logs": b.record.omitted_logs, "state_report_retained": b.record.logs.contains_key(&state_report_log())}),
    )?;
    for (side, evidence) in [("a", a), ("b", b)] {
        if evidence
            .record
            .report
            .as_ref()
            .is_some_and(|report| report.full_state.is_some())
            && !evidence.record.logs.contains_key(&state_report_log())
        {
            limitations.push(format!("{side}: {} not retained", state_report_log()));
        }
        for (name, reason) in &evidence.record.omitted_logs {
            limitations.push(format!("{side}: omitted {name}: {reason}"));
        }
    }
    let different = a.world != b.world
        || !identity_differences.is_empty()
        || !result_differences.is_empty()
        || !evidence_differences.is_empty()
        || machines.iter().any(|m| !m.differences.is_empty());
    Ok(DiffReport {
        status: if different {
            Status::Different
        } else {
            Status::Match
        },
        world_witness,
        machines,
        // Names are visited in order, so the first is the earliest by machine.
        first_record: terminals.into_iter().next(),
        identity_differences,
        result_differences,
        evidence_differences,
        limitations,
    })
}

pub fn replay_ram_pages(a: &Path, b: &Path) -> Result<RamPageReport> {
    ram_pages(a, &load(a)?, b, &load(b)?)
}

fn ram_only(a: &Machine, b: &Machine) -> bool {
    let digests_differ = a
        .state_digest
        .as_ref()
        .zip(b.state_digest.as_ref())
        .is_some_and(|(a, b)| a != b);
    digests_differ
        && a.end
            .state_digest_parts
            .as_ref()
            .zip(b.end.state_digest_parts.as_ref())
            .is_some_and(|(a, b)| a.ram_only_difference(b))
}

struct PageFile {
    file: File,
    regions: Vec<RamRegion>,
}
/// Page evidence is read only beside a validated stopped-state capture, which
/// always records its page file and physical RAM regions.
fn page_file(directory: &Path, evidence: &Evidence, machine: &Machine) -> Result<Option<PageFile>> {
    let (Some(name), Some(regions)) = (&machine.page_log, &machine.regions) else {
        return Err(bad(directory, "physical RAM-region metadata is missing"));
    };
    let Some(file) = log(directory, &evidence.record, name)? else {
        return Ok(None);
    };
    let (digest, expected) =
        crate::evidence::region_layout(regions).map_err(|error| bad(directory, error))?;
    if machine
        .end
        .state_digest_parts
        .as_ref()
        .and_then(|parts| parts.get("ram_regions"))
        != Some(digest.hex())
    {
        return Err(bad(
            directory,
            "RAM-region metadata disagrees with state parts",
        ));
    }
    if file.metadata().map_err(|e| bad(directory, e))?.len() != expected {
        return Err(bad(
            &directory.join("output").join(name),
            format!("page hash length must be {expected} bytes for recorded mapped RAM"),
        ));
    }
    Ok(Some(PageFile {
        file,
        regions: regions.clone(),
    }))
}

fn ram_pages(a: &Path, left: &Evidence, b: &Path, right: &Evidence) -> Result<RamPageReport> {
    let mut report = RamPageReport::default();
    for (machine, x) in &left.machines {
        let Some(y) = right.machines.get(machine) else {
            continue;
        };
        if !ram_only(x, y) {
            continue;
        }
        let name = x.page_log.clone().unwrap_or_default();
        let x = page_file(a, left, x)?;
        let y = page_file(b, right, y)?;
        if let (Some(x), Some(y)) = (x, y) {
            if x.regions != y.regions {
                return Err(bad(a, "compared page-hash physical RAM layouts differ"));
            }
            let pages = compare_pages(x.file, y.file, &x.regions)
                .map_err(|e| bad(a, format!("{name} compared with {}: {e}", b.display())))?;
            report.machines.insert(machine.clone(), pages);
        } else {
            report.limitations.push(format!(
                "{machine}: page hashes unavailable on one or both sides"
            ));
        }
    }
    Ok(report)
}

fn compare_pages(a: impl Read, b: impl Read, regions: &[RamRegion]) -> Result<PageHashDiff> {
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    let mut result = PageHashDiff {
        differing_pfns: 0,
        first_pfns: vec![],
        first_gpas: vec![],
    };
    crate::evidence::region_layout(regions)?;
    let mut addresses = regions.iter().flat_map(|region| {
        (0..region.length.div_ceil(4096)).map(move |page| region.base + page * 4096)
    });
    loop {
        let mut x = [0; 8];
        let mut y = [0; 8];
        if a.fill_buf()?.is_empty() && b.fill_buf()?.is_empty() {
            break;
        }
        a.read_exact(&mut x)?;
        b.read_exact(&mut y)?;
        let gpa = addresses
            .next()
            .ok_or_else(|| Error::Integrity("page hashes exceed mapped RAM regions".into()))?;
        if u64::from_le_bytes(x) != u64::from_le_bytes(y) {
            result.differing_pfns += 1;
            if result.first_pfns.len() < 8 {
                result.first_pfns.push(gpa / 4096);
                result.first_gpas.push(gpa);
            }
        }
    }
    if addresses.next().is_some() {
        return Err(Error::Integrity(
            "page hashes do not cover mapped RAM regions".into(),
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structural_identity_differences_include_images_and_escape_keys() {
        let a = serde_json::json!({"images":{"service":{"manifest":"one", "layers":["a"]}}, "semantic_parameters":{"x/y~z":"old"}});
        let b = serde_json::json!({"images":{"service":{"manifest":"two", "layers":["b"]}}, "semantic_parameters":{"x/y~z":"new", "added":"value"}});
        let fields: Vec<_> = compare_values(&a, &b)
            .unwrap()
            .into_iter()
            .map(|d| d.field)
            .collect();
        assert_eq!(
            fields,
            [
                "/images/service/layers",
                "/images/service/manifest",
                "/semantic_parameters/added",
                "/semantic_parameters/x~1y~0z"
            ]
        );
    }
}
