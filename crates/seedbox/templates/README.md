# Run this example

Start Docker or Podman, then run a complete Seedbox build (keep its kernel
beside the executable, or select another with `--kernel`):

```sh
seedbox .
```

Seedbox builds the workload's Dockerfile using the engine's normal cache and
prepares the application images before starting the simulation. Edit the workload
and run the same command again. Guest helpers are embedded in the release CLI;
no helper archive, Nix, Cargo, or guest builder is needed at runtime.

Run output names the retained directory. Use `seedbox replay RUN` to reproduce
it, `seedbox inspect RUN` for a summary, or `seedbox logs RUN` for guest output.
Replay needs neither the engine nor the original source files or kernel.

Guest time starts at the latest build time recorded in the prepared images, so
independent fresh runs of the same images start at the same time and can be
compared directly. Set `x-seedbox.wall_clock_epoch` (Unix seconds, at least
86400) in the Compose file to choose the start time. Replay always retains the
original guest clock.

## Linux performance

`seedbox run --watchdog-ms 200` raises the host CPU interval (default 20 ms,
range 1..60000) after which a guest that makes no exits is interrupted so the
engine can report where it is stuck. The option also works with
`replay` and `explore`, changes no simulation results, and is ignored by Mac. Set it explicitly; the CLI does not inherit watchdog environment
variables.
