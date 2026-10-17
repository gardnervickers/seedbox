//! Offline comparison of existing retained evidence; never executes a guest.

pub use crate::comparison::*;
use crate::{
    evidence::{
        page_hash_log, RamRegion, StateDigestParts, StateDisposition, StateReport, STATE_REPORT,
    },
    limits::{exceeds, read_bounded},
    record::{self as orchestration, RunRecord},
    retained,
    store::ContentDigest,
    Error, Result,
};
use seedbox_engine_protocol::trace::{InvalidTraceRecord, TraceRecord};
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

#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub time_ns: Option<u64>,
    pub kind: String,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct FirstRecord {
    pub machine: String,
    /// "trace" or "terminal"; a terminal observation cannot locate the actual first exit.
    pub source: String,
    /// One-based record number, for a trace only.
    pub record: Option<u64>,
    pub a: Option<Record>,
    pub b: Option<Record>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiffReport {
    pub status: Status,
    pub world_witness: Witness,
    pub machines: Vec<MachineDiff>,
    /// Earliest witness-log divergence across machines.
    pub divergence: Option<Divergence>,
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
const TRACE_LINE_LIMIT: u64 = 64 * 1024;

fn log(directory: &Path, record: &RunRecord, name: &str) -> Result<Option<File>> {
    crate::retained::log(directory, record, name, "diff")
}

fn load(directory: &Path) -> Result<Evidence> {
    let record =
        orchestration::inspect(directory).map_err(|e| bad(&directory.join("manifest.json"), e))?;
    let world = record.report.as_ref().and_then(|r| r.witness.clone());
    // Machine identity is retained even when full-state capture was not requested.
    // Trace/witness-log discovery must not depend on a stopped-state report.
    let mut machines: BTreeMap<_, _> = record
        .identity
        .inputs
        .guests
        .keys()
        .map(|name| (name.clone(), Machine::default()))
        .collect();
    if let Some(file) = log(directory, &record, STATE_REPORT)? {
        let path = directory.join("output").join(STATE_REPORT);
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
            machine.page_log = Some(capture.page_log);
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
fn terminal(machine: &Machine) -> Option<Record> {
    if machine.end.is_empty() {
        return None;
    }
    Some(Record {
        // A stopped capture does not claim a per-machine terminal timestamp.
        time_ns: None,
        kind: "terminal".into(),
        value: serde_json::to_value(&machine.end).ok()?,
    })
}
fn time(record: &FirstRecord) -> u64 {
    record
        .a
        .iter()
        .chain(record.b.iter())
        .filter_map(|r| r.time_ns)
        .min()
        .unwrap_or(u64::MAX)
}
fn earliest(records: Vec<FirstRecord>) -> Option<FirstRecord> {
    records
        .into_iter()
        .min_by(|a, b| (time(a), &a.machine).cmp(&(time(b), &b.machine)))
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
    let logs = divergence(a, &left, b, &right)?;
    result.limitations.extend(logs.limitations);
    result
        .evidence_differences
        .extend(logs.evidence_differences);
    if logs.divergence.is_some() {
        result.divergence = logs.divergence;
        result.status = Status::Different;
    }
    let mut traces = Vec::new();
    for machine in &result.machines {
        let name = format!("machines/{}.trace", machine.machine);
        let x = log(a, &left.record, &name)?;
        let y = log(b, &right.record, &name)?;
        match (x, y) {
            (Some(x), Some(y)) => {
                if let Some(record) = trace_diff(&machine.machine, x, y)
                    .map_err(|e| bad(a, format!("{name} compared with {}: {e}", b.display())))?
                {
                    traces.push(record);
                }
            }
            (None, None) => result.limitations.push(format!(
                "{}: no retained trace on either side",
                machine.machine
            )),
            _ => {
                result.evidence_differences.push(Difference {
                    field: format!("/logs/{}", name.replace('~', "~0").replace('/', "~1")),
                    a: left
                        .record
                        .logs
                        .get(&name)
                        .map(|d| Value::String(d.hex().into())),
                    b: right
                        .record
                        .logs
                        .get(&name)
                        .map(|d| Value::String(d.hex().into())),
                });
                result.limitations.push(format!(
                    "{}: trace missing on one side; no trace alignment available",
                    machine.machine
                ));
            }
        }
    }
    if let Some(first) = earliest(traces) {
        result.first_record = Some(first);
        result.status = Status::Different;
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
        if tx.as_ref().map(|r| &r.value) != ty.as_ref().map(|r| &r.value) {
            terminals.push(FirstRecord {
                machine: name.clone(),
                source: "terminal".into(),
                record: None,
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
        &serde_json::json!({"omitted_logs": a.record.omitted_logs, "state_report_retained": a.record.logs.contains_key(STATE_REPORT)}),
        &serde_json::json!({"omitted_logs": b.record.omitted_logs, "state_report_retained": b.record.logs.contains_key(STATE_REPORT)}),
    )?;
    for (side, evidence) in [("a", a), ("b", b)] {
        if evidence
            .record
            .report
            .as_ref()
            .is_some_and(|report| report.full_state.is_some())
            && !evidence.record.logs.contains_key(STATE_REPORT)
        {
            limitations.push(format!("{side}: {STATE_REPORT} not retained"));
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
        divergence: None,
        first_record: earliest(terminals),
        identity_differences,
        result_differences,
        evidence_differences,
        limitations,
    })
}

/// Manifest-relative witness log, beside the VMM's per-machine traces.
pub(crate) fn witness_log(machine: &str) -> String {
    format!(
        "{}/{machine}.witness-log",
        crate::evidence::MACHINE_OUTPUT_DIR
    )
}

/// Engine witness-log entry: little-endian u64 time_ns, records, witness prefix.
const SAMPLE_BYTES: usize = 24;

fn next_sample(reader: &mut impl Read) -> std::io::Result<Option<WitnessSample>> {
    let mut buffer = [0; SAMPLE_BYTES];
    let entry = &mut buffer[..];
    let mut filled = 0;
    while filled < entry.len() {
        match reader.read(&mut entry[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "witness log length is not a multiple of {} bytes",
                        entry.len()
                    ),
                ))
            }
            n => filled += n,
        }
    }
    let field = |i: usize| u64::from_le_bytes(entry[i * 8..i * 8 + 8].try_into().unwrap());
    Ok(Some(WitnessSample {
        time_ns: field(0),
        records: field(1),
        witness: format!("{:016x}", field(2)),
    }))
}

/// Compare two witness logs in lockstep. Logs are fully read so a corrupt
/// tail is reported even after the first difference.
fn compare_logs(machine: &str, a: impl Read, b: impl Read) -> Result<Option<Divergence>> {
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    let mut last = None;
    let mut first = None;
    let mut sample = 0;
    loop {
        let x = next_sample(&mut a)?;
        let y = next_sample(&mut b)?;
        if x.is_none() && y.is_none() {
            return Ok(first);
        }
        sample += 1;
        if first.is_some() {
            continue;
        }
        if x == y {
            last = x;
        } else {
            first = Some(Divergence {
                machine: machine.into(),
                sample,
                last_match: last.take(),
                a: x,
                b: y,
            });
        }
    }
}

fn start(d: &Divergence) -> (u64, u64) {
    let end = d.a.iter().chain(d.b.iter()).map(|s| s.time_ns).min();
    (
        d.last_match.as_ref().map_or(0, |s| s.time_ns),
        end.unwrap_or(u64::MAX),
    )
}

struct Logs {
    divergence: Option<Divergence>,
    evidence_differences: Vec<Difference>,
    limitations: Vec<String>,
}

fn divergence(a: &Path, left: &Evidence, b: &Path, right: &Evidence) -> Result<Logs> {
    let mut found = Vec::new();
    let mut evidence_differences = Vec::new();
    let mut limitations = Vec::new();
    let names: BTreeSet<_> = left.machines.keys().chain(right.machines.keys()).collect();
    for machine in names {
        let name = witness_log(machine);
        match (log(a, &left.record, &name)?, log(b, &right.record, &name)?) {
            (Some(x), Some(y)) => {
                if let Some(d) = compare_logs(machine, x, y)
                    .map_err(|e| bad(a, format!("{name} compared with {}: {e}", b.display())))?
                {
                    found.push(d);
                }
            }
            (None, None) => {
                limitations.push(format!("{machine}: no retained witness log on either side"))
            }
            _ => {
                let digest = |e: &Evidence| {
                    e.record
                        .logs
                        .get(&name)
                        .map(|d| Value::String(d.hex().into()))
                };
                evidence_differences.push(Difference {
                    field: format!("/logs/{}", name.replace('~', "~0").replace('/', "~1")),
                    a: digest(left),
                    b: digest(right),
                });
                limitations.push(format!(
                    "{machine}: witness log missing on one side; no divergence interval available"
                ));
            }
        }
    }
    let divergence = found
        .into_iter()
        .min_by(|x, y| (start(x), &x.machine).cmp(&(start(y), &y.machine)));
    Ok(Logs {
        divergence,
        evidence_differences,
        limitations,
    })
}

/// Witness-log divergence between a run and its replay; diagnostic only.
pub fn replay_divergence(a: &Path, b: &Path) -> Result<DivergenceReport> {
    let logs = divergence(a, &load(a)?, b, &load(b)?)?;
    Ok(DivergenceReport {
        divergence: logs.divergence,
        limitations: logs.limitations,
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
    physical_metadata: bool,
}
fn page_file(
    directory: &Path,
    evidence: &Evidence,
    machine: &Machine,
    name: &str,
) -> Result<Option<PageFile>> {
    let Some(file) = log(directory, &evidence.record, name)? else {
        return Ok(None);
    };
    let (regions, expected, physical_metadata) = if let Some(regions) = &machine.regions {
        let (digest, bytes) =
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
        (regions.clone(), bytes, true)
    } else {
        if machine
            .end
            .state_digest_parts
            .as_ref()
            .and_then(|parts| parts.get("ram_regions"))
            .is_some()
        {
            return Err(bad(directory, "physical RAM-region metadata is missing"));
        }
        let memory_mb = evidence
            .record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .memory_mb;
        if memory_mb == 0 {
            return Err(bad(directory, "page hashes require recorded memory_mb"));
        }
        let regions = vec![RamRegion {
            base: 0,
            length: u64::from(memory_mb) << 20,
        }];
        (regions, u64::from(memory_mb) * 2048, false)
    };
    if file.metadata().map_err(|e| bad(directory, e))?.len() != expected {
        return Err(bad(
            &directory.join("output").join(name),
            format!(
                "page hash length must be {expected} bytes for recorded {}RAM",
                if physical_metadata { "mapped " } else { "" }
            ),
        ));
    }
    Ok(Some(PageFile {
        file,
        regions,
        physical_metadata,
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
        let name = x.page_log.clone().unwrap_or_else(|| page_hash_log(machine));
        let other_name = y.page_log.clone().unwrap_or_else(|| page_hash_log(machine));
        let x = page_file(a, left, x, &name)?;
        let y = page_file(b, right, y, &other_name)?;
        if let (Some(x), Some(y)) = (x, y) {
            if x.regions != y.regions {
                return Err(bad(a, "compared page-hash physical RAM layouts differ"));
            }
            let pages = compare_pages(
                x.file,
                y.file,
                &x.regions,
                x.physical_metadata || y.physical_metadata,
            )
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

fn compare_pages(
    a: impl Read,
    b: impl Read,
    regions: &[RamRegion],
    physical_metadata: bool,
) -> Result<PageHashDiff> {
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
                if physical_metadata {
                    result.first_gpas.push(gpa);
                }
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

fn next_trace(reader: &mut impl BufRead) -> Result<Option<Record>> {
    let mut line = String::new();
    if reader.take(TRACE_LINE_LIMIT + 1).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if line.len() as u64 > TRACE_LINE_LIMIT {
        return Err(Error::Integrity(exceeds("trace record", TRACE_LINE_LIMIT)));
    }
    let TraceRecord {
        kind,
        time_ns,
        extra,
        rip,
    } = line
        .parse()
        .map_err(|e: InvalidTraceRecord| Error::Integrity(e.to_string()))?;
    let kind = char::from(kind).to_string();
    Ok(Some(Record {
        time_ns: Some(time_ns),
        value: serde_json::json!({"kind":kind, "time_ns":time_ns, "extra":extra, "rip":rip}),
        kind,
    }))
}
fn trace_diff(machine: &str, a: impl Read, b: impl Read) -> Result<Option<FirstRecord>> {
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    let mut index = 0;
    let mut first = None;
    loop {
        let x = next_trace(&mut a)?;
        let y = next_trace(&mut b)?;
        if x.is_none() && y.is_none() {
            break;
        }
        index += 1;
        if first.is_none() && x.as_ref().map(|r| &r.value) != y.as_ref().map(|r| &r.value) {
            first = Some(FirstRecord {
                machine: machine.into(),
                source: "trace".into(),
                record: Some(index),
                a: x,
                b: y,
            });
        }
    }
    Ok(first)
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use super::*;
    use crate as sb;
    include!("../tests/support/diff_fixture.rs");
    #[test]
    fn traces_report_first_changed_or_missing_record_and_validate_remainder() {
        let a = b"I 1 2 3\nH 5 6 7\n";
        let b = b"I 1 2 3\nP 9 6 7\n";
        let first = trace_diff("m", &a[..], &b[..]).unwrap().unwrap();
        assert_eq!(first.record, Some(2));
        assert_eq!(first.a.unwrap().kind, "H");
        assert_eq!(first.b.unwrap().time_ns, Some(9));
        let first = trace_diff("m", &a[..], &b[..8]).unwrap().unwrap();
        assert_eq!(first.record, Some(2));
        assert!(first.b.is_none());
        assert!(trace_diff("m", &a[..], &b"P 1 2 3\nbad\n"[..]).is_err());
        assert!(trace_diff("m", &a[..], &a[..]).unwrap().is_none());
    }

    fn sample(time_ns: u64, records: u64, witness: u64) -> WitnessSample {
        WitnessSample {
            time_ns,
            records,
            witness: format!("{witness:016x}"),
        }
    }

    #[test]
    fn witness_logs_report_first_differing_sample_and_validate_remainder() {
        let a = witness_log_bytes(&[[0, 1, 7], [1_000_050, 9, 8], [2_000_000, 20, 9]]);
        let b = witness_log_bytes(&[[0, 1, 7], [1_000_050, 9, 8], [2_000_000, 20, 10]]);
        assert!(compare_logs("m", &a[..], &a[..]).unwrap().is_none());
        let d = compare_logs("m", &a[..], &b[..]).unwrap().unwrap();
        assert_eq!(d.sample, 3);
        assert_eq!(d.last_match, Some(sample(1_000_050, 9, 8)));
        assert_eq!(d.a, Some(sample(2_000_000, 20, 9)));
        assert_eq!(d.b.unwrap().witness, "000000000000000a");
        // A log that ends early differs at its end; a first sample can differ.
        let d = compare_logs("m", &a[..], &b[..48]).unwrap().unwrap();
        assert_eq!((d.sample, d.b), (3, None));
        let d = compare_logs("m", &b[24..], &a[..]).unwrap().unwrap();
        assert_eq!((d.sample, d.last_match), (1, None));
        // A torn entry is an error even after the first difference.
        assert!(compare_logs("m", &a[..], &b[..71]).is_err());
        assert!(compare_logs("m", &b[..], &a[..71]).is_err());
    }

    #[test]
    fn earliest_divergence_is_chosen_by_interval_then_machine() {
        let make = |machine: &str, last: Option<u64>, end: u64| Divergence {
            machine: machine.into(),
            sample: 2,
            last_match: last.map(|t| sample(t, 1, 0)),
            a: Some(sample(end, 2, 1)),
            b: Some(sample(end + 5, 2, 2)),
        };
        let pick = |all: Vec<Divergence>| {
            all.into_iter()
                .min_by(|x, y| (start(x), &x.machine).cmp(&(start(y), &y.machine)))
                .unwrap()
                .machine
        };
        assert_eq!(
            pick(vec![make("a", Some(20), 30), make("z", Some(10), 40)]),
            "z"
        );
        assert_eq!(
            pick(vec![make("z", Some(10), 40), make("a", None, 50)]),
            "a"
        );
        assert_eq!(
            pick(vec![make("z", Some(10), 30), make("a", Some(10), 40)]),
            "z"
        );
        assert_eq!(
            pick(vec![make("z", Some(10), 30), make("a", Some(10), 30)]),
            "a"
        );
    }

    #[test]
    fn earliest_trace_is_chosen_by_time_then_machine_not_report_order() {
        let make = |machine: &str, time| FirstRecord {
            machine: machine.into(),
            source: "trace".into(),
            record: Some(2),
            a: Some(Record {
                time_ns: Some(time),
                kind: "I".into(),
                value: Value::Null,
            }),
            b: None,
        };
        assert_eq!(
            earliest(vec![make("a", 20), make("z", 10)])
                .unwrap()
                .machine,
            "z"
        );
        assert_eq!(
            earliest(vec![make("z", 10), make("a", 10)])
                .unwrap()
                .machine,
            "a"
        );
    }

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

    #[test]
    fn trace_line_limit_is_checked() {
        let bytes = vec![b'I'; 65537];
        assert!(trace_diff("m", &bytes[..], &b""[..])
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
    }
}
