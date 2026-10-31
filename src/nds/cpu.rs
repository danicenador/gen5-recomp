//! ARM9 CPU state shared with recompiled code, plus the banked registers
//! and program status registers the generated code never touches directly.

use std::ffi::c_void;

/// Must match `Cpu` in `gen5rt.h`.
#[repr(C)]
pub struct Cpu {
    pub r: [u32; 16],
    pub n: u32,
    pub z: u32,
    pub c: u32,
    pub v: u32,
    pub q: u32,
    pub read_pages: *mut *mut u8,
    pub write_pages: *mut *mut u8,
    pub runtime: *mut c_void,
}

pub const MODE_USER: u32 = 0x10;
pub const MODE_FIQ: u32 = 0x11;
pub const MODE_IRQ: u32 = 0x12;
pub const MODE_SVC: u32 = 0x13;
pub const MODE_ABORT: u32 = 0x17;
pub const MODE_UNDEFINED: u32 = 0x1B;
pub const MODE_SYSTEM: u32 = 0x1F;

const PSR_I: u32 = 1 << 7;
const PSR_F: u32 = 1 << 6;

fn bank(mode: u32) -> usize {
    match mode {
        MODE_FIQ => 1,
        MODE_IRQ => 2,
        MODE_SVC => 3,
        MODE_ABORT => 4,
        MODE_UNDEFINED => 5,
        _ => 0,
    }
}

/// Mode, interrupt masks, banked r13/r14 (and FIQ r8-r12) and SPSRs.
pub struct Banks {
    pub mode: u32,
    pub irq_disabled: bool,
    pub fiq_disabled: bool,
    sp_lr: [[u32; 2]; 6],
    spsr: [u32; 6],
    user_r8_r12: [u32; 5],
    fiq_r8_r12: [u32; 5],
}

impl Banks {
    pub fn new(mode: u32) -> Self {
        Self {
            mode,
            irq_disabled: true,
            fiq_disabled: true,
            sp_lr: [[0; 2]; 6],
            spsr: [0; 6],
            user_r8_r12: [0; 5],
            fiq_r8_r12: [0; 5],
        }
    }

    pub fn cpsr(&self, cpu: &Cpu) -> u32 {
        (cpu.n << 31)
            | (cpu.z << 30)
            | (cpu.c << 29)
            | (cpu.v << 28)
            | (cpu.q << 27)
            | if self.irq_disabled { PSR_I } else { 0 }
            | if self.fiq_disabled { PSR_F } else { 0 }
            | self.mode
    }

    pub fn spsr(&self) -> u32 {
        self.spsr[bank(self.mode)]
    }

    pub fn set_spsr(&mut self, value: u32, fields: u32) {
        let slot = &mut self.spsr[bank(self.mode)];
        *slot = merge(*slot, value, fields);
    }

    /// MSR to CPSR: `fields` is the c/x/s/f mask from the instruction.
    pub fn set_cpsr(&mut self, cpu: &mut Cpu, value: u32, fields: u32) {
        if fields & 8 != 0 {
            cpu.n = value >> 31;
            cpu.z = (value >> 30) & 1;
            cpu.c = (value >> 29) & 1;
            cpu.v = (value >> 28) & 1;
            cpu.q = (value >> 27) & 1;
        }
        if fields & 1 != 0 && self.mode != MODE_USER {
            self.irq_disabled = value & PSR_I != 0;
            self.fiq_disabled = value & PSR_F != 0;
            self.switch_mode(cpu, value & 0x1F);
        }
    }

    pub fn switch_mode(&mut self, cpu: &mut Cpu, mode: u32) {
        if mode == self.mode {
            return;
        }
        let (old, new) = (bank(self.mode), bank(mode));
        if old != new {
            self.sp_lr[old] = [cpu.r[13], cpu.r[14]];
            [cpu.r[13], cpu.r[14]] = self.sp_lr[new];
        }
        if (self.mode == MODE_FIQ) != (mode == MODE_FIQ) {
            let (save, load) = if mode == MODE_FIQ {
                (&mut self.user_r8_r12, self.fiq_r8_r12)
            } else {
                (&mut self.fiq_r8_r12, self.user_r8_r12)
            };
            save.copy_from_slice(&cpu.r[8..13]);
            cpu.r[8..13].copy_from_slice(&load);
        }
        self.mode = mode;
    }

    /// Set a mode's banked stack pointer without switching to it.
    pub fn set_banked_sp(&mut self, cpu: &mut Cpu, mode: u32, sp: u32) {
        if bank(mode) == bank(self.mode) {
            cpu.r[13] = sp;
        } else {
            self.sp_lr[bank(mode)][0] = sp;
        }
    }

    /// Enter an exception mode: save CPSR to the new mode's SPSR.
    pub fn enter(&mut self, cpu: &mut Cpu, mode: u32) {
        let cpsr = self.cpsr(cpu);
        self.switch_mode(cpu, mode);
        self.spsr[bank(mode)] = cpsr;
        self.irq_disabled = true;
    }

    /// Return from an exception: restore CPSR from the current SPSR.
    pub fn leave(&mut self, cpu: &mut Cpu) {
        let spsr = self.spsr();
        self.set_cpsr(cpu, spsr, 0xF);
    }
}

fn merge(old: u32, value: u32, fields: u32) -> u32 {
    let mut mask = 0;
    for byte in 0..4 {
        if fields & (1 << byte) != 0 {
            mask |= 0xFF << (8 * byte);
        }
    }
    (old & !mask) | (value & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu() -> Cpu {
        Cpu {
            r: [0; 16],
            n: 0,
            z: 0,
            c: 0,
            v: 0,
            q: 0,
            read_pages: std::ptr::null_mut(),
            write_pages: std::ptr::null_mut(),
            runtime: std::ptr::null_mut(),
        }
    }

    #[test]
    fn mode_switches_bank_stack_pointers() {
        let mut cpu = cpu();
        let mut banks = Banks::new(MODE_SVC);
        cpu.r[13] = 0x100;
        banks.set_cpsr(&mut cpu, 0xD2, 1);
        assert_eq!(banks.mode, MODE_IRQ);
        cpu.r[13] = 0x200;
        banks.set_cpsr(&mut cpu, 0x1F, 1);
        assert!(!banks.irq_disabled);
        cpu.r[13] = 0x300;
        banks.set_cpsr(&mut cpu, 0xD3, 1);
        assert_eq!(cpu.r[13], 0x100);
        banks.set_cpsr(&mut cpu, 0xD2, 1);
        assert_eq!(cpu.r[13], 0x200);
    }

    #[test]
    fn exception_entry_and_return_restore_cpsr() {
        let mut cpu = cpu();
        let mut banks = Banks::new(MODE_SYSTEM);
        banks.irq_disabled = false;
        cpu.z = 1;
        banks.enter(&mut cpu, MODE_IRQ);
        assert!(banks.irq_disabled);
        cpu.z = 0;
        banks.leave(&mut cpu);
        assert_eq!(
            (banks.mode, cpu.z, banks.irq_disabled),
            (MODE_SYSTEM, 1, false)
        );
    }
}
