# Run this example

Start Docker or Podman, then run Seedbox:

```sh
seedbox .
```

Seedbox builds the workload's Dockerfile using the engine's normal cache and
prepares the application images before starting the simulation. Edit the workload
and run the same command again. Guest helpers are embedded in the release CLI;
no helper archive, Nix, Cargo, or guest builder is needed at runtime.

The CLI asks before starting a persistent local server if none is running. Run output gives a short simulation
ID. Use `seedbox get simulations`, `seedbox describe simulation ID`, or
`seedbox rerun ID`. Rerun uses retained inputs and the original seed, without
contacting the container engine. Add `--detach` to return immediately, and use
`seedbox stop ID` to stop it later. The server stays running between commands. Use `seedbox shutdown` to stop it;
noninteractive callers must start `seedbox serve` explicitly.
Use `seedbox logs ID [MACHINE]` for live application output and `seedbox events ID`
for structured internal events. Logs replay recent output from a shared 64 MiB in-memory buffer before following
live output. Oldest records are evicted; deletion or server restart clears logs.
Events have no historical backlog.
`seedbox get simulation ID --watch` follows status until completion; Ctrl-C only detaches.
`seedbox tui [ID]` opens an interactive console with a live network map, logs,
events and fault controls.
Save `seedbox describe simulation ID` to `simulation.yaml` and submit it again
with `seedbox run simulation.yaml`.
`seedbox playback ID` inspects a running worker's committed position and playback rate.

Guest time starts at the latest build time recorded in the prepared images, so
independent fresh runs of the same images start at the same time and can be
compared directly. Resolved simulation definitions retain the exact guest clock.


`seedbox replay ID` submits a server-owned rerun of a completed simulation and
compares its retained results. It prints `MATCH`, `MISMATCH` or `NOT_COMPARABLE`
and exits 0, 1 or 2 respectively. Use `--json` for the comparison document.
`seedbox rerun ID` instead reports the workload oracle verdict.
`seedbox compare BASELINE_ID SIMULATION_ID` compares two completed simulations
without executing either. Comparisons never change either simulation's verdict.
Replay accepts simulation IDs; importing old offline run bundles into the server
catalogue is still pending.

## Results and defaults

The dedicated `oracle` section in `seedbox.yaml` specifies the test driver.
Its exit code determines the verdict: zero passes and nonzero fails. Execution
that ends before an oracle exit is incomplete. Omitted seeds are allocated once
by the server and appear in describe. Optional `network.latency` defaults to 1ms;
`simulator.blocksPerProgressCall` defaults to 10000 translated basic blocks.
Time limits are runtime choices: `seedbox run . --wall-time 5m` bounds execution
without changing the experiment definition. No simulated-time limit is authored.

Pause with `seedbox pause ID`, advance one round with `seedbox step ID`, or set
playback speed with `seedbox rate ID 1/10`. `seedbox resume ID` runs without pacing.
These commands return durable receipts; reuse `--request-id` to retry a command
and inspect its receipt with `seedbox playback ID REQUEST_ID`. Intentional waits
do not consume the execution-time budget, and cancellation remains available.

Simulation preparation has a 15-minute server deadline, separate from execution
limits. On expiry the server cancels preparation and waits for cleanup. Without
an oracle exit, a preparation or execution timeout reports `TIMEOUT` (exit 124)
and the simulation's test verdict remains incomplete.

### Interactive faults

Use `seedbox inject SIM_ID ACTION_FILE` to submit a typed YAML or JSON action.
For example, a file containing `action: stop` and `machine: db` stops the
application on machine `db`. Use `seedbox heal SIM_ID HANDLE` to remove a healable
network, clock or throttle fault. Both commands accept `--request-id` for safe
retries and emit an admission receipt; stream `seedbox events SIM_ID` to observe
the resulting effects. `ACTION_FILE` may be `-` for standard input.

Rerunning a completed simulation reapplies its retained interventions at the
same deterministic simulated-time positions. Pausing, stepping, pacing, and new
external overrides remain available during rerun. New interventions form a new
working definition. Restarting the server discards its catalogue; saved definitions
remain usable when their binary assets are available.

### Checkpoint branches (in development)

`seedbox checkpoint ID --request-id NAME` captures a paused simulation at a
committed boundary. List captures with `seedbox get checkpoints ID` and inspect
one with `seedbox describe checkpoint ID NAME` (both accept `--json`). Wait
for `checkpoint.state: captured` before running
`seedbox branch ID NAME --detach`. The branch gets a new simulation ID, preserving
its source seed, configuration and fault history up to the checkpoint. Its
annotations may name the source simulation and checkpoint; the new definition
is independent of that source. Follow its application
output with `seedbox logs BRANCH_ID [MACHINE]` and its internal events with
`seedbox events BRANCH_ID`. Historical application logs are not replayed.

Both Linux KVM and Apple Silicon HVF have native validation for capture, branch
logs, matching source results, fault isolation, and branch rerun after a server
restart.
`seedbox resume ID` continues a paused simulation; it does not create a branch.
After admitting a fault, step through one committed round before capturing a
checkpoint so the snapshot includes its effects.

New simulations use exact stop boundaries, with page timer and deferred TX
disabled. These engine settings have no Compose overrides. Machine memory,
timer jitter, network behavior and disk fault policy remain configurable.

### Exploring seeds

`seedbox explore . --seed 1 --count 10 --detach` asks the server to run ten
consecutive seeds using one preparation. Inspect them with
`seedbox get simulations SOURCE_ID`, or stop the range with
`seedbox stop SOURCE_ID --all-seeds`. Each member has an ordinary simulation ID
for describe, application logs and events. Closing the client does not cancel
accepted work. The server chooses concurrency; no worker-count flag is needed.

Server storage is selected by `SEEDBOX_STATE`. Offline `prepare`, `export` and
`import` use `--state` or `SEEDBOX_ARTIFACT_STATE`, defaulting to `.seedbox`.

Server metadata and log buffers are session-only and live in memory. Execution
uses private scratch directories under the server state's `sessions/` directory.
Immutable binary inputs (kernels, engines, boot archives and captured checkpoint
binaries) and image preparation cache entries live separately under `cache/` and
survive server restarts. Saved simulation definitions can reuse these assets on
the same server state directory; simulation IDs, run evidence and observations
remain session-only. The binary cache is not automatically pruned.
Graceful shutdown removes the session directory after workers stop and reports
cleanup failures. The next server startup, under the exclusive server lock,
removes abandoned session directories left by a crash. Existing offline runs
and older persistent catalogue data are not deleted or imported automatically.
Resource tables show newest simulations first; checkpoint lists use descending
simulation timeline time, with uncaptured requests last.

Start at the initial execution boundary with `seedbox run PROJECT --paused --detach`,
then open `seedbox tui ID` to inspect and resume it. `--paused` also works with
`explore`; every member starts paused. Preparation still runs before the pause.
Without `--detach`, the command waits for completion while another client controls
playback. Initial playback state does not change deterministic results; `rerun`,
`replay` and checkpoint branches start with normal playback.

The TUI's `3 timeline` view saves named recordings locally with application, seed
and date. Remove/restore entries with `d`, clear with `C` twice, undo with `u`,
and press Enter to run an edited recording as a new paused simulation after its
definition is ready. `w` saves an immutable definition; Enter submits a snapshot
without requiring a save. `N` selects the next fault; `p` runs to just before it. Fault forms accept schema-advertised units
such as `100ms` and `2.5%`, and keep values when the server rejects a submission.
