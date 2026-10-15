//! Query results against a model of a ledger's objects, for TigerBeetle's
//! `query_transfers`, `query_accounts` and `get_account_transfers`
//! (tigerbeetle#2544: the zig-zag merge join ended scans early and omitted
//! results of queries that filter on two or more fields).
//!
//! The model is every object that exists at the end of the run, with the
//! commit timestamp the database gave it (read back with lookups once the
//! cluster is quiet). TigerBeetle assigns timestamps in commit order, so the
//! objects committed before any query are a timestamp prefix. A query with
//! filter `F`, `limit` k and direction d therefore returns a contiguous run of
//! `M`, the final objects matching `F` in timestamp order:
//!
//! * ascending: `M[0..n]` with `n <= k`; descending: `M[c-n..c]` reversed for
//!   the query's cut `c`, and `c - n == 0` when `n < k`;
//! * the cut covers every matching object whose create was acknowledged before
//!   the query was invoked (all of `M` for a query after the workload), and no
//!   object whose create was invoked after the query completed.
//!
//! A result that breaks this is reported: `missing` (an object that had to be
//! there is not), `gap` or `order` (not a contiguous run in order), `unknown`
//! (an object that never existed), `mismatch` (an object that does not match
//! the filter or differs from what was created) and `future`.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
};

/// The fields a filter can select on, plus what identifies the object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields {
    pub ledger: u32,
    pub code: u16,
    pub user_data_32: u32,
    pub user_data_64: u64,
    pub user_data_128: u128,
    /// Transfers only.
    pub debit: u128,
    pub credit: u128,
}

/// What a query selects. Zero fields match anything, as in TigerBeetle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Filter {
    pub ledger: u32,
    pub code: u16,
    pub user_data_32: u32,
    pub user_data_64: u64,
    pub user_data_128: u128,
    /// `get_account_transfers`: the account, and which sides.
    pub account: u128,
    pub debits: bool,
    pub credits: bool,
    pub timestamp_min: u64,
    pub timestamp_max: u64,
}

impl Filter {
    /// How many fields select (the bug needs two or more intersected indexes).
    pub fn width(&self) -> usize {
        [
            self.ledger != 0,
            self.code != 0,
            self.user_data_32 != 0,
            self.user_data_64 != 0,
            self.user_data_128 != 0,
            self.account != 0,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    pub fn matches(&self, fields: &Fields, timestamp: u64) -> bool {
        (self.ledger == 0 || self.ledger == fields.ledger)
            && (self.code == 0 || self.code == fields.code)
            && (self.user_data_32 == 0 || self.user_data_32 == fields.user_data_32)
            && (self.user_data_64 == 0 || self.user_data_64 == fields.user_data_64)
            && (self.user_data_128 == 0 || self.user_data_128 == fields.user_data_128)
            && (self.account == 0
                || (self.debits && fields.debit == self.account)
                || (self.credits && fields.credit == self.account))
            && (self.timestamp_min == 0 || timestamp >= self.timestamp_min)
            && (self.timestamp_max == 0 || timestamp <= self.timestamp_max)
    }
}

/// One object of the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object {
    pub id: u128,
    pub timestamp: u64,
    pub fields: Fields,
    /// History position at which its create was acknowledged (u64::MAX if never).
    pub acked: u64,
    /// History position at which its create was invoked.
    pub invoked: u64,
}

/// One object a query returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Returned {
    pub id: u128,
    pub timestamp: u64,
    pub fields: Fields,
}

/// One completed query.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    /// `query_transfers`, `query_accounts` or `get_account_transfers`.
    pub op: String,
    pub filter: Filter,
    pub limit: u32,
    pub reversed: bool,
    pub invoked: u64,
    pub completed: u64,
    pub results: Vec<Returned>,
}

/// The final objects of one kind, by id.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub objects: BTreeMap<u128, Object>,
    /// Every object in timestamp order, built on first use.
    sorted: RefCell<Rc<Vec<Object>>>,
    /// Positions in `sorted` of the objects matching a filter (without its
    /// timestamp range), in timestamp order.
    cache: RefCell<HashMap<Filter, Rc<Vec<u32>>>>,
}

/// `M` for one query: a view of the matching objects in timestamp order.
pub struct Matching {
    sorted: Rc<Vec<Object>>,
    all: Rc<Vec<u32>>,
    low: usize,
    high: usize,
}

impl Matching {
    pub fn len(&self) -> usize {
        self.high - self.low
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> &Object {
        &self.sorted[self.all[self.low + index] as usize]
    }

    /// The position of the object with this timestamp.
    fn position(&self, timestamp: u64) -> Option<usize> {
        let slice = &self.all[self.low..self.high];
        let at = slice.partition_point(|i| self.sorted[*i as usize].timestamp < timestamp);
        (at < slice.len() && self.sorted[slice[at] as usize].timestamp == timestamp).then_some(at)
    }

    /// Up to ten objects of `range`, as report examples.
    fn examples(&self, range: std::ops::Range<usize>) -> Vec<Value> {
        range
            .take(10)
            .map(|i| {
                let o = self.get(i);
                json!({"id": o.id, "timestamp": o.timestamp, "acked": o.acked})
            })
            .collect()
    }
}

impl Model {
    pub fn insert(&mut self, object: Object) {
        self.objects.insert(object.id, object);
        *self.sorted.borrow_mut() = Rc::new(Vec::new());
        self.cache.borrow_mut().clear();
    }

    fn sorted(&self) -> Rc<Vec<Object>> {
        let mut sorted = self.sorted.borrow_mut();
        if sorted.len() != self.objects.len() {
            let mut all: Vec<Object> = self.objects.values().copied().collect();
            all.sort_by_key(|o| o.timestamp);
            *sorted = Rc::new(all);
        }
        sorted.clone()
    }

    /// `M`: the objects matching `filter`, in timestamp order.
    pub fn matching(&self, filter: &Filter) -> Matching {
        let sorted = self.sorted();
        let key = Filter {
            timestamp_min: 0,
            timestamp_max: 0,
            ..*filter
        };
        let all = self
            .cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| {
                Rc::new(
                    sorted
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| key.matches(&o.fields, o.timestamp))
                        .map(|(i, _)| i as u32)
                        .collect(),
                )
            })
            .clone();
        let low = all.partition_point(|i| sorted[*i as usize].timestamp < filter.timestamp_min);
        let high = if filter.timestamp_max == 0 {
            all.len()
        } else {
            all.partition_point(|i| sorted[*i as usize].timestamp <= filter.timestamp_max)
        };
        Matching {
            sorted,
            all,
            low,
            high: high.max(low),
        }
    }

    /// The first violation of `query`, if any, as a JSON report.
    pub fn check(&self, query: &Query) -> Option<Value> {
        let m = self.matching(&query.filter);
        let report = |kind: &str, extra: Value| {
            let mut value = json!({"kind": kind, "op": query.op, "filter": query.filter,
                "width": query.filter.width(), "limit": query.limit, "reversed": query.reversed,
                "returned": query.results.len(), "matching": m.len(),
                "invoked": query.invoked, "completed": query.completed});
            if let (Value::Object(v), Value::Object(e)) = (&mut value, extra) {
                v.extend(e);
            }
            Some(value)
        };
        let mut positions = Vec::with_capacity(query.results.len());
        for r in &query.results {
            let Some(object) = self.objects.get(&r.id) else {
                return report("unknown", json!({"id": r.id}));
            };
            if object.fields != r.fields
                || object.timestamp != r.timestamp
                || !query.filter.matches(&r.fields, r.timestamp)
            {
                return report(
                    "mismatch",
                    json!({"id": r.id, "returned": r, "model": {"fields": object.fields, "timestamp": object.timestamp}}),
                );
            }
            if object.invoked > query.completed {
                return report("future", json!({"id": r.id}));
            }
            match m.position(r.timestamp) {
                Some(index) => positions.push(index),
                None => return report("mismatch", json!({"id": r.id, "not_matching": true})),
            }
        }
        let step: isize = if query.reversed { -1 } else { 1 };
        for pair in positions.windows(2) {
            let delta = pair[1] as isize - pair[0] as isize;
            if delta != step {
                let forward = delta.signum() == step.signum();
                let skipped = if forward {
                    m.examples(pair[0].min(pair[1]) + 1..pair[0].max(pair[1]))
                } else {
                    Vec::new()
                };
                return report(
                    if forward { "gap" } else { "order" },
                    json!({"at": [pair[0], pair[1]], "missing": if forward { delta.unsigned_abs() - 1 } else { 0 }, "skipped": skipped}),
                );
            }
        }
        // The highest position the query's cut must cover: the last matching
        // object acknowledged before the query began. Objects are in
        // timestamp order, so scan from the end.
        let required = (0..m.len()).rev().find(|i| m.get(*i).acked < query.invoked);
        let limit = query.limit as usize;
        let n = positions.len();
        // Missing ranges of positions in M.
        let mut missing: Vec<std::ops::Range<usize>> = Vec::new();
        if query.reversed {
            // A run ending (highest) at the cut, down to 0 if it is short.
            if let Some(required) = required {
                match positions.first().copied() {
                    Some(high) if high >= required => {}
                    Some(high) => missing.push(high + 1..required + 1),
                    None => missing.push((required + 1).saturating_sub(limit)..required + 1),
                }
            }
            if n < limit {
                if let Some(&low) = positions.last() {
                    missing.push(0..low);
                }
            }
        } else {
            let start = positions.first().copied().unwrap_or(0);
            missing.push(0..start.min(m.len()));
            if let Some(required) = required {
                let need = (required + 1).min(limit);
                if start + n < need {
                    missing.push(start + n..need);
                }
            }
        }
        missing.retain(|r| !r.is_empty());
        if !missing.is_empty() {
            let count: usize = missing.iter().map(|r| r.len()).sum();
            let examples: Vec<Value> = missing
                .iter()
                .flat_map(|r| m.examples(r.clone()))
                .take(10)
                .collect();
            return report(
                "missing",
                json!({"missing": count, "ranges": missing.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>(), "examples": examples}),
            );
        }
        None
    }
}

/// Tallies of a set of query checks.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Summary {
    pub queries: usize,
    pub by_width: BTreeMap<usize, usize>,
    pub violations: usize,
    pub violations_by_kind: BTreeMap<String, usize>,
    pub violations_by_width: BTreeMap<usize, usize>,
    pub violations_by_op: BTreeMap<String, usize>,
    pub missing_objects: usize,
}

impl Summary {
    pub fn add(&mut self, query: &Query, violation: Option<&Value>) {
        self.queries += 1;
        let width = query.filter.width();
        *self.by_width.entry(width).or_default() += 1;
        if let Some(v) = violation {
            self.violations += 1;
            let kind = v["kind"].as_str().unwrap_or("?").to_string();
            *self.violations_by_kind.entry(kind).or_default() += 1;
            *self.violations_by_width.entry(width).or_default() += 1;
            *self.violations_by_op.entry(query.op.clone()).or_default() += 1;
            self.missing_objects += v["missing"].as_u64().unwrap_or(0) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(n: u64) -> Model {
        let mut model = Model::default();
        for i in 1..=n {
            model.insert(Object {
                id: i as u128,
                timestamp: i * 10,
                fields: Fields {
                    ledger: 1,
                    code: (i % 2 + 1) as u16,
                    ..Fields::default()
                },
                acked: i,
                invoked: i,
            });
        }
        model
    }

    fn query(
        model: &Model,
        filter: Filter,
        limit: u32,
        reversed: bool,
        ids: &[u128],
        at: u64,
    ) -> Query {
        Query {
            op: "query_transfers".into(),
            filter,
            limit,
            reversed,
            invoked: at,
            completed: at + 1,
            results: ids
                .iter()
                .map(|id| {
                    let o = &model.objects[id];
                    Returned {
                        id: o.id,
                        timestamp: o.timestamp,
                        fields: o.fields,
                    }
                })
                .collect(),
        }
    }

    #[test]
    fn complete_and_prefix_results_pass() {
        let m = model(10);
        let f = Filter {
            ledger: 1,
            code: 1,
            ..Filter::default()
        };
        // code 1: ids 2,4,6,8,10.
        assert_eq!(
            m.check(&query(&m, f, 100, false, &[2, 4, 6, 8, 10], 100)),
            None
        );
        assert_eq!(m.check(&query(&m, f, 2, false, &[2, 4], 100)), None);
        assert_eq!(m.check(&query(&m, f, 2, true, &[10, 8], 100)), None);
        // Invoked after only 2 and 4 were acknowledged: a shorter prefix is fine.
        assert_eq!(m.check(&query(&m, f, 100, false, &[2, 4], 5)), None);
        assert_eq!(m.check(&query(&m, f, 100, true, &[4, 2], 5)), None);
        // A reversed full page may end at any cut covering the acknowledged ones.
        assert_eq!(m.check(&query(&m, f, 2, true, &[6, 4], 5)), None);
    }

    #[test]
    fn truncated_results_are_missing() {
        let m = model(10);
        let f = Filter {
            ledger: 1,
            code: 1,
            ..Filter::default()
        };
        let v = m.check(&query(&m, f, 100, false, &[2, 4, 6], 100)).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["missing"].as_u64()),
            (Some("missing"), Some(2))
        );
        let v = m.check(&query(&m, f, 100, true, &[10, 8], 100)).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["missing"].as_u64()),
            (Some("missing"), Some(3))
        );
        let v = m.check(&query(&m, f, 100, false, &[], 100)).unwrap();
        assert_eq!(v["missing"].as_u64(), Some(5));
        let v = m
            .check(&query(&m, f, 100, false, &[2, 6, 8, 10], 100))
            .unwrap();
        assert_eq!(v["kind"].as_str(), Some("gap"));
        let v = m.check(&query(&m, f, 100, false, &[4, 6], 100)).unwrap();
        assert_eq!(v["kind"].as_str(), Some("missing"));
    }

    #[test]
    fn wrong_objects_are_reported() {
        let m = model(10);
        let f = Filter {
            ledger: 1,
            code: 1,
            ..Filter::default()
        };
        let mut q = query(&m, f, 100, false, &[2, 4, 6, 8, 10], 100);
        q.results[1].fields.code = 2;
        assert_eq!(m.check(&q).unwrap()["kind"].as_str(), Some("mismatch"));
        q.results[1].id = 99;
        assert_eq!(m.check(&q).unwrap()["kind"].as_str(), Some("unknown"));
        let q = query(&m, f, 100, false, &[4, 2, 6, 8, 10], 100);
        assert_eq!(m.check(&q).unwrap()["kind"].as_str(), Some("order"));
    }

    #[test]
    fn account_filters_select_sides_and_ranges() {
        let fields = Fields {
            debit: 5,
            credit: 6,
            code: 3,
            ..Fields::default()
        };
        let debits = Filter {
            account: 5,
            debits: true,
            ..Filter::default()
        };
        assert!(debits.matches(&fields, 1));
        assert!(!Filter {
            account: 5,
            credits: true,
            ..Filter::default()
        }
        .matches(&fields, 1));
        assert!(!Filter { code: 4, ..debits }.matches(&fields, 1));
        assert!(!Filter {
            timestamp_min: 2,
            ..debits
        }
        .matches(&fields, 1));
        assert_eq!(Filter { code: 4, ..debits }.width(), 2);
    }
}
