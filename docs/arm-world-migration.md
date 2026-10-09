# ARM coordinator migration: policy deltas

Baseline: #167 (`00059fb51805a99804d6e0903f1b7d89d6c156dc`). This classifies
source behavior; it is not native qualification or a claim of equal witnesses
between the old engine and the new engine.

The ARM application entry now constructs `seedbox_engine_core::world::World` and
calls its `simulate` directly. The removed ARM `World` no longer selects machines,
computes horizons, routes frames, orders control/fault work, or owns lifecycle
policy. `ArmMachine` embeds one `MachineState`; its separate stopped observation
cache contains only acknowledged physical IRQ/deadline/I/O/ABI/exit facts.

| Behavior | Baseline ARM loop | Shared coordinator integration |
| --- | --- | --- |
| Default run and replay | Cold boot, no launch barrier or snapshot | Preserved: `barrier=false`, `fork=None`; retained executable dispatch remains authoritative. |
| Causal effects | Processed the raw `Event` vector, routing frames inline | Shared order: committed console, readiness/fault ACKs, exit availability, framed controls, fault observation, then transmitted frames. Raw event order still contributes to the ARM transcript witness. |
| Console triggers | Guest operation rejected as unsupported | Shared trigger policy receives the first committed UART byte from a worker-local stop. Captured log batches never reconstruct trigger time. |
| Connection discovery/selectors | Guest operations rejected as unsupported | Shared connection policy observes actual transmitted frames and uses the existing incarnation-bound selectors. |
| Strict management delivery | Shared queue with ordinary frame events | Shared strict queue; replies/commands are never delivered early. Retained network hashing merges both queues by their original per-machine `(time, sequence)` keys. |
| Failed control | Old loop's boundary/error conditions | Shared fatal-error handling retains delivery of an already owed strict reply. |
| Paused ingress limit | Counted currently queued frames when each arrival was planned | Shared policy counts accepted ingress arrivals during each pause, with a fixed 256-frame limit. |
| Short Ethernet frame | Infrastructure error | Shared router ignores frames shorter than an Ethernet header. |
| Lifecycle incarnation | Incremented the public record at reboot | Shared record increments at crash while constructing the stopped successor. A final Down report still projects the predecessor's evidence incarnation, time and boot time, preserving the old report schema. |
| Crash persistence | Surviving disk stayed inside the old worker until reboot | Explicit terminal export and one-shot installation into the never-entered replacement. The artifact is bound to transaction, predecessor, intended successor, path, disk digest and artifact SHA-256. |
| Capture continuity | File sink and remaining retention cap stayed on the old machine wrapper | Factory owns one shared pair of sinks/caps per machine. Successors reuse them without truncating files. Framing and physical witnesses still consume every byte beyond the cap. |
| Invalid worker reply | Sequence and horizon checks only | Stamped request/ACK validation precedes scheduler/effect/file publication. Failure poisons and joins the owned worker; a failed succession terminates the world transports and blocks reuse. |

The ARM primitive currently handles cooperative idle inside the worker and does
not return a new Halted state to the coordinator. The adapter therefore reports
Running for ordinary Horizon/Device/Console stops. Its acknowledged pending IRQ
and deadline facts are real; it does not manufacture halted wakeups, missing IRQs,
empty I/O, or timer deadlines. `early_slack` is safely ignored by the exact-stop
primitive, and the retained `stop_tolerance_ns` remains inactive as before.

Launch-barrier release is available only at a physically observed guest barrier,
using `seedbox.barrier=1`; it reconciles time/timers before entry. Time parking
requires a completed stopped boundary with no owed I/O, but releasing a time park
needs unsupported entropy reseeding and returns an explicit error. Arbitrary
snapshot capture and restoration remain unsupported; no metadata-only restore
is admitted. Speed reseeding is also explicitly unsupported.

The public report keeps version 2, `arm64-pv-world-v1`, existing fields and witness
domains. Trigger evidence contributes semantic witness bytes only when triggers
were used. Changes in shared ordering or fault policy may change slices, traces,
counters and witnesses; such changes require explicit native evidence, not a
blanket baseline-mismatch acceptance. The worker IPC contract is version 2; old
runs continue to launch their own retained executable and its own dispatch.

Portable tests
check policy and IPC behavior; they do not qualify native HVF execution,
arbitrary CPU/device snapshots, or cross-engine witness equality.
