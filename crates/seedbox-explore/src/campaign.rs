//! Generation-synchronous campaign planning.
//!
//! A campaign runs a fixed logical batch of member simulations per
//! generation. Ordinals are assigned before dispatch and feedback is folded
//! in ordinal order once the whole batch has reported, so the plans depend on
//! results, never on which member finished first or how many ran at once.
//!
//! Generation 0 is a seed sweep of roots, each declaring the same candidate
//! anchors. Later generations mostly branch from members that showed new
//! features (property outcomes, semantic events, setup milestones, structured
//! choices) at one of their captured anchors, with a reseed and sometimes a
//! changed choice. A fixed share of every later batch starts fresh roots, so
//! guidance never stops uniform exploration.
use crate::rng::Rng;
use seedbox_engine_protocol::input_program::ChoiceOverride;
use seedbox_engine_protocol::property::Ledger;
use std::collections::{BTreeMap, BTreeSet};

/// The planner's own stream, separate from every simulation's seeds.
const DOMAIN: u64 = 0x6361_6d70_6169_676e;
pub const MAX_BUDGET: u32 = 4096;
pub const MAX_BATCH: u32 = 64;
pub const MAX_ANCHORS: usize = 16;
pub const MAX_CORPUS: u32 = 1024;

/// A campaign's logical plan. Budgets are request options, never part of a
/// simulation definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Seeds the planner's choices.
    pub seed: u64,
    /// The first root's simulation seed; later roots take the next ones.
    pub first_seed: u64,
    /// Member simulations in total.
    pub budget: u32,
    /// Members per generation.
    pub batch: u32,
    /// Share of each later generation that starts fresh roots, per mille.
    pub fresh_permille: u32,
    /// Members kept as branch sources.
    pub corpus_limit: u32,
    /// Candidate anchor times every root declares, ascending.
    pub anchors: Vec<u64>,
    /// Faults branches may place relative to what their parent did.
    pub mutations: Mutations,
}

/// The fault mutations a campaign may make: which kinds, on which targets.
/// Empty kinds make none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mutations {
    pub kinds: Vec<MutationKind>,
    pub targets: Vec<String>,
    /// Every machine, for isolating a target from the rest.
    pub machines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationKind {
    /// Crash the target, rebooting it after the duration.
    Crash,
    /// Partition the target from every other machine for the duration.
    Isolate,
    /// Pause the target's whole machine for the duration.
    Pause,
    /// Slow the target's disk by a latency drawn from this range, in
    /// nanoseconds, for the duration; only `targets` when it names any.
    SlowDisk {
        latency_ns: (u64, u64),
        targets: Vec<String>,
    },
    /// Perturb the target's thread scheduling at this rate, per million
    /// syscalls and progress calls, for the duration.
    Preempt { ppm: u32 },
}

impl MutationKind {
    /// A short word for summaries.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Crash => "crash",
            Self::Isolate => "isolate",
            Self::Pause => "pause",
            Self::SlowDisk { .. } => "slow disk of",
            Self::Preempt { .. } => "preempt",
        }
    }
}

/// A fault placed `delay_ns` after the `nth` occurrence of `when`, which the
/// parent was seen doing after the branch's anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutation {
    pub when: seedbox_control::EventSelector,
    pub nth: u64,
    pub delay_ns: u64,
    pub kind: MutationKind,
    pub target: String,
    pub duration_ns: u64,
    /// Every other machine, which an isolated target is cut off from.
    pub others: Vec<String>,
    /// The drawn size of a slow disk's latency, in nanoseconds; 0 otherwise.
    pub latency_ns: u64,
}

const DELAYS_NS: [u64; 6] = [0, 1_000_000, 3_000_000, 10_000_000, 30_000_000, 100_000_000];
const DURATIONS_NS: [u64; 3] = [100_000_000, 300_000_000, 1_000_000_000];

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.budget == 0 || self.budget > MAX_BUDGET {
            return Err(format!("campaign budget must be 1 to {MAX_BUDGET} members"));
        }
        if self.batch == 0 || self.batch > MAX_BATCH {
            return Err(format!("campaign batch must be 1 to {MAX_BATCH} members"));
        }
        if self.fresh_permille > 1000 {
            return Err("campaign fresh share is at most 1000 per mille".into());
        }
        if self.corpus_limit == 0 || self.corpus_limit > MAX_CORPUS {
            return Err(format!("campaign corpus holds 1 to {MAX_CORPUS} members"));
        }
        if self.anchors.len() > MAX_ANCHORS || self.anchors.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!(
                "campaign anchors are at most {MAX_ANCHORS} strictly increasing times"
            ));
        }
        self.first_seed
            .checked_add(u64::from(self.budget))
            .ok_or("campaign root seeds overflow")?;
        Ok(())
    }
}

/// The program key of a root's `index`th candidate anchor. Its capture is
/// the checkpoint `anchor-<key>`.
pub fn anchor_key(index: usize) -> String {
    format!("a{index}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub ordinal: u32,
    pub generation: u32,
    pub kind: PlanKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanKind {
    /// A fresh simulation with this seed and the declared anchors. With
    /// `kinds`, only those entries of the fault profile (swarm testing).
    Root {
        seed: u64,
        kinds: Option<Vec<u32>>,
    },
    Branch(Box<Branch>),
}

/// Continue `parent` from its capture at `anchor`, reseeded, with choice
/// inputs for occurrences after the anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub parent: u32,
    pub anchor: String,
    pub at_ns: u64,
    pub reseed: u64,
    pub identity: String,
    pub choices: Vec<ChoiceOverride>,
    pub mutation: Option<Mutation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Passed,
    Failed,
    Incomplete,
    Error,
}

/// Something a member was observed to do; `at_ns` is when it was first seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    pub key: String,
    pub at_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedChoice {
    pub machine: String,
    pub incarnation: u32,
    pub session: u32,
    pub site: String,
    pub occurrence: u64,
    pub alternatives: usize,
    pub selected: usize,
    pub requested_at_ns: u64,
}

/// What a finished member showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    pub ordinal: u32,
    pub outcome: Outcome,
    pub features: Vec<Feature>,
    /// Captured anchors it can be branched from, by key and time.
    pub anchors: Vec<(String, u64)>,
    pub choices: Vec<ObservedChoice>,
    /// Things it did that a trigger can select, with when it first did them.
    pub moments: Vec<(seedbox_control::EventSelector, u64)>,
}

/// The features a member's property evidence and choices show. Event counts
/// enter as power-of-two buckets, so reaching a milestone more often is new.
pub fn features(ledger: Option<&Ledger>, choices: &[ObservedChoice]) -> Vec<Feature> {
    let mut out = Vec::new();
    if let Some(ledger) = ledger {
        for (id, property) in &ledger.properties {
            if property.passes > 0 {
                out.push(Feature {
                    key: format!("property:{id}:holds"),
                    at_ns: property.first_pass.as_ref().map(|e| e.origin.time_ns),
                });
            }
            if property.failures > 0 {
                out.push(Feature {
                    key: format!("property:{id}:fails"),
                    at_ns: property.first_failure.as_ref().map(|e| e.origin.time_ns),
                });
            }
        }
        for (name, count) in &ledger.events {
            let first = ledger.event_first.get(name).map(|o| o.time_ns);
            for bucket in 0..u64::BITS - count.leading_zeros() {
                out.push(Feature {
                    key: format!("event:{name}:{bucket}"),
                    // Only the first occurrence has a position.
                    at_ns: if bucket == 0 { first } else { None },
                });
            }
        }
        for (key, origin) in &ledger.states {
            out.push(Feature {
                key: format!("state:{key}"),
                at_ns: Some(origin.time_ns),
            });
        }
        for (key, origin) in &ledger.transitions {
            out.push(Feature {
                key: format!("transition:{key}"),
                at_ns: Some(origin.time_ns),
            });
        }
        for (machine, origin) in &ledger.setup {
            out.push(Feature {
                key: format!("setup:{machine}"),
                at_ns: Some(origin.time_ns),
            });
        }
        for (machine, map) in &ledger.coverage {
            let bytes = seedbox_engine_protocol::property::decode_hex(map).unwrap_or_default();
            for (index, byte) in bytes.iter().enumerate() {
                for bit in (0..8).filter(|bit| byte & (1 << bit) != 0) {
                    out.push(Feature {
                        key: format!("cover:{machine}:{}", index * 8 + bit),
                        at_ns: None,
                    });
                }
            }
        }
        for exit in &ledger.exits {
            let how = match (exit.code, exit.signal) {
                (_, Some(signal)) => format!("signal-{signal}"),
                (code, None) => format!("code-{}", code.unwrap_or(-1)),
            };
            out.push(Feature {
                key: format!("exit:{}:{how}", exit.origin.machine),
                at_ns: Some(exit.origin.time_ns),
            });
        }
    }
    for choice in choices {
        out.push(Feature {
            key: format!("choice:{}:{}", choice.site, choice.selected),
            at_ns: Some(choice.requested_at_ns),
        });
    }
    out
}

/// One member as the planner sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub ordinal: u32,
    pub generation: u32,
    pub parent: Option<u32>,
    pub anchor: Option<String>,
    pub outcome: Option<Outcome>,
    /// Features no earlier member (in ordinal order) showed.
    pub novelty: u32,
    pub in_corpus: bool,
}

#[derive(Clone, Debug)]
struct Entry {
    ordinal: u32,
    depth: u32,
    anchors: Vec<(String, u64)>,
    choices: Vec<ObservedChoice>,
    moments: Vec<(seedbox_control::EventSelector, u64)>,
    novelty: u32,
    features: Vec<String>,
    /// Its failure features, so re-finding a known failure costs less budget.
    fails: Vec<String>,
    /// Features no more than two members had when it was folded.
    rarity: u32,
    /// When its earliest novel timed feature appeared.
    interest_ns: Option<u64>,
    picks: u32,
}

#[derive(Clone, Debug)]
pub struct Planner {
    config: Config,
    rng: Rng,
    next_ordinal: u32,
    roots: u64,
    generation: u32,
    outstanding: BTreeSet<u32>,
    buffered: BTreeMap<u32, Feedback>,
    seen: BTreeSet<String>,
    corpus: Vec<Entry>,
    nodes: Vec<Node>,
    /// Members no plan can branch from any more: their captures may go.
    released: Vec<u32>,
    /// How many folded members showed each feature.
    counts: BTreeMap<String, u32>,
    /// Entries in the fault profile, for swarm roots.
    profile_kinds: u32,
}

/// Members at or below this count make a feature rare.
const RARE: u32 = 2;
/// A known failure is re-found cheaply after this many members.
const KNOWN_FAILURE: u32 = 3;

impl Planner {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            rng: Rng::new(config.seed, DOMAIN),
            config,
            next_ordinal: 0,
            roots: 0,
            generation: 0,
            outstanding: BTreeSet::new(),
            buffered: BTreeMap::new(),
            seen: BTreeSet::new(),
            corpus: Vec::new(),
            nodes: Vec::new(),
            released: Vec::new(),
            counts: BTreeMap::new(),
            profile_kinds: 0,
        })
    }
    pub fn config(&self) -> &Config {
        &self.config
    }
    /// Set the mutations once the campaign knows its machines; plans made
    /// before then mutate nothing.
    pub fn set_mutations(&mut self, mutations: Mutations) {
        self.config.mutations = mutations;
    }
    /// The fault profile's entry count; with two or more, later fresh roots
    /// may sample a subset of them.
    pub fn set_profile_kinds(&mut self, kinds: u32) {
        self.profile_kinds = kinds;
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    /// Distinct features observed so far.
    pub fn features_seen(&self) -> usize {
        self.seen.len()
    }
    pub fn corpus_len(&self) -> usize {
        self.corpus.len()
    }
    /// Members folded outside the corpus or evicted from it since the last
    /// call. No later plan branches from them, so their captures may go.
    pub fn take_released(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.released)
    }
    /// Every member has been planned and folded.
    pub fn done(&self) -> bool {
        self.next_ordinal >= self.config.budget && self.outstanding.is_empty()
    }
    /// The next generation, once the previous one has been folded; empty
    /// while members are outstanding or when the budget is spent.
    pub fn next_batch(&mut self) -> Vec<Plan> {
        if !self.outstanding.is_empty() || self.next_ordinal >= self.config.budget {
            return Vec::new();
        }
        let size = self
            .config
            .batch
            .min(self.config.budget - self.next_ordinal);
        let generation = self.generation;
        let mut plans = Vec::new();
        for _ in 0..size {
            let ordinal = self.next_ordinal;
            self.next_ordinal += 1;
            let fresh = generation == 0
                || self.corpus.is_empty()
                || self.rng.chance(self.config.fresh_permille);
            let kind = if fresh {
                let seed = self.config.first_seed + self.roots;
                self.roots += 1;
                // Generation 0 stays a plain sweep; later fresh roots may
                // try a subset of the fault kinds.
                let kinds = (generation > 0 && self.profile_kinds > 1 && self.rng.chance(500))
                    .then(|| {
                        let mut kinds: Vec<u32> = (0..self.profile_kinds)
                            .filter(|_| self.rng.chance(500))
                            .collect();
                        if kinds.is_empty() {
                            kinds.push(self.rng.below(u64::from(self.profile_kinds)) as u32);
                        }
                        kinds
                    });
                PlanKind::Root { seed, kinds }
            } else {
                PlanKind::Branch(Box::new(self.branch(ordinal)))
            };
            let (parent, anchor) = match &kind {
                PlanKind::Branch(b) => (Some(b.parent), Some(b.anchor.clone())),
                PlanKind::Root { .. } => (None, None),
            };
            self.nodes.push(Node {
                ordinal,
                generation,
                parent,
                anchor,
                outcome: None,
                novelty: 0,
                in_corpus: false,
            });
            self.outstanding.insert(ordinal);
            plans.push(Plan {
                ordinal,
                generation,
                kind,
            });
        }
        self.generation += 1;
        plans
    }

    /// Weighted by novelty, against how often it was already picked and how
    /// deep it already is, so short prefixes and fresh discoveries win.
    fn branch(&mut self, ordinal: u32) -> Branch {
        // A portfolio: half the picks favour novelty, a quarter are uniform,
        // and a quarter favour rare features. Any member whose failures are
        // all already well known costs a quarter as much budget.
        let strategy = self.rng.below(4);
        let counts = &self.counts;
        let weights: Vec<u64> = self
            .corpus
            .iter()
            .map(|e| {
                let weight = match strategy {
                    0 | 1 => {
                        (1 + u64::from(e.novelty)) * 64
                            / (1 + u64::from(e.picks))
                            / (1 + u64::from(e.depth))
                            + 1
                    }
                    2 => 16,
                    _ => {
                        e.features
                            .iter()
                            .map(|f| 256 / u64::from(*counts.get(f).unwrap_or(&1)).max(1))
                            .sum::<u64>()
                            / (1 + u64::from(e.picks))
                            + 1
                    }
                };
                let known = !e.fails.is_empty()
                    && e.fails
                        .iter()
                        .all(|f| counts.get(f).is_some_and(|n| *n >= KNOWN_FAILURE));
                if known {
                    (weight / 4).max(1)
                } else {
                    weight
                }
            })
            .collect();
        let mut pick = self.rng.below(weights.iter().sum());
        let index = weights
            .iter()
            .position(|&w| {
                if pick < w {
                    true
                } else {
                    pick -= w;
                    false
                }
            })
            .expect("a pick within the total weight");
        let entry = &mut self.corpus[index];
        entry.picks += 1;
        // Prefer the last anchor before what made the parent interesting.
        let before: Vec<usize> = (0..entry.anchors.len())
            .filter(|&i| entry.interest_ns.is_some_and(|t| entry.anchors[i].1 <= t))
            .collect();
        let slot = match before.last() {
            Some(&last) if self.rng.chance(750) => last,
            _ => self.rng.below(entry.anchors.len() as u64) as usize,
        };
        let (anchor, at_ns) = entry.anchors[slot].clone();
        let later: Vec<&ObservedChoice> = entry
            .choices
            .iter()
            .filter(|c| c.requested_at_ns > at_ns && c.alternatives > 1)
            .collect();
        let mut choices = Vec::new();
        if !later.is_empty() && self.rng.chance(500) {
            let c = later[self.rng.below(later.len() as u64) as usize];
            let shift = 1 + self.rng.below(c.alternatives as u64 - 1) as usize;
            choices.push(ChoiceOverride {
                machine: c.machine.clone(),
                site: c.site.clone(),
                occurrence: c.occurrence,
                incarnation: Some(c.incarnation),
                session: Some(c.session),
                select: (c.selected + shift) % c.alternatives,
            });
        }
        let later: Vec<(seedbox_control::EventSelector, u64)> = entry
            .moments
            .iter()
            .filter(|(_, at)| *at > at_ns)
            .cloned()
            .collect();
        let mutations = &self.config.mutations;
        let mutation = if !later.is_empty()
            && !mutations.kinds.is_empty()
            && !mutations.targets.is_empty()
            && self.rng.chance(600)
        {
            let (when, _) = later[self.rng.below(later.len() as u64) as usize].clone();
            let kind =
                mutations.kinds[self.rng.below(mutations.kinds.len() as u64) as usize].clone();
            // A slow disk only targets machines its profile entry names.
            let targets = match &kind {
                MutationKind::SlowDisk { targets, .. } if !targets.is_empty() => targets,
                _ => &mutations.targets,
            };
            let near = when
                .machines()
                .into_iter()
                .find(|m| targets.iter().any(|t| t == m))
                .map(str::to_owned);
            let target = match near {
                Some(machine) if self.rng.chance(500) => machine,
                _ => targets[self.rng.below(targets.len() as u64) as usize].clone(),
            };
            let latency_ns = match &kind {
                MutationKind::SlowDisk {
                    latency_ns: (min, max),
                    ..
                } => min + self.rng.below(max.saturating_sub(*min) + 1),
                _ => 0,
            };
            Some(Mutation {
                when,
                nth: if self.rng.chance(250) { 2 } else { 1 },
                delay_ns: DELAYS_NS[self.rng.below(DELAYS_NS.len() as u64) as usize],
                kind,
                duration_ns: DURATIONS_NS[self.rng.below(DURATIONS_NS.len() as u64) as usize],
                latency_ns,
                others: mutations
                    .machines
                    .iter()
                    .filter(|m| **m != target)
                    .cloned()
                    .collect(),
                target,
            })
        } else {
            None
        };
        Branch {
            parent: entry.ordinal,
            anchor,
            at_ns,
            reseed: self.rng.next(),
            identity: format!("campaign-{:x}-{ordinal}", self.config.seed),
            choices,
            mutation,
        }
    }

    /// Record one outstanding member's feedback. The generation folds, in
    /// ordinal order, once all of its members have reported.
    pub fn report(&mut self, feedback: Feedback) -> Result<(), String> {
        let ordinal = feedback.ordinal;
        if !self.outstanding.contains(&ordinal) || self.buffered.contains_key(&ordinal) {
            return Err(format!(
                "campaign member {ordinal} is not awaiting feedback"
            ));
        }
        self.buffered.insert(ordinal, feedback);
        if self.buffered.len() == self.outstanding.len() {
            for (_, feedback) in std::mem::take(&mut self.buffered) {
                self.fold(feedback);
            }
            self.outstanding.clear();
        }
        Ok(())
    }

    fn fold(&mut self, feedback: Feedback) {
        let mut novelty = 0;
        let mut interest_ns: Option<u64> = None;
        for feature in &feedback.features {
            if self.seen.insert(feature.key.clone()) {
                novelty += 1;
                if let Some(at) = feature.at_ns {
                    interest_ns = Some(interest_ns.map_or(at, |t| t.min(at)));
                }
            }
        }
        for feature in &feedback.features {
            *self.counts.entry(feature.key.clone()).or_default() += 1;
        }
        let rarity = feedback
            .features
            .iter()
            .filter(|f| self.counts.get(&f.key).is_some_and(|n| *n <= RARE))
            .count() as u32;
        let index = feedback.ordinal as usize;
        let parent = self.nodes[index].parent;
        let depth = parent
            .and_then(|p| self.corpus.iter().find(|e| e.ordinal == p))
            .map_or(0, |e| e.depth + 1);
        let node = &mut self.nodes[index];
        node.outcome = Some(feedback.outcome);
        node.novelty = novelty;
        // Novel members join the corpus, and so do members with rare
        // features, so a useful state survives once the vocabulary saturates.
        if (novelty > 0 || rarity > 0) && !feedback.anchors.is_empty() {
            node.in_corpus = true;
            let fails = feedback
                .features
                .iter()
                .filter(|f| f.key.ends_with(":fails"))
                .map(|f| f.key.clone())
                .collect();
            self.corpus.push(Entry {
                ordinal: feedback.ordinal,
                depth,
                anchors: feedback.anchors,
                choices: feedback.choices,
                moments: feedback.moments,
                features: feedback.features.iter().map(|f| f.key.clone()).collect(),
                fails,
                rarity,
                novelty,
                interest_ns,
                picks: 0,
            });
            if self.corpus.len() > self.config.corpus_limit as usize {
                let evict = (0..self.corpus.len())
                    .min_by_key(|&i| {
                        let e = &self.corpus[i];
                        (e.novelty + e.rarity, std::cmp::Reverse(e.picks), e.ordinal)
                    })
                    .expect("a nonempty corpus");
                let gone = self.corpus.remove(evict).ordinal;
                self.nodes[gone as usize].in_corpus = false;
                self.released.push(gone);
            }
        } else {
            self.released.push(feedback.ordinal);
        }
    }
}

/// A mutation as two trigger inputs: the fault after the selected moment,
/// and its teardown after the fault's own effect.
fn mutation_inputs(
    m: &Mutation,
    ordinal: u32,
    at_ns: u64,
    timeout_ns: u64,
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>, String> {
    use seedbox_control::{Action, EventSelector, TriggerSpec};
    use seedbox_engine_protocol::input_program::{InputAction, RuleAction, TimedInput};
    let key = format!(
        "{}{ordinal}",
        seedbox_engine_protocol::input_program::MUTATION_PREFIX
    );
    let heal = Action::Heal {
        handle: key.clone(),
    };
    let (fault, undo) = match &m.kind {
        MutationKind::Crash => (
            Action::Crash {
                machine: m.target.clone(),
            },
            Action::Reboot {
                machine: m.target.clone(),
            },
        ),
        MutationKind::Pause => (
            Action::Pause {
                machine: m.target.clone(),
            },
            Action::Resume {
                machine: m.target.clone(),
            },
        ),
        MutationKind::Isolate => (
            Action::Partition {
                handle: key.clone(),
                left: vec![m.target.clone()],
                right: m.others.clone(),
            },
            heal,
        ),
        MutationKind::SlowDisk { .. } => (
            Action::DiskLatency {
                handle: key.clone(),
                machine: m.target.clone(),
                op: seedbox_control::DiskOp::Any,
                latency_ns: m.latency_ns,
                jitter_ns: 0,
            },
            heal,
        ),
        MutationKind::Preempt { ppm } => (
            Action::Preempt {
                handle: key.clone(),
                machine: m.target.clone(),
                ppm: *ppm,
            },
            heal,
        ),
    };
    let rule = |key: String, spec: TriggerSpec| TimedInput {
        key,
        at_ns,
        action: InputAction::Rule(RuleAction::Trigger(Box::new(spec))),
        timeout_ns,
    };
    Ok(vec![
        rule(
            key.clone(),
            TriggerSpec {
                when: m.when.clone(),
                nth: m.nth,
                delay_ns: m.delay_ns,
                within_ns: None,
                then: fault,
            },
        ),
        rule(
            format!("{key}-end"),
            TriggerSpec {
                when: EventSelector::Fired { trigger: key },
                nth: 1,
                delay_ns: m.duration_ns,
                within_ns: None,
                then: undo,
            },
        ),
    ])
}

/// A root's program: its authored inputs and the declared candidate anchors.
pub fn root_program(
    mut program: Vec<seedbox_engine_protocol::input_program::TimedInput>,
    anchors: &[u64],
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>, String> {
    use seedbox_engine_protocol::input_program::{BranchAction, InputAction, TimedInput};
    for (index, &at_ns) in anchors.iter().enumerate() {
        let key = anchor_key(index);
        if program.iter().any(|i| i.key == key) {
            return Err(format!("program already has an input named {key}"));
        }
        // Before any ordinary input at its time, as branch inputs must be.
        let position = program.partition_point(|i| i.at_ns < at_ns);
        program.insert(
            position,
            TimedInput {
                key,
                at_ns,
                action: InputAction::Branch(BranchAction::Anchor {}),
                timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            },
        );
    }
    Ok(program)
}

/// A branch's program from its parent's: the consumed prefix through the
/// anchor unchanged, then the reseed and choice inputs, then the parent's
/// pending inputs less its generated faults that had not started, plus
/// `suffix`: generated faults sampled for the branch, all after the anchor.
/// Teardowns of generated faults already started stay.
pub fn branch_program(
    parent: &[seedbox_engine_protocol::input_program::TimedInput],
    branch: &Branch,
    ordinal: u32,
    suffix: Vec<crate::faults::Generated>,
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>, String> {
    use seedbox_engine_protocol::input_program::{
        BranchAction, InputAction, RuleAction, TimedInput, GENERATED_END_SUFFIX, GENERATED_PREFIX,
    };
    let anchor = parent
        .iter()
        .position(|i| i.key == branch.anchor)
        .ok_or_else(|| format!("parent has no anchor {}", branch.anchor))?;
    if parent[anchor].at_ns != branch.at_ns {
        return Err(format!("anchor {} moved", branch.anchor));
    }
    let timeout_ns = seedbox_engine_protocol::input_program::default_timeout_ns();
    let mut program = parent[..=anchor].to_vec();
    let started: std::collections::BTreeSet<&str> = program
        .iter()
        .filter(|i| i.key.starts_with(GENERATED_PREFIX))
        .map(|i| i.key.as_str())
        .collect();
    let mut pending: Vec<TimedInput> = Vec::new();
    if let Some(m) = &branch.mutation {
        pending.extend(mutation_inputs(m, ordinal, branch.at_ns, timeout_ns)?);
    }
    pending.push(TimedInput {
        key: format!("reseed-{ordinal}"),
        at_ns: branch.at_ns,
        action: InputAction::Branch(BranchAction::Reseed {
            seed: branch.reseed,
            identity: branch.identity.clone(),
        }),
        timeout_ns,
    });
    for (index, choice) in branch.choices.iter().enumerate() {
        pending.push(TimedInput {
            key: format!("choice-{ordinal}-{index}"),
            at_ns: branch.at_ns,
            action: InputAction::Rule(RuleAction::Choice(choice.clone())),
            timeout_ns,
        });
    }
    for input in &parent[anchor + 1..] {
        if input.key.starts_with(GENERATED_PREFIX) {
            let start = input.key.strip_suffix(GENERATED_END_SUFFIX);
            if !start.is_some_and(|s| started.contains(s)) {
                continue;
            }
        }
        pending.push(input.clone());
    }
    for generated in suffix
        .into_iter()
        .filter(|g| g.at_ns > branch.at_ns || g.key.ends_with(GENERATED_END_SUFFIX))
    {
        // Renamed into this member's namespace, keeping the teardown pairing.
        let old = generated
            .key
            .strip_suffix(GENERATED_END_SUFFIX)
            .unwrap_or(&generated.key)
            .to_owned();
        let index = old.strip_prefix(GENERATED_PREFIX).unwrap_or(&old);
        let renamed = format!("{GENERATED_PREFIX}{ordinal}x{index}");
        let key = generated.key.replacen(&old, &renamed, 1);
        let mut action = serde_json::to_value(&generated.action).map_err(|e| e.to_string())?;
        if action.get("handle").and_then(|h| h.as_str()) == Some(old.as_str()) {
            action["handle"] = renamed.clone().into();
        }
        pending.push(TimedInput {
            key,
            at_ns: generated.at_ns.max(branch.at_ns),
            action: serde_json::from_value(action).map_err(|e| e.to_string())?,
            timeout_ns,
        });
    }
    // Drop teardowns whose start was sampled before the anchor and skipped.
    let keys: std::collections::BTreeSet<String> = pending
        .iter()
        .chain(&program)
        .map(|i| i.key.clone())
        .collect();
    pending.retain(|i| {
        i.key
            .strip_suffix(GENERATED_END_SUFFIX)
            .filter(|s| s.starts_with(GENERATED_PREFIX))
            .is_none_or(|start| keys.contains(start))
    });
    // Branch inputs first at their time, then the rest in order.
    pending.sort_by_key(|i| (i.at_ns, !matches!(i.action, InputAction::Branch(_))));
    program.extend(pending);
    Ok(program)
}

#[cfg(test)]
#[path = "campaign_tests.rs"]
mod tests;
