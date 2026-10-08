use super::{Effect, MmioDevice};
use crate::Result;
/// Synchronous transmit, permanently empty receive FIFO, level TX interrupt.
#[derive(Default)]
pub struct Pl011 {
    pub console: Vec<u8>,
    regs: [u32; 16],
}
impl Pl011 {
    pub fn state_digest(&self) -> String {
        let bytes: Vec<u8> = self.regs.iter().flat_map(|reg| reg.to_le_bytes()).collect();
        crate::witness::hash(&bytes)
    }
    pub fn irq_level(&self) -> bool {
        self.regs[0x38 / 4] & 0x20 != 0 && self.regs[0x30 / 4] & 0x101 == 0x101
    }
}
impl MmioDevice for Pl011 {
    fn read(&mut self, offset: u64, len: u8) -> Result<u64> {
        if !matches!(len, 1 | 2 | 4) || !offset.is_multiple_of(4) {
            return Err("invalid PL011 access".into());
        }
        Ok(match offset {
            0 | 4 => 0,
            0x18 => 0x90, // TXFE | RXFE
            0x24..=0x38 => self.regs[(offset / 4) as usize] as u64,
            0x3c => 0x20, // empty transmit FIFO: TX raw interrupt
            0x40 => {
                if self.irq_level() {
                    0x20
                } else {
                    0
                }
            }
            0x44 | 0x48 => 0,
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xfec => 0,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => return Err(format!("unknown PL011 read {offset:#x}")),
        })
    }
    fn write(&mut self, offset: u64, len: u8, value: u64) -> Result<Effect> {
        if !matches!(len, 1 | 2 | 4) || !offset.is_multiple_of(4) {
            return Err("invalid PL011 access".into());
        }
        match offset {
            0 => self.console.push(value as u8),
            4 | 0x44 | 0x48 => (),
            0x24..=0x38 => self.regs[(offset / 4) as usize] = value as u32,
            _ => return Err(format!("unknown PL011 write {offset:#x}")),
        }
        Ok(Effect::None)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fifo_ids_and_console() {
        let mut p = Pl011::default();
        assert_eq!(p.read(0x18, 4).unwrap(), 0x90);
        assert_eq!(p.read(0xfe0, 4).unwrap(), 0x11);
        p.write(0, 4, 0x141).unwrap();
        p.write(0, 1, 10).unwrap();
        assert_eq!(p.console, b"A\n");
        p.write(0x30, 4, 0x301).unwrap();
        assert_eq!(p.read(0x30, 4).unwrap(), 0x301);
        p.write(0x38, 4, 0x20).unwrap();
        assert!(p.irq_level());
        assert_eq!(p.read(0x40, 4).unwrap(), 0x20);
        p.write(0x38, 4, 0).unwrap();
        assert!(!p.irq_level());
        assert!(p.read(1, 4).is_err());
        assert!(p.write(0, 8, 0).is_err());
    }
}
