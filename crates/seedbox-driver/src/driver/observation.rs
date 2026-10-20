//! A single local handoff for committed events and optional frame facts.
use super::*;
#[cfg(test)]
mod tests;

/// One drain of committed events and bounded frame facts. Events are never
/// capacity-limited: consumers apply required bookkeeping from them once before
/// publishing them. Frame facts carry explicit loss watermarks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushResults {
    pub position: Position,
    pub events: Vec<Event>,
    pub frames: FrameFacts,
}

/// Bounded copies of routing facts, owned only by the host observer session.
#[derive(Clone, Default)]
pub(super) struct FrameObservations {
    pub(super) capacity: usize,
    pub(super) frames: VecDeque<FrameFact>,
    pub(super) omitted: u64,
}
impl FrameObservations {
    pub(super) fn record(&mut self, fact: FrameFact) {
        if self.capacity == 0 {
            return;
        }
        if self.frames.len() < self.capacity {
            self.frames.push_back(fact);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
    pub(super) fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        let discarded = self.frames.len().saturating_sub(capacity);
        self.frames.truncate(capacity);
        self.omitted = self.omitted.saturating_add(discarded as u64);
    }
    pub(super) fn drain(&mut self) -> FrameFacts {
        FrameFacts {
            facts: self.frames.drain(..).collect(),
            omitted: std::mem::take(&mut self.omitted),
        }
    }
    pub(super) fn mark(&self) -> (usize, u64) {
        (self.frames.len(), self.omitted)
    }
    pub(super) fn discard(&mut self, mark: (usize, u64)) {
        self.frames.truncate(mark.0);
        self.omitted = mark.1;
    }
}
