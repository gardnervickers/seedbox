//! Offline comparison of existing retained evidence; never executes a guest.

use crate::{
    orchestration::{self, RunRecord},
    store::ContentDigest,
    vmm::{page_hash_log, MachineReport, StateDigestParts, VmmReport},
    Error, Result,
};
use serde::{Deserialize, Serialize};
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

/// Diagnostic evidence only; never participates in identity or replay verdicts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHashDiff {
    pub differing_pfns: u64,
    pub first_pfns: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamPageReport {
    pub machines: BTreeMap<String, PageHashDiff>,
    pub limitations: Vec<String>,
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

/// One periodic sample of a machine's running exit witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessSample {
    pub time_ns: u64,
    /// Witness records hashed so far; one per trace line.
    pub records: u64,
    /// First eight witness bytes, as 16 hex digits.
    pub witness: String,
}

/// The first interval in which two runs' witness logs disagree. The first
/// differing exit lies after `last_match` and at or before the earlier of
/// the two differing samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    pub machine: String,
    /// One-based sample number of `a`/`b`.
    pub sample: u64,
    /// None when the first samples already differ.
    pub last_match: Option<WitnessSample>,
    /// None when that run's log ended first.
    pub a: Option<WitnessSample>,
    pub b: Option<WitnessSample>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceReport {
    pub divergence: Option<Divergence>,
    pub limitations: Vec<String>,
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

// Select semantic machine fields explicitly: scheduling/performance statistics and
// host timings in vmm.json are deliberately excluded.
#[derive(Clone, Debug, Default, Serialize)]
struct Machine {
    witness: Option<ContentDigest>,
    state_digest: Option<ContentDigest>,
    state_digest_parts: Option<StateDigestParts>,
    state: Option<String>,
    time_ns: Option<u64>,
    guest_exit_code: Option<u8>,
    guest_init_error: Option<u8>,
    infrastructure_error: Option<bool>,
    pending_io: Option<Value>,
}

impl From<MachineReport> for Machine {
    fn from(report: MachineReport) -> Self {
        Self {
            witness: report.witness,
            state_digest: report.state_digest,
            state_digest_parts: report.state_digest_parts,
            state: report.state,
            time_ns: report.time_ns,
            guest_exit_code: report.guest_exit_code,
            guest_init_error: report.guest_init_error,
            infrastructure_error: Some(report.infrastructure_error),
            pending_io: report.pending_io,
        }
    }
}
struct Evidence {
    record: RunRecord,
    machines: BTreeMap<String, Machine>,
    world: Option<ContentDigest>,
    legacy_samples: bool,
}

#[derive(Deserialize)]
struct ReportHeader {
    report_version: u32,
    #[serde(default)]
    timebase: Option<String>,
    seed: u64,
}

fn bad(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Setup(format!(
        "diff {}: {error}; use an intact retained run directory (import bundles first)",
        path.display()
    ))
}

fn log(directory: &Path, record: &RunRecord, name: &str) -> Result<Option<File>> {
    crate::retained::log(directory, record, name, "diff")
}

fn load(directory: &Path) -> Result<Evidence> {
    let record =
        orchestration::inspect(directory).map_err(|e| bad(&directory.join("manifest.json"), e))?;
    let mut world = record.report.as_ref().and_then(|r| r.witness.clone());
    let mut machines = BTreeMap::new();
    let parameters = &record.identity.inputs.profile.semantic_parameters;
    let mut legacy_samples = record.identity.inputs.profile.backend == "seedbox-kvm"
        && (parameters
            .get("engine_timebase")
            .is_some_and(|v| v == "counter-pv-v1")
            || (parameters.contains_key("timebase")
                && parameters.get("engine_timebase").map(String::as_str) != Some("pv-v1")));
    if let Some(file) = log(directory, &record, "vmm.json")? {
        let path = directory.join("output/vmm.json");
        // The engine report is metadata, not an arbitrarily large trace.
        let mut bytes = Vec::new();
        file.take(256 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| bad(&path, e))?;
        if bytes.len() > 256 * 1024 * 1024 {
            return Err(bad(&path, "report exceeds 256 MiB"));
        }
        let profile = &record.identity.inputs.profile;
        let executable = crate::engine::recorded_launch(profile).map_err(|e| bad(&path, e))?;
        let arm_world = executable == crate::engine::ExecutableKind::ArmWorld;
        if (profile.backend == "seedbox-hvf") != arm_world {
            return Err(bad(
                &path,
                "HVF backend and ARM executable descriptor disagree",
            ));
        }
        let header: ReportHeader = serde_json::from_slice(&bytes).map_err(|e| bad(&path, e))?;
        if header.report_version != if arm_world { 2 } else { 1 } {
            return Err(bad(&path, "unsupported VMM report version"));
        }
        legacy_samples |= !arm_world && header.timebase.is_some();
        if arm_world {
            if header.timebase.as_deref() != Some("arm64-pv-world-v1") {
                return Err(bad(
                    &path,
                    "HVF report timebase disagrees with executable profile",
                ));
            }
            if header.seed != record.identity.seed {
                return Err(bad(&path, "VMM report seed disagrees with manifest"));
            }
        }
        let raw: VmmReport = serde_json::from_slice(&bytes).map_err(|e| bad(&path, e))?;
        if world.as_ref().is_some_and(|w| w != &raw.world_witness) {
            return Err(bad(&path, "world witness disagrees with manifest"));
        }
        world = Some(raw.world_witness);
        for entry in raw.machines {
            let name = entry.name.clone();
            if machines.insert(name, Machine::from(entry)).is_some() {
                return Err(bad(&path, "duplicate machine name"));
            }
        }
    }
    // Full state remains available in the manifest even when vmm.json was omitted.
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
        legacy_samples,
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
    if machine.state.is_none()
        && machine.time_ns.is_none()
        && machine.guest_exit_code.is_none()
        && machine.guest_init_error.is_none()
        && machine.infrastructure_error.is_none()
        && machine.pending_io.is_none()
        && machine.state_digest_parts.is_none()
    {
        return None;
    }
    Some(Record {
        time_ns: machine.time_ns,
        kind: "terminal".into(),
        value: serde_json::json!({
            "state": machine.state, "time_ns": machine.time_ns,
            "guest_exit_code": machine.guest_exit_code, "guest_init_error": machine.guest_init_error,
            "infrastructure_error": machine.infrastructure_error, "pending_io": machine.pending_io,
            "state_digest_parts": machine.state_digest_parts,
        }),
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
        let witness = Witness::new(
            x.and_then(|m| m.witness.clone()),
            y.and_then(|m| m.witness.clone()),
        );
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
        &serde_json::json!({"omitted_logs": a.record.omitted_logs, "vmm_report_retained": a.record.logs.contains_key("vmm.json")}),
        &serde_json::json!({"omitted_logs": b.record.omitted_logs, "vmm_report_retained": b.record.logs.contains_key("vmm.json")}),
    )?;
    for (side, evidence) in [("a", a), ("b", b)] {
        if !evidence.record.logs.contains_key("vmm.json") {
            limitations.push(format!("{side}: vmm.json not retained"));
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
    format!("{}/{machine}.witness-log", crate::vmm::MACHINE_OUTPUT_DIR)
}

/// Engine witness-log entry: little-endian u64 time_ns, records, witness prefix.
const SAMPLE_BYTES: usize = 24;

fn next_sample(reader: &mut impl Read, legacy: bool) -> std::io::Result<Option<WitnessSample>> {
    let mut buffer = [0; 32];
    let entry = &mut buffer[..if legacy { 32 } else { SAMPLE_BYTES }];
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
        records: field(if legacy { 2 } else { 1 }),
        witness: format!("{:016x}", field(if legacy { 3 } else { 2 })),
    }))
}

/// Compare two witness logs in lockstep. Logs are fully read so a corrupt
/// tail is reported even after the first difference.
fn compare_logs(
    machine: &str,
    a: impl Read,
    b: impl Read,
    legacy_a: bool,
    legacy_b: bool,
) -> Result<Option<Divergence>> {
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    let mut last = None;
    let mut first = None;
    let mut sample = 0;
    loop {
        let x = next_sample(&mut a, legacy_a)?;
        let y = next_sample(&mut b, legacy_b)?;
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
                if let Some(d) =
                    compare_logs(machine, x, y, left.legacy_samples, right.legacy_samples)
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
pub(crate) fn replay_divergence(a: &Path, b: &Path) -> Result<DivergenceReport> {
    let logs = divergence(a, &load(a)?, b, &load(b)?)?;
    Ok(DivergenceReport {
        divergence: logs.divergence,
        limitations: logs.limitations,
    })
}

pub(crate) fn replay_ram_pages(a: &Path, b: &Path) -> Result<RamPageReport> {
    ram_pages(a, &load(a)?, b, &load(b)?)
}

fn ram_only(a: &Machine, b: &Machine) -> bool {
    let digests_differ = a
        .state_digest
        .as_ref()
        .zip(b.state_digest.as_ref())
        .is_some_and(|(a, b)| a != b);
    digests_differ
        && a.state_digest_parts
            .as_ref()
            .zip(b.state_digest_parts.as_ref())
            .is_some_and(|(a, b)| a.ram_only_difference(b))
}

fn page_file(directory: &Path, evidence: &Evidence, name: &str) -> Result<Option<File>> {
    let Some(file) = log(directory, &evidence.record, name)? else {
        return Ok(None);
    };
    let expected = evidence
        .record
        .identity
        .inputs
        .profile
        .semantic_parameters
        .get("memory_mb")
        .and_then(|m| m.parse::<u64>().ok())
        .filter(|m| *m > 0)
        .and_then(|m| m.checked_mul(2048))
        .ok_or_else(|| bad(directory, "page hashes require recorded memory_mb"))?;
    if file.metadata().map_err(|e| bad(directory, e))?.len() != expected {
        return Err(bad(
            &directory.join("output").join(name),
            format!("page hash length must be {expected} bytes for recorded RAM"),
        ));
    }
    Ok(Some(file))
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
        let name = page_hash_log(machine);
        let x = page_file(a, left, &name)?;
        let y = page_file(b, right, &name)?;
        if let (Some(x), Some(y)) = (x, y) {
            let pages = compare_pages(x, y)
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

fn compare_pages(a: impl Read, b: impl Read) -> Result<PageHashDiff> {
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    let mut result = PageHashDiff {
        differing_pfns: 0,
        first_pfns: vec![],
    };
    let mut pfn = 0;
    loop {
        let mut x = [0; 8];
        let mut y = [0; 8];
        if a.fill_buf()?.is_empty() && b.fill_buf()?.is_empty() {
            break;
        }
        a.read_exact(&mut x)?;
        b.read_exact(&mut y)?;
        if u64::from_le_bytes(x) != u64::from_le_bytes(y) {
            result.differing_pfns += 1;
            if result.first_pfns.len() < 8 {
                result.first_pfns.push(pfn);
            }
        }
        pfn += 1;
    }
    Ok(result)
}

// Engine trace format: kind time_ns extra(hex) rip(hex).
fn next_trace(reader: &mut impl BufRead) -> Result<Option<Record>> {
    let mut line = String::new();
    if reader.take(65537).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if line.len() > 65536 {
        return Err(Error::Integrity("trace record exceeds 64 KiB".into()));
    }
    let fields: Vec<_> = line.split_whitespace().collect();
    let invalid = || {
        Error::Integrity("invalid trace record: expected kind time_ns extra(hex) rip(hex)".into())
    };
    if !matches!(fields.len(), 4 | 5) || fields[0].len() != 1 {
        return Err(invalid());
    }
    let offset = usize::from(fields.len() == 5);
    if offset != 0 {
        fields[1].parse::<u64>().map_err(|_| invalid())?;
    }
    let time_ns = fields[1 + offset].parse::<u64>().map_err(|_| invalid())?;
    let extra = u64::from_str_radix(fields[2 + offset], 16).map_err(|_| invalid())?;
    let rip = u64::from_str_radix(fields[3 + offset], 16).map_err(|_| invalid())?;
    Ok(Some(Record {
        time_ns: Some(time_ns),
        kind: fields[0].into(),
        value: serde_json::json!({"kind":fields[0], "time_ns":time_ns, "extra":extra, "rip":rip}),
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
mod tests {
    use super::*;
    use crate as sb;
    include!("../tests/support/diff_fixture.rs");

    fn arm_fixture() -> sb::orchestration::RunRecord {
        let mut record = fixture();
        record.identity.inputs.profile.backend = "seedbox-hvf".into();
        record.identity.inputs.profile.semantic_parameters = sb::engine::arm_parameters();
        record
    }

    fn arm_report() -> Value {
        let mut report = raw_report();
        report["report_version"] = 2.into();
        report["timebase"] = "arm64-pv-world-v1".into();
        report
    }

    fn arm_report_error(record: &mut sb::orchestration::RunRecord, report: &Value) -> String {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(&dir.path().join("arm"), record, Some(report), None);
        diff(&dir.path().join("arm"), &dir.path().join("arm"))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn native_hvf_reports_require_the_arm_descriptor_and_matching_header() {
        let dir = tempfile::tempdir().unwrap();
        let report = arm_report();
        write_fixture(
            &dir.path().join("a"),
            &mut arm_fixture(),
            Some(&report),
            None,
        );
        write_fixture(
            &dir.path().join("b"),
            &mut arm_fixture(),
            Some(&report),
            None,
        );
        assert_eq!(
            diff(&dir.path().join("a"), &dir.path().join("b"))
                .unwrap()
                .status,
            Status::Match
        );

        let mut record = arm_fixture();
        record.identity.inputs.profile.backend = "seedbox-kvm".into();
        let error = arm_report_error(&mut record, &report);
        assert!(
            error.contains("HVF backend and ARM executable descriptor disagree"),
            "{error}"
        );

        let mut record = arm_fixture();
        record.identity.inputs.profile.semantic_parameters =
            BTreeMap::from([("memory_mb".into(), "512".into())]);
        let error = arm_report_error(&mut record, &report);
        assert!(
            error.contains("HVF backend and ARM executable descriptor disagree"),
            "{error}"
        );

        // Descriptive clock capability metadata does not gate report decoding.
        let mut descriptive = arm_fixture();
        descriptive
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("engine_timebase".into(), "future-capabilities".into());
        write_fixture(
            &dir.path().join("descriptive"),
            &mut descriptive,
            Some(&report),
            None,
        );
        assert_eq!(
            diff(
                &dir.path().join("descriptive"),
                &dir.path().join("descriptive")
            )
            .unwrap()
            .status,
            Status::Match
        );

        let mut wrong_version = report.clone();
        wrong_version["report_version"] = 1.into();
        let error = arm_report_error(&mut arm_fixture(), &wrong_version);
        assert!(error.contains("unsupported VMM report version"), "{error}");

        let mut wrong_timebase = report.clone();
        wrong_timebase["timebase"] = "pv".into();
        let error = arm_report_error(&mut arm_fixture(), &wrong_timebase);
        assert!(
            error.contains("HVF report timebase disagrees with executable profile"),
            "{error}"
        );

        let mut wrong_seed = report.clone();
        wrong_seed["seed"] = 1.into();
        let error = arm_report_error(&mut arm_fixture(), &wrong_seed);
        assert!(
            error.contains("VMM report seed disagrees with manifest"),
            "{error}"
        );
    }

    #[test]
    fn ram_pages_are_read_where_the_vmm_writes_them() {
        // vmm.rs passes --out output/machines, so NAME.pages sits beside NAME.trace.
        let (a, b) = ram_pair();
        let dir = tempfile::tempdir().unwrap();
        for (name, raw, pfns) in [("a", &a, vec![]), ("b", &b, vec![3usize])] {
            let path = dir.path().join(name);
            let mut record = fixture();
            write_fixture(&path, &mut record, Some(raw), None);
            let mut bytes = vec![0u8; 1024 * 1024];
            for pfn in pfns {
                bytes[pfn * 8] = 1;
            }
            std::fs::write(path.join("output/machines/server.pages"), &bytes).unwrap();
            record
                .logs
                .insert("machines/server.pages".into(), ContentDigest::of(&bytes));
            write_fixture(&path, &mut record, Some(raw), None);
        }
        let report = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        assert_eq!(
            report.machines[0].ram_pages.as_ref().unwrap().first_pfns,
            [3]
        );
    }

    #[test]
    fn ram_pages_count_all_pfns_and_bound_the_sample() {
        let (a, b) = ram_pair();
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("a");
        let right = dir.path().join("b");
        let mut ra = fixture();
        let mut rb = fixture();
        write_fixture(&left, &mut ra, Some(&a), None);
        write_fixture(&right, &mut rb, Some(&b), None);
        write_pages(&left, &mut ra, &[]);
        write_pages(&right, &mut rb, &[0, 3, 9, 10, 11, 12, 13, 14, 15, 131071]);
        write_fixture(&left, &mut ra, Some(&a), None);
        write_fixture(&right, &mut rb, Some(&b), None);
        let result = diff(&left, &right).unwrap();
        let pages = result.machines[0].ram_pages.as_ref().unwrap();
        assert_eq!(pages.differing_pfns, 10);
        assert_eq!(pages.first_pfns, [0, 3, 9, 10, 11, 12, 13, 14]);
        assert_eq!(ra.run_id, rb.run_id);
        assert!(result.identity_differences.is_empty());

        // Even another textual part changing rules out a RAM-only diagnostic.
        let mut mixed = b.clone();
        mixed["machines"][1]["state_digest_parts"]["events"] = "different events".into();
        write_fixture(&right, &mut rb, Some(&mixed), None);
        assert!(diff(&left, &right)
            .unwrap()
            .machines
            .iter()
            .all(|m| m.ram_pages.is_none()));
    }

    #[test]
    fn ram_page_evidence_is_optional_but_declared_bytes_are_validated() {
        let (a, b) = ram_pair();
        let (dir, result) = pair(&a, &b);
        assert!(result
            .limitations
            .iter()
            .any(|l| l.contains("page hashes unavailable")));
        let left = dir.path().join("a");
        let right = dir.path().join("b");
        let mut ra = fixture();
        let mut rb = fixture();
        // Unregistered files on disk are ignored.
        std::fs::write(left.join("output/machines/server.pages"), b"junk").unwrap();
        assert!(diff(&left, &right).unwrap().machines[0].ram_pages.is_none());
        write_pages(&left, &mut ra, &[]);
        write_pages(&right, &mut rb, &[1]);
        write_fixture(&left, &mut ra, Some(&a), None);
        write_fixture(&right, &mut rb, Some(&b), None);
        std::fs::write(right.join("output/machines/server.pages"), b"corrupt").unwrap();
        assert!(diff(&left, &right)
            .unwrap_err()
            .to_string()
            .contains("evidence checksum mismatch"));
        for length in [7, 8, 1024 * 1024 - 8] {
            let bytes = vec![0; length];
            std::fs::write(right.join("output/machines/server.pages"), &bytes).unwrap();
            rb.logs
                .insert("machines/server.pages".into(), ContentDigest::of(&bytes));
            write_fixture(&right, &mut rb, Some(&b), None);
            assert!(diff(&left, &right)
                .unwrap_err()
                .to_string()
                .contains("page hash length must be 1048576 bytes"));
        }
        rb.logs.remove("machines/server.pages");
        rb.omitted_logs
            .insert("machines/server.pages".into(), "retention cap".into());
        write_fixture(&right, &mut rb, Some(&b), None);
        let result = diff(&left, &right).unwrap();
        assert!(result.machines[0].ram_pages.is_none());
        assert!(result
            .limitations
            .iter()
            .any(|l| l.contains("page hashes unavailable")));
    }

    #[test]
    fn page_capture_presence_alone_does_not_change_diff_or_witnesses() {
        for full_state in [false, true] {
            let a = current_report(full_state);
            let (dir, before) = pair(&a, &a);
            let mut record = fixture();
            let left = dir.path().join("a");
            write_pages(&left, &mut record, &[1]);
            write_fixture(&left, &mut record, Some(&a), None);
            let after = diff(&left, &dir.path().join("b")).unwrap();
            assert_eq!(
                serde_json::to_vec(&before).unwrap(),
                serde_json::to_vec(&after).unwrap()
            );
        }
    }

    fn pair(a: &Value, b: &Value) -> (tempfile::TempDir, DiffReport) {
        let dir = tempfile::tempdir().unwrap();
        write_fixture(&dir.path().join("a"), &mut fixture(), Some(a), None);
        write_fixture(&dir.path().join("b"), &mut fixture(), Some(b), None);
        let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        (dir, result)
    }

    #[test]
    fn current_reports_without_full_state_preserve_witnesses() {
        let a = current_report(false);
        let (_, result) = pair(&a, &a);
        assert_eq!(result.status, Status::Match);
        assert_eq!(result.world_witness.matches, Some(true));
        assert!(result.first_record.is_none());
        for machine in result.machines {
            assert_eq!(machine.witness.matches, Some(true));
            assert_eq!(machine.full_state.matches, None);
            assert!(machine.differences.is_empty());
        }
    }

    #[test]
    fn current_parts_report_ram_and_text_events_with_trace_precedence() {
        for part in ["ram", "events"] {
            let a = current_report(true);
            let mut b = a.clone();
            b["machines"][0]["state_digest_parts"][part] = if part == "events" {
                "exc:none int:none shadow:1 nmi:0/0/0".into()
            } else {
                serde_json::to_value(ContentDigest::of(b"changed ram")).unwrap()
            };
            let (_, result) = pair(&a, &b);
            let first = result.first_record.unwrap();
            assert_eq!(first.machine, "workload");
            assert_eq!(first.source, "terminal");
            assert_eq!(
                first.a.unwrap().value["state_digest_parts"][part],
                a["machines"][0]["state_digest_parts"][part]
            );
            assert!(result.machines[1]
                .differences
                .iter()
                .any(|d| d.field == format!("/state_digest_parts/{part}")));

            let dir = tempfile::tempdir().unwrap();
            write_fixture(
                &dir.path().join("a"),
                &mut fixture(),
                Some(&a),
                Some(b"I 1 2 3\n"),
            );
            write_fixture(
                &dir.path().join("b"),
                &mut fixture(),
                Some(&b),
                Some(b"H 1 2 3\n"),
            );
            let first = diff(&dir.path().join("a"), &dir.path().join("b"))
                .unwrap()
                .first_record
                .unwrap();
            assert_eq!(first.source, "trace");
            assert_eq!(first.record, Some(1));
        }
    }

    #[test]
    fn performance_statistics_and_machine_order_do_not_diverge() {
        let mut a = raw_report();
        let mut second = a["machines"][0].clone();
        second["name"] = "other".into();
        a["machines"].as_array_mut().unwrap().push(second);
        let mut b = a.clone();
        b["machines"][0]["run_ms"] = 999.into();
        b["machines"][0]["shadow"] = serde_json::json!({"synced_pages":999, "restored_pages":1});
        b["machines"].as_array_mut().unwrap().reverse();
        assert_eq!(pair(&a, &b).1.status, Status::Match);
    }

    #[test]
    fn world_difference_does_not_invent_a_machine_or_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = fixture();
        let mut raw = raw_report();
        let digest = ContentDigest::of(b"world changed, machines equal");
        b.report.as_mut().unwrap().witness = Some(digest.clone());
        raw["world_witness"] = serde_json::to_value(digest).unwrap();
        write_fixture(
            &dir.path().join("a"),
            &mut fixture(),
            Some(&raw_report()),
            None,
        );
        write_fixture(&dir.path().join("b"), &mut b, Some(&raw), None);
        let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        assert_eq!(result.world_witness.matches, Some(false));
        assert!(result
            .machines
            .iter()
            .all(|m| m.witness.matches == Some(true)));
        assert!(result.first_record.is_none());
    }

    #[test]
    fn machine_hash_and_full_state_differences_have_no_claimed_exit_time() {
        let a = raw_report();
        let mut b = a.clone();
        b["machines"][0]["witness"] = serde_json::to_value(ContentDigest::of(b"changed")).unwrap();
        b["machines"][0]["state_digest"] = serde_json::to_value(ContentDigest::of(b"ram")).unwrap();
        let (_, result) = pair(&a, &b);
        assert_eq!(result.status, Status::Different);
        assert_eq!(result.machines[0].witness.matches, Some(false));
        assert!(result.first_record.is_none());
    }

    #[test]
    fn terminal_difference_reports_machine_time_and_kind() {
        let a = raw_report();
        let mut b = a.clone();
        b["machines"][0]["guest_exit_code"] = 1.into();
        b["machines"][0]["time_ns"] = 105.into();
        let (_, result) = pair(&a, &b);
        let first = result.first_record.unwrap();
        assert_eq!(first.machine, "workload");
        assert_eq!(first.source, "terminal");
        assert_eq!(first.a.unwrap().time_ns, Some(100));
        assert_eq!(first.b.unwrap().time_ns, Some(105));
    }

    #[test]
    fn identity_lists_semantics_and_ignores_attempt_and_performance() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = fixture();
        b.attempt = "another".into();
        b.performance_parameters
            .insert("SEEDBOX_UNDO".into(), "0".into());
        write_fixture(&dir.path().join("a"), &mut fixture(), None, None);
        write_fixture(&dir.path().join("b"), &mut b, None, None);
        assert_eq!(
            diff(&dir.path().join("a"), &dir.path().join("b"))
                .unwrap()
                .status,
            Status::Match
        );
        b.identity.seed = 2;
        b.identity.inputs.kernel = ContentDigest::of(b"kernel");
        b.identity.inputs.profile.engine = ContentDigest::of(b"engine");
        b.identity
            .inputs
            .profile
            .semantic_parameters
            .insert("memory_mb".into(), "1024".into());
        write_fixture(&dir.path().join("b"), &mut b, None, None);
        let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        let paths: Vec<_> = result
            .identity_differences
            .iter()
            .map(|d| d.field.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "/inputs/kernel",
                "/inputs/profile/engine",
                "/inputs/profile/semantic_parameters/memory_mb",
                "/seed"
            ]
        );
    }

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
        assert!(compare_logs("m", &a[..], &a[..], false, false)
            .unwrap()
            .is_none());
        let d = compare_logs("m", &a[..], &b[..], false, false)
            .unwrap()
            .unwrap();
        assert_eq!(d.sample, 3);
        assert_eq!(d.last_match, Some(sample(1_000_050, 9, 8)));
        assert_eq!(d.a, Some(sample(2_000_000, 20, 9)));
        assert_eq!(d.b.unwrap().witness, "000000000000000a");
        // A log that ends early differs at its end; a first sample can differ.
        let d = compare_logs("m", &a[..], &b[..48], false, false)
            .unwrap()
            .unwrap();
        assert_eq!((d.sample, d.b), (3, None));
        let d = compare_logs("m", &b[24..], &a[..], false, false)
            .unwrap()
            .unwrap();
        assert_eq!((d.sample, d.last_match), (1, None));
        // A torn entry is an error even after the first difference.
        assert!(compare_logs("m", &a[..], &b[..71], false, false).is_err());
        assert!(compare_logs("m", &b[..], &a[..71], false, false).is_err());
    }

    #[test]
    fn legacy_witness_samples_and_traces_remain_readable() {
        // Three old samples total 96 bytes, also a multiple of the new size:
        // byte length alone cannot select the encoding.
        let legacy: Vec<u8> = [[0u64, 0, 1, 7], [1_000_000, 2, 9, 8], [2_000_000, 3, 20, 9]]
            .into_iter()
            .flatten()
            .flat_map(u64::to_le_bytes)
            .collect();
        let current = witness_log_bytes(&[[0, 1, 7], [1_000_000, 9, 8], [2_000_000, 20, 9]]);
        assert!(compare_logs("m", &legacy[..], &current[..], true, false)
            .unwrap()
            .is_none());
        assert!(compare_logs("m", &legacy[..95], &current[..], true, false).is_err());
        assert!(trace_diff("m", &b"I 99 2 3 4\n"[..], &b"I 2 3 4\n"[..])
            .unwrap()
            .is_none());
        assert!(trace_diff("m", &b"I bad 2 3 4\n"[..], &b"I 2 3 4\n"[..]).is_err());
    }

    #[test]
    fn replacement_worker_uses_current_samples_with_historical_clock_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture();
        record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("engine_timebase".into(), "pv-v1".into());
        record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("timebase".into(), "pv".into());
        let bytes = witness_log_bytes(&[[0, 1, 7]]);
        write_fixture_logs(
            dir.path(),
            &mut record,
            Some(&raw_report()),
            &[("machines/workload.witness-log", Some(&bytes))],
        );
        assert!(!load(dir.path()).unwrap().legacy_samples);
        assert_eq!(diff(dir.path(), dir.path()).unwrap().status, Status::Match);
    }

    #[test]
    fn arm_descriptive_metadata_cannot_select_legacy_x86_samples() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = arm_fixture();
        record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("engine_timebase".into(), "counter-pv-v1".into());
        record
            .identity
            .inputs
            .profile
            .semantic_parameters
            .insert("timebase".into(), "pv".into());
        let mut report = raw_report();
        report["report_version"] = 2.into();
        report["timebase"] = "arm64-pv-world-v1".into();
        write_fixture(dir.path(), &mut record, Some(&report), None);
        assert!(!load(dir.path()).unwrap().legacy_samples);
    }

    #[test]
    fn diff_uses_retained_report_to_decode_legacy_samples() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = [0u64, 17, 1, 7]
            .into_iter()
            .flat_map(u64::to_le_bytes)
            .collect();
        let mut report = raw_report();
        report["timebase"] = "pv".into();
        for side in ["a", "b"] {
            write_fixture_logs(
                &dir.path().join(side),
                &mut fixture(),
                Some(&report),
                &[("machines/workload.witness-log", Some(&bytes))],
            );
        }
        assert_eq!(
            diff(&dir.path().join("a"), &dir.path().join("b"))
                .unwrap()
                .status,
            Status::Match
        );
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
    fn diff_reports_witness_log_divergence_and_one_sided_logs() {
        let dir = tempfile::tempdir().unwrap();
        let name = "machines/workload.witness-log";
        let x = witness_log_bytes(&[[0, 1, 7], [1_000_000, 4, 8]]);
        let y = witness_log_bytes(&[[0, 1, 7], [1_000_000, 4, 6]]);
        for (side, log) in [("a", Some(&x[..])), ("b", Some(&y[..])), ("c", None)] {
            write_fixture_logs(
                &dir.path().join(side),
                &mut fixture(),
                Some(&raw_report()),
                &[(name, log)],
            );
        }
        let result = diff(&dir.path().join("a"), &dir.path().join("a")).unwrap();
        assert_eq!(result.status, Status::Match);
        assert!(result.divergence.is_none());
        let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        assert_eq!(result.status, Status::Different);
        let d = result.divergence.unwrap();
        assert_eq!((d.machine.as_str(), d.sample), ("workload", 2));
        assert_eq!(d.last_match.unwrap().time_ns, 0);
        let report = replay_divergence(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        assert_eq!(report.divergence.unwrap().sample, 2);
        let result = diff(&dir.path().join("a"), &dir.path().join("c")).unwrap();
        assert_eq!(result.status, Status::Different);
        assert!(result.divergence.is_none());
        assert!(result
            .evidence_differences
            .iter()
            .any(|d| d.field == "/logs/machines~1workload.witness-log"));
        let result = diff(&dir.path().join("c"), &dir.path().join("c")).unwrap();
        assert_eq!(result.status, Status::Match);
        assert!(result
            .limitations
            .iter()
            .any(|l| l == "workload: no retained witness log on either side"));
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
    fn absent_and_omitted_evidence_stays_explicit_and_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture();
        record.report = None;
        record.outcome = sb::orchestration::Outcome::Incomplete;
        record
            .omitted_logs
            .insert("vmm.json".into(), "over quota".into());
        write_fixture(dir.path(), &mut record, None, None);
        std::fs::write(dir.path().join("output/vmm.json"), "not retained").unwrap();
        let result = diff(dir.path(), dir.path()).unwrap();
        assert_eq!(result.world_witness.matches, None);
        assert!(result.first_record.is_none());
        assert!(result.limitations.iter().any(|s| s.contains("over quota")));
    }

    #[test]
    fn full_state_survives_omitted_raw_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = fixture();
        record.report.as_mut().unwrap().full_state = Some(BTreeMap::from([(
            "workload".into(),
            ContentDigest::of(b"ram"),
        )]));
        write_fixture(dir.path(), &mut record, None, None);
        let result = diff(dir.path(), dir.path()).unwrap();
        assert_eq!(result.machines[0].full_state.matches, Some(true));
        assert_eq!(result.machines[0].witness.matches, None);
    }
    #[test]
    fn full_state_only_difference_does_not_fabricate_terminal_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = fixture();
        b.report.as_mut().unwrap().full_state = Some(BTreeMap::from([(
            "workload".into(),
            ContentDigest::of(b"ram"),
        )]));
        write_fixture(&dir.path().join("a"), &mut fixture(), None, None);
        write_fixture(&dir.path().join("b"), &mut b, None, None);
        let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
        assert_eq!(result.status, Status::Different);
        assert!(result.first_record.is_none());
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
