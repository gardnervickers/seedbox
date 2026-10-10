//! The windowed world scheduler: several machines execute the same
//! simulated interval concurrently, with results that do not depend on how
//! many host workers run them.
//!
//! # Execution contract
//!
//! The world advances in rounds. Each round executes a half-open simulated
//! interval `[T, B)`:
//!
//! - `T` is the earliest time any live machine can next act: its own next
//!   action, or the first event queued for it.
//! - `B` is `T + latency - early`, capped by the run's duration and by any
//!   scheduled global boundary after `T`. A frame sent at `s >= T` arrives no
//!   sooner than `s + latency`, and may be delivered up to `early` before
//!   that, so no frame produced in the round can be due before `B`.
//! - A machine takes part in a round only if it can act before `B`. It
//!   receives an immutable [`Assignment`]: the events already queued for it
//!   that may be delivered in the round, in [`EventKey`] order, when it
//!   wakes, and where to stop. A horizon stop may land up to `early` (and at
//!   most half the round) before `B`; a global boundary or the run's end is
//!   exact, and no event at or after it is delivered early across it.
//! - Effects a machine produces in the round are buffered in its [`Report`]
//!   with their actual send times. Only after every machine has finished does
//!   the coordinator sort them by (send time, machine, machine sequence),
//!   route them through the coordinator-owned [`Router`], check that none is
//!   due inside the executed interval, and queue them.
//! - A designated machine that exits ends the world after the round that
//!   contains its exit commits. The outcome records both its exit time and
//!   the world's final committed boundary.
//!
//! Events at the same instant are ordered by [`EventKey`]: time, then
//! [`Class`], then [`Source`], then the source's own sequence number. Worker
//! count, core assignment and host completion order never enter a decision:
//! [`run_serial`] is the reference and [`run_parallel`] must match it.

mod exec;
mod plan;

pub use exec::{run_parallel, run_serial, Flow, Hook, Machine, NoHook, Stats};
pub use plan::{
    Assignment, Class, Delivery, Error, Event, EventKey, MachineState, Outcome, Output, Params,
    Report, RoundPlan, RoundRecord, Router, Source, Step, Time, World, NEVER,
};
