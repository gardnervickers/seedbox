# timer-races

Determinism counterexample: timeout-versus-event races at tick granularity.

## Mechanism probed

Where timer interrupts land relative to syscall returns. Each of 400 rounds arms
a timeout and an event that should both fire at the same virtual deadline
`D = round_start + 1 ms + (round % 7) * 100 ns`, so some rounds sit on tick
boundaries and some do not. Four shapes run round-robin in one static C binary
(`races.c`), with one helper thread per round:

| shape | timeout side | event side | winner token |
| --- | --- | --- | --- |
| `cond` | `pthread_cond_timedwait(D)` | thread does `clock_nanosleep(ABSTIME, D)` then `pthread_cond_signal` | `signal` / `timeout` |
| `epoll` | `epoll_wait` on a pipe, 1 ms timeout | thread sleeps to `D`, writes the pipe | `ready1` / `ready0` |
| `itimer` | `setitimer(ITIMER_REAL, 1 ms)` SIGALRM | `nanosleep(1 ms + offset)` in the same thread | `eintr:rem<ns>` / `slept:rem<ns>` |
| `timerfd` | absolute `timerfd` at `D` | thread sleeps to `D`, writes an `eventfd`; both in one epoll set | ready fds in returned order, e.g. `timer`, `event`, `timer,event` |

If the virtual time at which a timer interrupt is delivered moves by even one
instruction relative to a syscall return, a race flips or a wake delta changes.

## Output

```
rounds: 400
round: <n> <shape> <winner> <delta_ns>     (400 lines)
tally: <shape> <label>=<count> <label>=<count>   (4 lines)
digest: <sha256 of all round lines>
```

`delta_ns` is CLOCK_MONOTONIC at wake minus round start. Nothing host-derived is
printed. A native Linux run gives mixed winners and a different digest on every
run; under seedbox two runs with seed 1101 must print identical bytes.

## Comparing two runs

Run it twice with the same seed and compare console bytes, or use
`seedbox diff` on the two runs. The first differing `round:` line shows which
shape and round diverged.

## Control knob

Vary `--watchdog-ms`: run once with `--watchdog-ms 1` and once with
`--watchdog-ms 1000`. Host watchdog interruptions must not become guest
interrupts or scheduling boundaries, so the output must be identical. Any
`round:` difference means a host interruption leaked into where a guest timer
interrupt landed.

Limits: seed 1101, simulated_time 20s (the rounds need well under 1 s of
virtual time), wall_time 15m.
