//! The in-memory [`History`] most scenarios record into.
use crate::{Completion, Event, History, Record};
use std::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

/// An append-only history in memory. Records take their index and time in
/// the order calls take its lock; times are guest monotonic nanoseconds since
/// the recorder was made, which under Seedbox is simulated time.
pub struct Recorder<O, V> {
    records: Mutex<Vec<Record<O, V>>>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

impl<O, V> Default for Recorder<O, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O, V> Recorder<O, V> {
    pub fn new() -> Self {
        let start = Instant::now();
        Self::with_clock(move || start.elapsed().as_nanos() as u64)
    }

    /// A recorder whose times come from `clock`, such as a test's counter.
    pub fn with_clock(clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            clock: Box::new(clock),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Record<O, V>>> {
        // A panicking client thread leaves complete records behind.
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

impl<O: Clone + Send, V: Clone + Send> History<O, V> for Recorder<O, V> {
    fn invoke(&self, process: u64, op: O) -> u64 {
        let mut records = self.lock();
        let index = records.len() as u64;
        records.push(Record {
            index,
            process,
            time_ns: (self.clock)(),
            event: Event::Invoke { op },
        });
        index
    }

    /// Completing an index that is not an invocation is a scenario bug.
    fn complete(&self, invoke: u64, completion: Completion<V>) {
        let mut records = self.lock();
        let process = match records.get(invoke as usize) {
            Some(Record {
                process,
                event: Event::Invoke { .. },
                ..
            }) => *process,
            _ => panic!("history index {invoke} is not an invocation"),
        };
        let index = records.len() as u64;
        let event = match completion {
            Completion::Ok(value) => Event::Ok { invoke, value },
            Completion::Fail(error) => Event::Fail { invoke, error },
            Completion::Info(error) => Event::Info { invoke, error },
        };
        records.push(Record {
            index,
            process,
            time_ns: (self.clock)(),
            event,
        });
    }

    fn records(&self) -> Vec<Record<O, V>> {
        self.lock().clone()
    }
}

/// The operation each completion completes, for checkers: pairs of the
/// invocation's op and the completing event, in completion order.
pub fn completions<O, V>(history: &[Record<O, V>]) -> impl Iterator<Item = (&O, &Record<O, V>)> {
    history.iter().filter_map(move |record| {
        let invoke = match record.event {
            Event::Invoke { .. } => return None,
            Event::Ok { invoke, .. } | Event::Fail { invoke, .. } | Event::Info { invoke, .. } => {
                invoke
            }
        };
        match &history.get(invoke as usize)?.event {
            Event::Invoke { op } => Some((op, record)),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    #[test]
    fn records_take_indexes_times_and_their_invokers_process() {
        let tick = Arc::new(AtomicU64::new(0));
        let clock = tick.clone();
        let history: Recorder<&str, u64> =
            Recorder::with_clock(move || clock.fetch_add(5, Ordering::SeqCst));
        let a = history.invoke(0, "a");
        let b = history.invoke(1, "b");
        history.complete(b, Completion::Info("timeout".into()));
        history.complete(a, Completion::Ok(7));
        // A late acknowledgement completes the same invocation again.
        history.complete(b, Completion::Ok(8));
        let records = history.records();
        assert_eq!(records.len(), 5);
        assert_eq!(
            records[3],
            Record {
                index: 3,
                process: 0,
                time_ns: 15,
                event: Event::Ok {
                    invoke: 0,
                    value: 7
                }
            }
        );
        assert_eq!(records[4].process, 1);
        let pairs: Vec<_> = completions(&records)
            .map(|(op, record)| (*op, record.index))
            .collect();
        assert_eq!(pairs, [("b", 2), ("a", 3), ("b", 4)]);
    }

    #[test]
    #[should_panic(expected = "is not an invocation")]
    fn completing_a_completion_is_a_bug() {
        let history: Recorder<u8, u8> = Recorder::new();
        let a = history.invoke(0, 1);
        history.complete(a, Completion::Ok(1));
        history.complete(a + 1, Completion::Ok(2));
    }
}
