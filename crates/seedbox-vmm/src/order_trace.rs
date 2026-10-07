//! EXPERIMENT ONLY (#60 equal-time ordering spike; never merge). With
//! `"order_trace": true` in the world config, write OUT/order-trace.jsonl: what the world does around each fault and
//! control boundary: boundary applications with every machine's time and key,
//! the slices that end at a boundary, and every routing decision. Routes are
//! written in full only within `order_trace_window_ns` (default 2 ms) of a
//! configured fault time; every route is still counted and checked.
//! Writes nothing the run reads back, so it cannot change a result.
use serde_json::{json, Value};
use std::io::Write;

pub struct OrderTrace {
    out: Option<std::io::BufWriter<std::fs::File>>,
    window: u64,
    times: Vec<u64>,
    pub routes: u64,
    /// Sends stamped at or after a boundary not yet applied: the convention broken.
    pub violations: u64,
    /// Sends stamped before the next boundary, routed while another machine
    /// already stood at or past it (the lagging-peer case).
    pub lagging: u64,
    /// Of those, sends whose guest had already read a clock at or past it.
    pub guest_saw_boundary: u64,
    pub max_clock_lead: u64,
    pub boundaries: u64,
    pub boundary_slices: [u64; 7],
}

pub fn stop_index(s: &crate::machine::Stop) -> usize {
    use crate::machine::Stop::*;
    match s { Reached => 0, Halted => 1, Exited => 2, Sent => 3, Barrier => 4, Control => 5 }
}

impl OrderTrace {
    pub fn new(cfg: &Value, dir: &str) -> Result<Self, String> {
        let out = if cfg["order_trace"] == true {
            let p = format!("{dir}/order-trace.jsonl");
            Some(std::io::BufWriter::new(std::fs::File::create(&p).map_err(|e| format!("{p}: {e}"))?))
        } else { None };
        let window = cfg["order_trace_window_ns"].as_u64().unwrap_or(2_000_000);
        let times = ["faults", "scenario"].iter().flat_map(|k| cfg[*k].as_array().into_iter().flatten()).filter_map(|f| f["at_ns"].as_u64()).collect();
        Ok(Self { out, window, times, routes: 0, violations: 0, lagging: 0, guest_saw_boundary: 0, max_clock_lead: 0, boundaries: 0, boundary_slices: [0; 7] })
    }
    pub fn on(&self) -> bool { self.out.is_some() }
    fn near(&self, t: u64) -> bool { self.times.iter().any(|&b| t.abs_diff(b) <= self.window) }
    pub fn write(&mut self, v: Value) {
        if let Some(o) = self.out.as_mut() { let _ = serde_json::to_writer(&mut *o, &v); let _ = o.write_all(b"\n"); }
    }
    pub fn boundary(&mut self, v: Value) { self.boundaries += 1; self.write(v); }
    pub fn slice(&mut self, limit_is_boundary: bool, stop: usize, v: Value) {
        if limit_is_boundary { self.boundary_slices[stop] += 1; self.write(v); }
    }
    /// One routed frame. `next`: the earliest boundary not yet applied;
    /// `max_time`: the furthest any machine stands; `clock`: the sender's guest clock.
    pub fn route(&mut self, send: u64, next: u64, max_time: u64, clock: u64, v: impl FnOnce() -> Value) {
        if !self.on() { return; }
        self.routes += 1;
        self.max_clock_lead = self.max_clock_lead.max(clock.saturating_sub(send));
        if send >= next { self.violations += 1; }
        else if max_time >= next {
            self.lagging += 1;
            if clock >= next { self.guest_saw_boundary += 1; }
        }
        if send >= next || self.near(send) { self.write(v()); }
    }
    pub fn finish(&mut self) {
        let s = json!({"summary": {"routes": self.routes, "violations": self.violations, "lagging": self.lagging,
            "guest_saw_boundary": self.guest_saw_boundary, "max_clock_lead": self.max_clock_lead,
            "boundaries": self.boundaries,
            "boundary_slices": {"reached": self.boundary_slices[0], "halted": self.boundary_slices[1], "exited": self.boundary_slices[2],
                "sent": self.boundary_slices[3], "barrier": self.boundary_slices[4], "control": self.boundary_slices[5]}}});
        self.write(s);
        if let Some(o) = self.out.as_mut() { let _ = o.flush(); }
    }
}
