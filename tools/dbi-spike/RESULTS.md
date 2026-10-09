# Binary instrumentation findings

Measured 2026-10-09 on one Linux x86-64 KVM host, seed 1009. These are bounded
experiments, not arbitrary-application compatibility or instruction-exact counting.

## Decision and semantics

Use PV with automatic application binary instrumentation. Application binaries
need no recompilation. Instrument all application blocks, including libraries
and JIT code; the translator itself and guest kernel are outside this coverage.
The client advances PV time with one raw getpid syscall per 10,000 translated
block entries per thread. This counts blocks, not retired instructions.
Translator options and instrumentation policy belong to retained execution inputs.

## Performance

Three etcd 3.5.2 members, GOMAXPROCS=4, eight guest client writers. Same kernel,
seed, 1 GiB per VM, and 1,000 ms watchdog. Engine time excludes preparation.

| Acknowledged writes | Native seconds | Initial client | Optimized client |
| ---: | ---: | ---: | ---: |
| 160 | 1.43 | 8.83 | 4.89 |
| 1,600 | 2.26 | 11.47 | 6.93 |
| 16,000 | 14.29 | 34.05 | 26.25 |

The short row is three-run medians; larger rows are single trials. The optimized
multiplier is 3.43x for short work and 1.84x for sustained work. Startup remains
expensive. Empty translation took 9.81 s for the short workload, demonstrating
that the counter is not the sole source of overhead. Empty translation is not
a strict cost floor because injected progress changes scheduling.

Optimizations: direct raw-TLS counter access, no diagnostic module lookup or
offset hashing, and DynamoRIO -opt_speed -disable_traces. Fast counting with
traces still enabled took 29.02 s for 16,000 writes, versus 26.25 s combined.
Earlier, increasing the watchdog from 20 to 200 ms reduced an unchanged run
from 13.23 to 8.54 s and eliminated 495,616 debug steps; replay still matched.

## Correctness evidence

- Existing etcd executable bytes were unchanged by the wrapper image.
- Native GOMAXPROCS=4 also passed: the historical Go stall was not reproduced.
- Optimized short and sustained etcd runs passed full-state replay, including
  changing the watchdog from 1,000 to 200 ms; all machine console bytes matched.
- Optimized application spinlock: 51,919,559 spins, owner sleep 5,051,230 ns,
  zero steps, full-state replay MATCH. Disabling injected progress stalled until
  the 20-second wall limit. The loop has no PAUSE, clock read or syscall.
- Ordinary Node JIT printed 2308187121, with zero steps and full-state MATCH.
- A guest-controlled etcd pause froze one whole machine for 278 ms; 4,274 writes
  were verified and replay matched. Its stopped PC was not proven inside a lock.
- Kernel cpu_relax fixture completed with a lock held and zero debug steps;
  its irqsave check preserved disabled interrupts. Raw PAUSE stalled for 20 s.
- Linux portable suite, CLI tests, client builds, static output/checkpoint-count
  comparison and Python syntax checks passed for their respective changes.

Retained runs remain on nixos.local under /tmp/seedbox-dbi-etcd/state/runs/;
optimized short: 3b1f86e090d7c320-pJKWZe; sustained: c090fd5a183887a6-AhiskK;
spinlock: c38447a72622e831-oQP9ep; JIT: bb08442f2503e611-LNzJU1.
Raw reports, per-trial Compose files and build logs were archived outside this
checkout during cleanup. Keep future generated evidence outside the source tree.
