//! Dependency-aware delta debugging over a simulation's external inputs.
//!
//! Only external overrides are candidates; oracle-generated controls are not
//! in the program and are never touched. A generated fault and its teardown
//! form one atom. Removing an anchor removes the reseeds that follow it, and
//! removing a fault removes the heals of its handle; a reseed or a heal may
//! still go alone. Every probe reruns the whole simulation and its oracle.
//! The result is "1-minimal under these transformations" only when ddmin
//! finished at single-atom granularity; otherwise it is merely reduced.
use seedbox_engine_protocol::input_program::{
    BranchAction, InputAction, TimedInput, GENERATED_END_SUFFIX, GENERATED_PREFIX,
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
        if let Some(start) = key
            .strip_suffix(GENERATED_END_SUFFIX)
            .filter(|s| s.starts_with(GENERATED_PREFIX))
        {
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

#[cfg(test)]
#[path = "shrink_tests.rs"]
mod tests;
