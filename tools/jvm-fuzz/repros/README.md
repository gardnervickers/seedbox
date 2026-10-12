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
- `progress.c`: the charge restores its scratch register before the application
  instruction. Excluding `java.util.TreeMap.rotate*` from C2 avoided the heap
  corruption; C2's code for it deoptimizes through an implicit null check whose
  handler saves a register that straight-line liveness considers dead.

Still open: JFR execution sampling (`Jfr <seed> 1 exec 1`) crashes C2 code with or
without the client; `srsample` does not reproduce it.
