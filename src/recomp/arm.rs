//! ARMv5TE instruction decoder for the ARM946E-S (ARM and Thumb states).
//!
//! Instructions decode into one structured form shared by both states, so the
//! analysis and the code generator handle a single representation. Thumb
//! instructions are expressed as their ARM equivalents (`lsls r0, r1` becomes
//! `movs r0, r0, lsl r1`), with these conventions for reads of `pc`:
//!
//! - ARM: `pc` reads as the instruction address + 8.
//! - Thumb: `pc` reads as the instruction address + 4. PC-relative loads
//!   (`ldr rd, [pc, #imm]`) use that value with bit 1 cleared.
//! - Thumb `add rd, pc, #imm` (ADR) is decoded as `mov rd, #address`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reg(pub u8);

pub const SP: Reg = Reg(13);
pub const LR: Reg = Reg(14);
pub const PC: Reg = Reg(15);

impl fmt::Display for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            13 => f.write_str("sp"),
            14 => f.write_str("lr"),
            15 => f.write_str("pc"),
            n => write!(f, "r{n}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Cs,
    Cc,
    Mi,
    Pl,
    Vs,
    Vc,
    Hi,
    Ls,
    Ge,
    Lt,
    Gt,
    Le,
    Al,
}

impl Cond {
    fn from_bits(bits: u32) -> Self {
        const ALL: [Cond; 15] = [
            Cond::Eq,
            Cond::Ne,
            Cond::Cs,
            Cond::Cc,
            Cond::Mi,
            Cond::Pl,
            Cond::Vs,
            Cond::Vc,
            Cond::Hi,
            Cond::Ls,
            Cond::Ge,
            Cond::Lt,
            Cond::Gt,
            Cond::Le,
            Cond::Al,
        ];
        ALL[(bits as usize).min(14)]
    }

    fn suffix(self) -> &'static str {
        match self {
            Cond::Eq => "eq",
            Cond::Ne => "ne",
            Cond::Cs => "cs",
            Cond::Cc => "cc",
            Cond::Mi => "mi",
            Cond::Pl => "pl",
            Cond::Vs => "vs",
            Cond::Vc => "vc",
            Cond::Hi => "hi",
            Cond::Ls => "ls",
            Cond::Ge => "ge",
            Cond::Lt => "lt",
            Cond::Gt => "gt",
            Cond::Le => "le",
            Cond::Al => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftKind {
    Lsl,
    Lsr,
    Asr,
    Ror,
    Rrx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftAmount {
    /// 0-32. `lsr #32` and `asr #32` are encoded as 0 in ARM and normalized here.
    Imm(u8),
    Reg(Reg),
}

/// The flexible second operand of a data-processing instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// `rotated` is set when the ARM encoding used a non-zero rotation; then a
    /// flag-setting logical instruction sets C from bit 31 of `value`.
    Imm { value: u32, rotated: bool },
    Reg {
        rm: Reg,
        kind: ShiftKind,
        amount: ShiftAmount,
    },
}

impl Operand {
    fn reg(rm: Reg) -> Self {
        Operand::Reg {
            rm,
            kind: ShiftKind::Lsl,
            amount: ShiftAmount::Imm(0),
        }
    }

    fn imm(value: u32) -> Self {
        Operand::Imm {
            value,
            rotated: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpOp {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Rsc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
}

impl DpOp {
    fn from_bits(bits: u32) -> Self {
        const ALL: [DpOp; 16] = [
            DpOp::And,
            DpOp::Eor,
            DpOp::Sub,
            DpOp::Rsb,
            DpOp::Add,
            DpOp::Adc,
            DpOp::Sbc,
            DpOp::Rsc,
            DpOp::Tst,
            DpOp::Teq,
            DpOp::Cmp,
            DpOp::Cmn,
            DpOp::Orr,
            DpOp::Mov,
            DpOp::Bic,
            DpOp::Mvn,
        ];
        ALL[(bits & 0xF) as usize]
    }

    /// Compare/test instructions only set flags and have no destination.
    pub fn is_compare(self) -> bool {
        matches!(self, DpOp::Tst | DpOp::Teq | DpOp::Cmp | DpOp::Cmn)
    }

    /// Move instructions ignore `rn`.
    pub fn is_move(self) -> bool {
        matches!(self, DpOp::Mov | DpOp::Mvn)
    }

    fn name(self) -> &'static str {
        match self {
            DpOp::And => "and",
            DpOp::Eor => "eor",
            DpOp::Sub => "sub",
            DpOp::Rsb => "rsb",
            DpOp::Add => "add",
            DpOp::Adc => "adc",
            DpOp::Sbc => "sbc",
            DpOp::Rsc => "rsc",
            DpOp::Tst => "tst",
            DpOp::Teq => "teq",
            DpOp::Cmp => "cmp",
            DpOp::Cmn => "cmn",
            DpOp::Orr => "orr",
            DpOp::Mov => "mov",
            DpOp::Bic => "bic",
            DpOp::Mvn => "mvn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemSize {
    Word,
    Byte,
    Half,
    SignedByte,
    SignedHalf,
    /// LDRD/STRD: `rt` and `rt + 1`.
    Double,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offset {
    Imm(i32),
    Reg {
        rm: Reg,
        kind: ShiftKind,
        amount: u8,
        subtract: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// `[rn, offset]`
    Offset,
    /// `[rn, offset]!`
    PreIndex,
    /// `[rn], offset`
    PostIndex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    Ia,
    Ib,
    Da,
    Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatOp {
    Qadd,
    Qsub,
    Qdadd,
    Qdsub,
}

/// The ARMv5TE DSP multiplies, selected by bits 22-21 of the encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedMulOp {
    /// SMLA<x><y>
    Smla,
    /// SMLAW<y> (x false) or SMULW<y> (x true)
    SmlawSmulw,
    /// SMLAL<x><y>
    Smlal,
    /// SMUL<x><y>
    Smul,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoprocOp {
    Mcr,
    Mrc,
    Cdp,
    Ldc,
    Stc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    DataProc {
        op: DpOp,
        set_flags: bool,
        rd: Reg,
        rn: Reg,
        operand: Operand,
    },
    Multiply {
        accumulate: bool,
        set_flags: bool,
        rd: Reg,
        rm: Reg,
        rs: Reg,
        rn: Reg,
    },
    MultiplyLong {
        signed: bool,
        accumulate: bool,
        set_flags: bool,
        rd_lo: Reg,
        rd_hi: Reg,
        rm: Reg,
        rs: Reg,
    },
    SignedMultiply {
        op: SignedMulOp,
        x: bool,
        y: bool,
        rd: Reg,
        rn: Reg,
        rs: Reg,
        rm: Reg,
    },
    Clz {
        rd: Reg,
        rm: Reg,
    },
    Saturating {
        op: SatOp,
        rd: Reg,
        rm: Reg,
        rn: Reg,
    },
    Memory {
        load: bool,
        size: MemSize,
        rt: Reg,
        rn: Reg,
        offset: Offset,
        addressing: Addressing,
        /// LDRT/STRT style user-mode access.
        user: bool,
    },
    Multiple {
        load: bool,
        rn: Reg,
        regs: u16,
        mode: BlockMode,
        writeback: bool,
        /// The `^` form (user bank or SPSR restore).
        user: bool,
    },
    Swap {
        byte: bool,
        rd: Reg,
        rm: Reg,
        rn: Reg,
    },
    /// B, BL, and BLX with an immediate target. `exchange` switches state.
    Branch {
        target: u32,
        link: bool,
        exchange: bool,
    },
    /// BX or BLX with a register target.
    BranchExchange {
        rm: Reg,
        link: bool,
    },
    Mrs {
        rd: Reg,
        spsr: bool,
    },
    Msr {
        spsr: bool,
        fields: u8,
        operand: Operand,
    },
    Coprocessor {
        op: CoprocOp,
        cp: u8,
        opc1: u8,
        rd: Reg,
        crn: u8,
        crm: u8,
        opc2: u8,
    },
    Swi {
        comment: u32,
    },
    Breakpoint {
        comment: u16,
    },
    Preload {
        rn: Reg,
        offset: Offset,
    },
    Undefined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instruction {
    pub address: u32,
    /// The encoding; for a Thumb BL pair the first halfword is in the low bits.
    pub raw: u32,
    pub size: u8,
    pub thumb: bool,
    pub cond: Cond,
    pub op: Op,
}

impl Instruction {
    pub fn end(&self) -> u32 {
        self.address.wrapping_add(u32::from(self.size))
    }

    /// The value `pc` reads as when used as an operand.
    pub fn pc_value(&self) -> u32 {
        self.address.wrapping_add(if self.thumb { 4 } else { 8 })
    }

    /// Registers this instruction writes, as a bit mask (bit n = rn).
    pub fn written_registers(&self) -> u16 {
        let bit = |reg: Reg| 1_u16 << reg.0;
        match self.op {
            Op::DataProc { op, rd, .. } if !op.is_compare() => bit(rd),
            Op::SignedMultiply {
                op: SignedMulOp::Smlal,
                rd,
                rn,
                ..
            } => bit(rd) | bit(rn),
            Op::Multiply { rd, .. }
            | Op::SignedMultiply { rd, .. }
            | Op::Clz { rd, .. }
            | Op::Saturating { rd, .. }
            | Op::Swap { rd, .. }
            | Op::Mrs { rd, .. } => bit(rd),
            Op::MultiplyLong { rd_lo, rd_hi, .. } => bit(rd_lo) | bit(rd_hi),
            Op::Memory {
                load,
                size,
                rt,
                rn,
                addressing,
                ..
            } => {
                let mut mask = 0;
                if load {
                    mask |= bit(rt);
                    if size == MemSize::Double {
                        mask |= bit(Reg(rt.0 + 1));
                    }
                }
                if addressing != Addressing::Offset {
                    mask |= bit(rn);
                }
                mask
            }
            Op::Multiple {
                load,
                rn,
                regs,
                writeback,
                ..
            } => (if load { regs } else { 0 }) | if writeback { bit(rn) } else { 0 },
            Op::Branch { link: true, .. } | Op::BranchExchange { link: true, .. } => bit(LR),
            Op::Coprocessor {
                op: CoprocOp::Mrc,
                rd,
                ..
            } => bit(rd),
            _ => 0,
        }
    }

    /// For `ldr rt, [pc, #imm]` style loads, the address that is read.
    pub fn pc_relative_address(&self) -> Option<u32> {
        match self.op {
            Op::Memory {
                rn: PC,
                offset: Offset::Imm(offset),
                addressing: Addressing::Offset,
                ..
            } => {
                let base = if self.thumb {
                    self.pc_value() & !3
                } else {
                    self.pc_value()
                };
                Some(base.wrapping_add(offset as u32))
            }
            _ => None,
        }
    }
}

fn reg(word: u32, shift: u32) -> Reg {
    Reg(((word >> shift) & 0xF) as u8)
}

fn bit(word: u32, index: u32) -> bool {
    (word >> index) & 1 != 0
}

fn sign_extend(value: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((value << shift) as i32) >> shift
}

/// Normalize an ARM immediate shift (type bits and 5-bit amount).
fn imm_shift(kind: u32, amount: u32) -> (ShiftKind, u8) {
    let amount = amount as u8;
    match kind & 3 {
        0 => (ShiftKind::Lsl, amount),
        1 => (ShiftKind::Lsr, if amount == 0 { 32 } else { amount }),
        2 => (ShiftKind::Asr, if amount == 0 { 32 } else { amount }),
        _ if amount == 0 => (ShiftKind::Rrx, 1),
        _ => (ShiftKind::Ror, amount),
    }
}

fn addressing(pre: bool, writeback: bool) -> Addressing {
    match (pre, writeback) {
        (false, _) => Addressing::PostIndex,
        (true, true) => Addressing::PreIndex,
        (true, false) => Addressing::Offset,
    }
}

pub fn decode_arm(address: u32, word: u32) -> Instruction {
    let make = |cond, op| Instruction {
        address,
        raw: word,
        size: 4,
        thumb: false,
        cond,
        op,
    };
    if word >> 28 == 0xF {
        if word & 0x0E00_0000 == 0x0A00_0000 {
            let offset = (sign_extend(word & 0x00FF_FFFF, 24) << 2) | ((word >> 23) & 2) as i32;
            let target = address.wrapping_add(8).wrapping_add(offset as u32);
            return make(
                Cond::Al,
                Op::Branch {
                    target,
                    link: true,
                    exchange: true,
                },
            );
        }
        if word & 0x0D70_F000 == 0x0550_F000 {
            let rn = reg(word, 16);
            let offset = single_transfer_offset(word);
            return make(Cond::Al, Op::Preload { rn, offset });
        }
        return make(Cond::Al, Op::Undefined);
    }
    let cond = Cond::from_bits(word >> 28);
    let op = match (word >> 25) & 7 {
        0b000 => decode_arm_000(word),
        0b001 => {
            let rotate = (word >> 8) & 0xF;
            let value = (word & 0xFF).rotate_right(rotate * 2);
            let operand = Operand::Imm {
                value,
                rotated: rotate != 0,
            };
            if word & 0x0190_0000 == 0x0100_0000 {
                if bit(word, 21) {
                    Op::Msr {
                        spsr: bit(word, 22),
                        fields: ((word >> 16) & 0xF) as u8,
                        operand,
                    }
                } else {
                    Op::Undefined
                }
            } else {
                Op::DataProc {
                    op: DpOp::from_bits(word >> 21),
                    set_flags: bit(word, 20),
                    rd: reg(word, 12),
                    rn: reg(word, 16),
                    operand,
                }
            }
        }
        0b010 | 0b011 => {
            if bit(word, 25) && bit(word, 4) {
                Op::Undefined
            } else {
                let pre = bit(word, 24);
                let writeback = bit(word, 21);
                Op::Memory {
                    load: bit(word, 20),
                    size: if bit(word, 22) {
                        MemSize::Byte
                    } else {
                        MemSize::Word
                    },
                    rt: reg(word, 12),
                    rn: reg(word, 16),
                    offset: single_transfer_offset(word),
                    addressing: addressing(pre, writeback),
                    user: !pre && writeback,
                }
            }
        }
        0b100 => {
            let mode = match (bit(word, 24), bit(word, 23)) {
                (false, true) => BlockMode::Ia,
                (true, true) => BlockMode::Ib,
                (false, false) => BlockMode::Da,
                (true, false) => BlockMode::Db,
            };
            Op::Multiple {
                load: bit(word, 20),
                rn: reg(word, 16),
                regs: (word & 0xFFFF) as u16,
                mode,
                writeback: bit(word, 21),
                user: bit(word, 22),
            }
        }
        0b101 => {
            let offset = sign_extend(word & 0x00FF_FFFF, 24) << 2;
            Op::Branch {
                target: address.wrapping_add(8).wrapping_add(offset as u32),
                link: bit(word, 24),
                exchange: false,
            }
        }
        0b110 => coprocessor(
            word,
            if bit(word, 20) {
                CoprocOp::Ldc
            } else {
                CoprocOp::Stc
            },
        ),
        _ => {
            if bit(word, 24) {
                Op::Swi {
                    comment: word & 0x00FF_FFFF,
                }
            } else if bit(word, 4) {
                coprocessor(
                    word,
                    if bit(word, 20) {
                        CoprocOp::Mrc
                    } else {
                        CoprocOp::Mcr
                    },
                )
            } else {
                coprocessor(word, CoprocOp::Cdp)
            }
        }
    };
    make(cond, op)
}

fn coprocessor(word: u32, op: CoprocOp) -> Op {
    let opc1 = match op {
        CoprocOp::Cdp => (word >> 20) & 0xF,
        CoprocOp::Mcr | CoprocOp::Mrc => (word >> 21) & 7,
        CoprocOp::Ldc | CoprocOp::Stc => word & 0xFF,
    };
    Op::Coprocessor {
        op,
        cp: ((word >> 8) & 0xF) as u8,
        opc1: opc1 as u8,
        rd: reg(word, 12),
        crn: ((word >> 16) & 0xF) as u8,
        crm: (word & 0xF) as u8,
        opc2: ((word >> 5) & 7) as u8,
    }
}

fn single_transfer_offset(word: u32) -> Offset {
    let subtract = !bit(word, 23);
    if bit(word, 25) {
        let (kind, amount) = imm_shift(word >> 5, (word >> 7) & 0x1F);
        Offset::Reg {
            rm: reg(word, 0),
            kind,
            amount,
            subtract,
        }
    } else {
        let value = (word & 0xFFF) as i32;
        Offset::Imm(if subtract { -value } else { value })
    }
}

fn decode_arm_000(word: u32) -> Op {
    if word & 0x0FC0_00F0 == 0x0000_0090 {
        return Op::Multiply {
            accumulate: bit(word, 21),
            set_flags: bit(word, 20),
            rd: reg(word, 16),
            rn: reg(word, 12),
            rs: reg(word, 8),
            rm: reg(word, 0),
        };
    }
    if word & 0x0F80_00F0 == 0x0080_0090 {
        return Op::MultiplyLong {
            signed: bit(word, 22),
            accumulate: bit(word, 21),
            set_flags: bit(word, 20),
            rd_hi: reg(word, 16),
            rd_lo: reg(word, 12),
            rs: reg(word, 8),
            rm: reg(word, 0),
        };
    }
    if word & 0x0FB0_0FF0 == 0x0100_0090 {
        return Op::Swap {
            byte: bit(word, 22),
            rn: reg(word, 16),
            rd: reg(word, 12),
            rm: reg(word, 0),
        };
    }
    if word & 0x0E00_0090 == 0x90 {
        let load = bit(word, 20);
        let sh = (word >> 5) & 3;
        let size = match (sh, load) {
            (1, _) => MemSize::Half,
            (2, true) => MemSize::SignedByte,
            (3, true) => MemSize::SignedHalf,
            (2 | 3, false) => MemSize::Double,
            _ => return Op::Undefined,
        };
        // LDRD is encoded with L=0 and SH=2; STRD with L=0 and SH=3.
        let load = load || (size == MemSize::Double && sh == 2);
        let subtract = !bit(word, 23);
        let offset = if bit(word, 22) {
            let value = (((word >> 4) & 0xF0) | (word & 0xF)) as i32;
            Offset::Imm(if subtract { -value } else { value })
        } else {
            Offset::Reg {
                rm: reg(word, 0),
                kind: ShiftKind::Lsl,
                amount: 0,
                subtract,
            }
        };
        return Op::Memory {
            load,
            size,
            rt: reg(word, 12),
            rn: reg(word, 16),
            offset,
            addressing: addressing(bit(word, 24), bit(word, 21)),
            user: false,
        };
    }
    if word & 0x0190_0000 == 0x0100_0000 {
        let op = (word >> 21) & 3;
        return match (word >> 4) & 0xF {
            0b0000 if op & 1 == 0 => Op::Mrs {
                rd: reg(word, 12),
                spsr: bit(word, 22),
            },
            0b0000 => Op::Msr {
                spsr: bit(word, 22),
                fields: ((word >> 16) & 0xF) as u8,
                operand: Operand::reg(reg(word, 0)),
            },
            0b0001 if op == 1 => Op::BranchExchange {
                rm: reg(word, 0),
                link: false,
            },
            0b0001 if op == 3 => Op::Clz {
                rd: reg(word, 12),
                rm: reg(word, 0),
            },
            0b0011 if op == 1 => Op::BranchExchange {
                rm: reg(word, 0),
                link: true,
            },
            0b0101 => Op::Saturating {
                op: [SatOp::Qadd, SatOp::Qsub, SatOp::Qdadd, SatOp::Qdsub][op as usize],
                rd: reg(word, 12),
                rn: reg(word, 16),
                rm: reg(word, 0),
            },
            0b0111 if op == 1 => Op::Breakpoint {
                comment: (((word >> 4) & 0xFFF0) | (word & 0xF)) as u16,
            },
            nibble if nibble & 0b1001 == 0b1000 => Op::SignedMultiply {
                op: [
                    SignedMulOp::Smla,
                    SignedMulOp::SmlawSmulw,
                    SignedMulOp::Smlal,
                    SignedMulOp::Smul,
                ][op as usize],
                x: bit(word, 5),
                y: bit(word, 6),
                rd: reg(word, 16),
                rn: reg(word, 12),
                rs: reg(word, 8),
                rm: reg(word, 0),
            },
            _ => Op::Undefined,
        };
    }
    let operand = if bit(word, 4) {
        if bit(word, 7) {
            return Op::Undefined;
        }
        Operand::Reg {
            rm: reg(word, 0),
            kind: [
                ShiftKind::Lsl,
                ShiftKind::Lsr,
                ShiftKind::Asr,
                ShiftKind::Ror,
            ][((word >> 5) & 3) as usize],
            amount: ShiftAmount::Reg(reg(word, 8)),
        }
    } else {
        let (kind, amount) = imm_shift(word >> 5, (word >> 7) & 0x1F);
        Operand::Reg {
            rm: reg(word, 0),
            kind,
            amount: ShiftAmount::Imm(amount),
        }
    };
    Op::DataProc {
        op: DpOp::from_bits(word >> 21),
        set_flags: bit(word, 20),
        rd: reg(word, 12),
        rn: reg(word, 16),
        operand,
    }
}

/// Decode a Thumb instruction. `next` is the following halfword, used to pair
/// the two halves of BL/BLX; a pair decodes as one 4-byte instruction.
pub fn decode_thumb(address: u32, half: u16, next: Option<u16>) -> Instruction {
    let word = u32::from(half);
    let mut ins = Instruction {
        address,
        raw: word,
        size: 2,
        thumb: true,
        cond: Cond::Al,
        op: Op::Undefined,
    };
    let low = |shift: u32| Reg(((word >> shift) & 7) as u8);
    let data = |op, set_flags, rd, rn, operand| Op::DataProc {
        op,
        set_flags,
        rd,
        rn,
        operand,
    };
    let memory = |load, size, rt, rn, offset| Op::Memory {
        load,
        size,
        rt,
        rn,
        offset,
        addressing: Addressing::Offset,
        user: false,
    };

    ins.op = match word >> 13 {
        0b000 if (word >> 11) & 3 == 3 => {
            let operand = if bit(word, 10) {
                Operand::imm((word >> 6) & 7)
            } else {
                Operand::reg(low(6))
            };
            let op = if bit(word, 9) { DpOp::Sub } else { DpOp::Add };
            data(op, true, low(0), low(3), operand)
        }
        0b000 => {
            let (kind, amount) = imm_shift(word >> 11, (word >> 6) & 0x1F);
            let operand = Operand::Reg {
                rm: low(3),
                kind,
                amount: ShiftAmount::Imm(amount),
            };
            data(DpOp::Mov, true, low(0), low(0), operand)
        }
        0b001 => {
            let rd = low(8);
            let operand = Operand::imm(word & 0xFF);
            let op = [DpOp::Mov, DpOp::Cmp, DpOp::Add, DpOp::Sub][((word >> 11) & 3) as usize];
            data(op, true, rd, rd, operand)
        }
        0b010 if word >> 10 == 0b010000 => {
            let rd = low(0);
            let rs = low(3);
            let shifted = |kind| Operand::Reg {
                rm: rd,
                kind,
                amount: ShiftAmount::Reg(rs),
            };
            match (word >> 6) & 0xF {
                0x2 => data(DpOp::Mov, true, rd, rd, shifted(ShiftKind::Lsl)),
                0x3 => data(DpOp::Mov, true, rd, rd, shifted(ShiftKind::Lsr)),
                0x4 => data(DpOp::Mov, true, rd, rd, shifted(ShiftKind::Asr)),
                0x7 => data(DpOp::Mov, true, rd, rd, shifted(ShiftKind::Ror)),
                0x9 => data(DpOp::Rsb, true, rd, rs, Operand::imm(0)),
                0xD => Op::Multiply {
                    accumulate: false,
                    set_flags: true,
                    rd,
                    rm: rs,
                    rs: rd,
                    rn: Reg(0),
                },
                op => data(DpOp::from_bits(op), true, rd, rd, Operand::reg(rs)),
            }
        }
        0b010 if word >> 10 == 0b010001 => {
            let rs = Reg(((word >> 3) & 0xF) as u8);
            let rd = Reg(((word & 7) | ((word >> 4) & 8)) as u8);
            match (word >> 8) & 3 {
                0 => data(DpOp::Add, false, rd, rd, Operand::reg(rs)),
                1 => data(DpOp::Cmp, true, rd, rd, Operand::reg(rs)),
                2 => data(DpOp::Mov, false, rd, rd, Operand::reg(rs)),
                _ => Op::BranchExchange {
                    rm: rs,
                    link: bit(word, 7),
                },
            }
        }
        0b010 if word >> 11 == 0b01001 => memory(
            true,
            MemSize::Word,
            low(8),
            PC,
            Offset::Imm(((word & 0xFF) * 4) as i32),
        ),
        0b010 => {
            let (load, size) = [
                (false, MemSize::Word),
                (false, MemSize::Half),
                (false, MemSize::Byte),
                (true, MemSize::SignedByte),
                (true, MemSize::Word),
                (true, MemSize::Half),
                (true, MemSize::Byte),
                (true, MemSize::SignedHalf),
            ][((word >> 9) & 7) as usize];
            let offset = Offset::Reg {
                rm: low(6),
                kind: ShiftKind::Lsl,
                amount: 0,
                subtract: false,
            };
            memory(load, size, low(0), low(3), offset)
        }
        0b011 => {
            let byte = bit(word, 12);
            let scale = if byte { 1 } else { 4 };
            let size = if byte { MemSize::Byte } else { MemSize::Word };
            let offset = Offset::Imm((((word >> 6) & 0x1F) * scale) as i32);
            memory(bit(word, 11), size, low(0), low(3), offset)
        }
        0b100 if !bit(word, 12) => {
            let offset = Offset::Imm((((word >> 6) & 0x1F) * 2) as i32);
            memory(bit(word, 11), MemSize::Half, low(0), low(3), offset)
        }
        0b100 => {
            let offset = Offset::Imm(((word & 0xFF) * 4) as i32);
            memory(bit(word, 11), MemSize::Word, low(8), SP, offset)
        }
        0b101 if !bit(word, 12) => {
            let imm = (word & 0xFF) * 4;
            if bit(word, 11) {
                data(DpOp::Add, false, low(8), SP, Operand::imm(imm))
            } else {
                let value = (address.wrapping_add(4) & !3).wrapping_add(imm);
                data(DpOp::Mov, false, low(8), low(8), Operand::imm(value))
            }
        }
        0b101 if word & 0x0F00 == 0 => {
            let op = if bit(word, 7) { DpOp::Sub } else { DpOp::Add };
            data(op, false, SP, SP, Operand::imm((word & 0x7F) * 4))
        }
        0b101 if word & 0x0600 == 0x0400 => {
            let load = bit(word, 11);
            let mut regs = (word & 0xFF) as u16;
            if bit(word, 8) {
                regs |= if load { 1 << 15 } else { 1 << 14 };
            }
            Op::Multiple {
                load,
                rn: SP,
                regs,
                mode: if load { BlockMode::Ia } else { BlockMode::Db },
                writeback: true,
                user: false,
            }
        }
        0b101 if word & 0x0F00 == 0x0E00 => Op::Breakpoint {
            comment: (word & 0xFF) as u16,
        },
        0b101 => Op::Undefined,
        0b110 if !bit(word, 12) => {
            let load = bit(word, 11);
            let rn = low(8);
            let regs = (word & 0xFF) as u16;
            Op::Multiple {
                load,
                rn,
                regs,
                mode: BlockMode::Ia,
                writeback: !(load && regs & (1 << rn.0) != 0),
                user: false,
            }
        }
        0b110 => match (word >> 8) & 0xF {
            0xF => Op::Swi {
                comment: word & 0xFF,
            },
            0xE => Op::Undefined,
            cond => {
                ins.cond = Cond::from_bits(cond);
                let offset = sign_extend(word & 0xFF, 8) << 1;
                Op::Branch {
                    target: address.wrapping_add(4).wrapping_add(offset as u32),
                    link: false,
                    exchange: false,
                }
            }
        },
        _ => match (word >> 11) & 3 {
            0b00 => {
                let offset = sign_extend(word & 0x7FF, 11) << 1;
                Op::Branch {
                    target: address.wrapping_add(4).wrapping_add(offset as u32),
                    link: false,
                    exchange: false,
                }
            }
            0b10 => match next.map(u32::from) {
                Some(suffix) if suffix >> 13 == 0b111 && suffix & 0x0800 != 0 => {
                    let exchange = suffix & 0x1000 == 0;
                    let high = sign_extend(word & 0x7FF, 11) << 12;
                    let target = address
                        .wrapping_add(4)
                        .wrapping_add(high as u32)
                        .wrapping_add((suffix & 0x7FF) << 1);
                    ins.size = 4;
                    ins.raw = word | (suffix << 16);
                    Op::Branch {
                        target: if exchange { target & !3 } else { target },
                        link: true,
                        exchange,
                    }
                }
                _ => Op::Undefined,
            },
            _ => Op::Undefined,
        },
    };
    ins
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cond = self.cond.suffix();
        match self.op {
            Op::DataProc {
                op,
                set_flags,
                rd,
                rn,
                operand,
            } => {
                let s = if set_flags && !op.is_compare() {
                    "s"
                } else {
                    ""
                };
                write!(f, "{}{s}{cond} ", op.name())?;
                if op.is_compare() {
                    write!(f, "{rn}, ")?;
                } else if op.is_move() {
                    write!(f, "{rd}, ")?;
                } else {
                    write!(f, "{rd}, {rn}, ")?;
                }
                write_operand(f, operand)
            }
            Op::Multiply {
                accumulate,
                set_flags,
                rd,
                rm,
                rs,
                rn,
            } => {
                let s = if set_flags { "s" } else { "" };
                if accumulate {
                    write!(f, "mla{s}{cond} {rd}, {rm}, {rs}, {rn}")
                } else {
                    write!(f, "mul{s}{cond} {rd}, {rm}, {rs}")
                }
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
                let name = match (signed, accumulate) {
                    (false, false) => "umull",
                    (false, true) => "umlal",
                    (true, false) => "smull",
                    (true, true) => "smlal",
                };
                let s = if set_flags { "s" } else { "" };
                write!(f, "{name}{s}{cond} {rd_lo}, {rd_hi}, {rm}, {rs}")
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
                let half = |top: bool| if top { 't' } else { 'b' };
                match op {
                    SignedMulOp::Smla => {
                        write!(f, "smla{}{}{cond} {rd}, {rm}, {rs}, {rn}", half(x), half(y))
                    }
                    SignedMulOp::SmlawSmulw if x => {
                        write!(f, "smulw{}{cond} {rd}, {rm}, {rs}", half(y))
                    }
                    SignedMulOp::SmlawSmulw => {
                        write!(f, "smlaw{}{cond} {rd}, {rm}, {rs}, {rn}", half(y))
                    }
                    SignedMulOp::Smlal => write!(
                        f,
                        "smlal{}{}{cond} {rn}, {rd}, {rm}, {rs}",
                        half(x),
                        half(y)
                    ),
                    SignedMulOp::Smul => {
                        write!(f, "smul{}{}{cond} {rd}, {rm}, {rs}", half(x), half(y))
                    }
                }
            }
            Op::Clz { rd, rm } => write!(f, "clz{cond} {rd}, {rm}"),
            Op::Saturating { op, rd, rm, rn } => {
                let name = match op {
                    SatOp::Qadd => "qadd",
                    SatOp::Qsub => "qsub",
                    SatOp::Qdadd => "qdadd",
                    SatOp::Qdsub => "qdsub",
                };
                write!(f, "{name}{cond} {rd}, {rm}, {rn}")
            }
            Op::Memory {
                load,
                size,
                rt,
                rn,
                offset,
                addressing,
                user,
            } => {
                let suffix = match size {
                    MemSize::Word => "",
                    MemSize::Byte => "b",
                    MemSize::Half => "h",
                    MemSize::SignedByte => "sb",
                    MemSize::SignedHalf => "sh",
                    MemSize::Double => "d",
                };
                let t = if user { "t" } else { "" };
                let name = if load { "ldr" } else { "str" };
                write!(f, "{name}{suffix}{t}{cond} {rt}, ")?;
                write_address(f, rn, offset, addressing)?;
                if let Some(address) = self.pc_relative_address() {
                    write!(f, " ; 0x{address:08X}")?;
                }
                Ok(())
            }
            Op::Multiple {
                load,
                rn,
                regs,
                mode,
                writeback,
                user,
            } => {
                let name = if load { "ldm" } else { "stm" };
                let mode = match mode {
                    BlockMode::Ia => "ia",
                    BlockMode::Ib => "ib",
                    BlockMode::Da => "da",
                    BlockMode::Db => "db",
                };
                let bang = if writeback { "!" } else { "" };
                write!(f, "{name}{mode}{cond} {rn}{bang}, ")?;
                write_reg_list(f, regs)?;
                if user {
                    f.write_str("^")?;
                }
                Ok(())
            }
            Op::Swap { byte, rd, rm, rn } => {
                let b = if byte { "b" } else { "" };
                write!(f, "swp{b}{cond} {rd}, {rm}, [{rn}]")
            }
            Op::Branch {
                target,
                link,
                exchange,
            } => {
                let name = match (link, exchange) {
                    (false, _) => "b",
                    (true, false) => "bl",
                    (true, true) => "blx",
                };
                write!(f, "{name}{cond} 0x{target:08X}")
            }
            Op::BranchExchange { rm, link } => {
                let name = if link { "blx" } else { "bx" };
                write!(f, "{name}{cond} {rm}")
            }
            Op::Mrs { rd, spsr } => {
                let psr = if spsr { "spsr" } else { "cpsr" };
                write!(f, "mrs{cond} {rd}, {psr}")
            }
            Op::Msr {
                spsr,
                fields,
                operand,
            } => {
                let psr = if spsr { "spsr" } else { "cpsr" };
                write!(f, "msr{cond} {psr}_")?;
                for (index, name) in ['c', 'x', 's', 'f'].into_iter().enumerate() {
                    if fields & (1 << index) != 0 {
                        write!(f, "{name}")?;
                    }
                }
                f.write_str(", ")?;
                write_operand(f, operand)
            }
            Op::Coprocessor {
                op,
                cp,
                opc1,
                rd,
                crn,
                crm,
                opc2,
            } => match op {
                CoprocOp::Mcr | CoprocOp::Mrc => {
                    let name = if op == CoprocOp::Mcr { "mcr" } else { "mrc" };
                    write!(
                        f,
                        "{name}{cond} p{cp}, {opc1}, {rd}, c{crn}, c{crm}, {opc2}"
                    )
                }
                CoprocOp::Cdp => write!(
                    f,
                    "cdp{cond} p{cp}, {opc1}, c{}, c{crn}, c{crm}, {opc2}",
                    rd.0
                ),
                CoprocOp::Ldc | CoprocOp::Stc => {
                    let name = if op == CoprocOp::Ldc { "ldc" } else { "stc" };
                    write!(f, "{name}{cond} p{cp} (0x{:08X})", self.raw)
                }
            },
            Op::Swi { comment } => write!(f, "swi{cond} 0x{comment:X}"),
            Op::Breakpoint { comment } => write!(f, "bkpt 0x{comment:X}"),
            Op::Preload { rn, offset } => {
                f.write_str("pld ")?;
                write_address(f, rn, offset, Addressing::Offset)
            }
            Op::Undefined => {
                if self.thumb {
                    write!(f, ".hword 0x{:04X}", self.raw & 0xFFFF)
                } else {
                    write!(f, ".word 0x{:08X}", self.raw)
                }
            }
        }
    }
}

fn shift_name(kind: ShiftKind) -> &'static str {
    match kind {
        ShiftKind::Lsl => "lsl",
        ShiftKind::Lsr => "lsr",
        ShiftKind::Asr => "asr",
        ShiftKind::Ror => "ror",
        ShiftKind::Rrx => "rrx",
    }
}

fn write_operand(f: &mut fmt::Formatter<'_>, operand: Operand) -> fmt::Result {
    match operand {
        Operand::Imm { value, .. } => write!(f, "#0x{value:X}"),
        Operand::Reg { rm, kind, amount } => {
            write!(f, "{rm}")?;
            match (kind, amount) {
                (ShiftKind::Lsl, ShiftAmount::Imm(0)) => Ok(()),
                (ShiftKind::Rrx, _) => f.write_str(", rrx"),
                (kind, ShiftAmount::Imm(value)) => write!(f, ", {} #{value}", shift_name(kind)),
                (kind, ShiftAmount::Reg(rs)) => write!(f, ", {} {rs}", shift_name(kind)),
            }
        }
    }
}

fn write_address(
    f: &mut fmt::Formatter<'_>,
    rn: Reg,
    offset: Offset,
    addressing: Addressing,
) -> fmt::Result {
    let offset_text = match offset {
        Offset::Imm(0) => String::new(),
        Offset::Imm(value) if value < 0 => format!(", #-0x{:X}", value.unsigned_abs()),
        Offset::Imm(value) => format!(", #0x{value:X}"),
        Offset::Reg {
            rm,
            kind,
            amount,
            subtract,
        } => {
            let sign = if subtract { "-" } else { "" };
            match (kind, amount) {
                (ShiftKind::Lsl, 0) => format!(", {sign}{rm}"),
                (ShiftKind::Rrx, _) => format!(", {sign}{rm}, rrx"),
                (kind, amount) => format!(", {sign}{rm}, {} #{amount}", shift_name(kind)),
            }
        }
    };
    match addressing {
        Addressing::Offset => write!(f, "[{rn}{offset_text}]"),
        Addressing::PreIndex => write!(f, "[{rn}{offset_text}]!"),
        Addressing::PostIndex => write!(f, "[{rn}]{offset_text}"),
    }
}

fn write_reg_list(f: &mut fmt::Formatter<'_>, regs: u16) -> fmt::Result {
    f.write_str("{")?;
    let mut first = true;
    for index in 0..16 {
        if regs & (1 << index) != 0 {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            write!(f, "{}", Reg(index))?;
        }
    }
    f.write_str("}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(word: u32) -> String {
        decode_arm(0x0200_0000, word).to_string()
    }

    fn thumb(half: u16) -> String {
        decode_thumb(0x0200_0000, half, None).to_string()
    }

    #[test]
    fn decodes_arm_data_processing() {
        assert_eq!(arm(0xE3A0_0001), "mov r0, #0x1");
        assert_eq!(arm(0xE081_0002), "add r0, r1, r2");
        assert_eq!(arm(0xE1B0_0221), "movs r0, r1, lsr #4");
        assert_eq!(arm(0xE1A0_0021), "mov r0, r1, lsr #32");
        assert_eq!(arm(0xE350_0005), "cmp r0, #0x5");
        assert_eq!(arm(0x908F_F100), "addls pc, pc, r0, lsl #2");
        assert_eq!(arm(0xE3A0_04FF), "mov r0, #0xFF000000");
    }

    #[test]
    fn decodes_arm_memory_and_blocks() {
        assert_eq!(arm(0xE59F_0010), "ldr r0, [pc, #0x10] ; 0x02000018");
        assert_eq!(arm(0xE531_0004), "ldr r0, [r1, #-0x4]!");
        assert_eq!(arm(0xE1D1_00B2), "ldrh r0, [r1, #0x2]");
        assert_eq!(arm(0xE1C1_00D8), "ldrd r0, [r1, #0x8]");
        assert_eq!(arm(0xE1C1_00F8), "strd r0, [r1, #0x8]");
        assert_eq!(arm(0xE92D_4010), "stmdb sp!, {r4, lr}");
        assert_eq!(arm(0xE8BD_8010), "ldmia sp!, {r4, pc}");
    }

    #[test]
    fn decodes_arm_branches_and_misc() {
        assert_eq!(arm(0xEB00_0002), "bl 0x02000010");
        assert_eq!(arm(0xEAFF_FFFE), "b 0x02000000");
        assert_eq!(arm(0xFB00_0000), "blx 0x0200000A");
        assert_eq!(arm(0xE12F_FF1E), "bx lr");
        assert_eq!(arm(0xE12F_FF33), "blx r3");
        assert_eq!(arm(0xE16F_0F11), "clz r0, r1");
        assert_eq!(arm(0xE10F_0000), "mrs r0, cpsr");
        assert_eq!(arm(0xE121_F000), "msr cpsr_c, r0");
        assert_eq!(arm(0xE000_0291), "mul r0, r1, r2");
        assert_eq!(arm(0xE0C1_0392), "smull r0, r1, r2, r3");
        assert_eq!(arm(0xE160_0281), "smulbb r0, r1, r2");
        assert_eq!(arm(0xEE19_0F11), "mrc p15, 0, r0, c9, c1, 0");
        assert_eq!(arm(0xEF00_0006), "swi 0x6");
    }

    #[test]
    fn decodes_thumb() {
        assert_eq!(thumb(0x2005), "movs r0, #0x5");
        assert_eq!(thumb(0x0088), "movs r0, r1, lsl #2");
        assert_eq!(thumb(0x1888), "adds r0, r1, r2");
        assert_eq!(thumb(0x4248), "rsbs r0, r1, #0x0");
        assert_eq!(thumb(0x4348), "muls r0, r1, r0");
        assert_eq!(thumb(0x4087), "movs r7, r7, lsl r0");
        assert_eq!(thumb(0x4487), "add pc, pc, r0");
        assert_eq!(thumb(0x4770), "bx lr");
        assert_eq!(thumb(0x4798), "blx r3");
        assert_eq!(thumb(0x4801), "ldr r0, [pc, #0x4] ; 0x02000008");
        assert_eq!(thumb(0x5888), "ldr r0, [r1, r2]");
        assert_eq!(thumb(0x8848), "ldrh r0, [r1, #0x2]");
        assert_eq!(thumb(0x9001), "str r0, [sp, #0x4]");
        assert_eq!(thumb(0xA001), "mov r0, #0x2000008");
        assert_eq!(thumb(0xB082), "sub sp, sp, #0x8");
        assert_eq!(thumb(0xB510), "stmdb sp!, {r4, lr}");
        assert_eq!(thumb(0xBD10), "ldmia sp!, {r4, pc}");
        assert_eq!(thumb(0xC803), "ldmia r0, {r0, r1}");
        assert_eq!(thumb(0xD0FE), "beq 0x02000000");
        assert_eq!(thumb(0xDF06), "swi 0x6");
        assert_eq!(thumb(0xE7FE), "b 0x02000000");
    }

    #[test]
    fn pairs_thumb_bl_halves() {
        let bl = decode_thumb(0x0200_0000, 0xF000, Some(0xF802));
        assert_eq!(bl.size, 4);
        assert_eq!(bl.to_string(), "bl 0x02000008");
        let blx = decode_thumb(0x0200_0002, 0xF000, Some(0xE802));
        assert_eq!(blx.to_string(), "blx 0x02000008");
        let backwards = decode_thumb(0x0200_1000, 0xF7FF, Some(0xFFFE));
        assert_eq!(backwards.to_string(), "bl 0x02001000");
        assert_eq!(decode_thumb(0, 0xF000, None).op, Op::Undefined);
    }
}
