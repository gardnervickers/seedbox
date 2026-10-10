//! One byte budget shared across all application log streams.
use super::{Delivery, Payload, PublishError, Record};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

pub struct History<T> {
    state: Mutex<State<T>>,
    limit: usize,
    next_stream: AtomicU64,
}
type ChargedRecord<T> = (Arc<Record<T>>, usize);

struct State<T> {
    records: BTreeMap<(u64, u64), ChargedRecord<T>>,
    order: VecDeque<(u64, u64)>,
    bytes: usize,
}
impl<T> History<T> {
    pub fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(State {
                records: BTreeMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
            limit,
            next_stream: AtomicU64::new(1),
        }
    }
    pub(super) fn allocate(&self) -> u64 {
        self.next_stream.fetch_add(1, Ordering::Relaxed)
    }
    pub(super) fn remove(&self, stream: u64) {
        if let Ok(mut state) = self.state.lock() {
            let keys: Vec<_> = state
                .records
                .range((stream, 0)..=(stream, u64::MAX))
                .map(|(key, _)| *key)
                .collect();
            for key in keys {
                if let Some((_, bytes)) = state.records.remove(&key) {
                    state.bytes -= bytes;
                }
            }
            state.order.retain(|(id, _)| *id != stream);
        }
    }
    pub(super) fn read(&self, stream: u64, next: &mut u64, end: u64) -> Delivery<T> {
        let record = self.state.lock().ok().and_then(|state| {
            state
                .records
                .range((stream, *next)..(stream, end))
                .next()
                .map(|(_, (record, _))| record.clone())
        });
        match record {
            Some(record) if record.sequence == *next => {
                *next += 1;
                Delivery::Record(record)
            }
            other => {
                let first = *next;
                *next = other.map_or(end, |record| record.sequence);
                Delivery::Gap {
                    first,
                    last: *next - 1,
                }
            }
        }
    }
}
impl<T: Payload> History<T> {
    pub(super) fn push(&self, stream: u64, record: Arc<Record<T>>) -> Result<(), PublishError> {
        let charge = record
            .payload
            .payload_bytes()
            .saturating_add(std::mem::size_of::<Record<T>>() + 64);
        let mut state = self.state.lock().map_err(|_| PublishError::Poisoned)?;
        while state.bytes.saturating_add(charge) > self.limit {
            let Some(key) = state.order.pop_front() else {
                break;
            };
            if let Some((_, bytes)) = state.records.remove(&key) {
                state.bytes -= bytes;
            }
        }
        if charge <= self.limit {
            let key = (stream, record.sequence);
            state.records.insert(key, (record, charge));
            state.order.push_back(key);
            state.bytes += charge;
        }
        Ok(())
    }
}
