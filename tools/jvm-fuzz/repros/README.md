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
| `sigregs WORKERS SIGNALS [SPINS [x28\|ibl]]` | x0–x26 and q0–q31 across `tgkill` signals whose handler posts a semaphore | passes with the client; without it, `ibl` (a `blr`/`ret` loop) hangs on the second signal to a thread |

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

Still open: without the client, `sigregs 1 2 2000 ibl` hangs (a second signal to
a thread looping through indirect branches is not delivered). Seedbox always runs
the client, whose checkpoints pass through dispatch, so it does not see this.
