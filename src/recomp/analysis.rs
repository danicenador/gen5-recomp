//! Function discovery and control-flow recovery across the ARM9 modules.
//!
//! Discovery starts from the ARM9 entry point and the overlays' static
//! initializer tables. It then follows direct calls and tail calls, and
//! accepts code pointers found in literal pools and data when they decode
//! cleanly. Function bodies are recovered by recursive traversal, so literal
//! pools and data between functions are never decoded as code. Discovery
//! repeats until no new functions appear. A final pass re-traverses every
//! function with the complete set of entry points, so a function never
//! swallows a neighbour that was only discovered later.

use super::arm::{
    self, Addressing, DpOp, Instruction, LR, MemSize, Offset, Op, Operand, PC, SP, ShiftAmount,
    ShiftKind,
};
use super::program::{Module, ModuleId, Program, Resolution};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Upper bound on one function's instructions, to stop runaway traversals.
const MAX_FUNCTION_INSTRUCTIONS: usize = 40_000;
const MAX_JUMP_TABLE_ENTRIES: u32 = 1024;
const MAX_ROUNDS: usize = 500;
/// Instructions carried into a branch target for pattern matching.
const HISTORY_CARRIED: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct FunctionKey {
    pub module: ModuleId,
    pub address: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Entry,
    Sinit,
    Call,
    TailCall,
    LiteralPointer,
    DataPointer,
    /// A cross-overlay call target in one of several candidate overlays.
    AmbiguousCall,
}

impl Source {
    /// Weak sources can be wrong: a data word that happens to look like a
    /// pointer, or a branch that is really internal to a larger function.
    fn is_weak(self) -> bool {
        matches!(
            self,
            Source::TailCall | Source::LiteralPointer | Source::DataPointer | Source::AmbiguousCall
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CallSite {
    pub from: u32,
    pub target: u32,
    /// The state the callee runs in.
    pub thumb: bool,
    pub tail: bool,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Serialize)]
pub struct JumpTable {
    /// The instruction that writes `pc`.
    pub jump: u32,
    pub table: u32,
    pub targets: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Function {
    pub module: ModuleId,
    pub address: u32,
    pub thumb: bool,
    /// One past the highest instruction byte reached.
    pub end: u32,
    pub instruction_count: usize,
    pub sources: BTreeSet<Source>,
    pub calls: Vec<CallSite>,
    pub jump_tables: Vec<JumpTable>,
    /// `pc` writes that are neither returns nor recognized jump tables.
    pub indirect_jumps: Vec<u32>,
    pub indirect_calls: Vec<u32>,
    /// Addresses of words read through PC-relative loads.
    pub literals: BTreeSet<u32>,
    /// Entry points of other functions this one runs straight into.
    pub falls_into: Vec<u32>,
    pub problems: Vec<String>,
    #[serde(skip)]
    pub instructions: BTreeMap<u32, Instruction>,
}

impl Function {
    pub fn key(&self) -> FunctionKey {
        FunctionKey {
            module: self.module,
            address: self.address,
        }
    }

    pub fn code_bytes(&self) -> u32 {
        self.instructions
            .values()
            .map(|ins| u32::from(ins.size))
            .sum()
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Analysis {
    pub functions: BTreeMap<FunctionKey, Function>,
    /// Functions seeded in both ARM and Thumb state; the first state wins.
    pub state_conflicts: Vec<FunctionKey>,
    /// Weakly seeded entry points folded back into the function that falls
    /// through into them.
    pub absorbed: BTreeSet<FunctionKey>,
    pub rounds: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Jump { target: u32, conditional: bool },
    Call { target: u32, thumb: bool },
    IndirectCall,
    Return { conditional: bool },
    IndirectJump { conditional: bool },
    Halt,
}

fn flow(ins: &Instruction, previous: Option<&Instruction>) -> Flow {
    let conditional = ins.cond != arm::Cond::Al;
    // ARM `mov lr, pc` followed by a `pc` write is an indirect call.
    let links = previous.is_some_and(|prev| {
        !prev.thumb
            && matches!(
                prev.op,
                Op::DataProc {
                    op: DpOp::Mov,
                    rd: LR,
                    operand: Operand::Reg {
                        rm: PC,
                        kind: ShiftKind::Lsl,
                        amount: ShiftAmount::Imm(0)
                    },
                    ..
                }
            )
    });
    let pc_write = |is_return: bool| {
        if links {
            Flow::IndirectCall
        } else if is_return {
            Flow::Return { conditional }
        } else {
            Flow::IndirectJump { conditional }
        }
    };
    match ins.op {
        Op::Branch {
            target,
            link: false,
            ..
        } => Flow::Jump {
            target,
            conditional,
        },
        Op::Branch {
            target,
            link: true,
            exchange,
        } => Flow::Call {
            target,
            thumb: ins.thumb != exchange,
        },
        Op::BranchExchange { link: true, .. } => Flow::IndirectCall,
        Op::BranchExchange { rm, .. } => pc_write(rm == LR),
        Op::DataProc { op, rd: PC, .. } if !op.is_compare() => pc_write(matches!(
            ins.op,
            Op::DataProc {
                op: DpOp::Mov,
                operand: Operand::Reg {
                    rm: LR,
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Imm(0)
                },
                ..
            }
        )),
        Op::Memory {
            load: true,
            rt: PC,
            rn,
            addressing,
            ..
        } => pc_write(rn == SP && addressing == Addressing::PostIndex),
        Op::Multiple {
            load: true,
            rn,
            regs,
            ..
        } if regs & (1 << 15) != 0 => pc_write(rn == SP),
        Op::Undefined | Op::Breakpoint { .. } => Flow::Halt,
        _ => Flow::Continue,
    }
}

pub fn decode_at(module: &Module, address: u32, thumb: bool) -> Option<Instruction> {
    if thumb {
        if address & 1 != 0 {
            return None;
        }
        let half = module.u16_at(address)?;
        let next = module.u16_at(address.wrapping_add(2));
        Some(arm::decode_thumb(address, half, next))
    } else {
        if address & 3 != 0 {
            return None;
        }
        Some(arm::decode_arm(address, module.u32_at(address)?))
    }
}

/// Recover one function's body by recursive traversal.
pub fn analyze_function(
    program: &Program,
    key: FunctionKey,
    thumb: bool,
    known: &HashSet<FunctionKey>,
) -> Function {
    let mut function = Function {
        module: key.module,
        address: key.address,
        thumb,
        end: key.address,
        instruction_count: 0,
        sources: BTreeSet::new(),
        calls: Vec::new(),
        jump_tables: Vec::new(),
        indirect_jumps: Vec::new(),
        indirect_calls: Vec::new(),
        literals: BTreeSet::new(),
        falls_into: Vec::new(),
        problems: Vec::new(),
        instructions: BTreeMap::new(),
    };
    let Some(module) = program.module(key.module) else {
        function.problems.push("module does not exist".to_owned());
        return function;
    };

    // Each block carries the instructions that led to it, so a switch can
    // find its bounding `cmp` across the conditional branch before it.
    let mut worklist: Vec<(u32, Vec<Instruction>)> = vec![(key.address, Vec::new())];
    let tail_call = |function: &mut Function, from: u32, target: u32, thumb: bool| {
        function.calls.push(CallSite {
            from,
            target,
            thumb,
            tail: true,
            resolution: program.resolve(key.module, target),
        });
    };
    let carried =
        |history: &[Instruction]| history[history.len().saturating_sub(HISTORY_CARRIED)..].to_vec();
    while let Some((start, mut history)) = worklist.pop() {
        let mut address = start;
        loop {
            if function.instructions.contains_key(&address) {
                break;
            }
            if address != key.address
                && known.contains(&FunctionKey {
                    module: key.module,
                    address,
                })
            {
                // Runs into another entry point. After a call this is the
                // next function following a call that does not return;
                // otherwise that entry point may really be inside this one.
                let after_call = history
                    .last()
                    .is_some_and(|ins| matches!(ins.op, Op::Branch { link: true, .. }));
                if !after_call {
                    function.falls_into.push(address);
                }
                let from = history.last().map_or(start, |ins| ins.address);
                tail_call(&mut function, from, address, thumb);
                break;
            }
            if function.instructions.len() >= MAX_FUNCTION_INSTRUCTIONS {
                function
                    .problems
                    .push("instruction limit reached".to_owned());
                worklist.clear();
                break;
            }
            let Some(ins) = decode_at(module, address, thumb) else {
                function
                    .problems
                    .push(format!("ran outside the module at 0x{address:08X}"));
                break;
            };
            function.instructions.insert(address, ins);
            if let Some(literal) = ins.pc_relative_address() {
                function.literals.insert(literal);
            }

            let next = ins.end();
            match flow(&ins, history.last()) {
                Flow::Continue => {}
                Flow::Call { target, thumb } => function.calls.push(CallSite {
                    from: address,
                    target,
                    thumb,
                    tail: false,
                    resolution: program.resolve(key.module, target),
                }),
                Flow::IndirectCall => function.indirect_calls.push(address),
                Flow::Jump {
                    target,
                    conditional,
                } => {
                    let conditional = conditional && !always_not_equal(&ins, &history);
                    let internal = module.contains(target)
                        && target >= key.address
                        && (target == key.address
                            || !known.contains(&FunctionKey {
                                module: key.module,
                                address: target,
                            }));
                    if internal {
                        history.push(ins);
                        worklist.push((target, carried(&history)));
                        history.pop();
                    } else {
                        tail_call(&mut function, address, target, thumb);
                    }
                    if !conditional {
                        break;
                    }
                }
                Flow::Return { conditional } => {
                    if !conditional {
                        break;
                    }
                }
                Flow::IndirectJump { conditional } => {
                    history.push(ins);
                    if let Some((target, target_thumb)) = constant_jump(module, &history) {
                        tail_call(&mut function, address, target, target_thumb);
                    } else if popped_return(&history) {
                        // `pop {..., rN}; bx rN`: an interworking return.
                    } else if let Some(table) = jump_table(module, &history) {
                        worklist.extend(table.targets.iter().map(|&target| (target, Vec::new())));
                        function.jump_tables.push(table);
                    } else {
                        function.indirect_jumps.push(address);
                    }
                    if !conditional {
                        break;
                    }
                    address = next;
                    continue;
                }
                Flow::Halt => {
                    function
                        .problems
                        .push(format!("undefined instruction at 0x{address:08X}"));
                    break;
                }
            }
            history.push(ins);
            address = next;
        }
    }

    function.instruction_count = function.instructions.len();
    function.end = function
        .instructions
        .values()
        .map(Instruction::end)
        .max()
        .unwrap_or(key.address);
    function
}

/// A `bne` that is always taken because Z was set from a non-zero address:
///
/// ```text
/// orr r0, pc, #0     ; r0 = address of the inline data below
/// adds r0, r0, #4
/// bne shared_code    ; always taken; data words follow
/// ```
///
/// The compiler emits this for static initializers that pass inline data.
fn always_not_equal(branch: &Instruction, history: &[Instruction]) -> bool {
    if branch.cond != arm::Cond::Ne {
        return false;
    }
    let [.., address_op, flags_op] = history else {
        return false;
    };
    let Op::DataProc {
        op: DpOp::Add | DpOp::Sub,
        set_flags: true,
        rd,
        rn,
        operand: Operand::Imm { value, .. },
    } = flags_op.op
    else {
        return false;
    };
    let from_pc = match address_op.op {
        Op::DataProc {
            op,
            rd: base,
            rn,
            operand,
            ..
        } if !op.is_compare() && base == rn_of(flags_op) => {
            rn == PC && !op.is_move() || matches!(operand, Operand::Reg { rm: PC, .. })
        }
        _ => false,
    };
    // A code address plus or minus a small constant cannot be zero.
    from_pc && rd == rn && value < 0x1000
}

fn rn_of(ins: &Instruction) -> arm::Reg {
    match ins.op {
        Op::DataProc { rn, .. } => rn,
        _ => PC,
    }
}

/// A `pc` write whose target is a constant: a veneer or a state switch.
///
/// - Thumb `bx pc`: continues in ARM state at the next word.
/// - `ldr rN, [pc, #k] ... bx rN` and `ldr pc, [pc, #k]`: jump to a literal,
///   with bit 0 selecting Thumb state.
fn constant_jump(module: &Module, history: &[Instruction]) -> Option<(u32, bool)> {
    let (jump, earlier) = history.split_last()?;
    let literal = |address: u32| {
        let value = module.u32_at(address)?;
        Some((value & !1, value & 1 != 0))
    };
    match jump.op {
        Op::BranchExchange {
            rm: PC,
            link: false,
        } if jump.thumb => Some((jump.pc_value() & !3, false)),
        Op::BranchExchange { rm, link: false } => {
            let writer = last_writer(earlier, rm)?;
            match writer.op {
                Op::Memory {
                    load: true,
                    size: MemSize::Word,
                    ..
                } => literal(writer.pc_relative_address()?),
                _ => None,
            }
        }
        Op::Memory { .. } => literal(jump.pc_relative_address()?),
        _ => None,
    }
}

/// The most recent instruction in `history` that wrote `reg`.
fn last_writer(history: &[Instruction], reg: arm::Reg) -> Option<&Instruction> {
    history
        .iter()
        .rev()
        .find(|ins| ins.written_registers() & (1 << reg.0) != 0)
}

/// `bx rN` where rN holds the return address: popped from the stack, or
/// copied from `lr` (`mov r12, lr ... bx r12`).
fn popped_return(history: &[Instruction]) -> bool {
    let Some((jump, earlier)) = history.split_last() else {
        return false;
    };
    let Op::BranchExchange { rm, link: false } = jump.op else {
        return false;
    };
    let Some(writer) = last_writer(earlier, rm) else {
        return false;
    };
    matches!(
        writer.op,
        Op::Multiple {
            load: true,
            rn: SP,
            ..
        } | Op::Memory {
            load: true,
            rn: SP,
            addressing: Addressing::PostIndex,
            ..
        } | Op::DataProc {
            op: DpOp::Mov,
            operand: Operand::Reg {
                rm: LR,
                kind: ShiftKind::Lsl,
                amount: ShiftAmount::Imm(0),
            },
            ..
        }
    )
}

/// The bound of a switch: `cmp reg, #n` (n + 1 cases), searched backwards.
fn switch_bound(history: &[Instruction]) -> Option<u32> {
    history.iter().rev().find_map(|ins| match ins.op {
        Op::DataProc {
            op: DpOp::Cmp,
            operand: Operand::Imm { value, .. },
            ..
        } if value < MAX_JUMP_TABLE_ENTRIES => Some(value + 1),
        _ => None,
    })
}

/// Recognize the compiler's switch dispatch ending at `history.last()`.
fn jump_table(module: &Module, history: &[Instruction]) -> Option<JumpTable> {
    let (jump, earlier) = history.split_last()?;
    match jump.op {
        // ARM: addls pc, pc, rN, lsl #2 ; b default ; b case0 ; b case1 ...
        Op::DataProc {
            op: DpOp::Add,
            rd: PC,
            rn: PC,
            operand:
                Operand::Reg {
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Imm(2),
                    ..
                },
            ..
        } if !jump.thumb => {
            let count = switch_bound(earlier)?;
            let table = jump.pc_value();
            Some(JumpTable {
                jump: jump.address,
                table,
                targets: (0..count).map(|index| table + 4 * index).collect(),
            })
        }
        // ARM: ldrls pc, [pc, rN, lsl #2] followed by a table of addresses.
        Op::Memory {
            load: true,
            size: MemSize::Word,
            rn: PC,
            offset:
                Offset::Reg {
                    kind: ShiftKind::Lsl,
                    amount: 2,
                    subtract: false,
                    ..
                },
            addressing: Addressing::Offset,
            ..
        } if !jump.thumb => {
            let count = switch_bound(earlier)?;
            let table = jump.pc_value();
            let targets = (0..count)
                .map(|index| module.u32_at(table + 4 * index))
                .collect::<Option<Vec<_>>>()?;
            if targets
                .iter()
                .any(|&target| target & 3 != 0 || !module.contains(target))
            {
                return None;
            }
            Some(JumpTable {
                jump: jump.address,
                table,
                targets,
            })
        }
        // Thumb:
        //   add rX, pc ; ldrh rX, [rX, #k] ; lsl rX, #16 ; asr rX, #16 ; add pc, rX
        // with halfword offsets relative to the `add pc` instruction's pc.
        Op::DataProc {
            op: DpOp::Add,
            rd: PC,
            rn: PC,
            operand:
                Operand::Reg {
                    rm: index_reg,
                    kind: ShiftKind::Lsl,
                    amount: ShiftAmount::Imm(0),
                },
            ..
        } if jump.thumb => {
            let load_at = earlier.iter().rposition(|ins| {
                matches!(ins.op, Op::Memory {
                    load: true,
                    size: MemSize::Half | MemSize::SignedHalf,
                    rt,
                    rn,
                    offset: Offset::Imm(_),
                    ..
                } if rt == index_reg && rn == index_reg)
            })?;
            let Op::Memory {
                offset: Offset::Imm(displacement),
                ..
            } = earlier[load_at].op
            else {
                return None;
            };
            let base = earlier[..load_at]
                .iter()
                .rev()
                .find_map(|ins| match ins.op {
                    Op::DataProc {
                        op: DpOp::Add,
                        rd,
                        operand: Operand::Reg { rm: PC, .. },
                        ..
                    } if rd == index_reg => Some(ins.pc_value()),
                    _ => None,
                })?;
            let count = switch_bound(&earlier[..load_at])?;
            let table = base.wrapping_add(displacement as u32);
            let origin = jump.pc_value();
            let targets = (0..count)
                .map(|index| {
                    let offset = module.u16_at(table + 2 * index)? as i16;
                    Some(origin.wrapping_add(offset as i32 as u32))
                })
                .collect::<Option<Vec<_>>>()?;
            if targets
                .iter()
                .any(|&target| target & 1 != 0 || !module.contains(target))
            {
                return None;
            }
            Some(JumpTable {
                jump: jump.address,
                table,
                targets,
            })
        }
        _ => None,
    }
}

/// Whether `address` starts with a push of `lr` in the given state.
fn has_prologue(module: &Module, address: u32, thumb: bool) -> bool {
    if thumb {
        // push {..., lr}
        module
            .u16_at(address)
            .is_some_and(|half| half & 0xFF00 == 0xB500)
    } else {
        is_arm_prologue(module, address)
    }
}

fn is_arm_prologue(module: &Module, address: u32) -> bool {
    // stmdb sp!, {..., lr}
    module
        .u32_at(address)
        .is_some_and(|word| word & 0xFFFF_4000 == 0xE92D_4000)
}

type Seeds = BTreeMap<FunctionKey, (bool, BTreeSet<Source>)>;
type Candidate = (FunctionKey, bool, Source);

/// Add a seed, returning whether a new function was recorded.
fn add_seed(
    analysis: &mut Analysis,
    seeds: &mut Seeds,
    key: FunctionKey,
    thumb: bool,
    source: Source,
) -> bool {
    if source.is_weak() && analysis.absorbed.contains(&key) {
        return false;
    }
    let is_new = !seeds.contains_key(&key);
    let entry = seeds.entry(key).or_insert((thumb, BTreeSet::new()));
    if entry.0 != thumb {
        if !analysis.state_conflicts.contains(&key) {
            analysis.state_conflicts.push(key);
        }
        return false;
    }
    entry.1.insert(source);
    is_new
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Follow calls and literal-pool pointers.
    Calls,
    /// Also scan data for code pointers.
    DataScan,
    /// Re-traverse every function with the complete set of entry points.
    Final,
}

/// Discover every function reachable in the program.
pub fn analyze(program: &Program) -> Analysis {
    let mut analysis = Analysis::default();
    let mut seeds = Seeds::new();
    let entry = FunctionKey {
        module: ModuleId::Main,
        address: program.entry,
    };
    add_seed(&mut analysis, &mut seeds, entry, false, Source::Entry);
    for module in &program.modules {
        let Some((start, end)) = module.sinit else {
            continue;
        };
        for slot in (start..end).step_by(4) {
            let Some(pointer) = module.u32_at(slot) else {
                continue;
            };
            if let Resolution::Module(target) = program.resolve(module.id, pointer & !1) {
                let key = FunctionKey {
                    module: target,
                    address: pointer & !1,
                };
                add_seed(
                    &mut analysis,
                    &mut seeds,
                    key,
                    pointer & 1 != 0,
                    Source::Sinit,
                );
            }
        }
    }

    let mut rejected: HashSet<FunctionKey> = HashSet::new();
    let mut phase = Phase::Calls;
    for round in 1..=MAX_ROUNDS {
        analysis.rounds = round;
        let known: HashSet<FunctionKey> = seeds.keys().copied().collect();
        for (&key, &(thumb, _)) in &seeds {
            let current = analysis
                .functions
                .get(&key)
                .is_some_and(|function| function.thumb == thumb);
            if current && phase != Phase::Final {
                continue;
            }
            let function = analyze_function(program, key, thumb, &known);
            analysis.functions.insert(key, function);
        }

        // Fold weakly seeded entry points back into the function that runs
        // into them; both are traversed again next round.
        let mut grew = false;
        let mut absorb = Vec::new();
        for function in analysis.functions.values() {
            for &address in &function.falls_into {
                let target = FunctionKey {
                    module: function.module,
                    address,
                };
                if seeds
                    .get(&target)
                    .is_some_and(|(_, sources)| sources.iter().all(|source| source.is_weak()))
                {
                    absorb.push((function.key(), target));
                }
            }
        }
        for (outer, inner) in absorb {
            seeds.remove(&inner);
            analysis.functions.remove(&inner);
            analysis.functions.remove(&outer);
            analysis.absorbed.insert(inner);
            grew = true;
        }

        let mut calls: Vec<Candidate> = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        for function in analysis.functions.values() {
            for call in &function.calls {
                if let Resolution::Module(module) = call.resolution {
                    let source = if call.tail {
                        Source::TailCall
                    } else {
                        Source::Call
                    };
                    let key = FunctionKey {
                        module,
                        address: call.target,
                    };
                    calls.push((key, call.thumb, source));
                }
                // Which overlay is loaded is only known at run time; still
                // discover each candidate that starts like a function.
                if let Resolution::Ambiguous(modules) = &call.resolution {
                    for &module in modules {
                        let target = program.module(module).unwrap();
                        if has_prologue(target, call.target, call.thumb) {
                            let key = FunctionKey {
                                module,
                                address: call.target,
                            };
                            candidates.push((key, call.thumb, Source::AmbiguousCall));
                        }
                    }
                }
            }
            let module = program.module(function.module).unwrap();
            for &literal in &function.literals {
                if let Some(pointer) = module.u32_at(literal) {
                    candidates.extend(pointer_candidate(
                        program,
                        function.module,
                        pointer,
                        Source::LiteralPointer,
                    ));
                }
            }
        }
        for (key, thumb, source) in calls {
            grew |= add_seed(&mut analysis, &mut seeds, key, thumb, source);
        }
        if phase == Phase::DataScan {
            candidates.extend(scan_data_pointers(program, &analysis));
        }
        for (key, thumb, source) in
            validate_candidates(program, &analysis, &seeds, &mut rejected, candidates)
        {
            grew |= add_seed(&mut analysis, &mut seeds, key, thumb, source);
        }

        phase = match (grew, phase) {
            (true, _) => Phase::Calls,
            (false, Phase::Calls) => Phase::DataScan,
            (false, Phase::DataScan) => Phase::Final,
            (false, Phase::Final) => break,
        };
    }

    // A seed that lost a state conflict keeps its first state.
    analysis.functions.retain(|key, _| seeds.contains_key(key));
    for (key, (_, sources)) in seeds {
        if let Some(function) = analysis.functions.get_mut(&key) {
            function.sources = sources;
        }
    }
    analysis
}

fn pointer_candidate(
    program: &Program,
    from: ModuleId,
    pointer: u32,
    source: Source,
) -> Option<Candidate> {
    let thumb = pointer & 1 != 0;
    let address = pointer & !1;
    // Cheap rejection before resolving: ARM9 code lives in ITCM or main RAM.
    if !(0x01FF_8000..0x0240_0000).contains(&address) || (!thumb && address & 3 != 0) {
        return None;
    }
    let Resolution::Module(module) = program.resolve(from, address) else {
        return None;
    };
    let target = program.module(module)?;
    if !thumb && (address & 3 != 0 || !is_arm_prologue(target, address)) {
        return None;
    }
    Some((FunctionKey { module, address }, thumb, source))
}

/// Words in each module, outside decoded code, that look like code pointers.
fn scan_data_pointers(program: &Program, analysis: &Analysis) -> Vec<Candidate> {
    let code = code_addresses(analysis);
    let mut candidates = Vec::new();
    for module in &program.modules {
        let mut address = module.base;
        while address + 4 <= module.end() {
            if !code.contains(&(module.id, address)) && !code.contains(&(module.id, address + 2)) {
                let pointer = module.u32_at(address).unwrap_or(0);
                candidates.extend(pointer_candidate(
                    program,
                    module.id,
                    pointer,
                    Source::DataPointer,
                ));
            }
            address += 4;
        }
    }
    candidates
}

fn code_addresses(analysis: &Analysis) -> HashSet<(ModuleId, u32)> {
    let mut code = HashSet::new();
    for function in analysis.functions.values() {
        for ins in function.instructions.values() {
            code.insert((function.module, ins.address));
            if ins.size == 4 {
                code.insert((function.module, ins.address + 2));
            }
        }
    }
    code
}

/// Accept pointer candidates that decode as a clean function and do not land
/// inside code that is already known.
fn validate_candidates(
    program: &Program,
    analysis: &Analysis,
    seeds: &Seeds,
    rejected: &mut HashSet<FunctionKey>,
    candidates: Vec<Candidate>,
) -> Vec<Candidate> {
    let fresh: Vec<_> = candidates
        .into_iter()
        .filter(|(key, _, _)| !seeds.contains_key(key) && !rejected.contains(key))
        .collect();
    if fresh.is_empty() {
        return Vec::new();
    }
    let code = code_addresses(analysis);
    let known: HashSet<FunctionKey> = seeds.keys().copied().collect();
    let mut accepted = Vec::new();
    let mut seen = HashSet::new();
    for (key, thumb, source) in fresh {
        if !seen.insert(key) {
            continue;
        }
        let function = analyze_function(program, key, thumb, &known);
        let clean = function.problems.is_empty()
            && !code.contains(&(key.module, key.address))
            && function.instruction_count >= 2;
        if clean {
            accepted.push((key, thumb, source));
        } else {
            rejected.insert(key);
        }
    }
    accepted
}

/// Coverage of one module: bytes decoded as instructions.
pub fn module_code_bytes(analysis: &Analysis, module: ModuleId) -> u32 {
    analysis
        .functions
        .values()
        .filter(|function| function.module == module)
        .map(Function::code_bytes)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(code: &[u16]) -> Program {
        let bytes = code.iter().flat_map(|half| half.to_le_bytes()).collect();
        Program {
            entry: 0x0200_0000,
            modules: vec![Module {
                id: ModuleId::Main,
                base: 0x0200_0000,
                bytes,
                bss_size: 0,
                sinit: None,
            }],
        }
    }

    fn key(address: u32) -> FunctionKey {
        FunctionKey {
            module: ModuleId::Main,
            address,
        }
    }

    #[test]
    fn follows_branches_and_calls_in_thumb() {
        // 0x00: push {r4, lr}
        // 0x02: cmp r0, #0
        // 0x04: beq 0x0C
        // 0x06: bl 0x10
        // 0x0A: pop {r4, pc}
        // 0x0C: movs r0, #1
        // 0x0E: pop {r4, pc}
        // 0x10: bx lr
        let program = program(&[
            0xB510, 0x2800, 0xD002, 0xF000, 0xF803, 0xBD10, 0x2001, 0xBD10, 0x4770,
        ]);
        let function = analyze_function(&program, key(0x0200_0000), true, &HashSet::new());
        assert!(function.problems.is_empty(), "{:?}", function.problems);
        assert_eq!(function.instruction_count, 7);
        assert_eq!(function.end, 0x0200_0010);
        assert_eq!(function.calls.len(), 1);
        assert_eq!(function.calls[0].target, 0x0200_0010);
        assert!(function.calls[0].thumb);
    }

    #[test]
    fn decodes_thumb_jump_table() {
        let program = program(&[
            0x2802, // 0x00: cmp r0, #2
            0xD808, // 0x02: bhi 0x16
            0x0040, // 0x04: lsls r0, r0, #1
            0x4478, // 0x06: add r0, pc (pc = 0x0A)
            0x88C0, // 0x08: ldrh r0, [r0, #6] (table at 0x10)
            0x0400, // 0x0A: lsls r0, r0, #16
            0x1400, // 0x0C: asrs r0, r0, #16
            0x4487, // 0x0E: add pc, r0 (pc = 0x12)
            0x0004, // 0x10: case 0 -> 0x16
            0x0006, // 0x12: case 1 -> 0x18
            0x0008, // 0x14: case 2 -> 0x1A
            0x4770, // 0x16: bx lr
            0x4770, // 0x18: bx lr
            0x4770, // 0x1A: bx lr
        ]);
        let function = analyze_function(&program, key(0x0200_0000), true, &HashSet::new());
        assert_eq!(function.jump_tables.len(), 1, "{:?}", function.problems);
        let table = &function.jump_tables[0];
        assert_eq!(table.table, 0x0200_0010);
        assert_eq!(table.targets, vec![0x0200_0016, 0x0200_0018, 0x0200_001A]);
        assert!(function.indirect_jumps.is_empty());
    }

    #[test]
    fn stops_at_known_function_and_records_tail_call() {
        // 0x00: movs r0, #0 ; 0x02: b 0x06 ; 0x04: bx lr ; 0x06: bx lr
        let program = program(&[0x2000, 0xE000, 0x4770, 0x4770]);
        let known: HashSet<_> = [key(0x0200_0000), key(0x0200_0006)].into();
        let function = analyze_function(&program, key(0x0200_0000), true, &known);
        assert_eq!(function.instruction_count, 2);
        assert!(
            function
                .calls
                .iter()
                .any(|call| call.tail && call.target == 0x0200_0006)
        );
    }

    fn arm_program(words: &[u32]) -> Program {
        let halves: Vec<u16> = words
            .iter()
            .flat_map(|word| [*word as u16, (*word >> 16) as u16])
            .collect();
        program(&halves)
    }

    #[test]
    fn treats_pc_derived_bne_as_always_taken() {
        let program = arm_program(&[
            0xE38F_0000, // orr r0, pc, #0
            0xE290_0004, // adds r0, r0, #4
            0x1A00_0001, // bne 0x14
            0xFFFF_FFFF, // inline data
            0xFFFF_FFFF, // inline data
            0xE12F_FF1E, // 0x14: bx lr
        ]);
        let function = analyze_function(&program, key(0x0200_0000), false, &HashSet::new());
        assert!(function.problems.is_empty(), "{:?}", function.problems);
        assert_eq!(function.instruction_count, 4);
    }

    #[test]
    fn resolves_literal_veneers_and_interworking_returns() {
        // ldr r3, [pc, #4] ; movs r0, #0 ; bx r3 ; (pad) ; .word 0x02000011
        let veneer_program = program(&[0x4B01, 0x2000, 0x4718, 0x0000, 0x0011, 0x0200, 0x4770]);
        let veneer = analyze_function(&veneer_program, key(0x0200_0000), true, &HashSet::new());
        assert!(veneer.indirect_jumps.is_empty());
        let call = veneer.calls.iter().find(|call| call.tail).unwrap();
        assert_eq!((call.target, call.thumb), (0x0200_0010, true));

        // push {r3, lr} ; pop {r3} ; bx r3
        let return_program = program(&[0xB508, 0xBC08, 0x4718]);
        let function = analyze_function(&return_program, key(0x0200_0000), true, &HashSet::new());
        assert!(function.indirect_jumps.is_empty());
        assert!(function.calls.is_empty());
    }

    #[test]
    fn discovers_callees_from_entry() {
        // ARM entry: bl 0x08 ; b . ; 0x08: bx lr
        let words: [u32; 3] = [0xEB00_0000, 0xEAFF_FFFE, 0xE12F_FF1E];
        let halves: Vec<u16> = words
            .iter()
            .flat_map(|word| [*word as u16, (*word >> 16) as u16])
            .collect();
        let program = program(&halves);
        let analysis = analyze(&program);
        assert!(analysis.functions.contains_key(&key(0x0200_0008)));
        assert_eq!(analysis.functions[&key(0x0200_0000)].instruction_count, 2);
    }
}
