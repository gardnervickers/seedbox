//! Host cost of windowed execution with machines that burn real CPU in
//! proportion to simulated time. Ignored by default; run with
//! `cargo test --release -p seedbox-engine-core --test window_overhead -- --ignored --nocapture`.

use seedbox_engine_core::window::{
    run_parallel, run_serial, Assignment, Delivery, Machine, NoHook, Output, Params, Report,
    Router, Time, World,
};
use std::hint::black_box;

/// Spins `spin` iterations per simulated microsecond and sends one frame to
/// its neighbour every `every` ns.
struct Burner {
    id: usize,
    n: usize,
    time: Time,
    spin: u64,
    every: Time,
    acc: u64,
}

impl Machine<u64> for Burner {
    type Context = ();

    fn run(&mut self, _: &mut (), a: &Assignment<u64>) -> Result<Report<u64>, String> {
        self.time = self.time.max(a.wake);
        let mut outputs = Vec::new();
        for e in &a.incoming {
            self.acc = self.acc.wrapping_mul(31).wrapping_add(e.payload);
        }
        while self.time < a.stop {
            let next = (self.time / self.every + 1) * self.every;
            let to = next.min(a.stop);
            for i in 0..(to - self.time) / 1_000 * self.spin {
                self.acc = black_box(self.acc.wrapping_mul(6364136223846793005).wrapping_add(i));
            }
            self.time = to;
            if to == next {
                outputs.push(Output {
                    sent_at: to,
                    payload: self.acc,
                });
            }
        }
        Ok(Report {
            time: self.time,
            next: self.time,
            consumed: a.incoming.len(),
            outputs,
            exited: None,
            suspended: false,
        })
    }
}

struct Ring(usize, Time);

impl Router<u64> for Ring {
    fn route(&mut self, from: usize, sent_at: Time, payload: u64, out: &mut Vec<Delivery<u64>>) {
        out.push(Delivery {
            to: (from + 1) % self.0,
            at: sent_at + self.1,
            payload,
        });
    }
}

fn machines(n: usize, spin: u64) -> Vec<Burner> {
    (0..n)
        .map(|id| Burner {
            id,
            n,
            time: 0,
            spin,
            every: 50_000,
            acc: id as u64,
        })
        .collect()
}

#[test]
#[ignore]
fn overhead() {
    let n = 4;
    for (latency, spin) in [
        (1_000_000, 0),
        (1_000_000, 200),
        (100_000, 200),
        (10_000, 200),
    ] {
        let p = Params {
            latency,
            early: 0,
            duration: 200_000_000,
            end_on: None,
            max_outputs: 1 << 16,
        };
        let mut serial_acc = None;
        for workers in [0, 1, 2, 4] {
            let mut world = World::new(n, p.clone()).unwrap();
            let mut ms = machines(n, spin);
            let (_, s) = if workers == 0 {
                run_serial(&mut world, &mut ms, &mut (), &mut Ring(n, latency), NoHook).unwrap()
            } else {
                run_parallel(
                    &mut world,
                    &mut ms,
                    workers,
                    |_| Ok(()),
                    &mut Ring(n, latency),
                    NoHook,
                )
                .unwrap()
            };
            let acc: Vec<_> = ms.iter().map(|m| (m.id, m.n, m.acc)).collect();
            assert_eq!(*serial_acc.get_or_insert(acc.clone()), acc);
            let per_round = s.wall.as_nanos() as f64 / s.rounds as f64 / 1_000.0;
            let sync = (s.round_wall.as_secs_f64()
                - s.machine.as_secs_f64() / workers.max(1) as f64)
                .max(0.0);
            println!(
                "latency {latency:>8} ns spin {spin:>3} workers {}: rounds {:>6} wall {:>7.3}s machine {:>7.3}s coordinator {:>6.3}s sync~{:>6.3}s ({per_round:.1} us/round)",
                if workers == 0 { "serial".to_string() } else { workers.to_string() },
                s.rounds,
                s.wall.as_secs_f64(),
                s.machine.as_secs_f64(),
                s.coordinator.as_secs_f64(),
                sync,
            );
        }
    }
}
