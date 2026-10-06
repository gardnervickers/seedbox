//! Minimal legacy PC devices. All state changes happen on the world thread at
//! VM exits, using the machine's virtual time; nothing reads host time.

use std::collections::VecDeque;

/// Master 8259A. The slave is accepted but never raises interrupts.
#[derive(Default)]
pub struct Pic {
    imr: u8,
    irr: u8,
    base: u8,
    icw: u8,
    slave_imr: u8,
    slave_icw: u8,
}

impl Pic {
    pub fn new() -> Self {
        Self {
            imr: 0xff,
            base: 0x30,
            slave_imr: 0xff,
            ..Default::default()
        }
    }
    pub fn write(&mut self, port: u16, v: u8) {
        match port {
            0x20 if v & 0x10 != 0 => {
                self.icw = 1;
                self.imr = 0;
            }
            0x21 => match self.icw {
                1 => {
                    self.base = v & 0xf8;
                    self.icw = 2;
                }
                2 => self.icw = 3,
                3 => self.icw = 0,
                _ => self.imr = v,
            },
            0xa0 if v & 0x10 != 0 => self.slave_icw = 1,
            0xa1 => match self.slave_icw {
                1 => self.slave_icw = 2,
                2 => self.slave_icw = 3,
                3 => self.slave_icw = 0,
                _ => self.slave_imr = v,
            },
            _ => {} // OCW2 (EOI) and OCW3: no ISR model needed.
        }
    }
    pub fn read(&self, port: u16) -> u8 {
        match port {
            0x21 => self.imr,
            0xa1 => self.slave_imr,
            _ => 0,
        }
    }
    pub fn raise(&mut self, irq: u8) {
        self.irr |= 1 << irq;
    }
    /// Highest-priority unmasked pending line, if any.
    pub fn pending(&self) -> Option<u8> {
        let p = self.irr & !self.imr;
        (p != 0).then(|| p.trailing_zeros() as u8)
    }
    pub fn take(&mut self, irq: u8) -> u32 {
        self.irr &= !(1 << irq);
        self.base as u32 + irq as u32
    }
    /// Interrupt masks, master | slave << 8.
    pub fn masks(&self) -> u16 {
        self.imr as u16 | (self.slave_imr as u16) << 8
    }
    pub fn set_masks(&mut self, m: u16) {
        self.imr = m as u8;
        self.slave_imr = (m >> 8) as u8;
    }
}

const PIT_HZ: u128 = 1_193_182;

#[derive(Default, Clone)]
struct PitChannel {
    mode: u8,
    access: u8,
    reload: u32,
    start: u64,
    low: Option<u8>,
    latch: Option<u16>,
    read_high: bool,
}

impl PitChannel {
    fn period(&self) -> u32 {
        if self.reload == 0 {
            65536
        } else {
            self.reload
        }
    }
    fn ticks(&self, now: u64) -> u64 {
        ((now.saturating_sub(self.start) as u128 * PIT_HZ) / 1_000_000_000) as u64
    }
    fn count(&self, now: u64) -> u16 {
        let r = self.period() as u64;
        let t = self.ticks(now);
        let c = match self.mode {
            2 | 3 => r - t % r,
            _ => r.saturating_sub(t),
        };
        c as u16
    }
}

/// 8254 timer: channel 0 drives IRQ0, channel 2 serves boot calibration.
#[derive(Default)]
pub struct Pit {
    ch: [PitChannel; 3],
    gate2: bool,
    /// Next channel-0 interrupt time and, for periodic modes, its period.
    pub deadline: Option<u64>,
    period_ns: Option<u64>,
    /// Set when the guest arms a new channel-0 deadline.
    pub fresh: bool,
}

fn ticks_to_ns(ticks: u64) -> u64 {
    ((ticks as u128 * 1_000_000_000 + PIT_HZ - 1) / PIT_HZ) as u64
}

impl Pit {
    /// Start channel 0 counting from its reload value.
    fn arm0(&mut self, now: u64) {
        let ch = &self.ch[0];
        let ns = ticks_to_ns(ch.period() as u64);
        self.deadline = Some(now + ns);
        self.period_ns = matches!(ch.mode, 2 | 3).then_some(ns);
        self.fresh = true;
    }

    /// Load channel 0 with `ticks` in one access (paravirtual timer port);
    /// same effect as writing the low and high count bytes.
    pub fn program(&mut self, ticks: u16, now: u64) {
        let ch = &mut self.ch[0];
        ch.reload = ticks as u32;
        ch.low = None;
        ch.start = now;
        self.arm0(now);
    }

    pub fn write(&mut self, port: u16, v: u8, now: u64) {
        match port {
            0x43 => {
                let c = (v >> 6) as usize;
                if c == 3 {
                    return; // read-back command: unsupported, unused by Linux here
                }
                let access = (v >> 4) & 3;
                if access == 0 {
                    self.ch[c].latch = Some(self.ch[c].count(now));
                    return;
                }
                let mut mode = (v >> 1) & 7;
                if mode > 5 {
                    mode -= 4;
                }
                self.ch[c] = PitChannel {
                    mode,
                    access,
                    start: now,
                    ..Default::default()
                };
                if c == 0 {
                    self.deadline = None;
                    self.period_ns = None;
                }
            }
            0x40..=0x42 => {
                let c = (port - 0x40) as usize;
                let ch = &mut self.ch[c];
                let complete = match ch.access {
                    1 => {
                        ch.reload = v as u32;
                        true
                    }
                    2 => {
                        ch.reload = (v as u32) << 8;
                        true
                    }
                    _ => match ch.low.take() {
                        None => {
                            ch.low = Some(v);
                            false
                        }
                        Some(lo) => {
                            ch.reload = lo as u32 | (v as u32) << 8;
                            true
                        }
                    },
                };
                if complete {
                    ch.start = now;
                    if c == 0 {
                        self.arm0(now);
                    }
                }
            }
            0x61 => {
                let gate = v & 1 != 0;
                if gate && !self.gate2 {
                    self.ch[2].start = now;
                }
                self.gate2 = gate;
            }
            _ => {}
        }
    }
    pub fn read(&mut self, port: u16, now: u64) -> u8 {
        match port {
            0x40..=0x42 => {
                let ch = &mut self.ch[(port - 0x40) as usize];
                let value = ch.latch.unwrap_or_else(|| ch.count(now));
                let byte = match ch.access {
                    1 => value as u8,
                    2 => (value >> 8) as u8,
                    _ => {
                        let b = if ch.read_high {
                            (value >> 8) as u8
                        } else {
                            value as u8
                        };
                        ch.read_high = !ch.read_high;
                        b
                    }
                };
                if ch.access != 3 || !ch.read_high {
                    ch.latch = None;
                }
                byte
            }
            0x61 => {
                let out2 = self.ch[2].ticks(now) >= self.ch[2].period() as u64;
                (self.gate2 as u8) | ((out2 as u8) << 5)
            }
            _ => 0,
        }
    }
    /// Called when virtual time reaches the deadline: returns true if IRQ0 fires.
    pub fn expire(&mut self, now: u64) -> bool {
        match self.deadline {
            Some(d) if d <= now => {
                self.deadline = self.period_ns.map(|p| d + p);
                true
            }
            _ => false,
        }
    }
}

/// 16550A UART with immediate transmit and an input queue.
#[derive(Default)]
pub struct Uart {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    pub rx: VecDeque<u8>,
    thre: bool,
    level: bool,
    pub out: Vec<u8>,
}

impl Uart {
    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }
    pub fn read(&mut self, off: u16) -> u8 {
        match off {
            0 if self.dlab() => self.dll,
            0 => self.rx.pop_front().unwrap_or(0),
            1 if self.dlab() => self.dlm,
            1 => self.ier,
            2 => {
                if self.ier & 1 != 0 && !self.rx.is_empty() {
                    0xc4
                } else if self.ier & 2 != 0 && self.thre {
                    self.thre = false;
                    0xc2
                } else {
                    0xc1
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60 | (!self.rx.is_empty()) as u8,
            6 => {
                if self.mcr & 0x10 != 0 {
                    let m = self.mcr;
                    ((m >> 1) & 1) << 4 | (m & 1) << 5 | ((m >> 2) & 1) << 6 | ((m >> 3) & 1) << 7
                } else {
                    0xb0
                }
            }
            _ => self.scr,
        }
    }
    pub fn write(&mut self, off: u16, v: u8) {
        match off {
            0 if self.dlab() => self.dll = v,
            0 => {
                self.out.push(v);
                self.thre = true;
            }
            1 if self.dlab() => self.dlm = v,
            1 => {
                let enabling = v & 2 != 0 && self.ier & 2 == 0;
                self.ier = v & 0x0f;
                if enabling {
                    self.thre = true;
                }
            }
            3 => self.lcr = v,
            4 => self.mcr = v,
            7 => self.scr = v,
            _ => {}
        }
    }
    /// Rising edge of the interrupt condition since the last call.
    pub fn edge(&mut self) -> bool {
        let level = (self.ier & 1 != 0 && !self.rx.is_empty()) || (self.ier & 2 != 0 && self.thre);
        let rising = level && !self.level;
        self.level = level;
        rising
    }
}

/// Fixed wall clock: 2026-01-01 00:00:00, BCD, 24-hour.
pub fn rtc_read(reg: u8) -> u8 {
    match reg {
        0x0a => 0x26,
        0x0b => 0x02,
        0x07 => 0x01,
        0x08 => 0x01,
        0x09 => 0x26,
        0x32 => 0x20,
        _ => 0,
    }
}
