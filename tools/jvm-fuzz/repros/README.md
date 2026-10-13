# Translator reproducers

Small C programs for bugs first seen as JVM probe failures. They need no Seedbox
or JVM: run them under the progress translator (`drrun` with the guest's flags,
optionally without the client) in a Linux/AArch64 container on one CPU. Natively
they always print `OK`.

| Program | What it checks | Before the fix |
| --- | --- | --- |
| `spawnjit SPAWNS 1 spawn` (or `vfork`) | `posix_spawn`/`vfork` while another thread writes and runs code | hangs: the stop-all-threads flush waits for the vfork parent, which cannot take the suspend signal until its child execs |
| `spawnjit 200 4 fork 4 20` | `fork` while a writer patches code that executor threads run | 83–158 of 200 forks return `0x1200011`, the clone flags, without a child |
| `sigext ROUNDS` | process-directed signals from a child while code is patched | passes; the JVM form (`Signals` probe) lost them, see below |
| `regcheck ROUNDS [SPINS [x28]]` | x0–x26 and q0–q31 across progress checkpoints and x28-based accesses | passes |
| `srsample SAMPLES` | HotSpot-style suspend/resume sampling (JFR's protocol) against fixed registers | passes |
| `sigregs WORKERS SIGNALS [SPINS [x28\|ibl]]` | x0–x26 and q0–q31 across `tgkill` signals whose handler posts a semaphore | `ibl` (a `blr`/`ret` loop, no client) hung in about one run in five |
| `spawnjit 60 1 fork 1 0` | `fork` while one thread rewrites and runs its code | the fork never ran: the writer's back-to-back flushes won every race for the stop-all lock |
| `mjit THREADS ITERS` | each thread rewrites `movz; ret` slots in its own page, flushes and calls | passes; with per-page flush tracking, a flush abandoned when another thread's flush redirected its thread left stale code |
| `map_noreplace` | `MAP_FIXED_NOREPLACE` over an existing mapping | `EPERM` instead of `EEXIST` |
| `sigframe_unwind` | `_Unwind_Backtrace` from a SIGSEGV handler | stopped after the handler: its return address was DR's own sigreturn stub |

Open bugs found by the P0 probes (`C.*` signal and `cfzsync` tests, JVM and Go probes):

| Program | What it checks | Under the translator |
| --- | --- | --- |
| `sigsys_call_addr` | SIGSYS `si_call_addr` from seccomp | a code-cache address |
| `tempmask_pending MODE` | `rt_sigpending`; ppoll, pselect6, epoll_pwait(2) and sigsuspend temporary masks | pending set empty; `epoll_pwait2` ignores its mask; signal delivered with EINTR although an fd is ready or the arguments are invalid |
| `sigtimedwait_queued MODE`, `sigtimedwait before\|flush N` | `rt_sigtimedwait` for a pending or arriving signal | misses an already pending signal; returns EINTR during another thread's flush, then the signal kills the process |
| `pending_overflow N` | N signals pending on one thread | crash at 9 or more; same-number realtime signals delivered in reverse order |
| `pending_fork_signalfd MODE N` | pending signals seen by a later `signalfd` and a fork child | `signalfd` reads none; the child receives the parent's |
| `suspend_signal_sharing SIG N [flush]` | queued SIGILL/SIGSTKFLT/SIGUSR2 while code is flushed | hangs with the client; SIGILL loses `si_pid`/`si_value`; SIGUSR2 handled more often than sent |
| `stopcont_signals MODE N ...` | process-directed signals with blocking threads and SIGSTOP/SIGCONT | handled once per unblocked thread; EINTR in threads that block it |
| `flush_eintr CALL N` | waits while another thread flushes code | spurious EINTR (thousands for `nanosleep`) |
| `vfork_exit vfork\|clone\|spawn` | a vfork child that exits without exec, `posix_spawn` of a missing program | the parent crashes |
| `clone_mask thread\|vm` | blocked mask inherited by raw `clone` children | not blocked |
| `vfork_flush N 0 PERIOD [spin]` | a vfork child running before exec during flushes | hangs |
| `jitspin W S MS` | flushes while threads call into the flushed code | 2 flushes in 3 s instead of thousands |
| `pid1_raise_dfl` | PID 1 raising a signal with the default action | spins at 100% CPU |
| `RedefineWrongMethod.java` | class redefinition with a thread parked in the class | JVM crash in `vframeStream` from `handle_wrong_method` |
| `go_exec_flaky.sh` | runs `Go.exec` repeatedly | about 3% of runs lose a signal, crash a child or hang |

`spawn_as_fork.c` is an `LD_PRELOAD` experiment that turns `vfork` and simple
`posix_spawn` into `fork`; it separated the two spawn bugs.

The fixes are in [`guest/progress`](../../../guest/progress):

- `dynamorio-vfork-synch.patch`: synch-all counts a thread waiting in a vfork-style
  clone as stopped; after the clone returns the thread waits for the synch.
- `dynamorio-fork-synch.patch`: a thread in `fork` pre-syscall handling waits as
  "no transfer", so another thread's flush resumes it in place instead of
  redirecting it past the syscall.
- `dynamorio-vfork-handlers.patch`: a vfork child's copied handlers no longer make
  later signals to initializing threads fatal ("multiple native handlers").
- `dynamorio-suspend-pending.patch`: signals that arrive while a thread is
  suspended are requeued to it. The JVM's primordial thread, blocked in
  `pthread_join`, received `kill -HUP <jvm>` that way and never ran the handler;
  `sigext` keeps its main thread busy and so does not hit this path.
- `dynamorio-safe-spot-signals.patch`: a signal that interrupts an application
  thread waiting at a synch-safe spot in DR (for another thread's flush, a
  contended lock or a fork) is delayed instead of handled at once. Handling it
  left the wait without its epilogue, so the thread stayed marked safe and later
  synch-alls redirected it from a stale context while it ran in the code cache.
  JFR execution sampling (HotSpot's SIGUSR2) and the `SignalBomb` and
  `InterruptedSyscalls` probes crashed C2 code with corrupted registers and
  untranslated cache PCs; the debug build failed the dispatch-entry check.
  The plain C reproducers do not reach it: they make few flushes.
- `progress.c`: the charge restores its scratch register before the application
  instruction. Excluding `java.util.TreeMap.rotate*` from C2 avoided the heap
  corruption; C2's code for it deoptimizes through an implicit null check whose
  handler saves a register that straight-line liveness considers dead.

- `dynamorio-ibl-hit-signals.patch`: a signal on the indirect-branch lookup's hit
  path, after the app's x2 is restored, now finds the target fragment through the
  hashtable entry, so a thread looping through indirect branches still exits to
  receive it (`sigregs ... ibl`).
- Emulation gaps found by the `C.*` probes (`probes/c/cfz.c`):
  `dynamorio-sigaltstack-autodisarm.patch` (handlers ran with the alternate stack
  still armed), `dynamorio-signalfd-value.patch` (`ssi_int`/`ssi_ptr` were zero),
  `dynamorio-fork-pending-signals.patch` (a signal sent to a fork child before
  its translator initialized was dropped, so `kill` right after `fork` or through
  a pidfd did nothing) and `dynamorio-clone-clear-sighand.patch` (`clone3`'s
  `CLONE_CLEAR_SIGHAND` left the parent's handlers in the child).
- `dynamorio-icache-batch.patch`, `dynamorio-synch-fairness.patch`: JIT flushes
  join a thread's consecutive `IC IVAU` lines and run at its next `DSB`, waiting
  for other flushes without being redirected; a thread starting a stop-all lets
  waiting ones go first (`spawnjit 60 1 fork 1 0`).
- `dynamorio-clone-record.patch`: a new AArch64 thread found its clone record by
  reading unused stack and could read a stale record (the `Signals` probe hung).
- `dynamorio-sigsuspend-pending.patch`: upstream PR #7958; `sigsuspend` hung for a
  signal it unblocked that was already pending (`C.sigsuspend`).
- `dynamorio-mmap-noreplace.patch` (`map_noreplace`) and
  `dynamorio-vdso-sigreturn.patch` (`sigframe_unwind`).
