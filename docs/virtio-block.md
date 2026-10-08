# Shared block model

The engine core owns block request decoding, disk operations, DMA completion and
statistics. Both adapters supply guest RAM, its physical base, the shared MMIO
transport and a dirty-write callback. KVM callers access the disk and statistics
explicitly through the adapter; ARM retains its existing disk-state projection.

Decoding preserves each existing descriptor contract, then produces one request
used by the common disk-operation and DMA path. This is a mechanical extraction;
policy convergence is a separate behavior change.

| Behavior | Application (ARM) | Block (KVM) |
| --- | --- | --- |
| Queue direction | Any, followed by operation-specific validation | Readable segments followed by writable segments |
| Entirely empty chain | Queue error | Request header/status error |
| Zero-length segment within a nonempty chain | Allowed | Allowed |
| Header | First descriptor, exactly 16 readable bytes | First 16 bytes across readable descriptors |
| Status | Last descriptor, exactly one writable byte | Last byte across writable descriptors |
| Read/write data | Whole sectors with uniform operation direction, otherwise setup error | Disk validates size/range; failures complete with IOERR |
| Disk read/write failure | IOERR; failed reads leave data RAM untouched | Same |
| FLUSH with data descriptors | Setup error | Flush succeeds, data ignored |
| GET_ID | Writable data descriptors, at most 20 bytes copied | Writable room before final status, at most 20 bytes copied |
| Unknown operation | UNSUPP after header/status checks; no extra data-direction check | UNSUPP after queue/header/status checks |
| IRQ | Set ISR after every completed request | Set ISR after a successful notification |
| Statistics and completion log | Disabled | Retain completed operation counts, sector counts, errors and request log |
| Register reset | Retain disk layers, reconstruct transport | Retain disk layers, statistics and uncollected dirty journal |

Readable input is captured before disk/RAM effects, preserving behavior when
header, payload, status or used-ring spans alias. The common DMA path writes
payload spans in order, then the status byte, used entry and used index. Every
actual write is reported through the callback; ARM supplies a no-op and KVM
records the exact sequence for undo.

A malformed later request does not roll back earlier completed requests. Their
disk effects, queue cursors and KVM statistics remain; ARM has already asserted
the ISR, while KVM has not crossed its successful-notification boundary. Clone,
restore and crash operations continue to use the shared disk's stable/volatile
layers and seeded survival policy.
