//! A single-owner source interval index. Owners must serialize mutations at a
//! stopped guest boundary. No locks, wall-time epochs or blocked-reader waits.
//! Backing offsets (not virtual aliases) identify writes. Private/COW mappings
//! need their own resolved backing identities before using this index.
use crate::Error;
use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use core::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Backing(pub u64);
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub backing: Backing,
    pub bytes: Range<u64>,
}
#[derive(Debug)]
pub struct Generation<T> {
    pub id: u64,
    pub entry: u64,
    pub spans: Vec<Span>,
    pub value: T,
}
/// A pin owns immutable code/maps for reconstruction, not permission to run.
pub type Pin<T> = Arc<Generation<T>>;

pub struct SourceIndex<T> {
    next: u64,
    live: BTreeMap<u64, Pin<T>>,
    entries: BTreeMap<u64, u64>,
    // Balanced start tree; deliberately conservative prefix scan. An augmented
    // max-end tree is a later optimization, not needed for correct overlap.
    starts: BTreeMap<(Backing, u64, u64), u64>,
}
impl<T> Default for SourceIndex<T> {
    fn default() -> Self {
        Self {
            next: 1,
            live: BTreeMap::new(),
            entries: BTreeMap::new(),
            starts: BTreeMap::new(),
        }
    }
}
impl<T> SourceIndex<T> {
    pub fn install(&mut self, entry: u64, spans: Vec<Span>, value: T) -> Result<Pin<T>, Error> {
        if spans.is_empty()
            || spans.iter().any(|s| s.bytes.start >= s.bytes.end)
            || self.entries.contains_key(&entry)
        {
            return Err(Error::Range);
        }
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(Error::GenerationExhausted)?;
        let pin = Arc::new(Generation {
            id,
            entry,
            spans,
            value,
        });
        for s in &pin.spans {
            self.starts
                .entry((s.backing, s.bytes.start, id))
                .and_modify(|end| *end = (*end).max(s.bytes.end))
                .or_insert(s.bytes.end);
        }
        self.entries.insert(entry, id);
        self.live.insert(id, pin.clone());
        Ok(pin)
    }
    pub fn lookup(&self, entry: u64) -> Option<Pin<T>> {
        self.entries
            .get(&entry)
            .and_then(|id| self.live.get(id))
            .cloned()
    }
    pub fn is_live(&self, pin: &Pin<T>) -> bool {
        self.live.get(&pin.id).is_some_and(|p| Arc::ptr_eq(p, pin))
    }
    /// Remove every dependent translation, including all its other spans.
    /// Existing pins keep maps alive; this never waits for a parked task.
    pub fn invalidate(&mut self, write: Span) -> Result<Vec<u64>, Error> {
        if write.bytes.start >= write.bytes.end {
            return Err(Error::Range);
        }
        let ids: BTreeSet<_> = self
            .starts
            .range((write.backing, 0, 0)..(write.backing, write.bytes.end, 0))
            .filter_map(|((_, _, id), end)| (*end > write.bytes.start).then_some(*id))
            .collect();
        for id in &ids {
            if let Some(pin) = self.live.remove(id) {
                self.entries.remove(&pin.entry);
                for s in &pin.spans {
                    self.starts.remove(&(s.backing, s.bytes.start, *id));
                }
            }
        }
        Ok(ids.into_iter().collect())
    }
    pub fn len(&self) -> usize {
        self.live.len()
    }
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{sync::Weak, vec};
    fn span(a: u64, b: u64) -> Span {
        Span {
            backing: Backing(1),
            bytes: a..b,
        }
    }
    #[test]
    fn append_keeps_neighbor_overwrite_retires_all_spans_without_waiting() {
        let mut tree = SourceIndex::default();
        let a = tree
            .install(
                10,
                vec![span(10, 20), span(4090, 4100)],
                "immutable old maps",
            )
            .unwrap();
        let b = tree
            .install(11, vec![span(11, 16)], "overlapping entry")
            .unwrap();
        let weak: Weak<_> = Arc::downgrade(&a);
        assert!(tree.invalidate(span(20, 24)).unwrap().is_empty());
        assert!(tree.is_live(&a));
        assert_eq!(tree.invalidate(span(12, 13)).unwrap(), vec![a.id, b.id]);
        assert!(tree.is_empty());
        assert!(!tree.is_live(&a));
        assert_eq!(a.value, "immutable old maps");
        assert!(tree.invalidate(span(4096, 4097)).unwrap().is_empty());
        let new = tree.install(10, vec![span(10, 20)], "new").unwrap();
        assert_ne!(new.id, a.id);
        assert!(weak.upgrade().is_some());
        drop(a);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn alias_backing_and_high_end_boundaries() {
        let mut t = SourceIndex::default();
        let p = t
            .install(4, vec![span(u64::MAX - 8, u64::MAX)], ())
            .unwrap();
        assert!(t
            .invalidate(Span {
                backing: Backing(2),
                bytes: 0..u64::MAX
            })
            .unwrap()
            .is_empty());
        assert_eq!(
            t.invalidate(span(u64::MAX - 1, u64::MAX)).unwrap(),
            vec![p.id]
        );
        assert_eq!(t.invalidate(span(9, 9)), Err(Error::Range));
    }
    #[test]
    fn duplicate_start_retains_longest_dependency_and_all_frame_pins() {
        let mut t = SourceIndex::default();
        let outer = t.install(10, vec![span(10, 40), span(10, 20)], ()).unwrap();
        let inner = outer.clone();
        let weak = Arc::downgrade(&outer);
        assert_eq!(t.invalidate(span(30, 31)).unwrap(), vec![outer.id]);
        drop(outer);
        assert!(weak.upgrade().is_some());
        drop(inner);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn generation_exhaustion_fails_without_installing() {
        let mut t = SourceIndex {
            next: u64::MAX,
            ..SourceIndex::default()
        };
        assert!(matches!(
            t.install(1, vec![span(1, 2)], ()),
            Err(Error::GenerationExhausted)
        ));
        assert!(t.is_empty());
    }
    #[test]
    fn second_page_overlap_invalidates_cross_page_translation() {
        let mut t = SourceIndex::default();
        let p = t.install(4092, vec![span(4092, 4104)], ()).unwrap();
        assert_eq!(t.invalidate(span(4100, 4104)).unwrap(), vec![p.id]);
    }
}
