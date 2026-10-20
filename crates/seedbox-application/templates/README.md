# Run this project

Start Docker or Podman, then run from this directory:

```sh
seedbox check .
seedbox run .
seedbox replay <sim-id>
```

`run` prints `PASS: <sim-id>`. Replay that ID to check for `MATCH`.
The CLI asks before starting a local server; keep it running between commands.
For scripts, start it explicitly with `seedbox server start`.

Edit `seedbox.yaml` to configure your **machines** and **oracle**. The oracle drives
the test: exit code zero means PASS; nonzero means FAIL. For longer etcd fault
experiments, increase the oracle's `--writes` and `--duration` arguments.

Use `seedbox logs <sim-id>` for application output or `seedbox tui <sim-id>`
for the interactive console. Run more seeds with `seedbox run . --count 100`.
Without KVM, use the slower software backend: `seedbox run . --backend tcg`.

In `seedbox.yaml`, `simulator.quantum` sets the maximum timeslice (default `1ms`);
network latency, timers and events can shorten it. `simulator.machineWorkers`
sets the execution thread count (1–64, default 1). Workers steal queued machine
runs; changing their count does not change results. Replay and checkpoints retain
the quantum.

Save an application-ready simulation before a bug:

```sh
seedbox checkpoint <sim-id> -o before-bug.seedbox
seedbox run before-bug.seedbox
```

The file contains the state and inputs needed for a new run, even on a fresh
server. Capture briefly pauses the source and restores its previous playback.

`seedbox server stop` stops the server and clears its simulations.
Use `seedbox help` for commands and options.

[Seedbox documentation](https://github.com/gardnervickers/seedbox#readme)
