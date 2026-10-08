# Shared virtio MMIO transport

The engine core owns feature selectors, queue-register updates, interrupt status,
status writes, and notification/reset decoding. Device adapters own configuration
bytes, DMA and device effects, and reset of disks, backlogs and statistics. ARM
validates access width and alignment before entering the shared transport.

The initial extraction preserves these existing register policies. They are
compatibility profiles for the implementations being moved, not a requirement
that future device models retain different platform behavior.

| Behavior | Application (ARM net/console/block) | Console (KVM) | Block (KVM) |
| --- | --- | --- | --- |
| Invalid selected queue write | Error | Error | Ignore |
| Queue size write | Nonzero power of two, at most 256 | Same | Truncate to u16; validate on DMA |
| Queue changed while ready | Allowed | Reject except QueueReady | Allowed |
| QueueReady value | 0 or 1 | 0 or 1 | Only 1 means ready |
| QueueReady cursor reset | On both 0 and 1 | Only on 0 | On every write |
| Invalid notification queue | Error | Error, before readiness check | Ignore |
| Notify before DRIVER_OK | Allowed | Error | Allowed |
| FEATURES_OK negotiation | Record status | Clear bit unless both feature words match | Record status |
| DRIVER_OK without FEATURES_OK | Record status | Error after recording status | Record status |
| Interrupt acknowledgment | Clear requested ISR bits | Same | Same |
| Reset | Fresh transport and model, retaining disk | Fresh transport/model, retain dirty journal | Fresh transport, retain disk, statistics and dirty journal |

Block also records FEATURES_OK without enforcing a feature negotiation policy.
The shared transport returns typed notification and reset effects. Adapters keep
all device-specific effect ordering, including dirty RAM journal entries. A reset
clears transport state; the adapter separately performs its existing model reset.

ARM retained state uses an explicit projection with the existing field names and
order: `dev_sel`, `drv_sel`, `features`, `queue_sel`, `queues`, `status`, `isr`.
Internal specifications and compatibility profiles do not change that projection.
Cloning a device includes transport selectors, queues and cursors as well as the
adapter's model data and pending journal.

The KVM network adapter remains on its existing implementation in this stage.
Its move to bounded shared DMA and the later convergence of device policies need
separate review and acceptance evidence.
