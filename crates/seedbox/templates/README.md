# Run this example

Start Docker or Podman, then point your release Seedbox executable at a kernel:

```sh
seedbox --kernel /path/to/kernel .
```

Seedbox builds the workload's Dockerfile using the engine's normal cache and
prepares the application images before starting the simulation. Edit the workload
and run the same command again. Guest helpers are embedded in the release CLI;
no helper archive or private GitHub download is needed at runtime.

Run output names the retained directory. Use `seedbox replay RUN` to reproduce
it, `seedbox inspect RUN` for a summary, or `seedbox logs RUN` for guest output.
Replay needs neither the engine nor the original source files or kernel.

To compare independent fresh runs, pass the same `--wall-clock-epoch SECS`
(Unix seconds) to each run. Otherwise guest time starts at host time after image
preparation. Replay always retains the original guest clock.

Seeds differ by default in timer jitter (20 µs), network jitter (10% of
latency) and, on Linux KVM, a seeded application start stagger of up to 20 ms
after every machine has booted. Set `x-seedbox.schedule.timer_jitter`,
`x-seedbox.network.jitter` or `x-seedbox.schedule.start_stagger` to `0ns` to
turn one off; replay restores the recorded values.
