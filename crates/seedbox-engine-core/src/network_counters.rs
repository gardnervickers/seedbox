//! Inspection counts nominal network events strictly before an admission.
//! Packet DMA may happen in an early window, so physical totals cannot be sampled
//! directly. Pending deltas are drained at every global frontier, not retained as
//! packet history. The receiver's actual acceptance outcome remains authoritative.
use std::collections::BTreeMap;
#[derive(Default, Clone)]
pub struct NetworkCounters {
    totals:BTreeMap<(usize,usize),[u64;4]>,
    pending:BTreeMap<u64,Vec<(usize,usize,usize)>>,
    frontier:u64,
}
impl NetworkCounters {
    pub fn record(&mut self,at:u64,from:usize,to:usize,outcome:usize) {
        if at<self.frontier {self.totals.entry((from,to)).or_default()[outcome]+=1;}
        else {self.pending.entry(at).or_default().push((from,to,outcome));}
    }
    pub fn advance(&mut self,before:u64) {
        assert!(before>=self.frontier,"network counter frontier moved backwards");
        self.frontier=before;
        while self.pending.first_key_value().is_some_and(|(&at,_)|at<before) {
            let (_,deltas)=self.pending.pop_first().unwrap();
            for (from,to,outcome) in deltas {self.totals.entry((from,to)).or_default()[outcome]+=1;}
        }
    }
    pub fn counts(&self)->&BTreeMap<(usize,usize),[u64;4]> {&self.totals}
}
