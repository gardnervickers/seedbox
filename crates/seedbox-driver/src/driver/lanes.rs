//! Whole-window lanes. Inside a round, every machine with work runs to the
//! round's end on its own lane: it takes its due timers and the frames that
//! arrive for it at their exact times, as it would at a round boundary. No
//! machine can affect another before the round commits, so lanes run side by
//! side and only each machine's own runs are ordered.
use super::batch::{BatchBounds, RunResult};
use super::workers::Stream;
use super::*;

/// A lane's run, as a worker job.
type LaneJob<M> = Box<dyn FnOnce(&mut <M as Machine>::Context) -> RunResult<M> + Send>;
type Lanes<M> = Stream<LaneJob<M>, RunResult<M>>;

/// A frame taken from the network for a lane, in network order.
struct Arrival {
    at: Timestamp,
    delivery: network::Delivery,
    /// The frame's length; its bytes move to the receiver's inbox.
    len: usize,
    /// Whether its receiver had exited by the arrival.
    dropped: bool,
}

/// A lane's run on a worker, with what its acceptance needs.
struct LaneRun {
    start: Timestamp,
    upto: Timestamp,
    bounds: BatchBounds,
    offered_commands: Vec<(MachineId, NonZeroU32, Timestamp)>,
    offered_stops: Vec<NonZeroU32>,
}

/// The lanes of one round.
struct Window {
    end: Timestamp,
    arrivals: Vec<Arrival>,
    /// Each machine's untaken arrivals, as indices into `arrivals`.
    lanes: Vec<VecDeque<usize>>,
    running: BTreeMap<MachineId, LaneRun>,
    /// Failed lanes. Lanes after the first failure stop, so the lowest
    /// failing machine is reported whatever the completion order.
    failed: BTreeSet<MachineId>,
    ran: bool,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Whether frames for `id` can arrive inside its lane. A paused or
    /// suspended receiver admits them at a round boundary instead, in a
    /// zero-budget run. Its state cannot change before the round commits.
    pub(super) fn lane_receiver(&self, id: MachineId) -> bool {
        let entry = &self.execution.machines[id.0];
        matches!(entry.stopped, StopReason::Exited { .. })
            || !entry.lifecycle.paused()
                && !self
                    .execution
                    .control
                    .service
                    .breaks
                    .suspended(&control::Control::source(id))
    }

    /// Run every machine's lane to `end`. Returns whether any machine ran.
    pub(super) fn run_lanes(&mut self, end: Timestamp) -> Result<bool, Error<M::Error>> {
        let receivers: Vec<bool> = (0..self.execution.machines.len())
            .map(|index| self.lane_receiver(MachineId(index)))
            .collect();
        let taken = self
            .execution
            .network
            .take_before(end.as_nanos(), |to| receivers[to.0]);
        let mut window = Window {
            end,
            arrivals: Vec::with_capacity(taken.len()),
            lanes: vec![VecDeque::new(); self.execution.machines.len()],
            running: BTreeMap::new(),
            failed: BTreeSet::new(),
            ran: false,
        };
        for (at, delivery) in taken {
            window.lanes[delivery.to.0].push_back(window.arrivals.len());
            window.arrivals.push(Arrival {
                at: Timestamp::from_nanos(at),
                len: delivery.bytes.len(),
                delivery,
                dropped: false,
            });
        }
        let mut errors = BTreeMap::new();
        let mut stream = Lanes::<M>::default();
        for index in 0..self.execution.machines.len() {
            if let Err(error) = self.advance_lane(MachineId(index), &mut window, &mut stream) {
                Self::fail_lane(MachineId(index), error, &mut window, &mut errors);
            }
        }
        while !window.running.is_empty() {
            let result = self.workers.next(&mut stream).expect("lane runs report");
            let id = result.id;
            let run = window.running.remove(&id).expect("running lane");
            let finished = self
                .finish_lane_run(result, run)
                .and_then(|()| self.advance_lane(id, &mut window, &mut stream));
            if let Err(error) = finished {
                Self::fail_lane(id, error, &mut window, &mut errors);
            }
        }
        if let Some((_, error)) = errors.pop_first() {
            return Err(error);
        }
        // Frame facts follow network order, as at a boundary.
        let position = self.position();
        for arrival in window.arrivals {
            let delivery = arrival.delivery;
            self.execution.network.delivery_result(
                Position {
                    at: arrival.at,
                    ..position
                },
                delivery.from,
                delivery.to,
                delivery.sent,
                arrival.len,
                arrival.dropped,
                &mut self.observations.frames,
            );
        }
        Ok(window.ran)
    }

    fn fail_lane(
        id: MachineId,
        error: Error<M::Error>,
        window: &mut Window,
        errors: &mut BTreeMap<MachineId, Error<M::Error>>,
    ) {
        window.failed.insert(id);
        errors.insert(id, error);
    }

    /// Take the lane's due frames and submit its next run, if it has one
    /// before the window's end.
    fn advance_lane(
        &mut self,
        id: MachineId,
        window: &mut Window,
        stream: &mut Lanes<M>,
    ) -> Result<(), Error<M::Error>> {
        if window.failed.first().is_some_and(|&first| first < id) {
            return Ok(());
        }
        loop {
            let arrival = window.lanes[id.0]
                .front()
                .map(|&index| window.arrivals[index].at);
            let entry = &mut self.execution.machines[id.0];
            let next = entry.next();
            if let Some(at) = arrival {
                if matches!(entry.stopped, StopReason::Exited { .. }) {
                    while let Some(&index) = window.lanes[id.0].front() {
                        window.arrivals[index].dropped = true;
                        window.lanes[id.0].pop_front();
                    }
                    continue;
                }
                if next.is_none_or(|start| start >= at) {
                    // Frames arriving at `at` wait in the inbox for the run
                    // from `at`; a blocked receiver becomes runnable then.
                    while let Some(&index) = window.lanes[id.0].front() {
                        if window.arrivals[index].at != at {
                            break;
                        }
                        window.lanes[id.0].pop_front();
                        let bytes = std::mem::take(&mut window.arrivals[index].delivery.bytes);
                        entry.inbox.push_back(Input {
                            at,
                            payload: Request::ReceiveFrame { bytes },
                        });
                    }
                    if let StopReason::Blocked { .. } = entry.stopped {
                        entry.stopped = StopReason::BoundaryReached;
                        entry.reached = at;
                        entry.timer = None;
                    }
                    continue;
                }
            }
            let Some(start) = next.filter(|&start| start < window.end) else {
                return Ok(());
            };
            let upto = arrival.map_or(window.end, |at| at.min(window.end));
            let ids = [id];
            let offered_commands = self.offered_commands(&ids);
            let offered_stops = self.offered_stops(&ids);
            let (bounds, mut views) = self.batch_views(&ids, start, upto)?;
            let view = views.pop().expect("one lane view");
            let (machine, job) = self.run_job(view);
            self.workers
                .submit(stream, machine, Box::new(job) as LaneJob<M>);
            window.running.insert(
                id,
                LaneRun {
                    start,
                    upto,
                    bounds,
                    offered_commands,
                    offered_stops,
                },
            );
            window.ran = true;
            return Ok(());
        }
    }

    /// Accept a lane's finished run and record it on its machine.
    fn finish_lane_run(
        &mut self,
        result: RunResult<M>,
        run: LaneRun,
    ) -> Result<(), Error<M::Error>> {
        let id = result.id;
        let (start, bound, offered) = run.bounds[&id];
        let report = self.receive_run(result, offered);
        let accepted = self.accept_run(report, start, bound, run.start, run.upto)?;
        self.confirm_offered_commands(&run.offered_commands, run.upto, None)?;
        self.commit_runs(vec![accepted], &run.offered_stops, None);
        Ok(())
    }
}
