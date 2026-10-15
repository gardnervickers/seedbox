# A leaner Seedbox progress client (AArch64): agreed design

Authors: Fable (Claude) with Codex (gpt-6-astra, xhigh) as reviewer.  Design only;
nothing was edited, built or committed in either repo.  Measurements:
[lean-progress-client-measurements.md](lean-progress-client-measurements.md).  The
review transcript and raw logs (`codex-transcript.md`, `matrix.log`, `dumps.log`,
`variants/`, `seedbox-build/`) were not copied into this repository.  DynamoRIO fork at master 25842b959; Seedbox's committed client at
5ebddb73:guest/progress/progress.c, and its branch `claude/lean-progress` whose head
4bbb627e already implements the design below (reviewed in section 7).

## 1. Summary

* Start-up cost is instruction-count-at-instrumentation-time: `bash -c true` builds 5214
  blocks; the client adds 61 ms to bare DR's 20 ms (Seedbox's 94/21 reproduces as
  81/20 in our VM).  Attribution (section 3): drreg's per-block liveness analysis
  **22-26 ms even with nothing reserved**, drreg reserve/restore bookkeeping 8-10 ms,
  and the clean call's 37-instruction inline skeleton 25-26 ms.
* `DR_CLEANCALL_ALWAYS_OUT_OF_LINE` is a no-op for this callee: callee analysis bails
  on the call to `dr_get_mcontext` (`core/arch/clean_call_opt_shared.c:258-271`) and the
  out-of-line switch is already selected by the SIMD threshold (`:784-800`).  Verified:
  identical emitted code and timing (variants v6/v7).  The "cheaper variant" (A) therefore
  only removes drreg: 81 -> 55 ms.
* **Agreed design ("B-TLS")**: no drreg; a fixed scratch register (x1) saved to and
  reloaded from the client's own raw-TLS slot around the unchanged 4-instruction charge;
  when the budget goes negative the block stores the charged instruction's app pc and
  its charge in two raw-TLS words and branches (`br`) to one shared routine the client
  emits once with `dr_insert_clean_call_ex` into `dr_nonheap_alloc` memory; the
  routine reloads x1 and runs DR's own out-of-line clean call to a no-argument callee
  that refills, gives the charge back and `dr_redirect_execution`s to the charged
  instruction, exactly as today.  14-16 meta instructions per charge point instead of
  44; code cache 1.53 MB -> 0.95 MB.  Measured on Seedbox's real client: **81 -> 37 ms
  (2.2x)**; the fast path executed per block is unchanged (6 instructions).
* Correctness rests on one invariant: DR never relocates a thread inside the window
  where x1 holds a tool value, because every instruction there has a NULL translation
  and the window contains no clean-call region; DR then reports pc-only success, a
  synch retries and an asynchronous signal is delayed.  A `restore_state_ex` event
  still repairs x1 in the pc-only context from the slot (note-marked ilist walk).
* Charge points, amounts, callee arithmetic and the dispatcher exit are unchanged, so
  the per-thread charge history is identical.  **Witness identity across a Seedbox
  simulation is not implied by that** (section 6): DR's own marked syscalls (heap/cache
  commits) move guest time with memory use, so this change, like any change to DR's
  memory footprint, perturbs simulated time slightly.  Seedbox's commit message already
  says so.

## 2. The client today (verified from source)

`progress.c` (5ebddb73) and our `suite/tests/client-interface/a64_progress_like.dll.c`
have the same AArch64 shape.  drmgr app2app rewrites WFE/WFI/WFET/WFIT to NOP (keeping
the translation); analysis counts app instructions and plans `points` charge points at
index `p*len/points`; insertion, before the point's instruction `where` (charge n):

```
drreg_reserve_register        str xS, [x28, #drreg_slot]       ; always spilled (conservative)
dr_insert_read_raw_tls        ldr xS, [x28, #BUDGET]
                              sub xS, xS, #n                   ; repeated for n > 4095; no flags
dr_insert_write_raw_tls       str xS, [x28, #BUDGET]
                              tbz xS, #63, skip
dr_insert_clean_call_ex(refill, READS_APP_CONTEXT|MULTIPATH, resume_pc, n)
   = drreg stateless reload of xS (1)
   + DR clean call: swap to dstack (9), out-of-line save: sub sp/str x30/mov-imm x30/
     blr x30/ldr x30 (7), args mov-imm x0 (3) movz x1 (1), mov-imm x11 + blr x11 (4),
     out-of-line restore (4) + add sp (1), swap back (7)                 = 37
skip: drreg_get_app_value     ldr xS, [x28, #drreg_slot]
drreg_unreserve_register
```

44 meta instructions, 176 bytes per point (`dumps.log`, v6).  The callee:
`do { *left += budget; progress_call(); } while (*left < 0); *left += charged;`
then `dr_get_mcontext`, `pc = resume`, `dr_redirect_execution`: the thread leaves
through the dispatcher at the charged instruction so that a synch-all, which in
Seedbox's guest only ever catches a thread at a progress syscall, finds it at a safe
spot (`docs/internals.md`, "Guest signals arrive only at progress calls").

x86-64 differs: `drreg_reserve_aflags`, `sub [tls], imm`, `jns`, a plain
`dr_insert_clean_call(checkpoint)` that returns.  It is left unchanged by this design.

## 3. Where the time goes

Per block, over bare DR, for 5214 blocks (`measurements.md`; proxies built from
`a64_progress_like`, then confirmed on Seedbox's own old and new clients):

| cost | ms | source | removed by |
|---|---|---|---|
| a bb hook exists: full decode + translation recording | 1.3 | `core/arch/interp.c:5286-5321` | nothing |
| drmgr phases and per-instruction dispatch | ~2 | `ext/drmgr/drmgr.c:990-1135, 1248-1300` | (kept: see 7) |
| drreg bb analysis: 31 GPRs x every instruction, reverse scan, a drvector per register plus aflags, for every block whether or not anything is reserved | 22-26 | `ext/drreg/drreg.c:367-462` | B-TLS |
| drreg reserve / stateless reload / get_app_value / unreserve (and the ff2e7b30d/9961d0a note handling) | 8-10 | `drreg.c` reserve/restore paths | B-TLS (2 plain TLS instructions) |
| clean-call skeleton, 37 instructions created and encoded per block | 25-26 | `core/lib/instrument.c:5261-5420`, `core/arch/mangle_shared.c:193-445`, `core/arch/aarchxx/mangle.c:848-905` | B-TLS (8-10 instructions per site) |
| callee analysis | 0 (cached per callee; `-opt_cleancall 0` changes nothing) | `clean_call_opt_shared.c:717-760` | |
| refills (clean-call execution, progress syscall) | ~0 at budget 50000; +4-8 ms at budget 1000 | | |

Each meta instruction emitted per block costs ~0.7-0.8 ms per 5214 blocks (~150 ns:
`instr_create`, operand construction, AArch64 encoding, `instr_length` re-encoding
during translation bookkeeping, cache space).  Instruction count per site is the lever.
Measured: current 81 ms; A (no drreg, same clean call) 55; B-TLS 36-37 (Seedbox's
4bbb627e: 37-39); fast path only (lower bound for this charge) 29; bare DR 20.

## 4. Translation facts the design relies on

Numbered as the constraints handed over by the DR-side review; each verified, two
corrected.

1. **DR restores only its own mangling spills: confirmed, and sharper.**  The walk
   tracks spills only inside `instr_is_our_mangling()` regions
   (`core/translate.c:369-560`) and only DR's slots (`instr_is_DR_reg_spill_or_restore`,
   `core/ir/instr_shared.c:4317-4371`): a client's `str x1, [x28, #raw_slot]` is ignored,
   never mistaken for a DR spill.  Correction: the restore event is not what keeps a
   mid-window thread correct.  Every instruction in the window has a NULL translation,
   so `recreate_app_state_from_ilist` returns `RECREATE_SUCCESS_PC` with a guessed pc
   (`translate.c:1120-1160`); `translate_mcontext` rejects anything but full-state
   success (`core/synch.c:507-524`); `at_safe_spot` requires full-state success
   (`synch.c:586-690`); a synch therefore retries, and an asynchronous signal is delayed
   (`core/unix/signal.c:5848-5900`, including user-sent SIGSEGV/SIGILL via `is_sys_kill`).
   A successful client restore callback cannot promote pc-only to full-state
   (`core/lib/instrument.c:1894-1920`).  A synchronous fault inside the window is
   impossible: it accesses only registers and DR's TLS through x28.
   **Exception (Codex):** a DR clean-call region (`DR_NOTE_CALL_SEQUENCE_START/END`) is
   treated as full state even at NULL translations, via `translate_restore_clean_call`
   (`translate.c:973-990` for stored tables, `:1138-1146` for ilists), with its own
   "XXX i#4219: not always full state" caveat.  Stored tables arise without
   `DR_EMIT_STORE_TRANSLATIONS`: a fragment pending deletion records them
   (`core/fragment.c:3285-3300`, `FRAG_WAS_DELETED`).  Schedule for the **current client
   and candidate A**: budget 10, charge 20 -> TLS budget -10; suspend after the inline
   clean call saved the context but before the callee; the fragment is pending deletion;
   translation reports full state and relocates to the charged pc; the point re-charges
   20 with no give-back.  B-TLS has no clean-call region in the fragment (the clean call
   lives in the shared routine outside the cache), so it is not exposed.
2. **Pure function of the block: confirmed.**  `recreate_ilist_from_code_copy`
   (`translate.c:1327-1368`) re-runs the bb event on the copy at app pcs with
   `translating=true`; `record_translation_info` (`translate.c:2146-2185`) stores only
   the copy for such blocks, and explicit stored translations disable code copies
   (`interp.c:5482-5500`), so `fragment_info.ilist == NULL` must be tolerated by the
   event (return true).  B-TLS's only global is the routine address, fixed before the
   first block is built.  DR's decode-then-copy race (`translate.c:2165` comment) is
   unchanged by the client.
3. **x28 / adjacent registers / NZCV / LR.**  x28 is the TLS base in meta code.  B-TLS
   uses SUB (not SUBS), TBZ, MOVZ/MOVK, STR/LDR, BR: none write NZCV; no BL, so no LR
   spill.  The "never a register the adjacent instruction touches" rule is stronger
   than needed (Codex concurs): the window closes before `where` (`where`'s mangling
   is inserted after the client's instructions, `mangle_shared.c:1924`), and `prev`'s
   mangling epilogue (ldstex, stolen-register restore `aarchxx/mangle.c:2297`) precedes
   the window; AArch64 epilogue emulation stops at the first instruction that is not a
   matching epilogue (`translate.c:665`), so it never emulates the charge.
4. **Code outside the cache is untranslatable: confirmed**, and identical to today: a pc
   in client or DR memory gives `RECREATE_FAILURE` (`translate.c:1706-1712, 1948`), the
   synch retries, and an asynchronous signal there is treated as "in DR" and delayed
   (`signal.c:5960-6000`; the routine's page is VMM-reserved, so `is_dynamo_address` is
   true, `core/vmareas.c:3768`).  Today's clean call enters DR's `clean_call_save`
   gencode and the client-library callee, which are the same kind of place.  Seedbox's
   dispatcher exit (the redirect) is what prevents the retry-forever under its guest's
   signal delivery, and it is kept.  `should_track_where_am_i()` is false in release
   (`instrument.c:5036`), so `whereami` stays `DR_WHERE_FCACHE` in the callee today too.
5. **9961d0a: confirmed, not reproducible here.**  That bug was drreg's restoration
   walk taking the tool's raw-TLS store for a DR scratch spill and missing the elided
   BL's later write of LR.  B-TLS has no walk of that kind and never holds a tool
   value across any app instruction or mangling; an elided BL before the point leaves
   its post-call LR untouched by the window, and a point before an elided BL sees the
   pre-call LR.  Both are the app's values at the resume pc.

Other paths: `dr_suspend_all_other_threads` + `dr_get_mcontext` translates lazily
(`instrument.c:6700-6720`) only after the thread was synched at a safe spot; nudges
unlink and queue (`signal.c:10342`); syscall restart handling concerns syscall pcs; the
`code_changed` downgrade (`translate.c:1236-1250`) only ever lowers a result; detach
requests valid contexts before translating (`synch.c:2337+`, `:2645`).  `-ldstex2cas`:
this fork always records the software reservation in TLS, even for an adjacent pair
(`aarchxx/mangle.c:3562`), so a charge between `ldxr` and `stxr` (points > 1) meets no
register-only reservation; a progress syscall there may invalidate the guest
reservation, as today.

## 5. The design (B-TLS)

Raw TLS: `dr_raw_tls_calloc(&seg, &off, 4, 0)`: BUDGET, SPILL, RESUME, CHARGED
(Seedbox's layout; a fifth word ROUTINE is optional, see 5.3).  `thread_init`: budget =
`instruction_budget`; record the thread's raw-TLS base in a drmgr TLS field
(`drmgr_register_tls_field`; never `dr_set_tls_field`, which drmgr owns:
`ext/drmgr/drmgr.c:2917, 3023`); the first thread emits the routine.

### 5.1 Per charge point (before `where`, app pc P, charge n)

```
      str  x1, [x28, #SPILL]        ; unmarked: before it runs the slot is stale
      ldr  x1, [x28, #BUDGET]       ; marked HOLDS (x1 = tool value from here on)
      sub  x1, x1, #n               ; repeated for n > 4095, as today
      str  x1, [x28, #BUDGET]
      tbz  x1, #63, skip
      movz x1, #n                   ; n <= 4095 (-max_bb_instrs 256, optionsx.h:939); assert
      str  x1, [x28, #CHARGED]
      movz/movk x1 = P              ; 3 instrs for a 48-bit address (zero halfwords skipped)
      str  x1, [x28, #RESUME]
      movz/movk x1 = routine        ; 3 instrs (or: ldr x1, [x28, #ROUTINE], 1 instr; 5.3)
      br   x1                       ; never returns
skip: ldr  x1, [x28, #SPILL]        ; marked HOLDS: at this pc x1 is not yet reloaded
```

14-16 meta instructions (56-64 bytes); 6 executed on the fast path, as today.  Register
and flag handling: only x1 (any fixed register except x28 and SP works; x1 matches
Seedbox's choice), NZCV untouched, no LR.  Notes: every instruction after the spill up
to (not including) `where` carries the client's note (`drmgr_reserve_note_range`); the
label has length 0 and is skipped by the walk; the note means "x1 holds a tool value
before this instruction executes and the app value is in SPILL".

### 5.2 The shared routine (emitted once, first thread-init event)

```
instrlist_t *il = instrlist_create(dc);  instr_t *end = INSTR_CREATE_label(dc); append(end)
dr_insert_read_raw_tls(dc, il, end, seg, SPILL, x1)        ; app x1 back before the save
dr_insert_clean_call_ex(dc, il, end, checkpoint_and_dispatch, 0 flags (or ALWAYS_OUT_OF_LINE), 0 args)
instrlist_meta_preinsert(il, end, trap)                    ; brk: the callee must not return
page = dr_nonheap_alloc(dr_page_size(), RW|EXEC); instrlist_encode(dc, il, page, true)
DC CVAU / DSB ISH / IC IVAU / DSB ISH / ISB over the encoded bytes; optionally dr_memory_protect(RX)
```

The emitted sequence is DR's own (stack swap via the dcontext found through TLS,
`mangle_shared.c:193-250`; shared 64-bit `clean_call_save/restore` gencode,
`core/arch/emit_utils_shared.c:5826+`; absolute targets, `aarchxx/mangle.c:829-846`), so
it is thread- and position-independent and inherits Seedbox's vector-register save
patch (`aarchxx/mangle.c:149`).  This construction is documented
(`core/lib/dr_ir_utils.h:400-415`).  The first thread-init event has a real dcontext and
runs before any block (`instrument.c:819`); no drreg means no clean-call insertion
callback (`instrument.c:5279`).  `READS_APP_CONTEXT|MULTIPATH` are no-ops without drreg
(`ext/drreg/drreg.c:874-925`).  The exit event frees the page and the raw TLS after all
threads are synched; reset the routine pointer so a re-attach re-emits it.

The callee (unchanged arithmetic):

```
checkpoint_and_dispatch(void):
  base = dr_get_dr_segment_base(seg); resume = *(app_pc*)(base+RESUME); charged = *(ptr_int_t*)(base+CHARGED)
  do { *left += instruction_budget; progress_call(); } while (*left < 0);   ; "negative", every intermediate call
  *left += charged;                                                        ; the point re-runs at resume
  mc = dr_get_mcontext(DR_MC_ALL); mc.pc = resume; dr_redirect_execution(&mc)
```

Because the routine reloaded x1 before DR's save, the dstack context is the true app
context (also what `dr_redirect_execution`'s kernel-transfer path re-reads,
`instrument.c:6888`); `dr_get_mcontext` does not supply a clean-call pc
(`instrument.c:6774`), hence RESUME.

### 5.3 Routine address: immediate or TLS word

Agreed (Codex's recommendation, Fable concurs): prefer the TLS word, `ldr x1, [x28,
#ROUTINE]; br x1`, initialized in every thread-init event after the routine exists
(the first thread-init runs before execution starts, `instrument.c:819`, and the client
already depends on thread-init for BUDGET and the recorded TLS base).  It saves two
instructions per point (~1.5 ms of 37 here; this exact variant is unmeasured, the
estimate follows the per-instruction cost).  The immediate, as in 4bbb627e, is
acceptable: an embedded address is not a re-attach defect, since old fragments are gone
by then; what must be fixed in either form is stale client state (a freed routine
pointer left set) across a detach/re-attach of a static or reused client.

### 5.4 Restore-state event

```
restore(dc, restore_memory, info):
  if (info->fragment_info.ilist == NULL || !info->raw_mcontext_valid) return true;  ; stored tables: nothing to claim
  pc = info->fragment_info.cache_start_pc
  for inst in ilist: len = instr_length(dc, inst); if (len == 0) continue;
      if (pc == raw_mcontext->pc) { if (note(inst) == HOLDS) mcontext->x1 = *(reg_t*)(tls_base(dc) + SPILL); break; }
      pc += len
  return true
```

`dc` is the translated thread's dcontext (`instrument_restore_state` passes
`tdcontext`), so `drmgr_get_tls_field(dc, field)` reads that thread's base.  Restoring
x1 never undoes the committed budget store; the event only makes the pc-only context
honest (Codex: do not assert that such a context is a native state at its reported pc).

### 5.5 Behaviour per translation path

| path | behaviour |
|---|---|
| synchronous fault in `where` (HotSpot implicit null check) | x1 already reloaded; frame is the app's; unchanged |
| fault inside the window | cannot occur |
| async signal / flush synch / `dr_suspend_all_other_threads` / detach with pc in the window | pc-only -> not a safe spot -> retried; signal delayed; event repairs x1 in the pc-only context |
| pc in the routine, DR gencode or the callee (incl. blocked in the progress syscall) | FAILURE -> retried / delayed; the callee leaves through the dispatcher, as today |
| pending-deletion fragment (stored tables) | NULL translation, no clean-call region -> pc-only; event returns true on `ilist == NULL` |
| `-xl8_code_copy` recreation, sandboxed selfmod blocks | bb event re-run on the copy: same immediates (P is the pc the copy is decoded as), same notes; same walk |
| traces (disabled; if enabled) | each constituent block gets the same sequence under `for_trace`; the redirect squashes a trace in progress; unverified by tests: `-disable_traces` stays in the approved contract |
| fork / exec | the routine page and TLS are inherited / rebuilt |
| redispatch at `where` (existing behaviour, old client too) | a thread suspended exactly at `where`, after the charge committed and x1 reloaded, translates fully; a flush synch-all then sends it to dispatch at P (`core/fragment.c:6052`, `synch.c:2004-2032`) and the new fragment at P charges again without an app instruction having executed. Not exactly-once charging across legitimate redispatches; see section 6 |

The meta `br x1` is handled correctly by every pass that looks at ctis, not only the
encoder (Codex): pre-mangling exit recognition needs an app instruction
(`interp.c:2804`), `instr_is_exit_cti` rejects meta (`instr_shared.c:2925`, so the
linkstub walk `emit.c:197` ignores it), cti mangling skips meta (`mangle_shared.c:2140`),
the translation walk's cti spill reset is inside `instr_is_our_mangling`
(`translate.c:401, 447`), fragment decoding leaves a register-target branch undecoded
(`interp.c:7881`), trace-boundary discovery skips meta (`interp.c:7238`); a block with an
external meta cti is excluded from coarse-grain caching (`interp.c:2970`), which is
protective.

### 5.6 Expected speed-up

`bash -c true`: 81 -> 37 ms measured on Seedbox's client (2.2x); overhead over bare DR
61 -> 17 ms; the remaining 17 ms is the 6-instruction fast path plus 8-10 slow-path
instructions (~9 ms), drmgr (~2), full decode (~1.3), and a smaller cache.  Seedbox's
94/21 should become ~40/21; the Galera transfer's per-process start-up cost scales the
same way (prediction, not measured).  Budget 1000 adds ~4-8 ms of refills either way.

### 5.7 Alternatives considered

* **A** (keep the per-block clean call, drop drreg; Seedbox's earlier uncommitted
  state): 55 ms; exposed to the stored-table clean-call schedule of section 4.1.
* **B (Fable's draft)**: LR spill + `blr x30` + packed `resume|charge` in x1: same cost,
  but an extra LR window, a packing limit, and a dstack context holding tool values;
  superseded by B-TLS (Codex).
* Direct `bl` to the routine: ~1.5 ms, but `dr_nonheap_alloc` is only `VMM_REACHABLE`
  within a 1 GB `-vm_size` (`optionsx.h:1618`), not +-128 MB; rejected.
* Dropping drmgr: ~2 ms; rejected for now (drmgr registers a signal event,
  `drmgr.c:555`, whose presence changes DR's signal handling, e.g. `signal.c:5769`;
  under a witness requirement that is not a pure cost change).
* Literal pool after a `bl`: towards the 29 ms floor; raw data in fragments is unusual
  for DR's walk; deferred.
* Lazy/per-region charging, `DR_CLEANCALL_NOSAVE_*`: charges must be identical from the
  first execution; the inline skeleton does not shrink.
* x86-64: Fable's sketch (`lea rsp,-128; call stub`) would fault below the red zone and
  mis-described the `lahf/seto` flag save; a TLS-continuation design is possible but
  unmeasured.  Keep the x86 client as is.

## 6. Charge identity versus witness identity

Agreed contract (Codex's wording, replacing the unconditional witness-identity
requirement): given the same block-construction inputs and the same application-pc
entry/re-entry history, the old and new clients select identical charge points and
amounts and perform identical budget/progress transitions; controlled differential tests
compare the ordered per-thread histories of `(P, n, budget-before, budget-after,
progress-call, give-back, redispatch/re-entry)`.  Independently scheduled runs need not
have identical histories or witnesses; a witness difference requires attribution and
does not by itself establish a client bug.  The inline arithmetic and the callee loop are
the same code and the progress syscall happens at the same point of the thread's
execution; but neither client charges exactly once across legitimate redispatches
(section 5.5), and the old client additionally has the stored-table clean-call
relocation of section 4.1, so neither a native trace nor the old client under arbitrary
interruption is an unconditional budget oracle.
What the design cannot promise is bit-identical Seedbox witnesses: DR's own syscalls
carry Seedbox's translator mark (`core/drlibc/drlibc_aarch64.asm:70`) and are charged a
tenth of a syscall (`kernel-arm64/seedbox.c:217`), and their number depends on heap and
code-cache commits (`core/unix/os.c:3957`), which this change halves, and on
synchronization and signal-return syscalls.  A scheduling boundary can therefore move.
Any change to DR's memory footprint, including merely dropping drreg, has this effect;
Seedbox's commit message already states it.  With the same incoming events, block
history and redispatch history, and translator-syscall charges accounted for separately,
neither of us found an application-visible operation that B-TLS adds.  The
witness/replay gate remains an empirical Seedbox decision.

## 7. Review of Seedbox's 4bbb627e against this design

Matches sections 5.1-5.4 with the routine address as an immediate; Codex found no new
translation blocker.  Confirmed correct: the spill itself unmarked (`progress.c:207`),
notes through the reload meaning state-before-execution (`:240`), the suspended thread's
base via its drmgr TLS field (`:174`), x1 reloaded before DR's save so the saved frame
holds the app's x1 and x30, exact instruction-start matching in the walk (valid pcs are
instruction-aligned; not a defect).  Gaps to record:
* Lifecycle reuse: `finished()` frees the routine but leaves `dispatch_routine` set
  (`:324`), so a static or reused client that re-attaches would skip regeneration (`:147`)
  and branch into freed memory.  Reset it (and the TLS words) at cleanup.  A dynamic
  client is unloaded at detach (`instrument.c:932-940`), so this does not bite Seedbox
  today.
* Documentation overstatement: `docs/internals.md:100` promises x1 restoration for a
  translation inside the charge; stored-table translations have no ilist and get none.
  State the primary no-relocation invariant, with best-effort restoration when the
  recreated ilist is available.
* Optional polish: a trailing trap after the clean call (defensive only: the callee's
  `DR_ASSERT(false)` after `dr_redirect_execution` aborts in release too,
  `core/lib/dr_tools.h:100-106`); protect the finished page RX; express the intent with
  `DR_CLEANCALL_ALWAYS_OUT_OF_LINE` (`READS_APP_CONTEXT|MULTIPATH` add nothing without
  drreg); a bounds check before encoding rather than the post-encoding assertion; the TLS
  routine word of 5.3.

## 8. Validation plan

Our repo:
* `client.a64_prop_cleancall`: add a `progress-lean` pattern (fixed scratch, TLS
  resume/charge words, shared routine, real restore event) under the existing oracles
  (fault frames at `where` equal to native; async-signal reruns; redirect variants;
  `-nohwcc` code-copy and `-sandbox` variants).  Fix the gaps Codex found: the progress
  callee sets the budget to 7 and ignores `charge` (`a64_prop_cleancall.c:906`), so add an
  exact budget-history oracle; `SIG_RUNS` is 20 not 30 (`:893`), and reruns must count
  raw pcs observed per instrumentation phase, not just repetitions.
* `api.a64_prop_jit`: a lean client mode for the `_progress` selfmod/stale-fault classes
  (code-copy recreation under the pattern); the elided-BL regression needs the external
  `drrun` route (`patch` mode, static DR disables elision, `a64_prop_jit.c:152`) with the
  lean client.
* New `client.a64_prop_progress_synch` (name open): executors run random blocks under
  the lean pattern with budgets that make every block refill (positive, zero, negative
  results; multi-budget overshoots; 1 and several points) while a client thread loops
  `dr_suspend_all_other_threads` + `dr_get_mcontext` + resume, a flusher uses the
  `a64_prop_flush` machinery, and a controller detaches/re-attaches as in
  `a64_prop_lifecycle`.  Checks: every successfully translated context equals a
  native-trace state at its pc (pc-only rejections kept separate); charge history equals
  the committed client's; liveness (workers progress after a rejected suspension,
  suspension completes, detach rebuilds routine/TLS; no worker parked waiting for a
  suspension that waits for it); coverage counts of interruptions in each phase (before
  the spill, after the budget store, during RESUME/CHARGED stores, at the `br`, in the
  routine's stack switch, in the progress syscall, around the redirect).
* `a64_smoke`: port `a64_progress_like.dll.c` to B-TLS and keep the drreg form as a
  second configuration (coverage for 9961d0a); run both at budgets 1000 and 50000;
  `jvm.jitstress_client_smoke` (DeoptCycle, IcNull) with the lean client.
* Old-vs-new charge-history differential: both clients log `(thread, P, n, before,
  after, progress-call, give-back, redispatch)` on deterministic programs pinned to one
  CPU; identical under matched entry/re-entry histories (section 6).  Keep the
  mathematical oracle (what the budget must be given the recorded events, modelling
  legitimate redispatches) separate from the old-client differential, and do not require
  the new client to reproduce the old stored-table bug.
* Translation representations exercised separately and observed: ordinary ilist
  recreation, code-copy recreation (writable pages after mutation), sandbox recreation,
  and stored tables (pending deletion, and `-store_translations`, which disables code
  copies, `interp.c:5484`).
* Interruption coverage counted from raw pcs before retry/translation (a successful
  `dr_suspend_all_other_threads` cannot show that the suspension first struck the stub:
  it synchronizes at safe spots, `instrument.c:4868`); directed boundaries before/after
  the budget store, at the fast reload, at the `br`, at context-save completion, in the
  progress syscall, at the give-back and the redirect; no clean call or `dr_sleep` as a
  phase gate (it changes the classification under test).  Full architectural state (SP,
  x1, x30, NZCV, vectors, fault frames) where full translation is promised.
* The fixed-budget test callees must be replaced: `a64_prop_cleancall.c:906` ignores
  `charge`; `a64_prop_jit.c:243` resets the budget and returns.
* Timing: `bash -c true` x25 under both clients, as in `measurements.md`.

Seedbox: witness identity on the JVM corpora and replay MATCH (expected to show the
translator-syscall perturbation of section 6, to be judged there), the
`guest/progress/fixtures` run, and the Galera transfer timing.

## 9. Disagreements and how they were resolved

Two rounds (`codex-transcript.md`).  Codex rejected Fable's draft B as written and
proposed the TLS-continuation variant, which Fable adopted (and which Seedbox had
independently committed).  Codex's remaining round-2 points were all accepted by Fable
as corrections of wording, not of the mechanism:

1. Charge-history identity is conditional on matched entry/re-entry histories; neither
   client charges exactly once across redispatches (sections 5.5, 6).
2. Meta ctis are processed by more than the encoder; the relevant passes support the
   branch (section 5.5).
3. Exact instruction-start matching in the restore walk is correct, not a defect.
4. The missing trap after the routine's clean call is defensive, not an executable
   fall-through bug (`DR_ASSERT` aborts in release).
5. An embedded routine address changing across completed DR lifetimes is not a
   re-attach defect; stale client initialization state is.
6. Documentation must not promise x1 restoration on stored-table translations.

Earlier draft positions Fable withdrew after round 1: `dr_set_tls_field` while drmgr is
loaded (drmgr owns that field); "if witnesses differ the bug is in the callee
arithmetic"; the x86-64 sketch; the hand-copied-DR-instructions fallback; the
"restore event is independent safety" framing.  Positions Codex and Fable agreed on from
the start: the attribution of cost, the no-op of `ALWAYS_OUT_OF_LINE` for this callee,
fixed x1 being safe next to any app instruction, keeping drmgr, and deferring direct
`bl`/literal pools.

No open disagreement remains on the B-TLS mechanism.  Codex's approval of its
correctness claims is conditional on the validation in section 8 being run; nothing has
been run beyond the timing measurements and the output check in `measurements.md`.
