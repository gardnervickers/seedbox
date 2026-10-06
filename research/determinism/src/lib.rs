//! Completed-boundary witness. This is an observation oracle, not a state saver.
//! Producers must canonically enumerate every future-relevant field; omission is
//! not discovered by hashing. Names and bytes are schema-owned, never host pointers.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub type Fields = BTreeMap<String, Vec<u8>>;
pub type Digests = BTreeMap<String, String>;

/// Flatten W1 World::snapshot() (or typed device JSON) into stable field paths.
/// Container type, keys and length are observed too, including empty containers.
/// Arrays retain semantic order. Objects sort keys regardless of serde features.
pub fn json_fields(prefix: &str, value: &serde_json::Value) -> Fields {
    fn visit(path: String, value: &serde_json::Value, out: &mut Fields) {
        let descriptor = match value {
            serde_json::Value::Object(map) => {
                let sorted: BTreeMap<_, _> = map.iter().collect();
                for (key, child) in &sorted {
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    visit(format!("{path}/{escaped}"), child, out);
                }
                serde_json::json!({"type":"object","keys":sorted.keys().collect::<Vec<_>>()})
            }
            serde_json::Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    visit(format!("{path}/{index}"), child, out);
                }
                serde_json::json!({"type":"array","length":values.len()})
            }
            scalar => serde_json::json!({"type":"scalar","value":scalar}),
        };
        out.insert(path, serde_json::to_vec(&descriptor).unwrap());
    }
    let mut out = Fields::new();
    visit(prefix.to_string(), value, &mut out);
    out
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct PageId {
    pub machine: u64,
    pub incarnation: u64,
    pub slot: u32,
    pub gpa: u64,
}
impl PageId {
    pub fn key(&self) -> String {
        format!(
            "machine/{}/boot/{}/slot/{}/gpa/{:016x}",
            self.machine, self.incarnation, self.slot, self.gpa
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page {
    pub id: PageId,
    pub bytes: Vec<u8>,
}

/// Pinned experiment inputs, coverage declaration and schema participate in the
/// root hash. Unsupported/hidden state must be declared, never zero-filled.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub schema: String,
    pub inputs: Digests,
    pub observed: Vec<String>,
    pub unavailable: Vec<String>,
    pub counter: CounterProfile,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CounterProfile {
    pub source: String,
    pub profile_id: String,
    pub width_bits: Option<u8>,
    /// Pinned event/CPU/filter configuration and exact counting semantics.
    pub configuration: Fields,
    pub validation: CounterValidation,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CounterValidation {
    SyntheticOnly,
    UnvalidatedHardware,
    Validated {
        evidence_sha256: String,
        scope: String,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct CpuId {
    pub machine: u64,
    pub incarnation: u64,
    pub vcpu: u32,
}
impl CpuId {
    pub fn prefix(&self) -> String {
        format!(
            "machine/{}/boot/{}/cpu/{}",
            self.machine, self.incarnation, self.vcpu
        )
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BoundaryKind {
    NaturalVmExit { reason: u32, semantic_tag: String },
    PrecisePreemption,
    CooperativeYield { tag: String },
    Lifecycle { tag: String },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CountValidity {
    Valid,
    Unavailable,
    Invalid { reason: String },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OverflowState {
    None,
    Accounted { wraps: u64 },
    Unaccounted,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BranchCount {
    pub source: String,
    pub profile_id: String,
    /// Epoch-relative guest branch count, including debugger correction execution.
    pub value: Option<u64>,
    pub validity: CountValidity,
    pub overflow: OverflowState,
    pub epoch: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LandingIdentity {
    pub count: BranchCount,
    pub rip: u64,
    /// Include RCX/RSI/RDI/RFLAGS for REP, and relevant interrupt/debug transition
    /// state. Complete all-machine architectural state remains separately required.
    pub registers: Fields,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BoundaryIdentity {
    pub kind: BoundaryKind,
    pub cpu: CpuId,
    pub landing: LandingIdentity,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CorrectionObservation {
    pub cpu: CpuId,
    pub phase: String,
    pub landing: LandingIdentity,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub identity: BoundaryIdentity,
    /// Complete live machine/vCPU inventory at this global boundary.
    pub cpu_inventory: Vec<CpuId>,
    pub cpu: Fields,
    pub world: Fields,
    /// Complete current RAM topology, including lengths and boot generations.
    pub layout: BTreeMap<String, usize>,
    pub dirty: Vec<Page>,
    /// Required initially and at checkpoints. Used to audit dirty-log coverage.
    pub full: Option<Vec<Page>>,
}
/// Minimal W1/W2 adapter. Capture AFTER completion-only KVM_RUN reentry and
/// BEFORE world mutation, IRQ injection, selecting another CPU or advancing time.
/// Serialize KVM_RUN's meaningful exit/continuation fields as CPU fields.
pub trait BoundarySource {
    fn profile(&self) -> Profile;
    fn capture(&mut self, request: CaptureRequest) -> Result<Observation, String>;
}
#[derive(Clone, Copy, Debug)]
pub struct CaptureRequest {
    pub boundary: u64,
    pub full_memory: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Detail {
    pub cpu: Fields,
    pub world: Fields,
    pub pages: Fields,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Boundary {
    pub index: u64,
    pub state_hash: String,
    pub chain_hash: String,
    pub identity: BoundaryIdentity,
    pub cpu: Digests,
    pub world: Digests,
    pub pages: Digests,
    pub changed_pages: Digests,
    pub detail: Option<Detail>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub profile: Profile,
    pub root: String,
    pub boundaries: Vec<Boundary>,
    /// Diagnostic correction exits; excluded from state/chain/compare and indices.
    pub corrections: Vec<CorrectionObservation>,
}

pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn object_hash<T: Serialize>(v: &T) -> String {
    hash(&serde_json::to_vec(v).expect("serialization of canonical struct"))
}
fn digests(fields: &Fields) -> Digests {
    fields.iter().map(|(k, v)| (k.clone(), hash(v))).collect()
}
fn collect_pages(pages: Vec<Page>) -> Result<Fields, String> {
    let mut out = Fields::new();
    for page in pages {
        if out.insert(page.id.key(), page.bytes).is_some() {
            return Err("duplicate RAM page identity".into());
        }
    }
    Ok(out)
}

pub struct Witness {
    pub trace: Trace,
    pages: Fields,
    page_hashes: Digests,
    checkpoint_every: u64,
    detail_interval: Option<(u64, u64)>,
}
impl Witness {
    pub fn new(
        mut profile: Profile,
        checkpoint_every: u64,
        detail_interval: Option<(u64, u64)>,
    ) -> Result<Self, String> {
        if checkpoint_every == 0 {
            return Err("checkpoint cadence must be positive".into());
        }
        profile.observed.sort();
        profile.unavailable.sort();
        let root = object_hash(&("seedbox-witness-root-v2", &profile));
        Ok(Self {
            trace: Trace {
                profile,
                root,
                boundaries: vec![],
                corrections: vec![],
            },
            pages: Fields::new(),
            page_hashes: Digests::new(),
            checkpoint_every,
            detail_interval,
        })
    }
    /// Intermediate debugger/signal-return observations only. Natural PIO/MMIO,
    /// device and other semantic exits must use observe(), including during
    /// correction. Keep all correction guest execution in the final count and
    /// cumulative RAM changes; do not clear dirty logs or advance world here.
    pub fn record_correction(&mut self, observation: CorrectionObservation) {
        self.trace.corrections.push(observation);
    }
    pub fn request(&self) -> CaptureRequest {
        let boundary = self.trace.boundaries.len() as u64;
        CaptureRequest {
            boundary,
            full_memory: boundary.is_multiple_of(self.checkpoint_every),
        }
    }
    pub fn observe_source(&mut self, source: &mut impl BoundarySource) -> Result<(), String> {
        if object_hash(&source.profile()) != object_hash(&self.trace.profile) {
            // Compare normalized profile so adapter ordering is immaterial.
            let mut profile = source.profile();
            profile.observed.sort();
            profile.unavailable.sort();
            if object_hash(&profile) != object_hash(&self.trace.profile) {
                return Err("adapter profile changed".into());
            }
        }
        let request = self.request();
        self.observe(source.capture(request)?)
    }
    pub fn observe(&mut self, observation: Observation) -> Result<(), String> {
        let request = self.request();
        let count = &observation.identity.landing.count;
        if count.source != self.trace.profile.counter.source
            || count.profile_id != self.trace.profile.counter.profile_id
        {
            return Err("landing counter source/profile mismatch".into());
        }
        if count.validity == CountValidity::Valid
            && (count.value.is_none() || count.overflow == OverflowState::Unaccounted)
        {
            return Err("valid count lacks value or has unaccounted overflow".into());
        }
        if matches!(observation.identity.kind, BoundaryKind::PrecisePreemption)
            && count.validity != CountValidity::Valid
        {
            return Err("precise preemption needs a valid declared count".into());
        }
        let inventory: BTreeSet<_> = observation.cpu_inventory.iter().collect();
        if inventory.len() != observation.cpu_inventory.len()
            || !inventory.contains(&observation.identity.cpu)
        {
            return Err("duplicate CPU inventory or landing CPU absent".into());
        }
        for id in &observation.cpu_inventory {
            for register in ["rip", "rflags"] {
                if !observation
                    .cpu
                    .contains_key(&format!("{}/{register}", id.prefix()))
                {
                    return Err(format!(
                        "CPU inventory missing register: {}/{register}",
                        id.prefix()
                    ));
                }
            }
        }
        let prefix = observation.identity.cpu.prefix();
        for register in ["rcx", "rsi", "rdi", "rflags"] {
            if !observation
                .identity
                .landing
                .registers
                .contains_key(register)
            {
                return Err(format!(
                    "x86 landing missing REP/flags register: {register}"
                ));
            }
        }
        if observation.cpu.get(&format!("{prefix}/rip"))
            != Some(&observation.identity.landing.rip.to_le_bytes().to_vec())
        {
            return Err("landing RIP disagrees with captured CPU".into());
        }
        for (register, value) in &observation.identity.landing.registers {
            if observation.cpu.get(&format!("{prefix}/{register}")) != Some(value) {
                return Err(format!(
                    "landing register disagrees with captured CPU: {register}"
                ));
            }
        }
        let mut dirty = collect_pages(observation.dirty)?;
        for (key, bytes) in &dirty {
            if observation.layout.get(key) != Some(&bytes.len()) {
                return Err(format!("dirty page outside layout/size: {key}"));
            }
        }
        if let Some(full) = observation.full {
            let full = collect_pages(full)?;
            let sizes: BTreeMap<_, _> = full.iter().map(|(k, v)| (k.clone(), v.len())).collect();
            if sizes != observation.layout {
                return Err("full RAM/layout mismatch".into());
            }
            if request.boundary != 0 {
                let missing = full
                    .iter()
                    .find(|(k, v)| dirty.get(*k).or_else(|| self.pages.get(*k)) != Some(*v))
                    .map(|(k, _)| k.as_str());
                if let Some(missing) = missing {
                    return Err(format!(
                        "dirty-log omission at checkpoint {}: {missing}",
                        request.boundary
                    ));
                }
            } else {
                dirty = full;
            }
        } else if request.full_memory {
            return Err(format!(
                "full RAM required at boundary {}",
                request.boundary
            ));
        }
        for (key, length) in &observation.layout {
            if dirty.get(key).or_else(|| self.pages.get(key)).map(Vec::len) != Some(*length) {
                return Err(format!("new RAM page lacks content: {key}"));
            }
        }
        let mut cpu_fields = observation.cpu;
        let mut inventory = observation.cpu_inventory;
        inventory.sort();
        cpu_fields.insert(
            "cpu_inventory".into(),
            serde_json::to_vec(&inventory).unwrap(),
        );
        let cpu = digests(&cpu_fields);
        let world = digests(&observation.world);
        // Only updated page CONTENT is hashed between audits. Unchanged RAM is
        // neither cloned nor rehashed. The ordered digest map is still O(pages).
        self.pages
            .retain(|key, _| observation.layout.contains_key(key));
        self.page_hashes
            .retain(|key, _| observation.layout.contains_key(key));
        let mut changed_pages = Digests::new();
        for (key, bytes) in dirty {
            let digest = hash(&bytes);
            if self.page_hashes.get(&key) != Some(&digest) {
                changed_pages.insert(key.clone(), digest.clone());
            }
            self.page_hashes.insert(key.clone(), digest);
            self.pages.insert(key, bytes);
        }
        let state_hash = object_hash(&(
            "seedbox-state-v2",
            &observation.identity,
            &cpu,
            &world,
            &self.page_hashes,
        ));
        let previous = self
            .trace
            .boundaries
            .last()
            .map(|b| b.chain_hash.as_str())
            .unwrap_or(&self.trace.root);
        let chain_hash = object_hash(&(
            "seedbox-boundary-v2",
            previous,
            request.boundary,
            &state_hash,
        ));
        let detailed = request.full_memory
            || self
                .detail_interval
                .is_some_and(|(start, end)| (start..=end).contains(&request.boundary));
        let detail = detailed.then(|| Detail {
            cpu: cpu_fields,
            world: observation.world,
            pages: self.pages.clone(),
        });
        self.trace.boundaries.push(Boundary {
            index: request.boundary,
            state_hash,
            chain_hash,
            identity: observation.identity,
            cpu,
            world,
            pages: self.page_hashes.clone(),
            changed_pages,
            detail,
        });
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Difference {
    pub boundary: Option<u64>,
    pub category: String,
    pub field: String,
    pub left_hash: Option<String>,
    pub right_hash: Option<String>,
    pub left_bytes: Option<String>,
    pub right_bytes: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct RunConfig {
    pub checkpoint_every: u64,
    pub detail_interval: Option<(u64, u64)>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Investigation {
    pub first_difference: Option<Difference>,
    pub detailed_difference: Option<Difference>,
    pub rerun_reproduced_original_chains: Option<bool>,
    pub compact: [Trace; 2],
    pub detailed: Option<[Trace; 2]>,
}
/// Adapter supplies FRESH regeneration from complete pinned inputs on each call.
/// Side selects only test/perturbation configuration. No trace/checkpoint/log is
/// passed to the generator. Rerun changes observation detail for a bounded window.
/// Uncontrolled leaks may recur with different values; reproduction is reported.
pub fn investigate(
    checkpoint_every: u64,
    mut regenerate: impl FnMut(bool, RunConfig) -> Result<Trace, String>,
) -> Result<Investigation, String> {
    if checkpoint_every == 0 {
        return Err("checkpoint cadence must be positive".into());
    }
    let config = RunConfig {
        checkpoint_every,
        detail_interval: None,
    };
    let compact = [regenerate(false, config)?, regenerate(true, config)?];
    let first_difference = compare(&compact[0], &compact[1]);
    let Some(boundary) = first_difference.as_ref().and_then(|d| d.boundary) else {
        return Ok(Investigation {
            first_difference,
            detailed_difference: None,
            rerun_reproduced_original_chains: None,
            compact,
            detailed: None,
        });
    };
    let config = RunConfig {
        checkpoint_every,
        detail_interval: Some((
            boundary / checkpoint_every * checkpoint_every,
            boundary.saturating_add(1),
        )),
    };
    let detailed = [regenerate(false, config)?, regenerate(true, config)?];
    let reproduced = compare(&compact[0], &detailed[0]).is_none()
        && compare(&compact[1], &detailed[1]).is_none();
    let detailed_difference = compare(&detailed[0], &detailed[1]);
    Ok(Investigation {
        first_difference,
        detailed_difference,
        rerun_reproduced_original_chains: Some(reproduced),
        compact,
        detailed: Some(detailed),
    })
}

pub fn compare(left: &Trace, right: &Trace) -> Option<Difference> {
    if left.root != right.root {
        return Some(Difference {
            boundary: None,
            category: "inputs/profile".into(),
            field: "root".into(),
            left_hash: Some(left.root.clone()),
            right_hash: Some(right.root.clone()),
            left_bytes: None,
            right_bytes: None,
        });
    }
    for (a, b) in left.boundaries.iter().zip(&right.boundaries) {
        if a.chain_hash == b.chain_hash {
            continue;
        }
        let identities = |i: &BoundaryIdentity| {
            let mut out = Fields::new();
            for (key, value) in [
                ("boundary/kind", serde_json::to_vec(&i.kind).unwrap()),
                ("landing/cpu", serde_json::to_vec(&i.cpu).unwrap()),
                (
                    "landing/count/source",
                    serde_json::to_vec(&i.landing.count.source).unwrap(),
                ),
                (
                    "landing/count/profile_id",
                    serde_json::to_vec(&i.landing.count.profile_id).unwrap(),
                ),
                (
                    "landing/count/value",
                    serde_json::to_vec(&i.landing.count.value).unwrap(),
                ),
                (
                    "landing/count/validity",
                    serde_json::to_vec(&i.landing.count.validity).unwrap(),
                ),
                (
                    "landing/count/overflow",
                    serde_json::to_vec(&i.landing.count.overflow).unwrap(),
                ),
                (
                    "landing/count/epoch",
                    serde_json::to_vec(&i.landing.count.epoch).unwrap(),
                ),
                ("landing/rip", i.landing.rip.to_le_bytes().to_vec()),
            ] {
                out.insert(key.into(), value);
            }
            out.extend(
                i.landing
                    .registers
                    .iter()
                    .map(|(key, value)| (format!("landing/registers/{key}"), value.clone())),
            );
            out
        };
        let ai = identities(&a.identity);
        let bi = identities(&b.identity);
        for key in ai.keys().chain(bi.keys()).collect::<BTreeSet<_>>() {
            if ai.get(key) == bi.get(key) {
                continue;
            }
            let category = if key == "boundary/kind" {
                "boundary-kind"
            } else if key.starts_with("landing/count/") {
                "branch-count"
            } else if key == "landing/rip" {
                "landing/RIP"
            } else if key.starts_with("landing/registers/") {
                "landing/register"
            } else {
                "landing/cpu"
            };
            return Some(Difference {
                boundary: Some(a.index),
                category: category.into(),
                field: key.clone(),
                left_hash: ai.get(key).map(|v| hash(v)),
                right_hash: bi.get(key).map(|v| hash(v)),
                left_bytes: ai.get(key).map(hex::encode),
                right_bytes: bi.get(key).map(hex::encode),
            });
        }
        for (category, am, bm) in [
            ("register/CPU", &a.cpu, &b.cpu),
            ("page", &a.pages, &b.pages),
            ("world", &a.world, &b.world),
        ] {
            let keys: BTreeSet<_> = am.keys().chain(bm.keys()).collect();
            for key in keys {
                if am.get(key) == bm.get(key) {
                    continue;
                }
                let bytes = |boundary: &Boundary| {
                    boundary
                        .detail
                        .as_ref()
                        .and_then(|d| match category {
                            "register/CPU" => d.cpu.get(key),
                            "page" => d.pages.get(key),
                            _ => d.world.get(key),
                        })
                        .map(hex::encode)
                };
                return Some(Difference {
                    boundary: Some(a.index),
                    category: category.into(),
                    field: key.clone(),
                    left_hash: am.get(key).cloned(),
                    right_hash: bm.get(key).cloned(),
                    left_bytes: bytes(a),
                    right_bytes: bytes(b),
                });
            }
        }
        return Some(Difference {
            boundary: Some(a.index),
            category: "chain/schema".into(),
            field: "chain_hash".into(),
            left_hash: Some(a.chain_hash.clone()),
            right_hash: Some(b.chain_hash.clone()),
            left_bytes: None,
            right_bytes: None,
        });
    }
    if left.boundaries.len() != right.boundaries.len() {
        return Some(Difference {
            boundary: Some(left.boundaries.len().min(right.boundaries.len()) as u64),
            category: "trace".into(),
            field: "length".into(),
            left_hash: None,
            right_hash: None,
            left_bytes: None,
            right_bytes: None,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_adapter_preserves_structure_and_escaped_names() {
        let a = json_fields(
            "world",
            &serde_json::json!({"a/b":[],"a":{"b":[]},"empty":{}}),
        );
        assert!(a.contains_key("world/a~1b"));
        assert!(a.contains_key("world/a/b"));
        assert!(a.contains_key("world/empty"));
        assert_ne!(
            json_fields("world", &serde_json::json!([])),
            json_fields("world", &serde_json::json!({}))
        );
    }
    fn profile() -> Profile {
        Profile {
            schema: "test".into(),
            inputs: Digests::new(),
            observed: vec!["cpu".into(), "ram".into(), "world".into()],
            unavailable: vec!["hardware".into()],
            counter: CounterProfile {
                source: "synthetic".into(),
                profile_id: "test-count-v1".into(),
                width_bits: Some(64),
                configuration: Fields::new(),
                validation: CounterValidation::SyntheticOnly,
            },
        }
    }
    fn page(incarnation: u64) -> Page {
        Page {
            id: PageId {
                machine: 1,
                incarnation,
                slot: 0,
                gpa: 0,
            },
            bytes: vec![7; 64],
        }
    }
    fn observation(incarnation: u64, full: bool, dirty: bool) -> Observation {
        let p = page(incarnation);
        Observation {
            identity: BoundaryIdentity {
                kind: BoundaryKind::Lifecycle { tag: "test".into() },
                cpu: CpuId {
                    machine: 1,
                    incarnation,
                    vcpu: 0,
                },
                landing: LandingIdentity {
                    count: BranchCount {
                        source: "synthetic".into(),
                        profile_id: "test-count-v1".into(),
                        value: Some(0),
                        validity: CountValidity::Valid,
                        overflow: OverflowState::None,
                        epoch: 0,
                    },
                    rip: 0,
                    registers: Fields::from([
                        ("rcx".into(), 0_u64.to_le_bytes().to_vec()),
                        ("rsi".into(), 0_u64.to_le_bytes().to_vec()),
                        ("rdi".into(), 0_u64.to_le_bytes().to_vec()),
                        ("rflags".into(), 2_u64.to_le_bytes().to_vec()),
                    ]),
                },
            },
            cpu_inventory: vec![CpuId {
                machine: 1,
                incarnation,
                vcpu: 0,
            }],
            cpu: ["rip", "rflags", "rcx", "rsi", "rdi"]
                .iter()
                .map(|r| {
                    (
                        format!("machine/1/boot/{incarnation}/cpu/0/{r}"),
                        if *r == "rflags" { 2_u64 } else { 0 }
                            .to_le_bytes()
                            .to_vec(),
                    )
                })
                .collect(),
            world: Fields::from([("boot".into(), incarnation.to_le_bytes().to_vec())]),
            layout: BTreeMap::from([(p.id.key(), 64)]),
            dirty: if dirty { vec![p.clone()] } else { vec![] },
            full: full.then_some(vec![p]),
        }
    }
    #[test]
    fn precise_count_rejects_unknown_overflow_profile_and_missing_value() {
        for change in 0..4 {
            let mut w = Witness::new(profile(), 1, None).unwrap();
            let mut o = observation(1, true, false);
            o.identity.kind = BoundaryKind::PrecisePreemption;
            match change {
                0 => o.identity.landing.count.overflow = OverflowState::Unaccounted,
                1 => o.identity.landing.count.profile_id = "wrong profile".into(),
                2 => o.identity.landing.count.value = None,
                _ => o.identity.landing.count.validity = CountValidity::Unavailable,
            }
            assert!(w.observe(o).is_err());
            assert!(w.trace.boundaries.is_empty());
        }
    }
    #[test]
    fn complete_inventory_and_consistent_rep_landing_are_required() {
        let mut w = Witness::new(profile(), 1, None).unwrap();
        let mut o = observation(1, true, false);
        o.cpu_inventory.push(CpuId {
            machine: 2,
            incarnation: 1,
            vcpu: 0,
        });
        assert!(w.observe(o).unwrap_err().contains("CPU inventory missing"));
        let mut o = observation(1, true, false);
        o.identity
            .landing
            .registers
            .insert("rcx".into(), 1_u64.to_le_bytes().to_vec());
        assert!(w.observe(o).unwrap_err().contains("disagrees"));
        let mut o = observation(1, true, false);
        o.identity.landing.registers.remove("rsi");
        assert!(w.observe(o).unwrap_err().contains("missing REP"));
    }
    #[test]
    fn correction_diagnostics_never_commit_boundary_or_change_world() {
        let mut a = Witness::new(profile(), 1, None).unwrap();
        let mut b = Witness::new(profile(), 1, None).unwrap();
        let o = observation(1, true, false);
        b.record_correction(CorrectionObservation {
            cpu: o.identity.cpu.clone(),
            phase: "single-step".into(),
            landing: o.identity.landing.clone(),
        });
        assert_eq!(b.request().boundary, 0);
        assert!(b.trace.boundaries.is_empty());
        a.observe(o.clone()).unwrap();
        b.observe(o).unwrap();
        assert!(compare(&a.trace, &b.trace).is_none());
        assert_eq!(b.trace.corrections.len(), 1);
    }
    #[test]
    fn reboot_removes_old_incarnation_pages() {
        let mut w = Witness::new(profile(), 2, None).unwrap();
        w.observe(observation(1, true, false)).unwrap();
        w.observe(observation(2, false, true)).unwrap();
        assert_eq!(w.trace.boundaries[1].pages.len(), 1);
        assert!(w.trace.boundaries[1].pages.contains_key(&page(2).id.key()));
        assert!(!w.trace.boundaries[1].pages.contains_key(&page(1).id.key()));
    }
    #[test]
    fn rejects_missing_checkpoint_unknown_page_and_duplicate_page() {
        let mut w = Witness::new(profile(), 2, None).unwrap();
        assert!(
            w.observe(observation(1, false, true))
                .unwrap_err()
                .contains("full RAM required")
        );
        let mut o = observation(1, true, true);
        o.dirty.push(page(1));
        assert!(w.observe(o).unwrap_err().contains("duplicate"));
        let mut o = observation(1, true, true);
        o.layout.clear();
        assert!(w.observe(o).unwrap_err().contains("outside layout"));
        assert!(w.trace.boundaries.is_empty());
    }
    #[test]
    fn adapter_capture_request_and_profile_change() {
        struct Source {
            profile: Profile,
        }
        impl BoundarySource for Source {
            fn profile(&self) -> Profile {
                self.profile.clone()
            }
            fn capture(&mut self, r: CaptureRequest) -> Result<Observation, String> {
                Ok(observation(1, r.full_memory, r.boundary == 0))
            }
        }
        let mut source = Source { profile: profile() };
        let mut w = Witness::new(source.profile(), 2, None).unwrap();
        for _ in 0..3 {
            w.observe_source(&mut source).unwrap();
        }
        source.profile.unavailable.push("new missing state".into());
        assert_eq!(
            w.observe_source(&mut source).unwrap_err(),
            "adapter profile changed"
        );
    }
    #[test]
    fn missing_world_field_truncation_and_profile_are_detected() {
        let mut w = Witness::new(profile(), 1, None).unwrap();
        w.observe(observation(1, true, false)).unwrap();
        let a = w.trace;
        let mut b = a.clone();
        b.boundaries.clear();
        assert_eq!(compare(&a, &b).unwrap().category, "trace");
        let mut changed = profile();
        changed.inputs.insert("seed".into(), hash(b"other"));
        let w = Witness::new(changed, 1, None).unwrap();
        assert_eq!(compare(&a, &w.trace).unwrap().category, "inputs/profile");
        let mut w = Witness::new(profile(), 1, None).unwrap();
        let mut o = observation(1, true, false);
        o.world.clear();
        w.observe(o).unwrap();
        assert_eq!(compare(&a, &w.trace).unwrap().field, "boot");
    }
}
