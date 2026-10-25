//! Campaign planning: a pluggable strategy that maps member results to the
//! next generation of member plans.
//!
//! The contract is that plans depend only on results, never on execution
//! capacity or completion order. A [`Planner`] therefore never sees members
//! as they finish. [`Generations`] numbers members in plan order before any
//! is dispatched, buffers results that arrive in any order, and asks for the
//! next generation only once every member planned so far has reported. The
//! planner then receives every completed member in ordinal order, with its
//! plan and what it showed, and nothing else: no worker count, no host time,
//! no arrival order. With the same campaign seed and the same results, any
//! correct planner makes the same plans however the members ran.
//!
//! A seed range is the planner whose every member is a root with the next
//! seed. Feature-guided planners (novelty, rare states, swarm roots,
//! event-relative mutations) implement the same trait. A campaign is a
//! server-side search over completed results, so it lives here; the seeded
//! fault-profile sampler is deterministic engine policy and belongs in
//! `seedbox-engine-core`. No server runs a planner yet.
use seedbox_api::{CampaignPlan, MemberPlan, TestVerdict};
use seedbox_engine_protocol::{
    choice::ChoiceRecord,
    control::{EventSelector, MAX_TRIGGERS},
    input_program::MUTATION_TRIGGERS,
};
use std::collections::BTreeMap;

/// Something a member was observed to do, from its retained evidence alone,
/// such as `property:ID:fails`, `event:NAME:BUCKET`, `state:MACHINE/NAME=VALUE`
/// or `choice:SITE:INDEX`. `at_ns` is when it was first seen, if known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feature {
    pub key: String,
    pub at_ns: Option<u64>,
}

/// What a finished member showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    pub ordinal: u32,
    pub verdict: TestVerdict,
    pub features: Vec<Feature>,
    /// Anchor captures it can be branched from: input key and time.
    pub anchors: Vec<(String, u64)>,
    /// The choices it answered.
    pub choices: Vec<ChoiceRecord>,
    /// Things a trigger can select, with when it first did each.
    pub moments: Vec<(EventSelector, u64)>,
}

/// A member whose result is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completed {
    pub ordinal: u32,
    pub generation: u32,
    pub plan: MemberPlan,
    pub feedback: Feedback,
}

/// What a planner is asked for: `size` plans for generation `index`, whose
/// members take ordinals from `first_ordinal`, given every earlier member.
#[derive(Clone, Copy, Debug)]
pub struct Generation<'a> {
    pub index: u32,
    pub first_ordinal: u32,
    pub size: u32,
    /// Every member of earlier generations, in ordinal order.
    pub history: &'a [Completed],
}

/// A search strategy. `plan` must be a function of its argument and the
/// planner's own seeded state, which only earlier calls may change.
pub trait Planner {
    fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String>;
}
/// A strategy chosen at run time, such as a portfolio's member.
impl<P: Planner + ?Sized> Planner for Box<P> {
    fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String> {
        (**self).plan(generation)
    }
}

/// Drives a [`Planner`] generation by generation, enforcing the contract:
/// a generation is planned only when the previous one has fully reported,
/// and results reach the planner in ordinal order.
pub struct Generations<P> {
    planner: P,
    budget: u32,
    batch: u32,
    generation: u32,
    next_ordinal: u32,
    outstanding: BTreeMap<u32, (u32, MemberPlan)>,
    buffered: BTreeMap<u32, Feedback>,
    history: Vec<Completed>,
    /// Trigger IDs a member's mutations may take: the run's shared budget
    /// less the triggers the roots' own program arms.
    trigger_budget: usize,
}

impl<P: Planner> Generations<P> {
    pub fn new(planner: P, plan: &CampaignPlan) -> Result<Self, String> {
        plan.validate()?;
        Ok(Self {
            planner,
            budget: plan.budget,
            batch: plan.batch,
            generation: 0,
            next_ordinal: 0,
            outstanding: BTreeMap::new(),
            buffered: BTreeMap::new(),
            history: Vec::new(),
            trigger_budget: MAX_TRIGGERS,
        })
    }

    /// The roots' program already arms `triggers` triggers, which every
    /// member inherits, so mutations get the rest of the run's budget.
    pub fn with_program_triggers(mut self, triggers: usize) -> Self {
        self.trigger_budget = MAX_TRIGGERS.saturating_sub(triggers);
        self
    }

    /// The next generation's members by ordinal, once every earlier member
    /// has reported; empty while any is outstanding or the budget is spent.
    pub fn next_generation(&mut self) -> Result<Vec<(u32, MemberPlan)>, String> {
        if !self.outstanding.is_empty() || self.next_ordinal >= self.budget {
            return Ok(Vec::new());
        }
        let size = self.batch.min(self.budget - self.next_ordinal);
        let plans = self.planner.plan(Generation {
            index: self.generation,
            first_ordinal: self.next_ordinal,
            size,
            history: &self.history,
        })?;
        if plans.len() != size as usize {
            return Err(format!(
                "planner made {} plans for a generation of {size}",
                plans.len()
            ));
        }
        for plan in &plans {
            self.check(plan)?;
        }
        let mut planned = Vec::new();
        for plan in plans {
            let ordinal = self.next_ordinal;
            self.next_ordinal += 1;
            self.outstanding
                .insert(ordinal, (self.generation, plan.clone()));
            planned.push((ordinal, plan));
        }
        self.generation += 1;
        Ok(planned)
    }

    /// A branch continues a completed member from an anchor it captured,
    /// and its lineage's mutations fit the trigger budget.
    fn check(&self, plan: &MemberPlan) -> Result<(), String> {
        let MemberPlan::Branch {
            parent,
            anchor,
            mutation,
            ..
        } = plan
        else {
            return Ok(());
        };
        let source = self
            .history
            .get(*parent as usize)
            .ok_or_else(|| format!("branch parent {parent} has not completed"))?;
        if !source.feedback.anchors.iter().any(|(key, _)| key == anchor) {
            return Err(format!("member {parent} has no capture at anchor {anchor}"));
        }
        let mut mutations = usize::from(mutation.is_some());
        let mut ancestor = Some(*parent);
        while let Some(ordinal) = ancestor {
            ancestor = match &self.history[ordinal as usize].plan {
                MemberPlan::Branch {
                    parent, mutation, ..
                } => {
                    mutations += usize::from(mutation.is_some());
                    Some(*parent)
                }
                MemberPlan::Root { .. } => None,
            };
        }
        if mutations * MUTATION_TRIGGERS > self.trigger_budget {
            return Err(format!(
                "a lineage of {mutations} mutations needs {} trigger IDs; {} are left",
                mutations * MUTATION_TRIGGERS,
                self.trigger_budget
            ));
        }
        Ok(())
    }

    /// Record a member's result, in any order. Each planned member reports
    /// exactly once.
    pub fn report(&mut self, feedback: Feedback) -> Result<(), String> {
        let ordinal = feedback.ordinal;
        if !self.outstanding.contains_key(&ordinal) || self.buffered.contains_key(&ordinal) {
            return Err(format!("member {ordinal} is not awaiting a result"));
        }
        self.buffered.insert(ordinal, feedback);
        if self.buffered.len() == self.outstanding.len() {
            // The generation is complete: fold it in ordinal order.
            for (ordinal, (generation, plan)) in std::mem::take(&mut self.outstanding) {
                let feedback = self
                    .buffered
                    .remove(&ordinal)
                    .expect("every member reported");
                self.history.push(Completed {
                    ordinal,
                    generation,
                    plan,
                    feedback,
                });
            }
        }
        Ok(())
    }

    /// Every member has been planned and has reported.
    pub fn done(&self) -> bool {
        self.next_ordinal >= self.budget && self.outstanding.is_empty()
    }

    /// Completed members in ordinal order.
    pub fn history(&self) -> &[Completed] {
        &self.history
    }
}
