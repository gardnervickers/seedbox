# Shared console model

The engine core owns the single-port handshake, backlogs, counters, receive,
notification and DMA processing. The ARM and KVM adapters supply the guest RAM
base and dirty-write callback, resolve MMIO effects, and project payloads to their
existing events. The KVM adapter concatenates transmitted chains; ARM preserves
one control event per transmitted chain. Register policy remains in the shared
MMIO transport.

This extraction preserves two existing compatibility profiles, rather than
changing replay behavior while moving code. These differences can be converged
in a separately reviewed behavior change.

| Behavior | Application (ARM) | Console (KVM) |
| --- | --- | --- |
| Queue validation | 4 MiB, any direction; direction checked when reading/writing chain | 64 KiB, uniform direction validated with descriptors |
| Transmit limit | 64 KiB per chain | 64 KiB total per notification |
| Transmit while port closed | Allowed | Error |
| Receive before DRIVER_OK | Allowed | Error |
| Receive while port closed | DMA allowed | Retain backlog until port opens |
| Receive backlog | At most 64 KiB | Same |
| Control backlog admission | Check after whole handshake enqueue | Check before each response enqueue |
| Control dequeue | After DMA and ring completion | Before DMA and ring completion |
| Data receive dequeue | After DMA, ring completion and counter update | Before DMA and ring completion |
| Counters | Checked transmit and receive-DMA counters | Disabled; no new overflow failures |
| Transmit IRQ | Set ISR after each completed chain | Set ISR at successful write boundary |
| Receive IRQ | Set ISR after each completed response or data chain | Set ISR at successful drain boundary |

State changes made before an error remain observable. In particular, ARM receive
counter overflow occurs after payload and used-ring writes but before consumption
of the receive backlog and before setting the ISR. A later transmit-chain failure
retains completed earlier chains on both profiles, but only ARM has already set
their ISR. The model owns these orderings explicitly.

The shared model is cloneable. Reset reconstructs the model through the adapter,
retaining the KVM adapter's uncollected dirty journal. ARM retained state uses the
same explicit JSON fields as before, excluding the constructor-owned policy.
