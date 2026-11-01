//! ARM9 I/O registers (0x04000000-0x04001FFF and 0x04100000).
//!
//! Registers are stored as bytes. Display timing, interrupt flags, the math
//! coprocessors and the IPC registers get behaviour; everything else keeps
//! what was written, and every register touched is counted for reports.
//! Effects that need memory access (DMA) are returned to the machine.

use std::collections::{BTreeMap, VecDeque};

const IO_SIZE: usize = 0x2000;

pub const DISPSTAT: u32 = 0x0400_0004;
pub const VCOUNT: u32 = 0x0400_0006;
pub const DMA_BASE: u32 = 0x0400_00B0;
pub const DMA_FILL_BASE: u32 = 0x0400_00E0;
pub const TIMER_BASE: u32 = 0x0400_0100;
pub const KEYINPUT: u32 = 0x0400_0130;
pub const IPCSYNC: u32 = 0x0400_0180;
pub const IPCFIFOCNT: u32 = 0x0400_0184;
pub const IPCFIFOSEND: u32 = 0x0400_0188;
pub const IME: u32 = 0x0400_0208;
pub const VRAMCNT: u32 = 0x0400_0240;
pub const IE: u32 = 0x0400_0210;
pub const IF: u32 = 0x0400_0214;
pub const DIVCNT: u32 = 0x0400_0280;
pub const SQRTCNT: u32 = 0x0400_02B0;
pub const POSTFLG: u32 = 0x0400_0300;
pub const IPCFIFORECV: u32 = 0x0410_0000;

pub const IRQ_VBLANK: u32 = 1 << 0;
pub const IRQ_TIMER0: u32 = 1 << 3;
pub const IRQ_IPC_SYNC: u32 = 1 << 16;
pub const IRQ_IPC_RECV: u32 = 1 << 18;

const LINES: u32 = 263;
const VISIBLE_LINES: u32 = 192;
/// ARM9 bus cycles (33.51 MHz) per scanline.
const CYCLES_PER_LINE: u32 = 2130;

pub enum Effect {
    /// A DMA channel was enabled with immediate start timing.
    Dma(usize),
    /// A word was sent to the ARM7 through the IPC FIFO.
    FifoSend(u32),
    /// The ARM9 wrote its IPCSYNC output nibble.
    Sync(u32),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Access {
    pub reads: u64,
    pub writes: u64,
}

pub struct Io {
    regs: Vec<u8>,
    pub line: u32,
    pub frame: u64,
    /// Words from the ARM7 waiting in the receive FIFO.
    pub fifo_recv: VecDeque<u32>,
    pub touched: BTreeMap<u32, Access>,
    timers: [Timer; 4],
}

/// A hardware timer: a 16-bit up-counter that reloads on overflow.
#[derive(Debug, Default, Clone, Copy)]
struct Timer {
    counter: u16,
    reload: u16,
    control: u16,
    /// Cycles not yet worth a tick at the current prescaler.
    remainder: u32,
}

impl Timer {
    fn enabled(&self) -> bool {
        self.control & 0x80 != 0
    }

    fn cascade(&self) -> bool {
        self.control & 0x04 != 0
    }

    fn prescaler(&self) -> u32 {
        [1, 64, 256, 1024][usize::from(self.control & 3)]
    }

    /// Count `ticks`, returning how many times the counter overflowed.
    fn count(&mut self, ticks: u64) -> u64 {
        let period = 0x1_0000 - u64::from(self.reload);
        let position = u64::from(self.counter) + ticks;
        if position < 0x1_0000 {
            self.counter = position as u16;
            return 0;
        }
        let past = position - 0x1_0000;
        self.counter = (u64::from(self.reload) + past % period) as u16;
        1 + past / period
    }
}

impl Io {
    pub fn new() -> Self {
        let mut io = Self {
            regs: vec![0; IO_SIZE],
            line: 0,
            frame: 0,
            fifo_recv: VecDeque::new(),
            touched: BTreeMap::new(),
            timers: [Timer::default(); 4],
        };
        io.set16(KEYINPUT, 0x03FF);
        io.set16(IPCFIFOCNT, 0x0101);
        io.set8(POSTFLG, 1);
        io
    }

    fn offset(address: u32) -> Option<usize> {
        let offset = address.wrapping_sub(0x0400_0000) as usize;
        (offset < IO_SIZE).then_some(offset)
    }

    pub fn get8(&self, address: u32) -> u8 {
        Self::offset(address).map_or(0, |offset| self.regs[offset])
    }

    pub fn set8(&mut self, address: u32, value: u8) {
        if let Some(offset) = Self::offset(address) {
            self.regs[offset] = value;
        }
    }

    pub fn get16(&self, address: u32) -> u16 {
        u16::from(self.get8(address)) | (u16::from(self.get8(address + 1)) << 8)
    }

    pub fn set16(&mut self, address: u32, value: u16) {
        self.set8(address, value as u8);
        self.set8(address + 1, (value >> 8) as u8);
    }

    pub fn get32(&self, address: u32) -> u32 {
        u32::from(self.get16(address)) | (u32::from(self.get16(address + 2)) << 16)
    }

    pub fn set32(&mut self, address: u32, value: u32) {
        self.set16(address, value as u16);
        self.set16(address + 2, (value >> 16) as u16);
    }

    pub fn raise(&mut self, flags: u32) {
        let value = self.get32(IF) | flags;
        self.set32(IF, value);
    }

    /// Interrupts that are flagged, enabled and globally enabled.
    pub fn pending(&self) -> u32 {
        if self.get32(IME) & 1 == 0 {
            return 0;
        }
        self.get32(IF) & self.get32(IE)
    }

    /// Move the display to the given line, setting status and interrupts.
    pub fn set_line(&mut self, line: u32) {
        let previous = self.line;
        self.line = line % LINES;
        let elapsed = (self.line + LINES - previous) % LINES;
        self.advance_timers(elapsed * CYCLES_PER_LINE);
        let mut status = self.get16(DISPSTAT);
        let in_vblank = self.line >= VISIBLE_LINES && self.line != LINES - 1;
        status = (status & !1) | u16::from(in_vblank);
        let target = u32::from(status >> 8) | (u32::from(status & 0x80) << 1);
        status = (status & !4) | if self.line == target { 4 } else { 0 };
        self.set16(DISPSTAT, status);
        self.set16(VCOUNT, self.line as u16);
        if previous < VISIBLE_LINES && self.line >= VISIBLE_LINES {
            self.frame += 1;
            if status & 0x08 != 0 {
                self.raise(IRQ_VBLANK);
            }
        }
    }

    /// Run the timers for `cycles` ARM9 bus cycles (33.5 MHz).
    fn advance_timers(&mut self, cycles: u32) {
        let mut overflows = 0;
        for index in 0..4 {
            let mut timer = self.timers[index];
            if !timer.enabled() {
                overflows = 0;
                continue;
            }
            let ticks = if timer.cascade() && index > 0 {
                overflows
            } else {
                let total = timer.remainder + cycles;
                timer.remainder = total % timer.prescaler();
                u64::from(total / timer.prescaler())
            };
            overflows = timer.count(ticks);
            if overflows > 0 && timer.control & 0x40 != 0 {
                self.raise(IRQ_TIMER0 << index);
            }
            self.timers[index] = timer;
        }
    }

    /// Run the display to the start of the next vertical blank.
    pub fn advance_to_vblank(&mut self) {
        if self.line >= VISIBLE_LINES {
            self.set_line(0);
        }
        self.set_line(VISIBLE_LINES);
    }

    pub fn read(&mut self, address: u32, size: u32) -> u32 {
        let aligned = address & !(size - 1);
        self.touched.entry(aligned).or_default().reads += 1;
        match aligned {
            // Polling the display status lets time move forward.
            DISPSTAT | VCOUNT => {
                self.set_line(self.line + 1);
            }
            IPCFIFORECV => {
                let value = self.fifo_recv.pop_front().unwrap_or(0);
                self.update_fifo_status();
                return value;
            }
            _ if (TIMER_BASE..TIMER_BASE + 16).contains(&aligned) => {
                let timer = self.timers[((aligned - TIMER_BASE) / 4) as usize];
                let value = u32::from(timer.counter) | (u32::from(timer.control) << 16);
                return match (size, aligned & 2) {
                    (4, _) => value,
                    (_, 0) => value & 0xFFFF,
                    _ => value >> 16,
                };
            }
            _ => {}
        }
        match size {
            1 => u32::from(self.get8(address)),
            2 => u32::from(self.get16(aligned)),
            _ => self.get32(aligned),
        }
    }

    pub fn write(&mut self, address: u32, size: u32, value: u32) -> Option<Effect> {
        let aligned = address & !(size - 1);
        self.touched.entry(aligned).or_default().writes += 1;
        if aligned == IF || aligned == IF + 2 && size == 2 {
            // Writing 1 acknowledges an interrupt.
            let shift = (aligned - IF) * 8;
            let flags = self.get32(IF) & !(value << shift);
            self.set32(IF, flags);
            return None;
        }
        if aligned == IPCFIFOSEND {
            return Some(Effect::FifoSend(value));
        }
        if (TIMER_BASE..TIMER_BASE + 16).contains(&aligned) {
            let index = ((aligned - TIMER_BASE) / 4) as usize;
            let (reload, control) = match (size, aligned & 2) {
                (4, _) => (Some(value as u16), Some((value >> 16) as u16)),
                (_, 0) => (Some(value as u16), None),
                _ => (None, Some(value as u16)),
            };
            let timer = &mut self.timers[index];
            if let Some(reload) = reload {
                timer.reload = reload;
            }
            if let Some(control) = control {
                // Starting a timer loads the reload value.
                if control & 0x80 != 0 && !timer.enabled() {
                    timer.counter = timer.reload;
                    timer.remainder = 0;
                }
                timer.control = control & 0xC7;
            }
            return None;
        }
        if aligned == IPCSYNC && size >= 2 || address == IPCSYNC + 1 {
            let value = if address == IPCSYNC + 1 {
                value << 8
            } else {
                value
            };
            let current = self.get16(IPCSYNC);
            let updated = (current & 0x000F) | (value as u16 & 0x6F00);
            self.set16(IPCSYNC, updated);
            return Some(Effect::Sync((u32::from(updated) >> 8) & 0xF));
        }
        if aligned == IPCFIFOCNT {
            let current = self.get16(IPCFIFOCNT);
            let mut updated = (current & 0x4303) | (value as u16 & 0x8404);
            if value & 0x4000 != 0 {
                updated &= !0x4000;
            }
            self.set16(IPCFIFOCNT, updated);
            self.update_fifo_status();
            return None;
        }
        match size {
            1 => self.set8(address, value as u8),
            2 => self.set16(aligned, value as u16),
            _ => self.set32(aligned, value),
        }
        if (0x0400_0000 + 0x280..0x0400_0000 + 0x2A0).contains(&aligned) {
            self.divide();
        } else if (SQRTCNT..SQRTCNT + 0x10).contains(&aligned) {
            self.square_root();
        } else if (DMA_BASE..DMA_BASE + 48).contains(&aligned) {
            let channel = ((aligned - DMA_BASE) / 12) as usize;
            let control = self.get16(DMA_BASE + 12 * channel as u32 + 10);
            // Enabled with start timing "immediately".
            if control & 0x8000 != 0 && (control >> 11) & 7 == 0 {
                return Some(Effect::Dma(channel));
            }
        }
        None
    }

    fn update_fifo_status(&mut self) {
        let mut control = self.get16(IPCFIFOCNT);
        // Send FIFO is always empty: the ARM7 consumes words immediately.
        control = (control & !0x0003) | 0x0001;
        control &= !0x0300;
        control |= if self.fifo_recv.is_empty() { 0x0100 } else { 0 };
        self.set16(IPCFIFOCNT, control);
    }

    /// Queue a word from the ARM7, raising the receive interrupt if enabled.
    pub fn push_from_arm7(&mut self, value: u32) {
        self.fifo_recv.push_back(value);
        self.update_fifo_status();
        if self.get16(IPCFIFOCNT) & 0x0400 != 0 {
            self.raise(IRQ_IPC_RECV);
        }
    }

    /// Set the IPCSYNC input nibble as the ARM7 would.
    pub fn set_sync_input(&mut self, value: u32) {
        let current = self.get16(IPCSYNC);
        self.set16(IPCSYNC, (current & !0xF) | (value as u16 & 0xF));
    }

    fn divide(&mut self) {
        let control = self.get16(DIVCNT);
        let numerator = self.get32(0x0400_0290) as i32 as i64;
        let numerator64 =
            (self.get32(0x0400_0290) as u64 | ((self.get32(0x0400_0294) as u64) << 32)) as i64;
        let denominator = self.get32(0x0400_0298) as i32 as i64;
        let denominator64 =
            (self.get32(0x0400_0298) as u64 | ((self.get32(0x0400_029C) as u64) << 32)) as i64;
        let (n, d) = match control & 3 {
            0 => (numerator, denominator),
            1 => (numerator64, denominator),
            _ => (numerator64, denominator64),
        };
        let (quotient, remainder) = if d == 0 {
            (if n < 0 { 1 } else { -1 }, n)
        } else if n == i64::MIN && d == -1 {
            (n, 0)
        } else {
            (n / d, n % d)
        };
        let zero = if denominator64 == 0 { 0x4000 } else { 0 };
        self.set16(DIVCNT, (control & 3) | zero);
        self.set32(0x0400_02A0, quotient as u32);
        self.set32(0x0400_02A4, (quotient as u64 >> 32) as u32);
        self.set32(0x0400_02A8, remainder as u32);
        self.set32(0x0400_02AC, (remainder as u64 >> 32) as u32);
    }

    fn square_root(&mut self) {
        let control = self.get16(SQRTCNT);
        let low = self.get32(0x0400_02B8) as u64;
        let value = if control & 1 != 0 {
            low | ((self.get32(0x0400_02BC) as u64) << 32)
        } else {
            low
        };
        self.set32(0x0400_02B4, value.isqrt() as u32);
        self.set16(SQRTCNT, control & 1);
    }
}

impl Default for Io {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vblank_raises_interrupt_when_enabled() {
        let mut io = Io::new();
        io.write(DISPSTAT, 2, 0x0008);
        io.advance_to_vblank();
        assert_eq!(io.get32(IF) & IRQ_VBLANK, IRQ_VBLANK);
        assert_eq!(io.frame, 1);
        io.write(IF, 4, IRQ_VBLANK);
        assert_eq!(io.get32(IF), 0);
    }

    #[test]
    fn timer_overflows_raise_interrupts_and_reload() {
        let mut io = Io::new();
        // Timer 1: reload 0xFF00, prescaler 1, interrupt, enabled.
        io.write(TIMER_BASE + 4, 4, 0x00C0_FF00);
        assert_eq!(io.read(TIMER_BASE + 4, 2), 0xFF00);
        io.advance_timers(0x100);
        assert_eq!(io.get32(IF) & (IRQ_TIMER0 << 1), IRQ_TIMER0 << 1);
        assert_eq!(io.read(TIMER_BASE + 4, 2), 0xFF00);
        io.advance_timers(0x10);
        assert_eq!(io.read(TIMER_BASE + 4, 2), 0xFF10);
    }

    #[test]
    fn divider_and_square_root() {
        let mut io = Io::new();
        io.write(DIVCNT, 2, 0);
        io.write(0x0400_0290, 4, (-7_i32) as u32);
        io.write(0x0400_0298, 4, 2);
        assert_eq!(io.get32(0x0400_02A0) as i32, -3);
        assert_eq!(io.get32(0x0400_02A8) as i32, -1);
        io.write(0x0400_02B8, 4, 1_000_000);
        assert_eq!(io.get32(0x0400_02B4), 1000);
    }

    #[test]
    fn dma_with_immediate_timing_is_reported() {
        let mut io = Io::new();
        let effect = io.write(DMA_BASE + 12 + 8, 4, 0x8400_0010);
        assert!(matches!(effect, Some(Effect::Dma(1))));
    }
}
