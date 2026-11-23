//! Whole-window lanes. Inside a round, every machine with work runs to the
//! round's end on its own lane: it takes its due timers and the frames that
//! arrive for it at their exact times, as it would at a round boundary. No
//! machine can affect another before the round commits, so lanes run side by
//! side and only each machine's own runs are ordered.
//!
//! A machine that executes elsewhere ([`Machine::split_run`], a PV worker
//! process) is not lent to a worker thread: the Driver's thread starts its
//! run, keeps it in flight beside the others, and finishes whichever stops
//! first, so a round crosses no thread inside the Driver.
use super::batch::RunResult;
use super::round::Runs;
use super::*;

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
    machine_start: Timestamp,
    machine_bound: Timestamp,
    /// The simulation time the run's machine times map from.
    lower: Timestamp,
    offered: usize,
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
    /// Machines held at a breakpoint (see `Driver::held`): at the round's
    /// start, or since a run of this round staged their hit.
    held: Vec<bool>,
    ran: bool,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Whether frames for each machine can arrive inside its lane: an exited
    /// receiver drops them there, and a machine held at a breakpoint takes
    /// them there without a budget (see `held`). Only a paused receiver
    /// admits them at a round boundary instead, in a zero-budget run. A
    /// pause changes only at a timeline item, which every schedule shares,
    /// so this cannot depend on where a round ended; a breakpoint hit's
    /// commit can, so it must not decide it.
    pub(super) fn lane_receivers(&self) -> Vec<bool> {
        self.execution
            .machines
            .iter()
            .map(|entry| {
                matches!(entry.stopped, StopReason::Exited { .. }) || !entry.lifecycle.paused()
            })
            .collect()
    }

    /// Whether machine `id` is held at a breakpoint: its hit is committed
    /// and live, or still staged. A held machine runs no guest work, so its
    /// lane gives it only zero-budget runs, which admit what arrives for it.
    /// Both forms hold from the hit until its resume, crash or the end of
    /// the run, all of which are needed barriers, so the answer is the same
    /// wherever rounds end.
    fn held(&self, id: MachineId) -> bool {
        let breaks = &self.execution.control.service.breaks;
        Self::staged_hit(&self.execution.machines[id.0].staged)
            || breaks.hits().next().is_some() && breaks.suspended(&control::Control::source(id))
    }

    /// Whether `outputs` hold a breakpoint hit, which suspends its machine.
    fn staged_hit(outputs: &[MachineResponse]) -> bool {
        outputs.iter().any(|output| {
            matches!(
                output.payload,
                Response::Break { .. } | Response::BlockRequestHeld { .. }
            )
        })
    }

    /// Run every machine's lane to `end`, taking the frames for `receivers`.
    /// Returns whether any machine ran, and the latest arrival taken.
    pub(super) fn run_lanes(
        &mut self,
        end: Timestamp,
        receivers: &[bool],
    ) -> Result<(bool, Option<Timestamp>), Error<M::Error>> {
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
            // Only an engine with breakpoints can hold a machine.
            held: (0..self.execution.machines.len())
                .map(|index| {
                    self.execution.control.service.breaks.supported && self.held(MachineId(index))
                })
                .collect(),
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
        let mut runs = Runs::new();
        for index in 0..self.execution.machines.len() {
            if let Err(error) = self.advance_lane(MachineId(index), &mut window, &mut runs) {
                Self::fail_lane(MachineId(index), error, &mut window, &mut errors);
            }
        }
        while !window.running.is_empty() {
            let result = self.next_run(&mut runs);
            let id = result.id;
            let run = window.running.remove(&id).expect("running lane");
            let finished = self.finish_lane_run(result, run).and_then(|hit| {
                window.held[id.0] |= hit;
                self.advance_lane(id, &mut window, &mut runs)
            });
            if let Err(error) = finished {
                Self::fail_lane(id, error, &mut window, &mut errors);
            }
        }
        if let Some((_, error)) = errors.pop_first() {
            return Err(error);
        }
        let arrived = window.arrivals.last().map(|arrival| arrival.at);
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
        Ok((window.ran, arrived))
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
        runs: &mut Runs<M>,
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
            // A machine whose timer falls inside the simulation nanosecond
            // it wakes at first runs from the timer's deadline to the end of
            // that nanosecond, also at a window's end, and only then takes
            // what arrives in it.
            let inside = next.is_some() && entry.timer_inside(id);
            if let Some(at) = arrival {
                if matches!(entry.stopped, StopReason::Exited { .. }) {
                    while let Some(&index) = window.lanes[id.0].front() {
                        window.arrivals[index].dropped = true;
                        window.lanes[id.0].pop_front();
                    }
                    continue;
                }
                if next.is_none_or(|start| start > at || start == at && !inside) {
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
            let Some(start) =
                next.filter(|&start| start < window.end || start == window.end && inside)
            else {
                return Ok(());
            };
            // A machine held at a breakpoint only admits what arrived.
            let upto = if window.held[id.0] {
                start
            } else {
                arrival.map_or(window.end, |at| at.min(window.end))
            };
            let ids = [id];
            let offered_commands = self.offered_commands(&ids);
            let offered_stops = self.offered_stops(&ids);
            let (view, lower) = self.batch_view(id, start, upto, false)?;
            let (_, machine_start, machine_bound, ref inbox) = view;
            let offered = inbox.len();
            self.submit_run(view, runs);
            window.running.insert(
                id,
                LaneRun {
                    start,
                    upto,
                    machine_start,
                    machine_bound,
                    lower,
                    offered,
                    offered_commands,
                    offered_stops,
                },
            );
            window.ran = true;
            return Ok(());
        }
    }

    /// Accept a lane's finished run and record it on its machine. Returns
    /// whether the run staged a breakpoint hit.
    fn finish_lane_run(
        &mut self,
        result: RunResult<M>,
        run: LaneRun,
    ) -> Result<bool, Error<M::Error>> {
        let id = result.id;
        let report = self.receive_run(result, run.offered);
        let accepted = self.accept_run(
            report,
            run.machine_start,
            run.machine_bound,
            run.lower,
            run.start,
            run.upto,
        )?;
        self.confirm_offered_commands(&run.offered_commands, None)?;
        let staged = self.execution.machines[id.0].staged.len();
        self.commit_runs([accepted], &run.offered_stops, None);
        Ok(Self::staged_hit(
            &self.execution.machines[id.0].staged[staged..],
        ))
    }
}
