# Network device compatibility

The shared network model serves both the KVM Network profile and the ARM
Application profile. The Network profile includes bounded split-ring validation,
notification suppression, EVENT_IDX and optional deferred TX. The Application
profile retains its existing features, interrupt behavior and state projection.

## KVM validation changes

All queue geometry, ring bounds, descriptor links, DMA spans, flags and direction
are validated before the current request changes RAM or consumes a head. Chains
are bounded to 4 MiB. Invalid geometry can fail even with no available heads:
validation precedes the pending-head check. Non-power-of-two sizes remain stored
by the existing register clamp, but DMA rejects them. Head/next indices, cycles,
available-ring overrun, misalignment, unsupported flags and address overflow
return execution errors instead of unchecked access or unbounded traversal.

Receive validates the entire writable chain, including tails that the old loop
might skip after copying enough payload. The KVM Network profile preserves its
prior behavior for a short posted buffer: it copies the packet prefix that fits,
completes the head with the number of bytes copied, and reports acceptance and
the normal batch IRQ. The ARM Application profile still rejects a buffer too
small for the complete header and frame. Incoming KVM frames larger than 4 MiB
minus 12 bytes fail before backlog admission, but the existing not-ready drop
check still runs first. A ready full backlog still drops valid-size frames. Its
256-frame bound is unchanged.

Transmit retains opaque 12-byte header stripping; header bytes are not interpreted
or restricted. Empty and short chains still complete without producing a frame.
A valid prefix before a later malformed head remains completed in RAM, counters
and the ordered dirty journal. The failed head remains pending. A transmit error
does not return the local vector of earlier frames: it terminates execution and
is not a recoverable retry-delivery API. Receive backlog entries are removed only
after successful completion; a failed head and later frames remain queued. ISR
changes still happen at the successful outer notification/drain boundary.

Both machine MMIO and inbound delivery collect dirty writes before propagating
errors. The world propagates delivery errors rather than counting them as drops.
Reset retains the pending dirty journal while clearing device state, counters and
backlog. The semantic deferred-TX setting survives reset. Snapshot cloning retains
its own backlog, queue cursors, deferred-TX setting and diagnostic observations.

## Notifications and deterministic polling

The Network profile offers EVENT_IDX (bit 29). It becomes active only after
FEATURES_OK and driver negotiation. Without EVENT_IDX, each completed batch
samples the live available-ring NO_INTERRUPT flag. With EVENT_IDX, each batch
samples the live used_event threshold using wrapping 16-bit ring arithmetic.
Completion and frame acceptance remain independent of whether an IRQ is raised.

Legacy drains set used-ring NO_NOTIFY while servicing validated requests and
clear it before checking for more work. EVENT_IDX drains instead arm avail_event
after the complete used ring, then check available heads again. These event and
flag writes are bounded, preserve unrelated flag bits, and enter the ordered DMA
journal. Unready queues never touch rings. Malformed requests still fail before
that request changes guest memory or advances a cursor; completed prefixes and
their suppression writes remain visible on a later failure.

Deterministic device polls can service both TX and queued RX when EVENT_IDX is
negotiated. Legacy RX remains driven by notifications. Deferred TX is an explicit
semantic run setting, disabled by default. It suppresses TX notifications through
NO_NOTIFY or a full-ring avail_event threshold and services TX at deterministic
stops, with a maximum 50,000 ns simulated polling interval while time advances.
It does not enable legacy RX polling. Polling limits saturate at u64::MAX.

The shared model reports doorbell exits, suppressed batch interrupts, polled TX
frames and upper bounds on pickup delay from the last observed empty TX queue.
These counters and timing observations are diagnostic only: they do not affect
service decisions or enter execution witnesses. The VMM adapter owns the RAM
base and journal and exposes fallible polling and timestamped writes.

## Evidence boundaries

Portable tests compile the actual KVM device source on macOS. They cover bounded
hostile-chain execution, rejected-request cursor preservation, ordered completed
prefixes, frame/chain limits, reset and snapshot ownership. Additional tests cover
live suppression flags, event thresholds and full-ring offsets, index wrap,
notification-free polls, deferred-TX bounds and pickup diagnostics. The earlier
valid-traffic differential established the bounded-validation checkpoint; the
subsequent suppression changes intentionally add flag/event DMA journal entries.
Machine-level KVM execution remains a Linux merge-train check; portable
device tests do not claim that host evidence.

## Shared model

`seedbox-engine-core::virtio_network::Network` owns the MAC, ingress backlog,
network counters, packet copying and queue completion. Both adapters pass their
RAM base and a dirty-write callback. The KVM adapter retains the ordered journal;
the ARM adapter uses its existing memory tracking. Both use the shared MMIO
transport. The network transport profile retains KVM's invalid-selector clamp to
queue 1, truncated/clamped queue sizes, exact-one ready flag and ignored invalid
notifications. The application transport keeps its stricter register admission.

| Behavior | Application profile (ARM) | Network profile (KVM) |
| --- | --- | --- |
| TX header | Requires 12 bytes and zero first 10 bytes | Opaque header; short/empty TX completes without frame |
| Empty chain | Rejected | Allowed |
| RX limit | 65536-byte frame | 4 MiB minus header |
| Admission | Not-ready/full backlog before size check | Not-ready before size; size before full backlog |
| RX direct path | Enqueue then drain; failed new frame remains queued | Try DMA first; failed new frame is not enqueued |
| TX notification | Also drains pending RX after TX completes | TX only |
| IRQ | Per completed chain, including prefix before error | Live suppression/event threshold at successful batch boundary |
| Features and polling | Existing MAC/VERSION_1 features; no deferred TX or EVENT_IDX | MAC/VERSION_1/EVENT_IDX; optional deferred TX |
| Counters | Existing retained-state projection has none | TX, RX, drops and report-only notification/pickup diagnostics |

The application projection retains the original `kind`, `mac` and `backlog`
fields, and transport projection field order is unchanged. These profiles preserve
existing engine behavior; sharing implementation does not establish cross-engine
replay equivalence.
