# CPU-time clocks and scheduler statistics

**Mechanism probed:** tick-based CPU accounting. Linux splits CPU time into
user and system time by sampling the mode a task is in when each timer
interrupt lands. `ITIMER_PROF`, `RUSAGE`, `times()`, `/proc/self/stat` and
`/proc/stat` all depend on where those interrupts fall in the instruction
stream. `CLOCK_*_CPUTIME_ID` and schedstat use the scheduler clock instead.
If the guest's timer interrupts are not delivered at deterministic points,
for example because the host watchdog becomes an interrupt boundary, these
values change while the application still produces the same computed result.

**Program** (`cputime.c`, static C, `-pthread`, one service):

- two busy threads: 900,000 rounds of 1,000 mixed integer and floating-point
  iterations plus a `getpid()` each, about 1 s of virtual time per thread;
- a sleeper thread: 150 sleeps of 10 ms;
- an alternator thread: 300 cycles of about 1 ms compute and 1 ms `nanosleep`;
- `setitimer(ITIMER_PROF, 10 ms)`. The handler counts each SIGPROF in a slot
  for the receiving thread and appends that slot to an ordered trace.

**Output** (`key: value` lines, flushed per line): for each worker, the thread
CPU clock, `RUSAGE_THREAD` utime/stime/nvcsw/nivcsw, `/proc/thread-self/schedstat`
and its compute result. Then the process and main-thread CPU clocks, `RUSAGE_SELF`
utime/stime/nvcsw/nivcsw/minflt, `times()`, the full `/proc/self/stat` line and
its fields 14-17 and 22, `/proc/self/schedstat`, the `cpu` lines of `/proc/stat`,
`/proc/loadavg`, `/proc/uptime`, and the MONOTONIC, MONOTONIC_RAW, BOOTTIME and
REALTIME clocks. Then the per-thread SIGPROF counts, the SIGPROF trace (64
receivers per line: 1, 2 busy, 3 sleeper, 4 alternator) and the delta between two
user counter reads (`tsc:`, `rdtsc` on x86-64, `cntvct_el0` on aarch64). Last
comes `digest:`, the sha256 of every preceding output line. The trace shows the
first tick where two runs diverge.

**Comparing runs:** run it twice with the same seed. Each `run` prints its run
directory. Compare the console bytes, or the full retained evidence:

```sh
seedbox run examples/determinism/cpu-time --watchdog-ms 1     # -> RUN_A
seedbox run examples/determinism/cpu-time --watchdog-ms 1000  # -> RUN_B
diff <(seedbox logs RUN_A --stream console) <(seedbox logs RUN_B --stream console)
seedbox diff RUN_A RUN_B
```

**Control knob:** `--watchdog-ms`. Compare `1` with `1000`. The watchdog may
only change performance, so the output must be identical, including the
`digest:` line. The `*_result` lines are a sanity check: they are pure
computation and never vary. If only the accounting lines differ, interrupt
placement leaked into guest-visible state.

The app exits 0 after printing. Seed 1102, simulated_time 15 s, about 2.3 s of
virtual CPU time in total.
