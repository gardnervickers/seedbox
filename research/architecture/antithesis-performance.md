# What Antithesis may have optimized, and what Seedbox should test

Research memo, 2026-10-06. Literature and source review only; the proposed experiments have not been run. Rankings below are engineering judgments, not claims about Antithesis's proprietary implementation.

## Finding

Public evidence does not identify a secret skid-free PMU mechanism. It does identify a deliberately specialized guest/hypervisor interface. The most interesting additional avenues for Seedbox are **removing guest time-read exceptions**, **handling administrative exits without returning to host userspace**, and **translating only expensive short execution regions while retaining hardware execution elsewhere**. The existing fixed-breakpoint and full-QEMU probes do not decide these questions.

### What is actually public

Antithesis's engineer describes a bhyve-derived hypervisor using Intel VMX, a whole test's containers inside one VM, one physical core per instance, guest scheduler control, and custom VMCALL I/O. They encountered both inaccurate retired-instruction counts and late PMC interrupts; they do not disclose the fixes. The early design accepted external inputs at guest-requested VMCALL boundaries, and **later added timed interrupt injection**. This historical statement does not establish that current production avoids precise interrupt stops. The stated optimization direction is keeping deterministic work inside the guest and favoring aggregate exploration throughput. [Antithesis engineering article, particularly “Time,” “CPU parallelism,” and “Deterministic I/O”](https://antithesis.com/blog/deterministic_hypervisor/).

Current documentation independently confirms a customized Linux kernel, possible performance degradation with customer kernels, container CPU-share controls, idle fast-forward, and expensive RDTSC/RDRAND operations. These facts support investigating guest cooperation; they do **not** establish a particular vDSO patch or a trap-free time implementation. [Environment documentation](https://antithesis.com/docs/configuration/the_antithesis_environment/). Its optimization guide explicitly identifies busy-waiting and nondeterministic instructions as simulation-efficiency problems. [Optimization guide](https://antithesis.com/docs/best_practices/optimizing/).

No retrieved primary source establishes Antithesis use of branch-step BTF, hybrid binary translation, compiler-inserted execution budgets, a particular PMU event, an internal correction loop, or disabled security mitigations. Its public performance claims also do not provide an apples-to-apples single-run benchmark against Seedbox. The talk linked from the engineering article was located, but a usable transcript was unavailable; no technical assertion here depends on an unheard part of that recording.

## Baseline and comparison discipline

The corrected Seedbox addendum records 184.739–215.095 seconds wall for roughly 60 simulated seconds, with 33.710–39.785 seconds inclusive correction (18–19%). Removing that entire category would give only about **1.22×** on those saved adaptive runs. This bounds a **correction-only** optimization, not all kernel or guest changes. The 232.228-second fixed-margin run has nearly the same correction time but many more transparent syscall yields; it is not a controlled margin comparison.

The 21.847 seconds measured inside debug-returning KVM_RUN calls contains guest execution, exit handling, mitigation and return work. Neither that bucket nor its complement identifies the removable fraction. The addendum also distinguishes matching exit witnesses from full RAM equality. [Published corrected evidence](https://github.com/gardnervickers/seedbox/issues/1#issuecomment-6018498004).

The experiment in [issue #10](https://github.com/gardnervickers/seedbox/issues/10) is qualifying one VM with network namespaces, delays and partitions. That is the right architecture comparison, but a loopback-only 3.2× result confounds networking, kernel duplication and memory. Once qualified, use that one-VM configuration as the baseline for the experiments below: it already removes inter-VM causal horizons, but retains timer deadlines and scheduled scenario inputs. Do not award those eliminated horizons a second time.

## Ranked experiments

### 1. Make the vDSO genuinely cheap inside the deterministic guest

**Confidence:** high that the existing path has avoidable work; unknown size in the real workload; no evidence of Antithesis's implementation.

Seedbox's captured c95c05f guest deliberately excludes BUILD_VDSO from its replacement of kernel RDTSC. User RDTSC/RDTSCP instead raises #GP, decodes the instruction, calls `seedbox_rdtsc`, writes result registers and resumes. The timepage therefore removes host time I/O while leaving a guest exception on the common user clock path. Go's runtime invokes the vDSO; the upstream Linux vDSO's TSC path uses `rdtsc_ordered`, and even conventional pvclock interpolation reads TSC. Simply enabling ordinary pvclock is insufficient. [Linux vDSO source](https://github.com/torvalds/linux/blob/v6.18/arch/x86/include/asm/vdso/gettimeofday.h), [Go runtime clock source](https://go.dev/src/runtime/time_linux_amd64.s).

**Bounded experiment:** count guest `seedbox_emulate_user_tsc` calls and guest branches spent on that path during the fixed workload. First compare current vDSO trapping against an explicit guest `clock_gettime` syscall path, then prototype a Seedbox-specific vDSO clock mode that reads deterministic state without RDTSC. Include a clock-intensive Go benchmark and the qualified one-VM etcd workload; two identical-seed replays per candidate, with timer/sleep checks and state digests. Stop if the workload's exception share is insignificant.

**Contract cost:** the current helper advances `last` by one on repeated reads. A read-only frozen epoch value would change progress semantics and could hang clock-polling loops without watchdog progress. A writable global increment would require carefully separated mappings and reentrancy/interrupt handling; never expose the entire VMM control page writable to userspace. Choose and version the clock contract explicitly. Keep raw RDTSC trapping for binaries that bypass the vDSO. A faster guest path changes the branch trace, so require repeatability within the new version rather than identical old/new branch witnesses.

**Why probes miss it:** guest #GP handling may execute entirely within the VM. Host-exit profiles can show fewer exits while much CPU time remains in this path. The opportunity persists with one VM. **Pursue measurement now; prototype if material.**

### 2. Consume administrative yields and correction exits inside KVM

**Confidence:** strong mechanism evidence; no measured Seedbox extension gain; Antithesis use unconfirmed.

Both upstream bhyve and KVM already have kernel loops that handle selected exits and resume the guest. Thus bhyve itself is not the differentiator; the relevant difference would be which deterministic operations a specialized fork keeps in that loop. [bhyve `vmx_run`/`vmx_exit_process`](https://github.com/freebsd/freebsd-src/blob/main/sys/amd64/vmm/intel/vmx.c), [KVM `vcpu_run` and `kvm_arch_vcpu_ioctl_run`](https://github.com/torvalds/linux/blob/v6.18/arch/x86/kvm/x86.c).

Linux explicitly documents that its conditional VMSCAPE IBPB is omitted when execution goes from VM-exit back to VM-entry without userspace running between them. An internal loop could save some cost currently *inside* the KVM_RUN bucket while preserving the configured mitigation. It still pays hardware VM-exit/entry, other required protections and useful guest work. [Linux VMSCAPE documentation](https://docs.kernel.org/admin-guide/hw-vuln/vmscape.html).

**Bounded experiment:** on a separately approved disposable kernel/host, implement only the Seedbox yield port and a declared logical soft/hard target. Consume pre-target administrative writes internally, return the first eligible semantic stop unchanged, and immediately fall back for unsupported exits. Compare identical accepted stops, logical counts, register/RAM/device state and wall time. Only then add a correction loop which receives the next breakpoint plan and re-enters without userspace. The actual workload must change breakpoint targets as it does today; a fixed cached address is an inadequate acceptance test.

**Costs:** PMU ownership and host-interrupt contamination, complete PIO semantics, migration/preemption, debug state, faults and cancellation all require qualification. The one-VM yield incidence must be remeasured: the four-VM 3.05M/14.32M transparent yields cannot be projected unchanged. Guest-side progress gating could prevent hardware exits altogether, but exposing a trustworthy guest-only counter is a separate ABI/PMU project.

**Decision:** pursue a narrow prototype after the one-VM profile establishes material remaining yield/correction cost; defer a generic production KVM fork. Do not change host mitigations for this test.

### 3. Translate only the correction tail or an exit-heavy cluster

**Confidence:** proven general virtualization technique; speculative Seedbox advantage; no evidence Antithesis uses it.

VMware's ATC'12 paper combines hardware execution with dynamically translated, cached clusters of exit-causing instructions, including control flow. This is a materially different option from replacing all execution with QEMU TCG. [Agesen et al., “Software Techniques for Avoiding Hardware Virtualization Exits”](https://www.usenix.org/conference/atc12/technical-sessions/presentation/agesen).

For Seedbox, enter a small interpreter/translator once near a hard target and execute the remaining bounded tail without repeatedly executing the VM-entry/#DB/VM-exit cycle. A second possible use is a short sequence with closely spaced time/device exits. The existing fixed-address breakpoint probe removes neither hardware exits nor their count; the QEMU CPU-loop probe translates everything and therefore cannot reject this hybrid.

**Bounded experiment:** first capture representative final-256-branch tails and select register/RAM-only basic blocks. In an isolated harness, compare the existing hardware stepper with a bounded interpreter, including register/state transfer and return-to-hardware cost. Explicitly stop before unsupported instructions, faults, MMIO, privileged effects, or a logical deadline. Validate post-state and logical branch counts over a small instruction corpus and real captured tails before attempting any Linux integration. Start with interpretation to test the break-even point; a JIT is justified only after it wins. For a correction-only integration, the saved baseline's roughly 1.22× all-correction-deletion figure remains an optimistic outer bound; realistic gains are smaller. Exit clusters outside correction could affect other categories, but their incidence is presently unmeasured.

**Critical determinism cost:** rr's authors report VMware's exit clustering broke retired-conditional-branch determinism. Their paper also explains why early PMU interrupts plus breakpoints are needed for precise replay. [rr technical report, §§2.4 and 5.1](https://arxiv.org/html/1705.05937). A hybrid must count **original guest branches** in software during translated execution, reconcile them with hardware counts at transitions, and keep host cache/JIT decisions out of logical time. Page translations, self-modifying code, exceptions, SIMD/xstate, and device ordering make unrestricted Linux emulation a large project.

QEMU's icount implementation demonstrates translation-block budgeting and a shortened block at the final deadline, but is not a ready-made drop-in KVM tail engine. [QEMU icount design](https://www.qemu.org/docs/master/devel/tcg-icount.html). **Pursue only a tightly bounded tail microprototype; defer unrestricted hybrid execution.**

### 4. Offer cooperative epochs as an explicit fidelity mode

**Confidence:** technically plausible, supported by the disclosed historical design; current Antithesis policy unknown.

Move ordinary simulation decisions to deterministic guest checkpoints—blocking transitions, selected syscall boundaries, network/device queue service, or a guest scheduling hook. Use a sparse exact branch watchdog for non-cooperative code and preserve an exact timed-injection mode. Publishing a sequence of already-chosen future inputs to an in-guest scheduler could also avoid returning to userspace for every intermediate decision.

**Bounded experiment:** on the qualified one-VM workload, compare the current exact timer/scenario policy against a versioned cooperative policy. Record timer and scenario delivery lateness, watchdog fallbacks, maximum uninterrupted guest progress, completed operations, guest scheduling coverage and wall time. Keep hard bounds for events whose deadlines are semantically required; do not merely accept PMU skid as deterministic. Test a CPU-bound infinite loop, clock-polling loop, blocking I/O and a narrow-window race as counterexamples. Stop if performance requires violating the promised lateness or losing the target bug classes.

**Fidelity cost:** fewer preemption points can hide races or alter timeout behavior. A deterministic replay of the changed model does not establish equivalence to exact injection or production scheduling. Compiler/runtime-inserted safepoints can improve cooperation but add toolchain dependence and must cover libraries/JITs/kernel code or retain watchdog fallback. Guest checkpoint selection is a model decision, not a secret way to deliver arbitrary interrupts precisely.

**Why one VM does not finish this:** timers and scenario inputs remain even without inter-VM horizons. Conversely, yielding on every syscall can create millions of administrative exits; sparse checkpoints must reduce host notification as well as correction. **Pursue as a separately labeled exploration mode after requirements are explicit.**

### 5. Keep modeled devices and idle detection inside the single guest

**Confidence:** public architectural match; incremental benefit beyond the namespace prototype is unmeasured.

After qualifying namespaces/delay/partition semantics, measure how much host traffic remains solely to administer internal network queues, timer arms or logging. A deterministic in-guest queue/fault scheduler can process a batch in stable order and notify the host only when new external guidance, a checkpoint, or the earliest deadline requires it. Coalesce notification, not semantically distinct delivery times. Preserve an ordered event ledger for replay and branching.

**Bounded experiment:** add idle, bursty and sustained-load variants of the same one-VM topology. Attribute simulated time advanced by idle fast-forward versus executed branches, runnable intervals, queue batches and timer interrupts. Compare notification batching with the same per-packet delivery order/timestamps and injected partitions. Then test an idle period with one pending packet and one earlier timer: fast-forward must stop at the earlier event. No host wall-clock timers or asynchronous completion races may decide guest-visible order.

**Costs:** guest fault machinery becomes part of the deterministic state and snapshot contract; a common guest kernel remains a fidelity difference from separate machines. Busy-wait detection must not mistake a productive loop for idle. Timer suppression requires proof that no runnable work or relevant deadline is skipped. **Pursue measured batching/idle opportunities; defer additional custom device machinery if the qualified one-VM profile already eliminates their cost.**

## Ideas to keep out of the immediate plan

- **BTF branch stepping:** Intel documents TF+BTF stepping on taken control transfers. That is not an exact stop after an arbitrary count of conditional branches: untaken conditional branches can advance Seedbox's count before the next trap. It also does not eliminate the hardware exit and needs capability qualification on AMD5950X. It may reduce decoder work in an Intel-specific tail loop, but is not a drop-in replacement for Seedbox's successor planner. [Intel SDM update, §19.4](https://cdrdv2-public.intel.com/868136/252046-081-sdm-change-document.pdf).
- **PMU precision features or a different counter:** no retrieved evidence establishes a skid-free, deterministic replacement valid on the actual host. A more accurate sample location is not the same as stopping execution there. Treat a CPU change as a separately measured portability project.
- **Full DBT or whole-system compiler instrumentation:** the narrow hybrid experiment is a cheaper discriminator. Whole-program instrumentation changes instruction streams, requires coverage of all executing code, and adds a substantial compatibility contract. No evidence attributes it to Antithesis.
- **A bhyve port solely for performance:** both existing hypervisors can internally handle exits. Porting the platform without identifying the expensive boundary would confound host, CPU, guest ABI and algorithm changes.

The next useful measurements are guest time-exception cost and the qualified one-VM exit/deadline distribution. Those select whether the first invasive experiment should be a tiny KVM internal handler or a tiny tail interpreter, without turning a plausible mechanism into an unsupported performance claim.

## Focused follow-up: single-vCPU LOCK-prefix elision

**Decision: a sound, narrowly scoped research avenue exists for a custom guest/toolchain contract; defer broad rewriting.** It could reduce locked-operation cost while leaving the SpecLockMap workaround enabled. Its magnitude is unknown, it would not remove host-side workaround costs, and no public evidence attributes this technique to Antithesis.

There is unusually direct precedent. Linux's x86 `LOCK_PREFIX` is empty with `CONFIG_SMP=n`; its SMP alternatives also support a UP execution mode. [Linux alternative definitions](https://github.com/torvalds/linux/blob/v6.18/arch/x86/include/asm/alternative.h). Linux's local-atomic documentation explicitly describes removing LOCK while retaining reentrancy against interrupts and NMI handlers, provided only the owning CPU modifies the data. It warns that remote observation does not have the usual ordering guarantees. [Linux local atomic operations](https://docs.kernel.org/core-api/local_ops.html). This supports instruction-level UP atomicity; it is not by itself proof that arbitrary language atomics can lose their ordering semantics.

The conservative candidate is an audited set of **single-instruction**, naturally aligned word-sized RMWs on ordinary write-back RAM—e.g. CMPXCHG, XADD, or non-returning AND/OR—emitted by a rebuilt runtime/compiler. Remove only the explicit LOCK prefix, retaining the instruction, its operand width, compiler memory dependencies and language-level atomic annotation. Same-vCPU interrupts, signals and thread switches do not require an inter-CPU lock for these single instructions; replacing one RMW with a load/compute/store sequence would introduce observable interruption points and is outside this proposal. Faulting/split accesses, MMIO, unusual memory types and wide operations need separate analysis and should initially remain unchanged.

The VM contract must guarantee that all observers/writers of the relevant memory are serialized through the one vCPU or fully completed deterministic stop boundaries. One vCPU alone is insufficient if a host device thread, vhost, passthrough DMA, shared-memory peer, or asynchronous inspector accesses that RAM while it runs. Guest-to-host publication still needs appropriate boundary ordering. A VM-exit in partially completed device emulation is not automatically an acceptable architectural boundary. Audit these conditions before relying on the UP argument.

Go is a plausible target because its amd64 atomic assembly contains explicit LOCK+CMPXCHG and LOCK+XADD. It also uses memory XCHG for exchange and atomic stores. [Go atomic assembly](https://go.dev/src/internal/runtime/atomic/atomic_amd64.s). **Memory XCHG is implicitly locked**, so deleting explicit prefix bytes cannot remove all locked operations. [Intel instruction reference, XCHG](https://cdrdv2-public.intel.com/774492/325383-sdm-vol-2abcd.pdf). Leave these exchanges alone initially. Audit compiler intrinsic lowering and the final binary as well as assembly helpers: editing helper source is not evidence that the hot program sites changed.

Go still requires sequentially consistent atomics and synchronization with ordinary accesses. Preserve compiler barriers and prove the restricted platform's ordering; do not change `sync/atomic` to ordinary source-level operations. [Go memory model, “Atomic Values”](https://go.dev/ref/mem). A single-vCPU execution model can supply the interleaving order, but this needs validation across actual scheduling, signal, GC and guest/host publication paths.

**Suggested next step, not performed:** inventory dynamic explicit-LOCK versus implicit-XCHG cost, then evaluate only a few rebuilt hot primitives under the unchanged MSR setting. Require instruction-boundary interruption checks, publication/atomic litmus tests, runtime stress and repeated whole-workload state validation. Gate the artifact to a single-vCPU contract with no hotplug and reject unsupported sharing. It is a distinct build: record its code hash and toolchain, account for changed instruction bytes/layout and any branch changes, and do not claim identical binary/self-inspection behavior or replay compatibility with the original artifact. Unlike a correction-tail change, any resulting speedup would affect guest execution throughout the run and is not bounded by the 18–19% correction share.
