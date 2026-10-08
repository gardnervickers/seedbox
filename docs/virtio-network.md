# Network device compatibility

The KVM network hardening checkpoint precedes extraction of the shared network
model. It uses the shared bounded split-queue validator while preserving valid
KVM traffic, register behavior, backlog policy, counters and completion order.
The ARM network model is unchanged at this checkpoint.

## KVM validation changes

All queue geometry, ring bounds, descriptor links, DMA spans, flags and direction
are validated before the current request changes RAM or consumes a head. Chains
are bounded to 4 MiB. Invalid geometry can fail even with no available heads:
validation precedes the pending-head check. Non-power-of-two sizes remain stored
by the existing register clamp, but DMA rejects them. Head/next indices, cycles,
available-ring overrun, misalignment, unsupported flags and address overflow
return execution errors instead of unchecked access or unbounded traversal.

Receive validates the entire writable chain, including tails that the old loop
might skip after copying enough payload. A buffer smaller than the 12-byte header
plus frame now fails without truncation or completion. Incoming frames larger
than 4 MiB minus 12 bytes fail before backlog admission, but the existing
not-ready drop check still runs first. A ready full backlog still drops valid-size
frames. Its 256-frame bound is unchanged.

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
backlog. Snapshot cloning retains its own backlog and queue cursors.

## Evidence boundaries

Portable tests compile the actual KVM device source on macOS. They cover bounded
hostile-chain execution, rejected-request cursor preservation, ordered completed
prefixes, frame/chain limits, reset and snapshot ownership. The valid-traffic
differential compares old and hardened production device code, including complete
RAM, queue state, backlog, counters, interrupts, returned frames and dirty order.
Machine-level KVM execution and undo remain Linux merge-train checks; portable
device tests do not claim that host evidence.
