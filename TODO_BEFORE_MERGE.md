# Before merging the console

Engine, server and API work found while reviewing the console against the etcd
example on Apple Silicon HVF. The console shows what it can derive today; each
item says what it does instead.

## Machine and application state

- **Application state is invisible.** `stop` and `pause_process` leave
  `MachineState.condition` at `ready`, so the map and machine table show a
  healthy machine whose application is stopped. Expose an application state
  (running, stopped, paused, exited) on `MachineState`. The console currently
  shows the newest fault on the machine's node instead.
- **Machine transitions have no history.** `MachineState.last_transition` has no
  time and the event stream has no replay backlog, so downtime spans (crashed →
  rebooted, exited) exist only for transitions seen while the console was open.
  Expose timestamped transitions in `WorldState`, or replay events from the start
  of the simulation.
- **`active` covers only network, clock and throttle faults.** An application
  stop stays in effect until `restart`/`resume_process`, but the API reports it
  as a one-shot fault. Either mark it active until closed or say which action
  closes which, so the console can draw it as a span like a partition.

## Checkpoints and branches

- **Checkpoints re-emit fault events.** Capturing a checkpoint at 12.106 s emitted
  `FaultChanged` events for `stop n2` and `restart n2` again, stamped with the
  checkpoint time, so they look like they fired twice.
- **Restored machines stay `starting`.** After branching from a checkpoint every
  machine reports `starting` indefinitely while traffic flows normally.
- **Restored link counters restart at zero** and the branch reports
  `lifecycle running` at 0.000 s, before its restored position.
- **A checkpoint requested while paused waits for the next boundary.** Pressing
  `c` then `b` while paused fails ("no captured checkpoint") until the user steps
  or resumes. Capturing at the current boundary while paused would make
  checkpoint-then-branch work directly. The console now says the request is
  pending.
- **Lineage is not canonical.** The server's checkpoint definition sets the
  `derived-from` annotation but not `metadata.parent` / `metadata.checkpoint`;
  the CLI's rerun sets `derived-from`; the console now also writes `derivation`
  (`rerun`, `edited`, `branch`) and `branch-at-ns`. Pick one lineage
  representation in the API and set it server-side for every derived run.

## Scheduling and failures

- **Queued simulations give no reason.** A branch sat in `queued` until another
  run was stopped. Expose why (slot capacity, position) on the resource; the
  console currently shows how many simulations are running.
- **VMM failure after crash, reboot and exit.** Run `9wn7szx7` (etcd example,
  partition → heal → crash n3 → reboot n3 → n3 exits 1) ended at 990.56 s with
  `execution: vmm failed; see vmm.stderr; eof while parsing a value at line 1
  column 0`.
- **Guest wall clock goes backwards across a reboot.** After `reboot n3` its zap
  timestamps restarted at 02:56:29, before the first boot's 02:56:31. Confirm the
  per-boot epoch is intended; real hosts keep their clock across reboots.

## Catalogue and docs

- **Fault descriptors have no category.** The inject palette can only group
  "heal" and "inject"; a `category` (network, machine, application, clock) on
  `FaultDescriptor` would let it group the rest without guessing from names.
- **`examples/README.md` is stale.** It refers to `compose.yaml` and
  `tools/build.py`; the etcd example now uses `seedbox.yaml`.

## API change made for the console

- `seedbox-api` now re-exports `CheckpointState` and `CheckpointStatus` so the
  console matches captured checkpoints by type instead of by serialized string.
