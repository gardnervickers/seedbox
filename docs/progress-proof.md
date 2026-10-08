# Shared PV watchdog and cycle proof

`seedbox-engine-core::pv_progress` owns watchdog admission, backoff and cycle
proof. A host timer requests an observation; elapsed host time never advances
guest time or proves the guest idle. The engine requires no PMU.

The KVM PV adapter uses this engine. ARM's current horizon clock is unchanged in
this extraction stage: ARM still needs interrupt/debug landing and complete
comparison primitives before it can use the same proof policy. This stage does
not claim ARM no-exit-loop qualification or a completed shared world coordinator.

## Architecture boundary

`ProbeMachine` supplies a stable ordered landing key, a guest clock marker,
comparison-baseline capture, whole-state comparison, the next exact debug landing
and debugger cleanup. A key is a necessary equality filter, not a proof. Baselines
must include CPU, PV page and RAM; dirty maps identify pages to byte-compare and
must include device DMA. With no dirty tracking, compare full RAM. Only documented
debugger-owned state may be normalized. Capturing a comparison baseline does not
imply support for restoring a complete machine snapshot.

The adapter reports an exact landing, a finite straight-line planner cap, or an
unsupported operation. A planner cap abandons the proof. Unsupported stepping is
an error, never fabricated progress or idle. A confirmed debug landing and an
unrelated host interruption are distinct. KVM retains its existing EINTR-to-
watchdog mapping; the more specific interruption variant is available to adapters
that can distinguish the cause. Host timer creation and signal masking remain
platform mechanisms; the shared engine decides when delivery must be held.

`ProgressContext` exposes per-machine accounting by reference. The shared engine
increments those counters and enforces the false-alarm limit; adapters cannot
filter observations as a diagnostic sink. Accounting remains with the machine,
including across snapshot restore, even when a world reuses one progress engine.
It does not become a cumulative counter owned by that shared engine. Counters
remain excluded from semantic witnesses.

## Preserved policy

After a natural exit, two watchdog observations without intervening natural
progress start a probe. Failed probes back off exponentially to 1024 skipped
ticks. Natural exits reset backoff. An interrupted, unconfirmed landing abandons
the probe; watchdog delivery is held while probing so it cannot hide a breakpoint.

Brent search keeps a full-state reference, doubles its search window and gives up
after 4096 landings. The clock marker is captured once for the whole probe,
including across search-reference changes. Any change abandons the search.

A complete recurring state proves a cycle. Its unique least landing key selects
a canonical point independent of when the watchdog noticed the loop. Duplicate
minimal keys are ambiguous and fail; canonical traversal has its own bound and
fails if the proved cycle is lost. Only reaching that canonical point permits
idle. A state-changing infinite loop can remain unproven and run until the outer
wall-time limit, as in the existing PV model.

## Validation boundary

Portable scripted-machine tests cover genuine cycles from different starting
points, identical registers with changing RAM, ambiguous canonical keys, clock
progress across reference changes, lost landings, planner caps, unrelated host
interruptions, backoff and bounded errors. Negative controls deliberately remove
RAM equality, clock-progress abandonment, canonical selection and planner-cap
handling; the corresponding tests must fail.

KVM's existing real-guest tests remain in its adapter and exercise varying watchdog
periods and dirty/full-RAM modes. They require Linux and remain for the final merge
train. Portable policy tests do not establish hardware stepping correctness or
state-inventory completeness. ARM requires native no-PMU guest qualification when
its primitives are implemented.
