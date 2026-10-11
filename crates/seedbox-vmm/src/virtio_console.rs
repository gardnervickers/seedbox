//! One named non-console virtio-console port over modern virtio-mmio.
//! Shared model owns handshake, queue processing and exact DMA journal ordering.
use seedbox_engine_core::virtio_console::{Console, Policy as ConsolePolicy};
use seedbox_engine_core::virtio_mmio::{Effect, Policy as MmioPolicy, Spec, Transport};
#[derive(Default)]
pub struct WriteOutcome {
    pub irq: bool,
    pub bytes: Vec<u8>,
}
#[derive(Default, Debug, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub accepted: bool,
    pub irq: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(into = "Snapshot", try_from = "Snapshot")]
pub struct VirtioConsole {
    transport: Transport,
    model: Console,
    /// Every payload and used-ring write must be incorporated into undo's RAM journal.
    pub dirty: Vec<(usize, usize)>,
}
impl Default for VirtioConsole {
    fn default() -> Self {
        Self::new()
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    transport: seedbox_engine_core::virtio_mmio::Snapshot,
    model: seedbox_engine_core::virtio_console::Snapshot,
    dirty: Vec<(usize, usize)>,
}
impl From<VirtioConsole> for Snapshot {
    fn from(device: VirtioConsole) -> Self {
        Self {
            transport: device.transport.snapshot(),
            model: device.model.snapshot(),
            dirty: device.dirty,
        }
    }
}
impl TryFrom<Snapshot> for VirtioConsole {
    type Error = String;
    fn try_from(snapshot: Snapshot) -> Result<Self, Self::Error> {
        let device = Self::new();
        Ok(Self {
            transport: device.transport.restore_snapshot(snapshot.transport)?,
            model: device.model.restore_snapshot(snapshot.model)?,
            dirty: snapshot.dirty,
        })
    }
}
impl VirtioConsole {
    pub fn new() -> Self {
        Self {
            transport: Transport::new(
                Spec {
                    device_id: 3,
                    features: [2, 1],
                },
                4,
                MmioPolicy::Console,
            ),
            model: Console::new(ConsolePolicy::Console),
            dirty: Vec::new(),
        }
    }
    pub fn pending_rx_bytes(&self) -> usize {
        self.model.pending_rx_bytes()
    }
    pub fn read(&mut self, off: u64) -> u32 {
        if off == 0x104 {
            1
        } else {
            self.transport.read(off)
        }
    }
    pub fn receive(&mut self, m: &mut [u8], bytes: &[u8]) -> Result<ReceiveOutcome, String> {
        let dirty = &mut self.dirty;
        let irq = self
            .model
            .receive(&mut self.transport, m, 0, bytes, |a, n| dirty.push((a, n)))?;
        Ok(ReceiveOutcome {
            accepted: true,
            irq,
        })
    }
    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<WriteOutcome, String> {
        match self.transport.write(off, v)? {
            Effect::Notify(index) => {
                let dirty = &mut self.dirty;
                let out = self
                    .model
                    .notify(&mut self.transport, m, 0, index, |a, n| dirty.push((a, n)))?;
                Ok(WriteOutcome {
                    irq: out.irq,
                    bytes: out.payloads.into_iter().flatten().collect(),
                })
            }
            Effect::Reset => {
                self.model = Console::new(ConsolePolicy::Console);
                Ok(WriteOutcome::default())
            }
            Effect::None => Ok(WriteOutcome::default()),
        }
    }
}
