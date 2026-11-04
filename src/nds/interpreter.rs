//! Fallback interpreter for code without a recompiled function: routines
//! the game copies into RAM before running them, and jumps into the middle
//! of a function. It decodes with `recomp::arm` and follows the same
//! semantics as the generated C. Calls to recompiled functions go back to
//! native code; execution returns when `pc` reaches the caller's `lr`.

use super::cpu::Cpu;
use super::machine::{self, Machine};
use crate::recomp::arm::{
    self, Addressing, BlockMode, Cond, CoprocOp, DpOp, Instruction, MemSize, Offset, Op, Operand,
    PC, SatOp, ShiftAmount, ShiftKind, SignedMulOp,
};

const STEP_LIMIT: u64 = 200_000_000;

fn condition(cpu: &Cpu, cond: Cond) -> bool {
    let (n, z, c, v) = (cpu.n != 0, cpu.z != 0, cpu.c != 0, cpu.v != 0);
    match cond {
        Cond::Eq => z,
        Cond::Ne => !z,
        Cond::Cs => c,
        Cond::Cc => !c,
        Cond::Mi => n,
        Cond::Pl => !n,
        Cond::Vs => v,
        Cond::Vc => !v,
        Cond::Hi => c && !z,
        Cond::Ls => !c || z,
        Cond::Ge => n == v,
        Cond::Lt => n != v,
        Cond::Gt => !z && n == v,
        Cond::Le => z || n != v,
        Cond::Al => true,
    }
}

/// Barrel shifter. `by_register` selects register-shift rules, where an
/// amount of 0 leaves the value and carry unchanged.
fn shift(value: u32, kind: ShiftKind, amount: u32, carry: u32, by_register: bool) -> (u32, u32) {
    if amount == 0 && (by_register || kind == ShiftKind::Lsl) {
        return (value, carry);
    }
    match kind {
        ShiftKind::Lsl if amount < 32 => (value << amount, (value >> (32 - amount)) & 1),
        ShiftKind::Lsl => (0, if amount == 32 { value & 1 } else { 0 }),
        ShiftKind::Lsr if amount < 32 => (value >> amount, (value >> (amount - 1)) & 1),
        ShiftKind::Lsr => (0, if amount == 32 { value >> 31 } else { 0 }),
        ShiftKind::Asr if amount < 32 => (
            ((value as i32) >> amount) as u32,
            (value >> (amount - 1)) & 1,
        ),
        ShiftKind::Asr => (((value as i32) >> 31) as u32, value >> 31),
        ShiftKind::Ror => {
            let amount = amount & 31;
            if amount == 0 {
                (value, value >> 31)
            } else {
                (value.rotate_right(amount), (value >> (amount - 1)) & 1)
            }
        }
        ShiftKind::Rrx => ((carry << 31) | (value >> 1), value & 1),
    }
}

fn add_flags(cpu: &mut Cpu, a: u32, b: u32, carry_in: u32) -> u32 {
    let wide = u64::from(a) + u64::from(b) + u64::from(carry_in);
    let result = wide as u32;
    cpu.c = (wide >> 32) as u32;
    cpu.v = ((!(a ^ b)) & (a ^ result)) >> 31;
    result
}

fn set_nz(cpu: &mut Cpu, value: u32) {
    cpu.n = value >> 31;
    cpu.z = u32::from(value == 0);
}

fn saturate(cpu: &mut Cpu, value: i64) -> i32 {
    if value > i64::from(i32::MAX) {
        cpu.q = 1;
        i32::MAX
    } else if value < i64::from(i32::MIN) {
        cpu.q = 1;
        i32::MIN
    } else {
        value as i32
    }
}

fn half(value: u32, top: bool) -> i32 {
    if top {
        i32::from((value >> 16) as i16)
    } else {
        i32::from(value as i16)
    }
}

/// Where execution goes after an instruction.
enum Next {
    Continue,
    Jump(u32),
}

/// Interpret from `target` (bit 0 selects Thumb) until the code returns to
/// the `lr` it was entered with.
pub fn run(cpu_ptr: *mut Cpu, target: u32) {
    let return_address = unsafe { (*cpu_ptr).r[14] } & !1;
    let mut pc = target & !1;
    let mut thumb = target & 1 != 0;
    let mut steps = 0_u64;
    loop {
        steps += 1;
        if steps > STEP_LIMIT {
            unsafe { machine::get(cpu_ptr) }.stop(&format!(
                "interpreter step limit at 0x{pc:08X} (entered at 0x{target:08X})"
            ));
        }
        let ins = {
            let machine = unsafe { machine::get(cpu_ptr) };
            // Interpreted code is often a loop waiting on the ARM7.
            if steps.is_multiple_of(256) {
                machine.arm7_tick();
                machine.tick();
            }
            fetch(machine, pc, thumb)
        };
        let cpu = unsafe { &mut *cpu_ptr };
        trace(cpu_ptr, &ins);
        let next = if condition(cpu, ins.cond) {
            execute(cpu_ptr, &ins)
        } else {
            Next::Continue
        };
        match next {
            Next::Continue => pc = ins.end(),
            Next::Jump(destination) => {
                thumb = destination & 1 != 0;
                pc = destination & !1;
                if pc == return_address {
                    return;
                }
            }
        }
    }
}

/// GEN5_INTERP_TRACE=frame:path logs interpreted instructions, with the
/// registers before each one, from that frame on (at most a million lines).
fn trace(cpu_ptr: *mut Cpu, ins: &Instruction) {
    use std::io::Write;
    use std::sync::Mutex;
    static TRACE: std::sync::OnceLock<Option<(u64, Mutex<(std::io::BufWriter<std::fs::File>, u32)>)>> =
        std::sync::OnceLock::new();
    let Some((from, out)) = TRACE.get_or_init(|| {
        let spec = std::env::var("GEN5_INTERP_TRACE").ok()?;
        let (frame, path) = spec.split_once(':')?;
        let file = std::fs::File::create(path).ok()?;
        Some((frame.parse().ok()?, Mutex::new((std::io::BufWriter::new(file), 0))))
    }) else {
        return;
    };
    if unsafe { machine::get(cpu_ptr) }.frame() < *from {
        return;
    }
    let mut out = out.lock().unwrap();
    if out.1 >= 1_000_000 {
        return;
    }
    out.1 += 1;
    let cpu = unsafe { &*cpu_ptr };
    let r = &cpu.r;
    let _ = writeln!(
        out.0,
        "{:08X} {:40} r0={:08X} r1={:08X} r2={:08X} r3={:08X} r4={:08X} r12={:08X} sp={:08X} lr={:08X}",
        ins.address, ins.to_string(), r[0], r[1], r[2], r[3], r[4], r[12], r[13], r[14]
    );
    let _ = out.0.flush();
}

fn fetch(machine: &mut Machine, pc: u32, thumb: bool) -> Instruction {
    if thumb {
        let half = machine.read16(pc) as u16;
        let next = machine.read16(pc + 2) as u16;
        arm::decode_thumb(pc, half, Some(next))
    } else {
        arm::decode_arm(pc, machine.read32(pc))
    }
}

fn read_reg(cpu: &Cpu, ins: &Instruction, reg: arm::Reg) -> u32 {
    if reg == PC {
        ins.pc_value()
    } else {
        cpu.r[reg.0 as usize]
    }
}

/// A call from interpreted code: native function if there is one,
/// otherwise interpret the callee.
fn call(cpu: *mut Cpu, target: u32, link: u32) {
    unsafe { (*cpu).r[14] = link };
    machine::call_any(cpu, target);
}

fn execute(cpu_ptr: *mut Cpu, ins: &Instruction) -> Next {
    let cpu = unsafe { &mut *cpu_ptr };
    let return_link = ins.end() | u32::from(ins.thumb);
    let state = |value: u32| value | u32::from(ins.thumb);
    match ins.op {
        Op::DataProc {
            op,
            set_flags,
            rd,
            rn,
            operand,
        } => {
            let (b, shifter_carry) = match operand {
                Operand::Imm { value, rotated } => {
                    (value, if rotated { value >> 31 } else { cpu.c })
                }
                Operand::Reg { rm, kind, amount } => {
                    let value = read_reg(cpu, ins, rm);
                    match amount {
                        ShiftAmount::Imm(amount) => {
                            shift(value, kind, u32::from(amount), cpu.c, false)
                        }
                        ShiftAmount::Reg(rs) => {
                            shift(value, kind, read_reg(cpu, ins, rs) & 0xFF, cpu.c, true)
                        }
                    }
                }
            };
            let a = read_reg(cpu, ins, rn);
            let c = cpu.c;
            let (result, logical) = match op {
                DpOp::And | DpOp::Tst => (a & b, true),
                DpOp::Eor | DpOp::Teq => (a ^ b, true),
                DpOp::Orr => (a | b, true),
                DpOp::Bic => (a & !b, true),
                DpOp::Mov => (b, true),
                DpOp::Mvn => (!b, true),
                _ if set_flags => (
                    match op {
                        DpOp::Add | DpOp::Cmn => add_flags(cpu, a, b, 0),
                        DpOp::Adc => add_flags(cpu, a, b, c),
                        DpOp::Sub | DpOp::Cmp => add_flags(cpu, a, !b, 1),
                        DpOp::Sbc => add_flags(cpu, a, !b, c),
                        DpOp::Rsb => add_flags(cpu, b, !a, 1),
                        _ => add_flags(cpu, b, !a, c),
                    },
                    false,
                ),
                DpOp::Add | DpOp::Cmn => (a.wrapping_add(b), false),
                DpOp::Adc => (a.wrapping_add(b).wrapping_add(c), false),
                DpOp::Sub | DpOp::Cmp => (a.wrapping_sub(b), false),
                DpOp::Sbc => (a.wrapping_sub(b).wrapping_sub(1 - c), false),
                DpOp::Rsb => (b.wrapping_sub(a), false),
                _ => (b.wrapping_sub(a).wrapping_sub(1 - c), false),
            };
            if set_flags && rd != PC {
                set_nz(cpu, result);
                if logical {
                    cpu.c = shifter_carry;
                }
            }
            if op.is_compare() {
                return Next::Continue;
            }
            if rd == PC {
                if set_flags {
                    let machine = unsafe { machine::get(cpu_ptr) };
                    machine.banks.leave(unsafe { &mut *cpu_ptr });
                    return Next::Jump(result);
                }
                // Thumb `add pc`/`mov pc` stay in Thumb; ARM writes stay ARM.
                return Next::Jump(state(result & !1));
            }
            cpu.r[rd.0 as usize] = result;
            Next::Continue
        }
        Op::Multiply {
            accumulate,
            set_flags,
            rd,
            rm,
            rs,
            rn,
        } => {
            let mut result = cpu.r[rm.0 as usize].wrapping_mul(cpu.r[rs.0 as usize]);
            if accumulate {
                result = result.wrapping_add(cpu.r[rn.0 as usize]);
            }
            cpu.r[rd.0 as usize] = result;
            if set_flags {
                set_nz(cpu, result);
            }
            Next::Continue
        }
        Op::MultiplyLong {
            signed,
            accumulate,
            set_flags,
            rd_lo,
            rd_hi,
            rm,
            rs,
        } => {
            let (m, s) = (cpu.r[rm.0 as usize], cpu.r[rs.0 as usize]);
            let mut wide = if signed {
                (i64::from(m as i32) * i64::from(s as i32)) as u64
            } else {
                u64::from(m) * u64::from(s)
            };
            if accumulate {
                wide = wide.wrapping_add(
                    (u64::from(cpu.r[rd_hi.0 as usize]) << 32) | u64::from(cpu.r[rd_lo.0 as usize]),
                );
            }
            cpu.r[rd_lo.0 as usize] = wide as u32;
            cpu.r[rd_hi.0 as usize] = (wide >> 32) as u32;
            if set_flags {
                cpu.n = (wide >> 63) as u32;
                cpu.z = u32::from(wide == 0);
            }
            Next::Continue
        }
        Op::SignedMultiply {
            op,
            x,
            y,
            rd,
            rn,
            rs,
            rm,
        } => {
            let (m, s, n) = (
                cpu.r[rm.0 as usize],
                cpu.r[rs.0 as usize],
                cpu.r[rn.0 as usize],
            );
            match op {
                SignedMulOp::Smul => cpu.r[rd.0 as usize] = (half(m, x) * half(s, y)) as u32,
                SignedMulOp::Smla => {
                    let sum = i64::from(half(m, x) * half(s, y)) + i64::from(n as i32);
                    if sum != i64::from(sum as i32) {
                        cpu.q = 1;
                    }
                    cpu.r[rd.0 as usize] = sum as u32;
                }
                SignedMulOp::SmlawSmulw => {
                    let product = ((i64::from(m as i32) * i64::from(half(s, y))) >> 16) as i32;
                    if x {
                        cpu.r[rd.0 as usize] = product as u32;
                    } else {
                        let sum = i64::from(product) + i64::from(n as i32);
                        if sum != i64::from(sum as i32) {
                            cpu.q = 1;
                        }
                        cpu.r[rd.0 as usize] = sum as u32;
                    }
                }
                SignedMulOp::Smlal => {
                    let accumulator =
                        (u64::from(cpu.r[rd.0 as usize]) << 32) | u64::from(cpu.r[rn.0 as usize]);
                    let sum = accumulator.wrapping_add(i64::from(half(m, x) * half(s, y)) as u64);
                    cpu.r[rn.0 as usize] = sum as u32;
                    cpu.r[rd.0 as usize] = (sum >> 32) as u32;
                }
            }
            Next::Continue
        }
        Op::Clz { rd, rm } => {
            cpu.r[rd.0 as usize] = cpu.r[rm.0 as usize].leading_zeros();
            Next::Continue
        }
        Op::Saturating { op, rd, rm, rn } => {
            let m = i64::from(cpu.r[rm.0 as usize] as i32);
            let n = i64::from(cpu.r[rn.0 as usize] as i32);
            let result = match op {
                SatOp::Qadd => saturate(cpu, m + n),
                SatOp::Qsub => saturate(cpu, m - n),
                SatOp::Qdadd => {
                    let doubled = i64::from(saturate(cpu, 2 * n));
                    saturate(cpu, m + doubled)
                }
                SatOp::Qdsub => {
                    let doubled = i64::from(saturate(cpu, 2 * n));
                    saturate(cpu, m - doubled)
                }
            };
            cpu.r[rd.0 as usize] = result as u32;
            Next::Continue
        }
        Op::Memory {
            load,
            size,
            rt,
            rn,
            offset,
            addressing,
            ..
        } => {
            let base = if rn == PC && ins.thumb {
                ins.pc_value() & !3
            } else {
                read_reg(cpu, ins, rn)
            };
            let (amount, subtract) = match offset {
                Offset::Imm(value) => (value.unsigned_abs(), value < 0),
                Offset::Reg {
                    rm,
                    kind,
                    amount,
                    subtract,
                } => (
                    shift(cpu.r[rm.0 as usize], kind, u32::from(amount), cpu.c, false).0,
                    subtract,
                ),
            };
            let moved = if subtract {
                base.wrapping_sub(amount)
            } else {
                base.wrapping_add(amount)
            };
            let address = if addressing == Addressing::PostIndex {
                base
            } else {
                moved
            };
            let machine = unsafe { machine::get(cpu_ptr) };
            let cpu = unsafe { &mut *cpu_ptr };
            if load {
                let value = match size {
                    MemSize::Word => machine.read32(address).rotate_right((address & 3) * 8),
                    MemSize::Byte => machine.read8(address),
                    MemSize::Half => machine.read16(address),
                    MemSize::SignedByte => machine.read8(address) as u8 as i8 as i32 as u32,
                    MemSize::SignedHalf => machine.read16(address) as u16 as i16 as i32 as u32,
                    MemSize::Double => machine.read32(address),
                };
                let second = (size == MemSize::Double).then(|| machine.read32(address + 4));
                if addressing != Addressing::Offset {
                    cpu.r[rn.0 as usize] = moved;
                }
                if rt == PC {
                    return Next::Jump(value);
                }
                cpu.r[rt.0 as usize] = value;
                if let Some(second) = second {
                    cpu.r[rt.0 as usize + 1] = second;
                }
            } else {
                let value = if rt == PC {
                    ins.pc_value() + 4
                } else {
                    cpu.r[rt.0 as usize]
                };
                match size {
                    MemSize::Word => machine.write32(address, value),
                    MemSize::Byte => machine.write8(address, value),
                    MemSize::Half => machine.write16(address, value),
                    MemSize::Double => {
                        machine.write32(address, value);
                        machine.write32(address + 4, cpu.r[rt.0 as usize + 1]);
                    }
                    MemSize::SignedByte | MemSize::SignedHalf => {}
                }
                if addressing != Addressing::Offset {
                    cpu.r[rn.0 as usize] = moved;
                }
            }
            Next::Continue
        }
        Op::Multiple {
            load,
            rn,
            regs,
            mode,
            writeback,
            user,
        } => {
            let base = cpu.r[rn.0 as usize];
            let bytes = 4 * regs.count_ones();
            let start = match mode {
                BlockMode::Ia => base,
                BlockMode::Ib => base + 4,
                BlockMode::Da => base.wrapping_sub(bytes).wrapping_add(4),
                BlockMode::Db => base.wrapping_sub(bytes),
            };
            let end = match mode {
                BlockMode::Ia | BlockMode::Ib => base.wrapping_add(bytes),
                BlockMode::Da | BlockMode::Db => base.wrapping_sub(bytes),
            };
            let machine = unsafe { machine::get(cpu_ptr) };
            let cpu = unsafe { &mut *cpu_ptr };
            let mut address = start;
            let mut values = [0_u32; 16];
            for reg in (0..16).filter(|reg| regs & (1 << reg) != 0) {
                if load {
                    values[reg] = machine.read32(address);
                } else {
                    let value = if reg == 15 {
                        ins.pc_value() + 4
                    } else if reg == rn.0 as usize {
                        base
                    } else {
                        cpu.r[reg]
                    };
                    machine.write32(address, value);
                }
                address = address.wrapping_add(4);
            }
            if writeback {
                cpu.r[rn.0 as usize] = end;
            }
            if load {
                for reg in (0..15).filter(|reg| regs & (1 << reg) != 0) {
                    cpu.r[reg] = values[reg];
                }
                if regs & (1 << 15) != 0 {
                    if user {
                        machine.banks.leave(cpu);
                    }
                    return Next::Jump(values[15]);
                }
            }
            Next::Continue
        }
        Op::Swap { byte, rd, rm, rn } => {
            let machine = unsafe { machine::get(cpu_ptr) };
            let cpu = unsafe { &mut *cpu_ptr };
            let address = cpu.r[rn.0 as usize];
            let value = cpu.r[rm.0 as usize];
            let old = if byte {
                let old = machine.read8(address);
                machine.write8(address, value);
                old
            } else {
                let old = machine.read32(address).rotate_right((address & 3) * 8);
                machine.write32(address, value);
                old
            };
            cpu.r[rd.0 as usize] = old;
            Next::Continue
        }
        Op::Branch {
            target,
            link,
            exchange,
        } => {
            let destination = if ins.thumb != exchange {
                target | 1
            } else {
                target
            };
            if link {
                call(cpu_ptr, destination, return_link);
                Next::Continue
            } else {
                Next::Jump(destination)
            }
        }
        Op::BranchExchange { rm, link } => {
            let destination = if rm == PC {
                ins.pc_value() & !3
            } else {
                cpu.r[rm.0 as usize]
            };
            if link {
                call(cpu_ptr, destination, return_link);
                Next::Continue
            } else {
                Next::Jump(destination)
            }
        }
        Op::Mrs { rd, spsr } => {
            cpu.r[rd.0 as usize] = machine::host_read_psr(cpu_ptr, u32::from(spsr));
            Next::Continue
        }
        Op::Msr {
            spsr,
            fields,
            operand,
        } => {
            let value = match operand {
                Operand::Imm { value, .. } => value,
                Operand::Reg { rm, .. } => cpu.r[rm.0 as usize],
            };
            machine::host_write_psr(cpu_ptr, u32::from(spsr), u32::from(fields), value);
            Next::Continue
        }
        Op::Coprocessor {
            op: CoprocOp::Mcr,
            cp,
            opc1,
            rd,
            crn,
            crm,
            opc2,
        } => {
            let value = read_reg(cpu, ins, rd);
            machine::host_mcr(
                cpu_ptr,
                u32::from(cp),
                u32::from(opc1),
                u32::from(crn),
                u32::from(crm),
                u32::from(opc2),
                value,
            );
            Next::Continue
        }
        Op::Coprocessor {
            op: CoprocOp::Mrc,
            cp,
            opc1,
            rd,
            crn,
            crm,
            opc2,
        } => {
            let value = machine::host_mrc(
                cpu_ptr,
                u32::from(cp),
                u32::from(opc1),
                u32::from(crn),
                u32::from(crm),
                u32::from(opc2),
            );
            if rd != PC {
                cpu.r[rd.0 as usize] = value;
            }
            Next::Continue
        }
        Op::Swi { comment } => {
            machine::host_swi(cpu_ptr, comment, u32::from(ins.thumb));
            Next::Continue
        }
        Op::Preload { .. } => Next::Continue,
        Op::Coprocessor { .. } | Op::Breakpoint { .. } | Op::Undefined => {
            machine::host_undefined(cpu_ptr, ins.address, ins.raw);
            Next::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifter_matches_arm_rules() {
        assert_eq!(shift(0x8000_0000, ShiftKind::Lsr, 32, 0, false), (0, 1));
        assert_eq!(shift(1, ShiftKind::Lsl, 32, 0, true), (0, 1));
        assert_eq!(shift(5, ShiftKind::Lsl, 0, 1, true), (5, 1));
        assert_eq!(
            shift(0x8000_0001, ShiftKind::Ror, 32, 0, true),
            (0x8000_0001, 1)
        );
        assert_eq!(shift(3, ShiftKind::Rrx, 1, 1, false), (0x8000_0001, 1));
    }
}
