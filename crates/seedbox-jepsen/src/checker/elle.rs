//! An Elle-style checker for list-append transaction histories.
//!
//! Each transaction is a list of micro-operations on keys whose values are
//! lists: `append k v` adds `v` to the end of `k`'s list, and `r k L` reads
//! `k`'s whole list (`None` while unknown). Appended values are unique per
//! key, so every element of a read names the transaction that wrote it, and
//! because a read returns the whole list the version order of a key is the
//! prefix order of the lists read: the longest read is the order, and every
//! other read must be a prefix of it. See Kingsbury and Alvaro, "Elle:
//! Inferring Isolation Anomalies from Experimental Observations" (VLDB 2020).
//!
//! A history is a slice of [`wgl::Operation`]s, the same representation the
//! linearizability checks use: [`OpOutcome::Completed`] is an `ok`
//! (committed) transaction whose return value holds its micro-operations with
//! reads filled in, [`OpOutcome::Failed`] a definite abort, and
//! [`OpOutcome::Pending`] an indeterminate (`info`) one, which counts as
//! committed only when a committed read observes one of its appends.
//!
//! Non-cyclic anomalies: `G1a` (aborted read), `G1b` (intermediate read),
//! `internal` (a read disagrees with the transaction's own earlier reads and
//! appends), `incompatible-order` (neither read a prefix of the other),
//! `duplicate-elements`, `garbage` (a value nobody appended) and
//! `lost-update` (Adya's P4: two committed transactions read the same version
//! of a key, then both append to it).
//!
//! The dependency graph over committed transactions has ww, wr and rw
//! (anti-dependency) edges. A committed append that no read observed must
//! follow every observed element, so it gets a ww edge from the last observed
//! writer and rw edges from every committed read of that key that missed it.
//! Cycles, one example of each type per strongly connected component, are
//! classified as in Adya's thesis: `G0` (ww only), `G1c` (ww and wr),
//! `G-single` (exactly one rw) and `G2-item` (two or more rw).
//!
//! Under serializability every anomaly is a violation. Snapshot isolation
//! allows G2-item only, and read committed G-single, G2-item, internal
//! inconsistency and lost updates as well.
//! Adya's repeatable read (PL-2.99) allows only predicate G2, which this
//! workload cannot produce, so it forbids what serializability does;
//! PostgreSQL's REPEATABLE READ is snapshot isolation and is checked as such.
//!
//! This is a faithful port of the earlier Python checker, including its
//! deterministic search order (identifiers sort by their Python `repr`), so it
//! reports the same anomalies, cycles and counts; Jepsen's Elle remains the
//! reference (see the `elle-cross-check` command).
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use wgl::{OpOutcome, Operation};

/// How many example edges each cycle search starts from, per component.
pub const SEARCH_LIMIT: usize = 200;
const PATH_BUDGET: i64 = 200_000;
pub const CYCLE_TYPES: [&str; 4] = ["G0", "G1c", "G-single", "G2-item"];

/// The consistency models and the anomalies each allows.
pub fn allowed(model: &str) -> Option<&'static [&'static str]> {
    match model {
        "serializable" | "repeatable-read" => Some(&[]),
        "snapshot-isolation" => Some(&["G2-item"]),
        // PL-2 forbids only G0 and G1: a non-repeatable read inside one
        // transaction (`internal`) and a lost update are allowed.
        "read-committed" => Some(&["G-single", "G2-item", "internal", "lost-update"]),
        _ => None,
    }
}
pub const MODELS: [&str; 4] = [
    "read-committed",
    "repeatable-read",
    "serializable",
    "snapshot-isolation",
];

/// A key, transaction ID or process: an integer or a name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Atom {
    Int(i64),
    Str(String),
}

impl Atom {
    /// Python's `repr`, which orders the original checker's search.
    pub fn repr(&self) -> String {
        match self {
            Self::Int(n) => n.to_string(),
            Self::Str(s) => format!("'{s}'"),
        }
    }
    pub fn from_json(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_i64().map(Self::Int),
            Value::String(s) => Some(Self::Str(s.clone())),
            _ => None,
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Self::Int(n) => Value::from(*n),
            Self::Str(s) => Value::from(s.clone()),
        }
    }
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(n) => write!(f, "{n}"),
            Self::Str(s) => f.write_str(s),
        }
    }
}

impl From<i64> for Atom {
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}
impl From<&str> for Atom {
    fn from(s: &str) -> Self {
        Self::Str(s.into())
    }
}

pub type Key = Atom;
pub type TxnId = Atom;

/// One micro-operation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Mop {
    Append(Key, i64),
    Read(Key, Option<Vec<i64>>),
}

impl Mop {
    pub fn key(&self) -> &Key {
        match self {
            Self::Append(k, _) | Self::Read(k, _) => k,
        }
    }
    /// The invocation form: reads unknown.
    pub fn invocation(&self) -> Self {
        match self {
            Self::Read(k, _) => Self::Read(k.clone(), None),
            other => other.clone(),
        }
    }
    fn python(&self) -> String {
        match self {
            Self::Append(k, v) => format!("['append', {}, {v}]", k.repr()),
            Self::Read(k, None) => format!("['r', {}, None]", k.repr()),
            Self::Read(k, Some(values)) => format!(
                "['r', {}, [{}]]",
                k.repr(),
                values
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Self::Append(k, v) => json!(["append", k.to_json(), v]),
            Self::Read(k, values) => json!(["r", k.to_json(), values]),
        }
    }
    /// `["append", k, v]` or `["r", k, [..] | null]`.
    pub fn from_json(value: &Value) -> Option<Self> {
        let parts = value.as_array()?;
        let key = Atom::from_json(parts.get(1)?)?;
        match parts.first()?.as_str()? {
            "append" => Some(Self::Append(key, parts.get(2)?.as_i64()?)),
            "r" => {
                let read = match parts.get(2) {
                    None | Some(Value::Null) => None,
                    Some(list) => Some(
                        list.as_array()?
                            .iter()
                            .map(Value::as_i64)
                            .collect::<Option<Vec<_>>>()?,
                    ),
                };
                Some(Self::Read(key, read))
            }
            _ => None,
        }
    }
}

/// A transaction's invocation and the metadata reports need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txn {
    pub id: TxnId,
    /// The process; `None` when unknown.
    pub process: Option<Atom>,
    /// Micro-operations as invoked (reads unknown).
    pub mops: Vec<Mop>,
    /// When an indeterminate completion was observed: wgl records those as
    /// `Pending`, but an EDN export still writes their `:info` completion.
    pub info_at: Option<u64>,
    pub error: Option<String>,
}

/// One transaction of a list-append history.
pub type TxnOp = Operation<Txn, Vec<Mop>>;

/// Build an `ok`, `fail` or `info` transaction; `value` is what completed
/// (reads filled for `ok`; any reads are dropped otherwise).
pub fn txn(
    id: impl Into<Atom>,
    kind: &str,
    value: Vec<Mop>,
    process: Option<Atom>,
    invoked_at: u64,
    completed_at: u64,
    error: Option<String>,
) -> Result<TxnOp, String> {
    let id = id.into();
    let mops: Vec<Mop> = value.iter().map(Mop::invocation).collect();
    let (outcome, info_at) = match kind {
        "ok" => (
            OpOutcome::Completed {
                at: completed_at,
                ret: value,
            },
            None,
        ),
        "fail" => (OpOutcome::Failed { at: completed_at }, None),
        "info" => (OpOutcome::Pending, Some(completed_at)),
        other => {
            return Err(format!(
                "transaction {}: type {other:?} is not ok, fail or info",
                id.repr()
            ))
        }
    };
    Ok(Operation {
        op: Txn {
            id,
            process,
            mops,
            info_at,
            error,
        },
        invoked_at,
        outcome,
    })
}

pub fn kind_of(op: &TxnOp) -> &'static str {
    match op.outcome {
        OpOutcome::Completed { .. } => "ok",
        OpOutcome::Failed { .. } => "fail",
        OpOutcome::Pending => "info",
    }
}

/// The micro-operations to analyze: completed values for `ok`, the
/// invocation for the others.
fn value_of(op: &TxnOp) -> &[Mop] {
    match &op.outcome {
        OpOutcome::Completed { ret, .. } => ret,
        _ => &op.op.mops,
    }
}

/// Parse `H {"i", "p", "type", "t0", "t1", "value", "error"}` lines, the
/// list-append oracle's history format (times in seconds).
pub fn from_h_lines(lines: &[Value]) -> Result<Vec<TxnOp>, String> {
    lines
        .iter()
        .map(|line| {
            let id = Atom::from_json(&line["i"]).ok_or("history line without i")?;
            let value = line["value"]
                .as_array()
                .ok_or("history line without value")?
                .iter()
                .map(|m| Mop::from_json(m).ok_or_else(|| format!("bad micro-op {m}")))
                .collect::<Result<Vec<_>, _>>()?;
            let seconds = |key: &str| line[key].as_f64().map(seconds_to_ns).unwrap_or(0);
            txn(
                id,
                line["type"].as_str().unwrap_or(""),
                value,
                Atom::from_json(&line["p"]),
                seconds("t0"),
                seconds("t1"),
                line["error"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// Seconds as nanoseconds, rounded half to even as Python's `round` does.
pub fn seconds_to_ns(seconds: f64) -> u64 {
    (seconds * 1e9).round_ties_even().max(0.0) as u64
}

/// An `H` line for a completed transaction.
pub fn h_line(op: &TxnOp) -> Value {
    let (t1, value) = match &op.outcome {
        OpOutcome::Completed { at, ret } => (*at, ret.clone()),
        OpOutcome::Failed { at } => (*at, op.op.mops.clone()),
        OpOutcome::Pending => (op.op.info_at.unwrap_or(op.invoked_at), op.op.mops.clone()),
    };
    json!({
        "i": op.op.id.to_json(),
        "p": op.op.process.as_ref().map(Atom::to_json),
        "type": kind_of(op),
        "t0": op.invoked_at as f64 / 1e9,
        "t1": t1 as f64 / 1e9,
        "value": value.iter().map(Mop::to_json).collect::<Vec<_>>(),
        "error": op.op.error,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    Ww,
    Wr,
    Rw,
}

impl EdgeKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ww => "ww",
            Self::Wr => "wr",
            Self::Rw => "rw",
        }
    }
}

const KINDS: [EdgeKind; 3] = [EdgeKind::Ww, EdgeKind::Wr, EdgeKind::Rw];

/// Why an edge exists: the key and the values involved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Why {
    pub k: Key,
    pub v: Option<i64>,
    pub w: Option<i64>,
}

impl Why {
    fn to_json(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("k".into(), self.k.to_json());
        if let Some(v) = self.v {
            map.insert("v".into(), v.into());
        }
        if let Some(w) = self.w {
            map.insert("w".into(), w.into());
        }
        map
    }
}

/// Dependency edges between transactions (by index into the history), each
/// kind with its first explanation.
#[derive(Default, Debug)]
pub struct Graph {
    edges: BTreeMap<usize, BTreeMap<usize, BTreeMap<EdgeKind, Why>>>,
}

impl Graph {
    fn add(&mut self, a: usize, b: usize, kind: EdgeKind, why: Why) {
        if a == b {
            return;
        }
        self.edges
            .entry(a)
            .or_default()
            .entry(b)
            .or_default()
            .entry(kind)
            .or_insert(why);
    }

    pub fn count(&self) -> BTreeMap<&'static str, usize> {
        let mut counts = BTreeMap::new();
        for targets in self.edges.values() {
            for kinds in targets.values() {
                for kind in kinds.keys() {
                    *counts.entry(kind.name()).or_default() += 1;
                }
            }
        }
        counts
    }

    fn why(&self, a: usize, b: usize, kind: EdgeKind) -> &Why {
        &self.edges[&a][&b][&kind]
    }
}

/// One step of a cycle.
type Step = (usize, usize, EdgeKind);

struct Analysis<'a> {
    ops: &'a [TxnOp],
    /// Each node's position in Python `repr` order of its transaction ID.
    rank: Vec<usize>,
    graph: Graph,
}

impl Analysis<'_> {
    /// A node's targets sorted by `repr` of their IDs.
    fn targets(&self, node: usize) -> Vec<usize> {
        let mut targets: Vec<usize> = self
            .graph
            .edges
            .get(&node)
            .map(|t| t.keys().copied().collect())
            .unwrap_or_default();
        targets.sort_by_key(|&t| self.rank[t]);
        targets
    }

    fn nodes(&self) -> Vec<usize> {
        let mut nodes: BTreeSet<usize> = self.graph.edges.keys().copied().collect();
        for targets in self.graph.edges.values() {
            nodes.extend(targets.keys().copied());
        }
        let mut nodes: Vec<usize> = nodes.into_iter().collect();
        nodes.sort_by_key(|&n| self.rank[n]);
        nodes
    }

    /// Strongly connected components with more than one node (iterative Tarjan).
    fn components(&self) -> Vec<Vec<usize>> {
        let mut index: HashMap<usize, usize> = HashMap::new();
        let mut low: HashMap<usize, usize> = HashMap::new();
        let mut on_stack: HashSet<usize> = HashSet::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut out = Vec::new();
        let mut counter = 0;
        for root in self.nodes() {
            if index.contains_key(&root) {
                continue;
            }
            let mut work: Vec<(usize, Vec<usize>, usize)> = vec![(root, self.targets(root), 0)];
            index.insert(root, counter);
            low.insert(root, counter);
            counter += 1;
            stack.push(root);
            on_stack.insert(root);
            while let Some((node, children, position)) = work.last_mut() {
                let node = *node;
                let mut advanced = None;
                while *position < children.len() {
                    let child = children[*position];
                    *position += 1;
                    if !index.contains_key(&child) {
                        advanced = Some(child);
                        break;
                    }
                    if on_stack.contains(&child) {
                        let value = low[&node].min(index[&child]);
                        low.insert(node, value);
                    }
                }
                if let Some(child) = advanced {
                    index.insert(child, counter);
                    low.insert(child, counter);
                    counter += 1;
                    stack.push(child);
                    on_stack.insert(child);
                    let targets = self.targets(child);
                    work.push((child, targets, 0));
                    continue;
                }
                work.pop();
                if let Some((parent, _, _)) = work.last() {
                    let value = low[parent].min(low[&node]);
                    low.insert(*parent, value);
                }
                if low[&node] == index[&node] {
                    let mut members = Vec::new();
                    loop {
                        let member = stack.pop().expect("Tarjan stack");
                        on_stack.remove(&member);
                        members.push(member);
                        if member == node {
                            break;
                        }
                    }
                    if members.len() > 1 {
                        members.sort_by_key(|&m| self.rank[m]);
                        out.push(members);
                    }
                }
            }
        }
        out
    }

    /// Outgoing steps inside `members`, strongest kind first.
    fn steps(
        &self,
        node: usize,
        members: &HashSet<usize>,
        allowed: &[EdgeKind],
    ) -> Vec<(usize, EdgeKind)> {
        let mut out = Vec::new();
        for target in self.targets(node) {
            if members.contains(&target) {
                let kinds = &self.graph.edges[&node][&target];
                for kind in KINDS {
                    if kinds.contains_key(&kind) && allowed.contains(&kind) {
                        out.push((target, kind));
                    }
                }
            }
        }
        out
    }

    /// Shortest simple path `start -> goal` inside `members` over `allowed`
    /// kinds: breadth-first, or with `need_rw` an iterative deepening search
    /// for a path with at least one rw edge, within a fixed budget.
    fn path(
        &self,
        members: &HashSet<usize>,
        start: usize,
        goal: usize,
        allowed: &[EdgeKind],
        need_rw: bool,
    ) -> Option<Vec<Step>> {
        if !need_rw {
            let mut parents: HashMap<usize, Option<(usize, EdgeKind)>> = HashMap::new();
            parents.insert(start, None);
            let mut queue = VecDeque::from([start]);
            while let Some(node) = queue.pop_front() {
                for (target, kind) in self.steps(node, members, allowed) {
                    if parents.contains_key(&target) {
                        continue;
                    }
                    parents.insert(target, Some((node, kind)));
                    if target == goal {
                        let mut steps = Vec::new();
                        let mut at = target;
                        while let Some(Some((previous, kind))) = parents.get(&at) {
                            steps.push((*previous, at, *kind));
                            at = *previous;
                        }
                        steps.reverse();
                        return Some(steps);
                    }
                    queue.push_back(target);
                }
            }
            return None;
        }
        let mut remaining = PATH_BUDGET;
        for depth in 1..members.len() {
            let mut seen: HashSet<usize> = HashSet::from([start, goal]);
            let mut steps = Vec::new();
            let found = self.deepen(
                members,
                goal,
                allowed,
                start,
                depth,
                false,
                &mut seen,
                &mut steps,
                &mut remaining,
            );
            if found.is_some() || remaining < 0 {
                return found;
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn deepen(
        &self,
        members: &HashSet<usize>,
        goal: usize,
        allowed: &[EdgeKind],
        node: usize,
        depth: usize,
        used: bool,
        seen: &mut HashSet<usize>,
        steps: &mut Vec<Step>,
        remaining: &mut i64,
    ) -> Option<Vec<Step>> {
        *remaining -= 1;
        if *remaining < 0 {
            return None;
        }
        for (target, kind) in self.steps(node, members, allowed) {
            let uses = used || kind == EdgeKind::Rw;
            if target == goal {
                if uses {
                    let mut found = steps.clone();
                    found.push((node, target, kind));
                    return Some(found);
                }
                continue;
            }
            if depth > 1 && !seen.contains(&target) {
                seen.insert(target);
                steps.push((node, target, kind));
                let found = self.deepen(
                    members,
                    goal,
                    allowed,
                    target,
                    depth - 1,
                    uses,
                    seen,
                    steps,
                    remaining,
                );
                steps.pop();
                seen.remove(&target);
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    /// One shortest example cycle of each type in one component.
    fn cycles(&self, members: &[usize]) -> Vec<(&'static str, Vec<Step>)> {
        let member_set: HashSet<usize> = members.iter().copied().collect();
        let edges_of = |kind: EdgeKind| -> Vec<Step> {
            let mut out = Vec::new();
            for &a in members {
                for b in self.targets(a) {
                    if member_set.contains(&b) && self.graph.edges[&a][&b].contains_key(&kind) {
                        out.push((a, b, kind));
                    }
                }
            }
            out
        };
        let best =
            |candidates: &[Step], allowed: &[EdgeKind], need_rw: bool| -> Option<Vec<Step>> {
                let mut shortest: Option<Vec<Step>> = None;
                for &(a, b, kind) in candidates.iter().take(SEARCH_LIMIT) {
                    if let Some(back) = self.path(&member_set, b, a, allowed, need_rw) {
                        if shortest.as_ref().is_none_or(|s| back.len() + 1 < s.len()) {
                            let mut cycle = vec![(a, b, kind)];
                            cycle.extend(back);
                            shortest = Some(cycle);
                        }
                    }
                }
                shortest
            };
        use EdgeKind::*;
        let ww = edges_of(Ww);
        let wr = edges_of(Wr);
        let rw = edges_of(Rw);
        let mut found = Vec::new();
        if let Some(cycle) = best(&ww, &[Ww], false) {
            found.push(("G0", cycle));
        }
        if let Some(cycle) = best(&wr, &[Ww, Wr], false) {
            found.push(("G1c", cycle));
        }
        if let Some(cycle) = best(&rw, &[Ww, Wr], false) {
            found.push(("G-single", cycle));
        }
        if let Some(cycle) = best(&rw, &[Ww, Wr, Rw], true) {
            found.push(("G2-item", cycle));
        }
        found
    }

    fn describe(&self, node: usize) -> String {
        let op = &self.ops[node];
        let process = op
            .op
            .process
            .as_ref()
            .map(Atom::to_string)
            .unwrap_or_else(|| "None".into());
        let value: Vec<String> = value_of(op).iter().map(Mop::python).collect();
        format!(
            "{{id {}, process {process}, {}, value [{}]}}",
            op.op.id,
            kind_of(op),
            value.join(", ")
        )
    }

    /// Elle-style text: each step `Ta < Tb` with its reason, then the contradiction.
    fn explain(&self, cycle: &[Step]) -> String {
        let mut names: Vec<(usize, String)> = Vec::new();
        for &(a, _, _) in cycle {
            if !names.iter().any(|(n, _)| *n == a) {
                names.push((a, format!("T{}", names.len() + 1)));
            }
        }
        let name = |node: usize| {
            names
                .iter()
                .find(|(n, _)| *n == node)
                .map(|(_, s)| s.clone())
                .unwrap_or_default()
        };
        let mut lines = vec!["Let:".to_string()];
        for (node, label) in &names {
            lines.push(format!("  {label} = {}", self.describe(*node)));
        }
        lines.push("Then:".into());
        for (position, &(a, b, kind)) in cycle.iter().enumerate() {
            let why = self.graph.why(a, b, kind);
            let last = position == cycle.len() - 1;
            let (na, nb) = (name(a), name(b));
            let value = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();
            let reason = match kind {
                EdgeKind::Ww => format!(
                    "{na} appended {} to {} and {nb} appended {} after it",
                    value(why.v),
                    why.k,
                    value(why.w)
                ),
                EdgeKind::Wr => format!(
                    "{nb} observed {na}'s append of {} to {}",
                    value(why.v),
                    why.k
                ),
                EdgeKind::Rw => format!(
                    "{na} did not observe {nb}'s append of {} to {}",
                    value(why.w),
                    why.k
                ),
            };
            lines.push(format!(
                "  - {}{na} < {nb}, because {reason}{}",
                if last { "However, " } else { "" },
                if last { ": a contradiction!" } else { "." }
            ));
        }
        lines.join("\n")
    }
}

fn classify(cycle: &[Step]) -> &'static str {
    let rw = cycle.iter().filter(|s| s.2 == EdgeKind::Rw).count();
    let wr = cycle.iter().filter(|s| s.2 == EdgeKind::Wr).count();
    if rw >= 2 {
        "G2-item"
    } else if rw == 1 {
        "G-single"
    } else if wr > 0 {
        "G1c"
    } else {
        "G0"
    }
}

/// Anomalies by type, the dependency graph's edge counts and transaction types.
pub struct Analyzed {
    pub anomalies: BTreeMap<String, Vec<Value>>,
    pub edges: BTreeMap<&'static str, usize>,
}

/// Infer dependencies and anomalies.
pub fn analyze(ops: &[TxnOp]) -> Result<Analyzed, String> {
    let mut by_id: HashMap<&TxnId, usize> = HashMap::new();
    for (index, op) in ops.iter().enumerate() {
        if by_id.insert(&op.op.id, index).is_some() {
            return Err(format!("duplicate transaction id {}", op.op.id.repr()));
        }
    }
    let mut order_of: Vec<usize> = (0..ops.len()).collect();
    order_of.sort_by_cached_key(|&i| ops[i].op.id.repr());
    let mut rank = vec![0; ops.len()];
    for (position, &index) in order_of.iter().enumerate() {
        rank[index] = position;
    }
    let kind = |i: usize| kind_of(&ops[i]);
    let mut anomalies: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut report = |name: &str, item: Value| anomalies.entry(name.into()).or_default().push(item);

    // Writers, in history order, and each transaction's last append per key.
    let mut writer: HashMap<(Key, i64), usize> = HashMap::new();
    let mut writer_order: Vec<((Key, i64), usize)> = Vec::new();
    let mut last_append: HashMap<(usize, Key), i64> = HashMap::new();
    for (index, op) in ops.iter().enumerate() {
        for mop in value_of(op) {
            if let Mop::Append(k, v) = mop {
                if let Some(previous) = writer.insert((k.clone(), *v), index) {
                    return Err(format!(
                        "value {v} appended to key {} twice ({} and {})",
                        k.repr(),
                        ops[previous].op.id.repr(),
                        op.op.id.repr()
                    ));
                }
                writer_order.push(((k.clone(), *v), index));
                last_append.insert((index, k.clone()), *v);
            }
        }
    }

    // Internal consistency, and the committed reads that feed version orders.
    let mut reads: Vec<(usize, Key, Vec<i64>)> = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        if kind(index) != "ok" {
            continue;
        }
        let id = op.op.id.to_json();
        let mut known: HashMap<Key, Vec<i64>> = HashMap::new();
        let mut suffix: HashMap<Key, Vec<i64>> = HashMap::new();
        for (position, mop) in value_of(op).iter().enumerate() {
            match mop {
                Mop::Append(k, v) => {
                    if let Some(list) = known.get_mut(k) {
                        list.push(*v);
                    } else {
                        suffix.entry(k.clone()).or_default().push(*v);
                    }
                }
                Mop::Read(_, None) => {}
                Mop::Read(k, Some(value)) => {
                    let mine = suffix.get(k).cloned().unwrap_or_default();
                    if let Some(expected) = known.get(k) {
                        if value != expected {
                            report(
                                "internal",
                                json!({"txn": id, "op": position, "key": k.to_json(), "expected": expected, "read": value}),
                            );
                        }
                    } else if !mine.is_empty()
                        && (value.len() < mine.len()
                            || value[value.len() - mine.len()..] != mine[..])
                    {
                        report(
                            "internal",
                            json!({"txn": id, "op": position, "key": k.to_json(), "expected-suffix": mine, "read": value}),
                        );
                    }
                    known.insert(k.clone(), value.clone());
                    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
                    for x in value {
                        *counts.entry(*x).or_default() += 1;
                    }
                    if counts.len() != value.len() {
                        let mut duplicates: Vec<i64> = counts
                            .into_iter()
                            .filter(|(_, n)| *n > 1)
                            .map(|(x, _)| x)
                            .collect();
                        duplicates.sort_by_key(|x| x.to_string());
                        report(
                            "duplicate-elements",
                            json!({"txn": id, "key": k.to_json(), "read": value, "duplicates": duplicates}),
                        );
                    }
                    reads.push((index, k.clone(), value.clone()));
                }
            }
        }
    }

    // Lost updates (Adya's P4): committed transactions that read the same
    // version of a key before appending to it themselves. A version is named
    // by its last element (None for the empty list), as in Elle.
    let mut updates: BTreeMap<(Key, Option<i64>), Vec<usize>> = BTreeMap::new();
    for (index, op) in ops.iter().enumerate() {
        if kind(index) != "ok" {
            continue;
        }
        let mut first_read: HashMap<&Key, Option<i64>> = HashMap::new();
        let mut appended: HashSet<&Key> = HashSet::new();
        let mut updated: BTreeSet<(Key, Option<i64>)> = BTreeSet::new();
        for mop in value_of(op) {
            match mop {
                Mop::Read(k, Some(value)) if !appended.contains(k) => {
                    first_read.entry(k).or_insert_with(|| value.last().copied());
                }
                Mop::Append(k, _) => {
                    appended.insert(k);
                    if let Some(version) = first_read.get(k) {
                        updated.insert((k.clone(), *version));
                    }
                }
                Mop::Read(..) => {}
            }
        }
        for key in updated {
            updates.entry(key).or_default().push(index);
        }
    }
    for ((k, version), txns) in &updates {
        if txns.len() > 1 {
            report(
                "lost-update",
                json!({"key": k.to_json(), "value": version,
                       "txns": txns.iter().map(|&i| ops[i].op.id.to_json()).collect::<Vec<_>>()}),
            );
        }
    }

    // Version order per key: the longest committed read, which every read must prefix.
    let mut order: HashMap<Key, Vec<i64>> = HashMap::new();
    for (_, k, value) in &reads {
        let entry = order.entry(k.clone()).or_default();
        if value.len() > entry.len() {
            *entry = value.clone();
        }
    }
    let mut compatible: Vec<(usize, Key, Vec<i64>)> = Vec::new();
    for (index, k, value) in &reads {
        let longest = &order[k];
        if longest.len() >= value.len() && longest[..value.len()] == value[..] {
            compatible.push((*index, k.clone(), value.clone()));
        } else {
            report(
                "incompatible-order",
                json!({"key": k.to_json(), "txn": ops[*index].op.id.to_json(), "read": value, "longest": longest}),
            );
        }
    }

    let mut observed: HashSet<(Key, i64)> = HashSet::new();
    for (index, k, value) in &reads {
        let id = ops[*index].op.id.to_json();
        for v in value {
            observed.insert((k.clone(), *v));
            match writer.get(&(k.clone(), *v)) {
                None => report(
                    "garbage",
                    json!({"txn": id, "key": k.to_json(), "value": v}),
                ),
                Some(&source) if kind(source) == "fail" => report(
                    "G1a",
                    json!({"reader": id, "writer": ops[source].op.id.to_json(), "key": k.to_json(), "value": v}),
                ),
                Some(_) => {}
            }
        }
        // The last element another transaction wrote must be that writer's final append to the key.
        let foreign: Vec<i64> = value
            .iter()
            .copied()
            .filter(|v| writer.get(&(k.clone(), *v)).is_some_and(|w| w != index))
            .collect();
        if let Some(&v) = foreign.last() {
            let source = writer[&(k.clone(), v)];
            let final_append = last_append[&(source, k.clone())];
            if kind(source) != "fail" && final_append != v {
                report(
                    "G1b",
                    json!({"reader": id, "writer": ops[source].op.id.to_json(), "key": k.to_json(),
                           "value": v, "writer-final": final_append}),
                );
            }
        }
    }

    let mut committed: HashSet<usize> = (0..ops.len()).filter(|&i| kind(i) == "ok").collect();
    for kv in &observed {
        if let Some(&source) = writer.get(kv) {
            if kind(source) == "info" {
                committed.insert(source);
            }
        }
    }
    let committed_writer = |k: &Key, v: i64| -> Option<usize> {
        writer
            .get(&(k.clone(), v))
            .copied()
            .filter(|s| committed.contains(s))
    };

    let mut graph = Graph::default();
    let mut unobserved: BTreeMap<Key, Vec<(i64, usize)>> = BTreeMap::new();
    for ((k, v), source) in &writer_order {
        if committed.contains(source) && !observed.contains(&(k.clone(), *v)) {
            unobserved.entry(k.clone()).or_default().push((*v, *source));
        }
    }
    let keys: BTreeSet<Key> = order
        .keys()
        .cloned()
        .chain(unobserved.keys().cloned())
        .collect();
    for k in &keys {
        let versions: Vec<(i64, usize)> = order
            .get(k)
            .map(|list| {
                list.iter()
                    .filter_map(|&v| committed_writer(k, v).map(|w| (v, w)))
                    .collect()
            })
            .unwrap_or_default();
        for pair in versions.windows(2) {
            let ((v, a), (w, b)) = (pair[0], pair[1]);
            graph.add(
                a,
                b,
                EdgeKind::Ww,
                Why {
                    k: k.clone(),
                    v: Some(v),
                    w: Some(w),
                },
            );
        }
        if let Some(&(v, a)) = versions.last() {
            for &(w, b) in unobserved.get(k).into_iter().flatten() {
                graph.add(
                    a,
                    b,
                    EdgeKind::Ww,
                    Why {
                        k: k.clone(),
                        v: Some(v),
                        w: Some(w),
                    },
                );
            }
        }
    }
    for (reader, k, value) in &compatible {
        let mine: HashSet<i64> = value_of(&ops[*reader])
            .iter()
            .filter_map(|m| match m {
                Mop::Append(kk, v) if kk == k => Some(*v),
                _ => None,
            })
            .collect();
        for &v in value.iter().rev() {
            if mine.contains(&v) {
                continue;
            }
            if let Some(source) = committed_writer(k, v) {
                graph.add(
                    source,
                    *reader,
                    EdgeKind::Wr,
                    Why {
                        k: k.clone(),
                        v: Some(v),
                        w: None,
                    },
                );
                break;
            }
        }
        for &w in &order[k][value.len()..] {
            let Some(source) = committed_writer(k, w) else {
                continue; // an aborted append is no version
            };
            if source != *reader {
                graph.add(
                    *reader,
                    source,
                    EdgeKind::Rw,
                    Why {
                        k: k.clone(),
                        v: None,
                        w: Some(w),
                    },
                );
            }
            break;
        }
        for &(w, source) in unobserved.get(k).into_iter().flatten() {
            if source != *reader {
                graph.add(
                    *reader,
                    source,
                    EdgeKind::Rw,
                    Why {
                        k: k.clone(),
                        v: None,
                        w: Some(w),
                    },
                );
            }
        }
    }

    let analysis = Analysis { ops, rank, graph };
    for members in analysis.components() {
        for (name, cycle) in analysis.cycles(&members) {
            debug_assert_eq!(classify(&cycle), name);
            let steps: Vec<Value> = cycle
                .iter()
                .map(|&(a, b, kind)| {
                    let mut step = analysis.graph.why(a, b, kind).to_json();
                    step.insert("from".into(), ops[a].op.id.to_json());
                    step.insert("to".into(), ops[b].op.id.to_json());
                    step.insert("type".into(), kind.name().into());
                    Value::Object(step)
                })
                .collect();
            report(
                name,
                json!({
                    "cycle": cycle.iter().map(|&(a, _, _)| ops[a].op.id.to_json()).collect::<Vec<_>>(),
                    "steps": steps,
                    "explanation": analysis.explain(&cycle),
                    "component-size": members.len(),
                }),
            );
        }
    }
    Ok(Analyzed {
        anomalies,
        edges: analysis.graph.count(),
    })
}

/// The verdict for one consistency model.
#[derive(Debug, Clone)]
pub struct Checked {
    pub valid: bool,
    pub model: String,
    pub illegal: Vec<String>,
    pub counts: BTreeMap<String, usize>,
    pub anomalies: BTreeMap<String, Vec<Value>>,
    pub txns: BTreeMap<&'static str, usize>,
    pub edges: BTreeMap<&'static str, usize>,
}

impl Checked {
    pub fn to_json(&self) -> Value {
        json!({
            "valid": self.valid,
            "model": self.model,
            "anomaly-types": self.counts.keys().collect::<Vec<_>>(),
            "illegal": self.illegal,
            "counts": self.counts,
            "anomalies": self.anomalies,
            "txns": self.txns,
            "edges": self.edges,
        })
    }
}

/// Check a history against a consistency model.
pub fn check(ops: &[TxnOp], model: &str) -> Result<Checked, String> {
    let allowed = allowed(model).ok_or_else(|| {
        format!("unknown consistency model {model:?}; expected one of {MODELS:?}")
    })?;
    let analyzed = analyze(ops)?;
    let mut txns = BTreeMap::new();
    for op in ops {
        *txns.entry(kind_of(op)).or_default() += 1;
    }
    let illegal: Vec<String> = analyzed
        .anomalies
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .cloned()
        .collect();
    Ok(Checked {
        valid: illegal.is_empty(),
        model: model.into(),
        counts: analyzed
            .anomalies
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect(),
        illegal,
        anomalies: analyzed.anomalies,
        txns,
        edges: analyzed.edges,
    })
}

#[cfg(test)]
#[path = "elle_tests.rs"]
mod tests;
