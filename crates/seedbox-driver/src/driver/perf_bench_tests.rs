//! TEMPORARY driver round-overhead microbenchmark for the simulation-speed
//! investigation (not for merge). Machines do no CPU work, so wall time is the
//! Driver's own cost: rounds, zero-budget batches, worker dispatch and commits.
//! Run: cargo test -p seedbox-driver --release perf_bench -- --ignored --nocapture
use super::fixture::definition;
use super::*;
use crate::definition::{MacAddress, NetworkInterface};

const MS: u64 = 1_000_000;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

fn mac(n: u8) -> [u8; 6] {
    [2, 0, 0, 0, 0, n]
}

/// Busy when `period == 0`; otherwise blocks between timer bursts of `burst`
/// ns every `period` ns (phase-shifted), optionally sending one frame to the
/// next machine at the end of each burst. A received frame starts a burst.
struct Synthetic {
    period: u64,
    burst: u64,
    next_timer: u64,
    active_until: Option<u64>,
    reached: u64,
    peer: Option<[u8; 6]>,
}

impl Machine for Synthetic {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();

    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let start = context.clock.now();
        self.reached = self.reached.max(start);
        let end = upto.as_nanos();
        let mut woken = false;
        while let Some(input) = context.inbox.take() {
            if let Request::ReceiveFrame { .. } = input.payload {
                woken = true;
            }
        }
        if self.period == 0 {
            self.reached = end;
            return Ok(StopReason::BoundaryReached);
        }
        if self.active_until.is_none() && (woken || start >= self.next_timer) {
            self.active_until = Some(start + self.burst);
            while self.next_timer <= start {
                self.next_timer += self.period;
            }
        }
        if let Some(until) = self.active_until {
            if end < until {
                self.reached = end;
                return Ok(StopReason::BoundaryReached);
            }
            self.reached = until;
            self.active_until = None;
            if let Some(to) = self.peer {
                let mut bytes = to.to_vec();
                bytes.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0x88, 0xb5, 0]);
                context
                    .outbox
                    .emit(crate::pv::Output {
                        at: at(until),
                        payload: Response::TransmitFrame { bytes },
                    })
                    .unwrap();
            }
        }
        while self.next_timer <= self.reached {
            self.next_timer += self.period;
        }
        Ok(StopReason::Blocked {
            reached: at(self.reached),
            deadline: Some(at(self.next_timer)),
        })
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn run_scenario(name: &str, machines: usize, period: u64, burst: u64, frames: bool) {
    let definitions: Vec<_> = (0..machines)
        .map(|index| {
            let mut definition = definition(index);
            definition.network = Some(
                NetworkInterface::new(
                    MacAddress::new(mac(index as u8 + 1)).unwrap(),
                    std::net::Ipv4Addr::new(10, 0, 0, index as u8 + 1),
                    24,
                )
                .unwrap(),
            );
            definition
        })
        .collect();
    let mut iter = definitions.into_iter();
    let oracle = iter.next().unwrap();
    let others: Vec<_> = iter.collect();
    let mut index = 0usize;
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(MS),
        machines,
        crate::rng::SeededRng::new(1),
        || Ok(()),
        oracle,
        others,
        Vec::new(),
        NetworkConfig::default(),
        move |_| {
            let me = index;
            index += 1;
            Ok(Synthetic {
                period,
                burst,
                next_timer: if period == 0 {
                    0
                } else {
                    period * (me as u64 + 1) / machines as u64
                },
                active_until: None,
                reached: 0,
                peer: frames.then(|| mac(((me + 1) % machines) as u8 + 1)),
            })
        },
    )
    .unwrap();
    let simulated = 2_000 * MS;
    let started = std::time::Instant::now();
    let mut steps = 0u64;
    loop {
        steps += 1;
        match driver.step(at(simulated)).unwrap() {
            StepOutcome::BarrierReached { at } | StepOutcome::Blocked { at }
                if at >= Timestamp::from_nanos(simulated) =>
            {
                break
            }
            StepOutcome::BarrierReached { .. } | StepOutcome::Blocked { .. } => {}
            other => panic!("unexpected outcome {other:?}"),
        }
        let _ = driver.drain_events();
    }
    let wall = started.elapsed();
    crate::perf::final_dump();
    println!(
        "PERF-BENCH {name}: machines={machines} period_ns={period} burst_ns={burst} frames={frames} \
         simulated_s=2 steps={steps} wall_ms={:.1} us_per_step={:.2} sim_per_wall={:.2}",
        wall.as_secs_f64() * 1e3,
        wall.as_secs_f64() * 1e6 / steps as f64,
        2.0 / wall.as_secs_f64(),
    );
}

#[test]
#[ignore = "temporary benchmark"]
fn perf_bench() {
    let only = std::env::var("PERF_BENCH_SCENARIO").ok();
    let scenarios: [(&str, u64, u64, bool); 5] = [
        ("busy", 0, 0, false),
        ("timers-10ms", 10 * MS, 200_000, false),
        ("timers-1ms", MS, 100_000, false),
        ("timers-frames-10ms", 10 * MS, 200_000, true),
        ("timers-frames-1ms", MS, 100_000, true),
    ];
    for (name, period, burst, frames) in scenarios {
        if only.as_deref().is_none_or(|only| only == name) {
            run_scenario(name, 4, period, burst, frames);
        }
    }
}
