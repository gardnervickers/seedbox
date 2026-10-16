//! Dependency-aware delta debugging over a simulation's external inputs.
//!
//! Only external overrides are candidates; oracle-generated controls are not
//! in the program and are never touched. A generated fault and its teardown
//! form one atom. Removing an anchor removes the reseeds that follow it, and
//! removing a fault removes the heals of its handle; a reseed or a heal may
//! still go alone. Every probe reruns the whole simulation and its oracle.
//! The result is "1-minimal under these transformations" only when ddmin
//! finished at single-atom granularity; otherwise it is merely reduced.
//!
//! A [`Simplifier`] then edits what remains, one input at a time, keeping
//! an edit only when the failure persists: a fault ends sooner (its teardown
//! or heal moves halfway towards its start), a trigger fires without its
//! delay or after half of it, and a choice input selects the first
//! alternative. Keys never change, so the kept set is the same.
use seedbox_engine_protocol::input_program::{
    BranchAction, InputAction, RuleAction, TimedInput, GENERATED_END_SUFFIX,
};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PROBES: u32 = 1024;

/// A program split into its fixed prefix and removable atoms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidates {
    /// Keys that every probe keeps: a checkpoint's consumed prefix.
    pub fixed: Vec<String>,
    /// Each atom's keys, removed together.
    pub atoms: Vec<Vec<String>>,
    /// Keys that must go when their prerequisite does.
    pub requires: BTreeMap<String, String>,
}

/// Inputs from `pending` on are removable; earlier ones are fixed.
pub fn candidates(inputs: &[TimedInput], pending: usize) -> Candidates {
    let pending = pending.min(inputs.len());
    let fixed = inputs[..pending].iter().map(|i| i.key.clone()).collect();
    let removable = &inputs[pending..];
    let mut atoms: Vec<Vec<String>> = Vec::new();
    let mut requires = BTreeMap::new();
    let mut anchor_at: BTreeMap<u64, String> = BTreeMap::new();
    let mut handles: BTreeMap<String, String> = BTreeMap::new();
    for input in removable {
        let key = &input.key;
        // A teardown goes with its fault: generated pairs and mutations.
        if let Some(start) = key.strip_suffix(GENERATED_END_SUFFIX) {
            if let Some(atom) = atoms.iter_mut().find(|a| a[0] == start) {
                atom.push(key.clone());
                continue;
            }
        }
        match &input.action {
            InputAction::Branch(BranchAction::Anchor {}) => {
                anchor_at.insert(input.at_ns, key.clone());
            }
            InputAction::Branch(BranchAction::Reseed { .. }) => {
                if let Some(anchor) = anchor_at.get(&input.at_ns) {
                    requires.insert(key.clone(), anchor.clone());
                }
            }
            InputAction::Fault(action) => {
                let value = serde_json::to_value(action).unwrap_or_default();
                if let Some(handle) = value.get("handle").and_then(|h| h.as_str()) {
                    if action.name() == "heal" {
                        if let Some(owner) = handles.get(handle) {
                            requires.insert(key.clone(), owner.clone());
                        }
                    } else {
                        handles.insert(handle.to_owned(), key.clone());
                    }
                }
            }
            InputAction::Rule(_) => {}
        }
        atoms.push(vec![key.clone()]);
    }
    Candidates {
        fixed,
        atoms,
        requires,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Result {
    /// The unchanged program did not reproduce the failure.
    NotReproduced,
    /// The probe budget ran out first.
    Reduced { keep: Vec<String> },
    /// No single atom can be removed and still reproduce the failure.
    Minimal { keep: Vec<String> },
}

/// A ddmin state machine: ask for a probe, run it, report whether it still
/// failed the same way.
#[derive(Clone, Debug)]
pub struct Shrinker {
    candidates: Candidates,
    max_probes: u32,
    current: Vec<usize>,
    granularity: usize,
    /// Candidate atom subsets still to try at this granularity.
    queue: Vec<Vec<usize>>,
    /// Kept key sets already tested, and whether each failed.
    tested: BTreeMap<BTreeSet<String>, bool>,
    probing: Option<Vec<usize>>,
    probes: u32,
    verified: bool,
    result: Option<Result>,
}

impl Shrinker {
    pub fn new(candidates: Candidates, max_probes: u32) -> Self {
        let all: Vec<usize> = (0..candidates.atoms.len()).collect();
        Self {
            candidates,
            max_probes: max_probes.clamp(1, MAX_PROBES),
            granularity: 2,
            queue: vec![all.clone()],
            current: all,
            tested: BTreeMap::new(),
            probing: None,
            probes: 0,
            verified: false,
            result: None,
        }
    }
    pub fn probes(&self) -> u32 {
        self.probes
    }
    pub fn result(&self) -> Option<&Result> {
        self.result.as_ref()
    }
    /// The keys a probe keeps: the fixed prefix and the atoms, less whatever
    /// lost a prerequisite.
    fn keep(&self, atoms: &[usize]) -> BTreeSet<String> {
        let mut kept: BTreeSet<String> = self.candidates.fixed.iter().cloned().collect();
        for &atom in atoms {
            kept.extend(self.candidates.atoms[atom].iter().cloned());
        }
        loop {
            let orphan = kept
                .iter()
                .find(|k| {
                    self.candidates
                        .requires
                        .get(*k)
                        .is_some_and(|p| !kept.contains(p))
                })
                .cloned();
            match orphan {
                Some(key) => kept.remove(&key),
                None => break kept,
            };
        }
    }
    fn ordered(&self, kept: &BTreeSet<String>) -> Vec<String> {
        (self.candidates.fixed.iter())
            .chain(self.candidates.atoms.iter().flatten())
            .filter(|k| kept.contains(*k))
            .cloned()
            .collect()
    }
    /// The next program to run, as the keys it keeps in program order; None
    /// once the result is known.
    pub fn next_probe(&mut self) -> Option<Vec<String>> {
        if let Some(atoms) = &self.probing {
            return Some(self.ordered(&self.keep(atoms)));
        }
        while self.result.is_none() {
            let Some(atoms) = self.queue.first().cloned() else {
                self.refine();
                continue;
            };
            let kept = self.keep(&atoms);
            if let Some(&failed) = self.tested.get(&kept) {
                self.queue.remove(0);
                self.apply(atoms, failed);
                continue;
            }
            if self.probes >= self.max_probes {
                self.result = Some(Result::Reduced {
                    keep: self.ordered(&self.keep(&self.current)),
                });
                return None;
            }
            self.probes += 1;
            self.probing = Some(atoms);
            return Some(self.ordered(&kept));
        }
        None
    }
    /// Whether the last probe still failed the same way.
    pub fn report(&mut self, failed: bool) -> std::result::Result<(), String> {
        let atoms = self.probing.take().ok_or("no shrink probe outstanding")?;
        self.tested.insert(self.keep(&atoms), failed);
        self.queue.remove(0);
        self.apply(atoms, failed);
        Ok(())
    }
    fn apply(&mut self, atoms: Vec<usize>, failed: bool) {
        if !self.verified {
            self.verified = true;
            if !failed {
                self.result = Some(Result::NotReproduced);
            } else {
                self.refine_from(2);
            }
            return;
        }
        if failed {
            // Reduce to this subset or complement and start over coarsely.
            let len = atoms.len();
            let was = self.current.len();
            self.current = atoms;
            let next = if len * self.granularity <= was {
                2
            } else {
                self.granularity.saturating_sub(1).max(2)
            };
            self.refine_from(next);
        }
    }
    /// Subsets then complements of the current atoms at `granularity`.
    fn refine_from(&mut self, granularity: usize) {
        let len = self.current.len();
        if len == 0 {
            self.finish();
            return;
        }
        self.granularity = granularity.min(len).max(1);
        let n = self.granularity;
        let chunks: Vec<Vec<usize>> = (0..n)
            .map(|i| self.current[i * len / n..(i + 1) * len / n].to_vec())
            .filter(|c| !c.is_empty())
            .collect();
        let mut queue = Vec::new();
        if n > 1 {
            queue.extend(chunks.iter().cloned());
        }
        for chunk in &chunks {
            queue.push(
                self.current
                    .iter()
                    .copied()
                    .filter(|a| !chunk.contains(a))
                    .collect(),
            );
        }
        self.queue = queue;
    }
    /// Nothing at this granularity reproduced: go finer, or stop.
    fn refine(&mut self) {
        if self.granularity >= self.current.len() {
            self.finish();
        } else {
            self.refine_from((self.granularity * 2).min(self.current.len()));
        }
    }
    fn finish(&mut self) {
        self.result = Some(Result::Minimal {
            keep: self.ordered(&self.keep(&self.current)),
        });
    }
}

/// The shortest fault a simplification leaves, and the smallest delay it halves.
const MIN_SPAN_NS: u64 = 1_000_000;

/// One simplifying edit: `key`'s input replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub key: String,
    pub input: TimedInput,
    pub summary: String,
}

/// The simplifying edits of a program, from input `fixed` on, most
/// simplifying first for each input. No edit moves an input before
/// `floor_ns` (a checkpoint's time) or the fixed prefix's last input.
pub fn simplifications(program: &[TimedInput], fixed: usize, floor_ns: u64) -> Vec<Edit> {
    let floor = program[..fixed.min(program.len())]
        .last()
        .map_or(0, |i| i.at_ns)
        .max(floor_ns);
    let mut edits = Vec::new();
    let mut starts: BTreeMap<&str, u64> = BTreeMap::new();
    let mut handles: BTreeMap<String, u64> = BTreeMap::new();
    for (index, input) in program.iter().enumerate() {
        let start = match &input.action {
            InputAction::Fault(action) => {
                let value = serde_json::to_value(action).unwrap_or_default();
                let handle = value
                    .get("handle")
                    .and_then(|h| h.as_str())
                    .map(str::to_owned);
                match handle {
                    Some(handle) if action.name() == "heal" => handles.get(&handle).copied(),
                    Some(handle) => {
                        handles.insert(handle, input.at_ns);
                        None
                    }
                    None => None,
                }
            }
            _ => None,
        }
        .or_else(|| {
            let start = input.key.strip_suffix(GENERATED_END_SUFFIX)?;
            starts.get(start).copied()
        });
        starts.insert(&input.key, input.at_ns);
        if index < fixed {
            continue;
        }
        let mut edit = |input: TimedInput, summary: String| {
            edits.push(Edit {
                key: input.key.clone(),
                input,
                summary,
            })
        };
        let shorter_at = start
            .filter(|&s| input.at_ns >= s + 2 * MIN_SPAN_NS)
            .map(|s| (s, (s + (input.at_ns - s) / 2).max(floor)))
            .filter(|&(_, at)| at < input.at_ns);
        if let Some((start, at_ns)) = shorter_at {
            let mut shorter = input.clone();
            shorter.at_ns = at_ns;
            let summary = format!("end {} after {} ns", input.key, shorter.at_ns - start);
            edit(shorter, summary);
        }
        match &input.action {
            InputAction::Rule(RuleAction::Trigger(spec)) if spec.delay_ns > 0 => {
                let mut immediate = input.clone();
                if let InputAction::Rule(RuleAction::Trigger(spec)) = &mut immediate.action {
                    spec.delay_ns = 0;
                }
                edit(immediate, format!("fire {} without delay", input.key));
                if spec.delay_ns >= 2 * MIN_SPAN_NS {
                    let mut half = input.clone();
                    if let InputAction::Rule(RuleAction::Trigger(spec)) = &mut half.action {
                        spec.delay_ns /= 2;
                    }
                    let summary = format!("fire {} after {} ns", input.key, spec.delay_ns / 2);
                    edit(half, summary);
                }
            }
            InputAction::Rule(RuleAction::Choice(choice)) if choice.select > 0 => {
                let mut first = input.clone();
                if let InputAction::Rule(RuleAction::Choice(choice)) = &mut first.action {
                    choice.select = 0;
                }
                edit(
                    first,
                    format!("choose the first alternative at {}", input.key),
                );
            }
            _ => {}
        }
    }
    edits
}

/// A program with one edit applied, back in program order: by time, branch
/// inputs first at their time, otherwise as before.
pub fn apply(program: &[TimedInput], edit: &Edit) -> Vec<TimedInput> {
    let mut edited: Vec<TimedInput> = program
        .iter()
        .map(|i| {
            if i.key == edit.key {
                edit.input.clone()
            } else {
                i.clone()
            }
        })
        .collect();
    edited.sort_by_key(|i| (i.at_ns, !matches!(i.action, InputAction::Branch(_))));
    edited
}

/// Greedy simplification: try each edit of the current program, keep it when
/// the failure persists, and start over from the edited program until no
/// edit helps or the probes run out. A failed edit is never retried.
#[derive(Clone, Debug)]
pub struct Simplifier {
    program: Vec<TimedInput>,
    fixed: usize,
    floor_ns: u64,
    max_probes: u32,
    probes: u32,
    queue: Vec<Edit>,
    rejected: BTreeSet<String>,
    probing: Option<Edit>,
    accepted: Vec<(String, String)>,
    done: bool,
}

impl Simplifier {
    /// Simplify `program`; its first `fixed` inputs stay as they are and no
    /// input moves before `floor_ns`.
    pub fn new(program: Vec<TimedInput>, fixed: usize, floor_ns: u64, max_probes: u32) -> Self {
        let queue = simplifications(&program, fixed, floor_ns);
        Self {
            program,
            fixed,
            floor_ns,
            max_probes,
            probes: 0,
            queue,
            rejected: BTreeSet::new(),
            probing: None,
            accepted: Vec::new(),
            done: false,
        }
    }
    fn fingerprint(edit: &Edit) -> String {
        serde_json::to_string(&edit.input).unwrap_or_default()
    }
    /// The next program to run and what it changed; None when finished.
    pub fn next_probe(&mut self) -> Option<(Vec<TimedInput>, String)> {
        if let Some(edit) = &self.probing {
            return Some((apply(&self.program, edit), edit.summary.clone()));
        }
        while !self.done {
            if self.probes >= self.max_probes {
                self.done = true;
                break;
            }
            let Some(edit) = (!self.queue.is_empty()).then(|| self.queue.remove(0)) else {
                self.done = true;
                break;
            };
            if self.rejected.contains(&Self::fingerprint(&edit)) {
                continue;
            }
            self.probes += 1;
            let probe = (apply(&self.program, &edit), edit.summary.clone());
            self.probing = Some(edit);
            return Some(probe);
        }
        None
    }
    /// Whether the last probe still failed the same way.
    pub fn report(&mut self, failed: bool) -> std::result::Result<(), String> {
        let edit = self.probing.take().ok_or("no simplification outstanding")?;
        if failed {
            self.program = apply(&self.program, &edit);
            self.accepted.push((edit.key, edit.summary));
            self.queue = simplifications(&self.program, self.fixed, self.floor_ns);
        } else {
            self.rejected.insert(Self::fingerprint(&edit));
        }
        Ok(())
    }
    pub fn done(&self) -> bool {
        self.done
    }
    pub fn probes(&self) -> u32 {
        self.probes
    }
    /// The simplified program so far.
    pub fn program(&self) -> &[TimedInput] {
        &self.program
    }
    /// The last kept edit of each input, in the order inputs were first
    /// edited: repeated halvings show only where they ended.
    pub fn accepted(&self) -> Vec<String> {
        let mut last: Vec<(&str, &str)> = Vec::new();
        for (key, summary) in &self.accepted {
            match last.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1 = summary,
                None => last.push((key, summary)),
            }
        }
        last.into_iter().map(|(_, s)| s.to_owned()).collect()
    }
}

#[cfg(test)]
#[path = "shrink_tests.rs"]
mod tests;
