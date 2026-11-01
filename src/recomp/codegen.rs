//! C code generation for analyzed ARM9 functions.
//!
//! Each discovered function becomes `void f_<module>_<address>(Cpu *cpu)`.
//! Instructions are translated one by one in address order, with labels at
//! branch targets. The ABI (CPU state, memory access, runtime calls) is in
//! `gen5rt.h`. Conventions:
//!
//! - A call sets `lr` and calls the callee's C function directly when the
//!   callee is known, or `rt_call` otherwise. A return is a C `return`.
//! - Tail calls call the target and return.
//! - Switch tables become C `switch` statements over the known targets.
//! - Words loaded from literal pools are folded into constants.

use super::analysis::{Analysis, CallSite, Function, FunctionKey};
use super::arm::{
    Addressing, BlockMode, Cond, CoprocOp, DpOp, Instruction, MemSize, Offset, Op, Operand, PC,
    Reg, SatOp, ShiftAmount, ShiftKind, SignedMulOp,
};
use super::program::{Module, ModuleId, Program, Resolution};
use crate::Result;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

pub const RUNTIME_HEADER: &str = include_str!("gen5rt.h");
/// Forwards the `rt_*` hooks to host callbacks inside the game library.
pub const HOST_SHIM: &str = include_str!("gen5host.c");

pub fn function_name(key: FunctionKey) -> String {
    format!("f_{}_{:08X}", key.module, key.address)
}

/// The module number used in the runtime's function table.
pub fn module_number(module: ModuleId) -> u16 {
    match module {
        ModuleId::Main => 0xFFFF,
        ModuleId::Autoload(index) => 0xFF00 + u16::from(index),
        ModuleId::Overlay(id) => id,
    }
}

#[derive(Debug, Default)]
pub struct GenerateReport {
    pub files: usize,
    pub functions: usize,
    pub bytes: usize,
}

/// Write `gen5rt.h`, `recomp.h`, one C file per module and `functions.c`.
pub fn generate(program: &Program, analysis: &Analysis, out_dir: &Path) -> Result<GenerateReport> {
    fs::create_dir_all(out_dir)
        .map_err(|error| format!("could not create '{}': {error}", out_dir.display()))?;
    // Unchanged files keep their timestamps, so rebuilds reuse their objects.
    let write = |name: &str, text: &str| {
        let path = out_dir.join(name);
        if fs::read(&path).is_ok_and(|existing| existing == text.as_bytes()) {
            return Ok(());
        }
        fs::write(&path, text).map_err(|error| format!("could not write '{name}': {error}"))
    };
    let mut report = GenerateReport::default();

    write("gen5rt.h", RUNTIME_HEADER)?;
    write("gen5host.c", HOST_SHIM)?;
    let mut header = String::from(
        "/* Generated: prototypes of all recompiled functions. */\n\
         #ifndef GEN5_RECOMP_H\n#define GEN5_RECOMP_H\n#include \"gen5rt.h\"\n",
    );
    for key in analysis.functions.keys() {
        let _ = writeln!(header, "void {}(Cpu *cpu);", function_name(*key));
    }
    header.push_str("#endif\n");
    write("recomp.h", &header)?;
    report.files += 3;

    let mut table = String::from(
        "/* Generated: recompiled functions for indirect calls. */\n#include \"recomp.h\"\n\n\
         const Gen5FunctionEntry gen5_functions[] = {\n",
    );
    for module in &program.modules {
        let functions: Vec<&Function> = analysis
            .functions
            .values()
            .filter(|function| function.module == module.id)
            .collect();
        if functions.is_empty() {
            continue;
        }
        let mut text = format!(
            "/* Generated from {} (0x{:08X}, {} bytes). */\n#include \"recomp.h\"\n\n",
            module.id,
            module.base,
            module.bytes.len()
        );
        for function in &functions {
            text.push_str(&emit_function(analysis, module, function));
            text.push('\n');
            let _ = writeln!(
                table,
                "    {{0x{:04X}, {}, 0x{:08X}u, {}}},",
                module_number(function.module),
                u8::from(function.thumb),
                function.address,
                function_name(function.key())
            );
        }
        report.functions += functions.len();
        report.bytes += text.len();
        write(&format!("{}.c", module.id), &text)?;
        report.files += 1;
    }
    let _ = write!(
        table,
        "}};\nconst uint32_t gen5_function_count = {};\n",
        report.functions
    );
    write("functions.c", &table)?;
    report.files += 1;
    Ok(report)
}

/// Translate one function to C.
pub fn emit_function(analysis: &Analysis, module: &Module, function: &Function) -> String {
    if let Some(hle) = super::hle::detect(module, function.address, function.thumb) {
        return format!(
            "void {}(Cpu *cpu) {{\n    /* {hle:?}: implemented by the runtime. */\n    rt_hle(cpu, {});\n}}\n",
            function_name(function.key()),
            hle as u32
        );
    }
    let instructions: Vec<&Instruction> = function.instructions.values().collect();
    let mut labels: BTreeSet<u32> = BTreeSet::new();
    let mut body = String::new();
    let context = Context {
        analysis,
        module,
        function,
    };

    for (index, ins) in instructions.iter().enumerate() {
        let next_emitted = instructions.get(index + 1).map(|next| next.address);
        let _ = writeln!(body, "    /* {:08X}: {ins} */", ins.address);
        if let Some(hle) = super::hle::detect_inline(module, ins.address, ins.thumb) {
            let _ = writeln!(
                body,
                "    /* {hle:?}: implemented by the runtime. */\n    rt_hle(cpu, {});\n    return;",
                hle as u32
            );
        }
        let mut code = String::new();
        let falls_through = context.emit_instruction(&mut code, ins, &mut labels);
        if ins.cond != Cond::Al && !code.is_empty() {
            let _ = writeln!(body, "    if ({}) {{", condition(ins.cond));
            for line in code.lines() {
                let _ = writeln!(body, "    {line}");
            }
            body.push_str("    }\n");
        } else {
            body.push_str(&code);
        }

        if falls_through || ins.cond != Cond::Al {
            let next = ins.end();
            if next_emitted == Some(next) {
                continue;
            }
            if function.instructions.contains_key(&next) {
                labels.insert(next);
                let _ = writeln!(body, "    goto L_{next:08X};");
            } else if let Some(call) = function
                .calls
                .iter()
                .find(|call| call.tail && call.from == ins.address && call.target == next)
            {
                body.push_str(&context.call(call, None));
                body.push_str("    return;\n");
            } else {
                let _ = writeln!(
                    body,
                    "    rt_jump(cpu, 0x{:08X}u, 0x{:08X}u);\n    return;",
                    next | u32::from(function.thumb),
                    ins.address
                );
            }
        }
    }

    // Insert labels where they are used.
    let mut out = String::new();
    let _ = writeln!(
        out,
        "void {}(Cpu *cpu) {{\n    uint32_t *r = cpu->r;\n    (void)r;",
        function_name(function.key())
    );
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("    /* ")
            && let Some(address) = rest
                .get(..8)
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            && labels.contains(&address)
            && rest.as_bytes().get(8) == Some(&b':')
        {
            let _ = writeln!(out, "L_{address:08X}:;");
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

struct Context<'a> {
    analysis: &'a Analysis,
    module: &'a Module,
    function: &'a Function,
}

fn condition(cond: Cond) -> &'static str {
    match cond {
        Cond::Eq => "cpu->z",
        Cond::Ne => "!cpu->z",
        Cond::Cs => "cpu->c",
        Cond::Cc => "!cpu->c",
        Cond::Mi => "cpu->n",
        Cond::Pl => "!cpu->n",
        Cond::Vs => "cpu->v",
        Cond::Vc => "!cpu->v",
        Cond::Hi => "cpu->c && !cpu->z",
        Cond::Ls => "!cpu->c || cpu->z",
        Cond::Ge => "cpu->n == cpu->v",
        Cond::Lt => "cpu->n != cpu->v",
        Cond::Gt => "!cpu->z && cpu->n == cpu->v",
        Cond::Le => "cpu->z || cpu->n != cpu->v",
        Cond::Al => "1",
    }
}

fn hex(value: u32) -> String {
    format!("0x{value:08X}u")
}

/// The value of a register as an operand of `ins`.
fn read(ins: &Instruction, reg: Reg) -> String {
    if reg == PC {
        hex(ins.pc_value())
    } else {
        format!("r[{}]", reg.0)
    }
}

/// A shift by a constant amount: (value, carry out or None if unchanged).
fn shift_imm(value: &str, kind: ShiftKind, amount: u8) -> (String, Option<String>) {
    let n = u32::from(amount);
    match (kind, n) {
        (ShiftKind::Lsl, 0) => (value.to_owned(), None),
        (ShiftKind::Lsl, _) => (
            format!("({value} << {n})"),
            Some(format!("(({value} >> {}) & 1u)", 32 - n)),
        ),
        (ShiftKind::Lsr, 32) => ("0u".to_owned(), Some(format!("({value} >> 31)"))),
        (ShiftKind::Lsr, _) => (
            format!("({value} >> {n})"),
            Some(format!("(({value} >> {}) & 1u)", n - 1)),
        ),
        (ShiftKind::Asr, 32) => (
            format!("(uint32_t)((int32_t){value} >> 31)"),
            Some(format!("({value} >> 31)")),
        ),
        (ShiftKind::Asr, _) => (
            format!("(uint32_t)((int32_t){value} >> {n})"),
            Some(format!("(({value} >> {}) & 1u)", n - 1)),
        ),
        (ShiftKind::Ror, _) => (
            format!("ror32({value}, {n})"),
            Some(format!("(({value} >> {}) & 1u)", n - 1)),
        ),
        (ShiftKind::Rrx, _) => (
            format!("((cpu->c << 31) | ({value} >> 1))"),
            Some(format!("({value} & 1u)")),
        ),
    }
}

impl Context<'_> {
    /// Emit statements for the flexible operand into `code`; returns the
    /// operand expression and the shifter carry expression.
    fn operand(
        &self,
        code: &mut String,
        ins: &Instruction,
        operand: Operand,
    ) -> (String, Option<String>) {
        match operand {
            Operand::Imm { value, rotated } => {
                (hex(value), rotated.then(|| format!("{}u", value >> 31)))
            }
            Operand::Reg {
                rm,
                kind,
                amount: ShiftAmount::Imm(amount),
            } => {
                let _ = writeln!(code, "    uint32_t m = {};", read(ins, rm));
                shift_imm("m", kind, amount)
            }
            Operand::Reg {
                rm,
                kind,
                amount: ShiftAmount::Reg(rs),
            } => {
                let function = match kind {
                    ShiftKind::Lsl => "shift_lsl",
                    ShiftKind::Lsr => "shift_lsr",
                    ShiftKind::Asr => "shift_asr",
                    ShiftKind::Ror | ShiftKind::Rrx => "shift_ror",
                };
                let _ = writeln!(
                    code,
                    "    uint32_t sc = cpu->c;\n    uint32_t op2 = {function}({}, {} & 0xFFu, &sc);",
                    read(ins, rm),
                    read(ins, rs)
                );
                ("op2".to_owned(), Some("sc".to_owned()))
            }
        }
    }

    /// Emit one instruction (without its condition). Returns whether control
    /// can continue to the next instruction.
    fn emit_instruction(
        &self,
        code: &mut String,
        ins: &Instruction,
        labels: &mut BTreeSet<u32>,
    ) -> bool {
        let link_value = ins.end() | u32::from(ins.thumb);
        match ins.op {
            Op::DataProc {
                op,
                set_flags,
                rd,
                rn,
                operand,
            } => {
                code.push_str("    {\n");
                let (b, carry) = self.operand(code, ins, operand);
                let a = read(ins, rn);
                let (result, arithmetic) = match op {
                    DpOp::And | DpOp::Tst => (format!("{a} & {b}"), false),
                    DpOp::Eor | DpOp::Teq => (format!("{a} ^ {b}"), false),
                    DpOp::Orr => (format!("{a} | {b}"), false),
                    DpOp::Bic => (format!("{a} & ~{b}"), false),
                    DpOp::Mov => (b.clone(), false),
                    DpOp::Mvn => (format!("~{b}"), false),
                    DpOp::Add | DpOp::Cmn if set_flags => {
                        (format!("add_flags(cpu, {a}, {b}, 0)"), true)
                    }
                    DpOp::Adc if set_flags => (format!("add_flags(cpu, {a}, {b}, cpu->c)"), true),
                    DpOp::Sub | DpOp::Cmp if set_flags => {
                        (format!("add_flags(cpu, {a}, ~{b}, 1)"), true)
                    }
                    DpOp::Sbc if set_flags => (format!("add_flags(cpu, {a}, ~{b}, cpu->c)"), true),
                    DpOp::Rsb if set_flags => (format!("add_flags(cpu, {b}, ~{a}, 1)"), true),
                    DpOp::Rsc if set_flags => (format!("add_flags(cpu, {b}, ~{a}, cpu->c)"), true),
                    DpOp::Add | DpOp::Cmn => (format!("{a} + {b}"), true),
                    DpOp::Adc => (format!("{a} + {b} + cpu->c"), true),
                    DpOp::Sub | DpOp::Cmp => (format!("{a} - {b}"), true),
                    DpOp::Sbc => (format!("{a} - {b} - !cpu->c"), true),
                    DpOp::Rsb => (format!("{b} - {a}"), true),
                    DpOp::Rsc => (format!("{b} - {a} - !cpu->c"), true),
                };
                let _ = writeln!(code, "    uint32_t res = {result};");
                if set_flags && rd != PC {
                    code.push_str("    set_nz(cpu, res);\n");
                    if !arithmetic && let Some(carry) = carry {
                        let _ = writeln!(code, "    cpu->c = {carry};");
                    }
                }
                let continues = if op.is_compare() {
                    true
                } else if rd == PC {
                    if set_flags {
                        code.push_str("    rt_exception_return(cpu, res);\n    return;\n");
                        false
                    } else if ins.thumb {
                        // A Thumb data-processing write to pc stays in Thumb.
                        code.push_str("    res |= 1u;\n");
                        self.pc_write(code, ins, "res", labels)
                    } else {
                        self.pc_write(code, ins, "res", labels)
                    }
                } else {
                    let _ = writeln!(code, "    r[{}] = res;", rd.0);
                    true
                };
                code.push_str("    }\n");
                continues
            }
            Op::Multiply {
                accumulate,
                set_flags,
                rd,
                rm,
                rs,
                rn,
            } => {
                let add = if accumulate {
                    format!(" + {}", read(ins, rn))
                } else {
                    String::new()
                };
                let _ = writeln!(
                    code,
                    "    r[{}] = {} * {}{add};",
                    rd.0,
                    read(ins, rm),
                    read(ins, rs)
                );
                if set_flags {
                    let _ = writeln!(code, "    set_nz(cpu, r[{}]);", rd.0);
                }
                true
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
                let product = if signed {
                    format!(
                        "(uint64_t)((int64_t)(int32_t){} * (int64_t)(int32_t){})",
                        read(ins, rm),
                        read(ins, rs)
                    )
                } else {
                    format!("(uint64_t){} * {}", read(ins, rm), read(ins, rs))
                };
                let accumulator = if accumulate {
                    format!(" + (((uint64_t)r[{}] << 32) | r[{}])", rd_hi.0, rd_lo.0)
                } else {
                    String::new()
                };
                let _ = writeln!(
                    code,
                    "    {{\n    uint64_t wide = {product}{accumulator};\n    \
                     r[{}] = (uint32_t)wide;\n    r[{}] = (uint32_t)(wide >> 32);",
                    rd_lo.0, rd_hi.0
                );
                if set_flags {
                    code.push_str(
                        "    cpu->n = (uint32_t)(wide >> 63);\n    cpu->z = wide == 0;\n",
                    );
                }
                code.push_str("    }\n");
                true
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
                let (m, s, n) = (read(ins, rm), read(ins, rs), read(ins, rn));
                let x = u8::from(x);
                let y = u8::from(y);
                match op {
                    SignedMulOp::Smul => {
                        let _ = writeln!(
                            code,
                            "    r[{}] = (uint32_t)(half_of({m}, {x}) * half_of({s}, {y}));",
                            rd.0
                        );
                    }
                    SignedMulOp::Smla => {
                        let _ = writeln!(
                            code,
                            "    {{\n    int64_t sum = (int64_t)(half_of({m}, {x}) * half_of({s}, {y})) + (int32_t){n};\n    \
                             if (sum != (int32_t)sum) cpu->q = 1;\n    r[{}] = (uint32_t)sum;\n    }}",
                            rd.0
                        );
                    }
                    SignedMulOp::SmlawSmulw => {
                        let product =
                            format!("(int32_t)(((int64_t)(int32_t){m} * half_of({s}, {y})) >> 16)");
                        if x == 1 {
                            let _ = writeln!(code, "    r[{}] = (uint32_t){product};", rd.0);
                        } else {
                            let _ = writeln!(
                                code,
                                "    {{\n    int64_t sum = (int64_t){product} + (int32_t){n};\n    \
                                 if (sum != (int32_t)sum) cpu->q = 1;\n    r[{}] = (uint32_t)sum;\n    }}",
                                rd.0
                            );
                        }
                    }
                    SignedMulOp::Smlal => {
                        // RdHi is encoded in `rd`, RdLo in `rn`.
                        let _ = writeln!(
                            code,
                            "    {{\n    uint64_t acc = ((uint64_t)r[{hi}] << 32) | r[{lo}];\n    \
                             acc += (uint64_t)(int64_t)(half_of({m}, {x}) * half_of({s}, {y}));\n    \
                             r[{lo}] = (uint32_t)acc;\n    r[{hi}] = (uint32_t)(acc >> 32);\n    }}",
                            hi = rd.0,
                            lo = rn.0
                        );
                    }
                }
                true
            }
            Op::Clz { rd, rm } => {
                let _ = writeln!(code, "    r[{}] = clz32({});", rd.0, read(ins, rm));
                true
            }
            Op::Saturating { op, rd, rm, rn } => {
                let (m, n) = (read(ins, rm), read(ins, rn));
                let doubled = format!("(int64_t)saturate64(cpu, 2 * (int64_t)(int32_t){n})");
                let expression = match op {
                    SatOp::Qadd => format!("(int64_t)(int32_t){m} + (int32_t){n}"),
                    SatOp::Qsub => format!("(int64_t)(int32_t){m} - (int32_t){n}"),
                    SatOp::Qdadd => format!("(int64_t)(int32_t){m} + {doubled}"),
                    SatOp::Qdsub => format!("(int64_t)(int32_t){m} - {doubled}"),
                };
                let _ = writeln!(
                    code,
                    "    r[{}] = (uint32_t)saturate64(cpu, {expression});",
                    rd.0
                );
                true
            }
            Op::Memory { .. } => self.memory(code, ins, labels),
            Op::Multiple { .. } => self.multiple(code, ins, labels),
            Op::Swap { byte, rd, rm, rn } => {
                let (load, store) = if byte {
                    ("read8", "write8")
                } else {
                    ("read32_rotated", "write32")
                };
                let _ = writeln!(
                    code,
                    "    {{\n    uint32_t a = {};\n    uint32_t old = {load}(cpu, a);\n    \
                     {store}(cpu, a, {});\n    r[{}] = old;\n    }}",
                    read(ins, rn),
                    read(ins, rm),
                    rd.0
                );
                true
            }
            Op::Branch {
                target,
                link: false,
                ..
            } => {
                if self.function.instructions.contains_key(&target)
                    && !self.is_tail_call_at(ins.address, target)
                {
                    labels.insert(target);
                    if target <= ins.address {
                        code.push_str("    if (--cpu->budget == 0) rt_tick(cpu);\n");
                    }
                    let _ = writeln!(code, "    goto L_{target:08X};");
                } else if let Some(call) = self.tail_call_at(ins.address) {
                    code.push_str(&self.call(call, None));
                    code.push_str("    return;\n");
                } else {
                    let _ = writeln!(
                        code,
                        "    rt_jump(cpu, {}, {});\n    return;",
                        hex(target | u32::from(ins.thumb)),
                        hex(ins.address)
                    );
                }
                false
            }
            Op::Branch {
                target, link: true, ..
            } => {
                match self
                    .function
                    .calls
                    .iter()
                    .find(|call| !call.tail && call.from == ins.address)
                {
                    Some(call) => code.push_str(&self.call(call, Some(link_value))),
                    None => {
                        let _ = writeln!(
                            code,
                            "    r[14] = {};\n    rt_call(cpu, {});",
                            hex(link_value),
                            hex(target)
                        );
                    }
                }
                true
            }
            Op::BranchExchange { rm, link: true } => {
                let _ = writeln!(
                    code,
                    "    {{\n    uint32_t target = {};\n    r[14] = {};\n    rt_call(cpu, target);\n    }}",
                    read(ins, rm),
                    hex(link_value)
                );
                true
            }
            Op::BranchExchange { rm, link: false } => {
                let target = if rm == PC && ins.thumb {
                    hex(ins.pc_value() & !3)
                } else {
                    read(ins, rm)
                };
                let _ = writeln!(code, "    {{\n    uint32_t target = {target};");
                let continues = self.pc_write(code, ins, "target", labels);
                code.push_str("    }\n");
                continues
            }
            Op::Mrs { rd, spsr } => {
                let _ = writeln!(
                    code,
                    "    r[{}] = rt_read_psr(cpu, {});",
                    rd.0,
                    u8::from(spsr)
                );
                true
            }
            Op::Msr {
                spsr,
                fields,
                operand,
            } => {
                code.push_str("    {\n");
                let (value, _) = self.operand(code, ins, operand);
                let _ = writeln!(
                    code,
                    "    rt_write_psr(cpu, {}, {fields}, {value});\n    }}",
                    u8::from(spsr)
                );
                true
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
                let _ = writeln!(
                    code,
                    "    rt_mcr(cpu, {cp}, {opc1}, {crn}, {crm}, {opc2}, {});",
                    read(ins, rd)
                );
                true
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
                let call = format!("rt_mrc(cpu, {cp}, {opc1}, {crn}, {crm}, {opc2})");
                if rd == PC {
                    let _ = writeln!(
                        code,
                        "    {{\n    uint32_t flags = {call};\n    cpu->n = flags >> 31;\n    \
                         cpu->z = (flags >> 30) & 1u;\n    cpu->c = (flags >> 29) & 1u;\n    \
                         cpu->v = (flags >> 28) & 1u;\n    }}"
                    );
                } else {
                    let _ = writeln!(code, "    r[{}] = {call};", rd.0);
                }
                true
            }
            Op::Swi { comment } => {
                let _ = writeln!(
                    code,
                    "    rt_swi(cpu, 0x{comment:X}u, {});",
                    u8::from(ins.thumb)
                );
                true
            }
            Op::Preload { .. } => true,
            Op::Coprocessor { .. } | Op::Breakpoint { .. } | Op::Undefined => {
                let _ = writeln!(
                    code,
                    "    rt_undefined(cpu, {}, {});\n    return;",
                    hex(ins.address),
                    hex(ins.raw)
                );
                false
            }
        }
    }

    fn tail_call_at(&self, address: u32) -> Option<&CallSite> {
        self.function
            .calls
            .iter()
            .find(|call| call.tail && call.from == address)
    }

    fn is_tail_call_at(&self, address: u32, target: u32) -> bool {
        self.tail_call_at(address)
            .is_some_and(|call| call.target == target)
    }

    /// A call to a known site. `link` sets `lr` first (None for tail calls).
    fn call(&self, call: &CallSite, link: Option<u32>) -> String {
        let mut code = String::new();
        if let Some(link) = link {
            let _ = writeln!(code, "    r[14] = {};", hex(link));
        }
        let callee = match &call.resolution {
            Resolution::Module(module) => {
                let key = FunctionKey {
                    module: *module,
                    address: call.target,
                };
                self.analysis
                    .functions
                    .get(&key)
                    .filter(|callee| callee.thumb == call.thumb)
                    .map(|_| key)
            }
            Resolution::Ambiguous(_) | Resolution::Outside => None,
        };
        match callee {
            Some(key) => {
                let _ = writeln!(code, "    {}(cpu);", function_name(key));
            }
            None => {
                let _ = writeln!(
                    code,
                    "    rt_call(cpu, {});",
                    hex(call.target | u32::from(call.thumb))
                );
            }
        }
        code
    }

    /// Control transfer for an instruction that writes `pc` with `value`.
    /// Returns whether execution continues after it (indirect calls only).
    fn pc_write(
        &self,
        code: &mut String,
        ins: &Instruction,
        value: &str,
        labels: &mut BTreeSet<u32>,
    ) -> bool {
        let function = self.function;
        if let Some(table) = function
            .jump_tables
            .iter()
            .find(|table| table.jump == ins.address)
        {
            let _ = writeln!(code, "    switch ({value} & ~1u) {{");
            let targets: BTreeSet<u32> = table.targets.iter().copied().collect();
            for target in targets {
                if function.instructions.contains_key(&target) {
                    labels.insert(target);
                    let _ = writeln!(code, "    case {}: goto L_{target:08X};", hex(target));
                    continue;
                }
                // The case body was discovered as a separate function.
                let key = FunctionKey {
                    module: function.module,
                    address: target,
                };
                let jump = match self.analysis.functions.get(&key) {
                    Some(callee) if callee.thumb == ins.thumb => {
                        format!("{}(cpu);", function_name(key))
                    }
                    _ => format!(
                        "rt_jump(cpu, {}, {});",
                        hex(target | u32::from(ins.thumb)),
                        hex(ins.address)
                    ),
                };
                let _ = writeln!(code, "    case {}: {jump} return;", hex(target));
            }
            let _ = writeln!(
                code,
                "    default: rt_jump(cpu, {value}, {}); return;\n    }}",
                hex(ins.address)
            );
            false
        } else if let Some(call) = self.tail_call_at(ins.address) {
            let _ = writeln!(code, "    (void){value};");
            code.push_str(&self.call(call, None));
            code.push_str("    return;\n");
            false
        } else if let Some(&(_, continuation)) = function
            .calls_with_return
            .iter()
            .find(|(call, _)| *call == ins.address)
        {
            // lr already holds the return address.
            labels.insert(continuation);
            let _ = writeln!(
                code,
                "    rt_call(cpu, {value});\n    goto L_{continuation:08X};"
            );
            false
        } else if function.indirect_calls.contains(&ins.address) {
            // `mov lr, pc` already set the return address.
            let _ = writeln!(code, "    rt_call(cpu, {value});");
            true
        } else if function.indirect_jumps.contains(&ins.address) {
            let _ = writeln!(
                code,
                "    rt_jump(cpu, {value}, {});\n    return;",
                hex(ins.address)
            );
            false
        } else {
            let _ = writeln!(code, "    (void){value};\n    return;");
            false
        }
    }

    fn memory(&self, code: &mut String, ins: &Instruction, labels: &mut BTreeSet<u32>) -> bool {
        let Op::Memory {
            load,
            size,
            rt,
            rn,
            offset,
            addressing,
            ..
        } = ins.op
        else {
            unreachable!()
        };

        // Fold PC-relative literal loads into constants.
        if load
            && size == MemSize::Word
            && let Some(literal) = ins.pc_relative_address()
            && let Some(value) = self.module.u32_at(literal)
            && literal & 3 == 0
        {
            if rt == PC {
                let _ = writeln!(code, "    {{\n    uint32_t value = {};", hex(value));
                let continues = self.pc_write(code, ins, "value", labels);
                code.push_str("    }\n");
                return continues;
            }
            let _ = writeln!(code, "    r[{}] = {};", rt.0, hex(value));
            return true;
        }

        code.push_str("    {\n");
        let base = if rn == PC && ins.thumb {
            hex(ins.pc_value() & !3)
        } else {
            read(ins, rn)
        };
        let _ = writeln!(code, "    uint32_t base = {base};");
        let (offset_value, subtract) = match offset {
            Offset::Imm(value) => (hex(value.unsigned_abs()), value < 0),
            Offset::Reg {
                rm,
                kind,
                amount,
                subtract,
            } => {
                let _ = writeln!(code, "    uint32_t m = {};", read(ins, rm));
                (shift_imm("m", kind, amount).0, subtract)
            }
        };
        let sign = if subtract { '-' } else { '+' };
        let _ = writeln!(code, "    uint32_t moved = base {sign} {offset_value};");
        let address = if addressing == Addressing::PostIndex {
            "base"
        } else {
            "moved"
        };
        let writeback = addressing != Addressing::Offset;
        let mut continues = true;
        if load {
            let read_value = match size {
                MemSize::Word => format!("read32_rotated(cpu, {address})"),
                MemSize::Byte => format!("read8(cpu, {address})"),
                MemSize::Half => format!("read16(cpu, {address})"),
                MemSize::SignedByte => format!("(uint32_t)(int32_t)(int8_t)read8(cpu, {address})"),
                MemSize::SignedHalf => {
                    format!("(uint32_t)(int32_t)(int16_t)read16(cpu, {address})")
                }
                MemSize::Double => format!("read32(cpu, {address})"),
            };
            let _ = writeln!(code, "    uint32_t value = {read_value};");
            if size == MemSize::Double {
                let _ = writeln!(code, "    uint32_t value2 = read32(cpu, {address} + 4u);");
            }
            if writeback {
                let _ = writeln!(code, "    r[{}] = moved;", rn.0);
            }
            if rt == PC {
                continues = self.pc_write(code, ins, "value", labels);
            } else {
                let _ = writeln!(code, "    r[{}] = value;", rt.0);
                if size == MemSize::Double {
                    let _ = writeln!(code, "    r[{}] = value2;", rt.0 + 1);
                }
            }
        } else {
            let value = if rt == PC {
                hex(ins.pc_value() + 4)
            } else {
                read(ins, rt)
            };
            match size {
                MemSize::Word => {
                    let _ = writeln!(code, "    write32(cpu, {address}, {value});");
                }
                MemSize::Byte => {
                    let _ = writeln!(code, "    write8(cpu, {address}, {value});");
                }
                MemSize::Half => {
                    let _ = writeln!(code, "    write16(cpu, {address}, {value});");
                }
                MemSize::Double => {
                    let _ = writeln!(
                        code,
                        "    write32(cpu, {address}, {value});\n    write32(cpu, {address} + 4u, {});",
                        read(ins, Reg(rt.0 + 1))
                    );
                }
                MemSize::SignedByte | MemSize::SignedHalf => {}
            }
            if writeback {
                let _ = writeln!(code, "    r[{}] = moved;", rn.0);
            }
        }
        code.push_str("    }\n");
        continues
    }

    fn multiple(&self, code: &mut String, ins: &Instruction, labels: &mut BTreeSet<u32>) -> bool {
        let Op::Multiple {
            load,
            rn,
            regs,
            mode,
            writeback,
            user,
        } = ins.op
        else {
            unreachable!()
        };
        let count = regs.count_ones();
        let bytes = 4 * count;
        let start = match mode {
            BlockMode::Ia => "base".to_owned(),
            BlockMode::Ib => "base + 4u".to_owned(),
            BlockMode::Da => format!("base - {}u", bytes.saturating_sub(4)),
            BlockMode::Db => format!("base - {bytes}u"),
        };
        let end = match mode {
            BlockMode::Ia | BlockMode::Ib => format!("base + {bytes}u"),
            BlockMode::Da | BlockMode::Db => format!("base - {bytes}u"),
        };
        let _ = writeln!(
            code,
            "    {{\n    uint32_t base = {};\n    uint32_t a = {start};",
            read(ins, rn)
        );
        let registers: Vec<u8> = (0..16).filter(|index| regs & (1 << index) != 0).collect();
        let mut continues = true;
        if load {
            for (slot, reg) in registers.iter().enumerate() {
                let _ = writeln!(
                    code,
                    "    uint32_t v{reg} = read32(cpu, a + {}u);",
                    4 * slot
                );
            }
            if writeback {
                let _ = writeln!(code, "    r[{}] = {end};", rn.0);
            }
            for reg in &registers {
                if *reg != 15 {
                    let _ = writeln!(code, "    r[{reg}] = v{reg};");
                }
            }
            if regs & (1 << 15) != 0 {
                if user {
                    code.push_str("    rt_exception_return(cpu, v15);\n    return;\n");
                    continues = false;
                } else {
                    continues = self.pc_write(code, ins, "v15", labels);
                }
            }
        } else {
            for (slot, reg) in registers.iter().enumerate() {
                let value = if *reg == 15 {
                    hex(ins.pc_value() + 4)
                } else if Reg(*reg) == rn {
                    "base".to_owned()
                } else {
                    format!("r[{reg}]")
                };
                let _ = writeln!(code, "    write32(cpu, a + {}u, {value});", 4 * slot);
            }
            if writeback {
                let _ = writeln!(code, "    r[{}] = {end};", rn.0);
            }
        }
        code.push_str("    }\n");
        continues
    }
}

#[cfg(test)]
mod tests;
