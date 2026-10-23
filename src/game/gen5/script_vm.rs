use crate::Result;
use crate::formats::{gen5_script, gen5_text};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default)]
pub struct FieldScriptState {
    work: HashMap<u16, u32>,
    flags: HashSet<u16>,
}

/// A deliberately small, resumable Gen V field-script interpreter. Unsupported
/// commands fail with their original opcode and byte offset, so the VM can grow
/// as more retail scripts are brought into the native runtime.
pub struct FieldScriptVm {
    script_id: u16,
    bytecode: Vec<u8>,
    messages: Vec<String>,
    pc: usize,
    current_message: Option<String>,
    waiting_for_input: bool,
    ended: bool,
    stack: Vec<u32>,
    state: FieldScriptState,
    return_stack: Vec<usize>,
    stack_comparison: Option<bool>,
    last_ordering: Option<Ordering>,
}

impl FieldScriptVm {
    pub fn start(
        script_bank: &[u8],
        script_id: u16,
        message_bank: &[u8],
        state: &FieldScriptState,
    ) -> Result<Self> {
        let pc = gen5_script::script_offset(script_bank, script_id)?;
        let messages = gen5_text::parse_message_bank(message_bank)?;
        let mut vm = Self {
            script_id,
            bytecode: script_bank.to_vec(),
            messages,
            pc,
            current_message: None,
            waiting_for_input: false,
            ended: false,
            stack: Vec::new(),
            state: state.clone(),
            return_stack: Vec::new(),
            stack_comparison: None,
            last_ordering: None,
        };
        vm.run_until_yield()?;
        Ok(vm)
    }

    pub fn press_action(&mut self) -> Result<()> {
        if self.ended || !self.waiting_for_input {
            return Ok(());
        }
        self.waiting_for_input = false;
        self.run_until_yield()
    }

    pub fn dialogue(&self) -> Option<&str> {
        self.current_message.as_deref()
    }

    pub fn is_waiting_for_input(&self) -> bool {
        self.waiting_for_input
    }

    pub fn is_ended(&self) -> bool {
        self.ended
    }

    pub fn state(&self) -> &FieldScriptState {
        &self.state
    }

    fn run_until_yield(&mut self) -> Result<()> {
        // Guard against a corrupt jump or accidental tight loop while keeping
        // normal field scripts free to execute many commands in one frame.
        for _ in 0..4096 {
            let command_offset = self.pc;
            let command = self.read_u16()?;
            tracing::trace!(
                script_id = self.script_id,
                command_offset = format_args!("0x{command_offset:X}"),
                command = format_args!("0x{command:04X}"),
                "field script command"
            );
            match command {
                0x0000 | 0x0001 | 0x002E | 0x002F | 0x0030 | 0x0074 => {}
                0x0002 => {
                    self.ended = true;
                    self.current_message = None;
                    return Ok(());
                }
                0x0004 => {
                    let relative = self.read_i32()?;
                    self.return_stack.push(self.pc);
                    self.jump_relative(relative)?;
                }
                0x0005 => match self.return_stack.pop() {
                    Some(return_pc) => self.pc = return_pc,
                    None => {
                        self.ended = true;
                        self.current_message = None;
                        return Ok(());
                    }
                },
                0x0008 => {
                    let value = self.read_u16()?;
                    self.stack.push(u32::from(value));
                }
                0x0009 => {
                    let value = self.read_u16()?;
                    self.stack.push(self.value_of(value));
                }
                0x000A => {
                    let variable = self.read_u16()?;
                    let value = self.stack.pop().unwrap_or_default();
                    self.state.work.insert(variable, value);
                }
                0x000B => {
                    self.stack.pop();
                }
                0x000C..=0x000F => self.run_stack_arithmetic(command)?,
                0x0010 => {
                    let flag = self.read_u16()?;
                    self.stack.push(u32::from(self.state.flags.contains(&flag)));
                }
                0x0011 => {
                    let comparison = self.read_u16()?;
                    let right = self.stack.pop().unwrap_or_default();
                    let left = self.stack.pop().unwrap_or_default();
                    let ordering = left.cmp(&right);
                    self.last_ordering = Some(ordering);
                    self.stack_comparison = Some(compare_ordering(ordering, comparison));
                }
                0x0012 | 0x0013 => {
                    let variable = self.read_u16()?;
                    let operand = self.read_u16()?;
                    let left = self.state.work.get(&variable).copied().unwrap_or_default();
                    let right = self.value_of(operand);
                    self.state.work.insert(
                        variable,
                        if command == 0x0012 {
                            left & right
                        } else {
                            left | right
                        },
                    );
                }
                0x0019 => {
                    let variable = self.read_u16()?;
                    let value = u32::from(self.read_u16()?);
                    let left = self.state.work.get(&variable).copied().unwrap_or_default();
                    let ordering = left.cmp(&value);
                    self.last_ordering = Some(ordering);
                    self.stack_comparison = Some(ordering == Ordering::Equal);
                }
                0x001A => {
                    let left = self.read_u16()?;
                    let right = self.read_u16()?;
                    let left_value = self.state.work.get(&left).copied().unwrap_or_default();
                    let right_value = self.state.work.get(&right).copied().unwrap_or_default();
                    let ordering = left_value.cmp(&right_value);
                    self.last_ordering = Some(ordering);
                    self.stack_comparison = Some(ordering == Ordering::Equal);
                }
                0x001E => {
                    let relative = self.read_i32()?;
                    self.jump_relative(relative)?;
                }
                0x001F => {
                    let condition = self.read_u8()?;
                    let relative = self.read_i32()?;
                    if self.condition_matches(condition) {
                        self.jump_relative(relative)?;
                    }
                }
                0x0023 => {
                    let flag = self.read_u16()?;
                    self.state.flags.insert(flag);
                }
                0x0024 => {
                    let flag = self.read_u16()?;
                    self.state.flags.remove(&flag);
                }
                0x0025 => {
                    let flag = self.read_u16()?;
                    let variable = self.read_u16()?;
                    let value = u32::from(self.state.flags.contains(&flag));
                    self.state.work.insert(variable, value);
                }
                0x0026..=0x002D => self.run_work_operation(command)?,
                0x0032 => {
                    self.waiting_for_input = true;
                    return Ok(());
                }
                0x003C => {
                    let _args = self.read_bytes(10)?;
                    let message_id = u16::from_le_bytes([_args[2], _args[3]]);
                    self.open_message(message_id)?;
                }
                0x003D => {
                    let _args = self.read_bytes(8)?;
                    let message_id = u16::from_le_bytes([_args[2], _args[3]]);
                    self.open_message(message_id)?;
                }
                0x003E | 0x003F => self.current_message = None,
                0x00A6 => {
                    let _sound_id = self.read_u16()?;
                }
                0x0290 => {
                    // This retail field command's single byte selects a
                    // message mode. The dialogue slice needs its size to walk
                    // the original NPC script; audio itself is handled later.
                    let _mode = self.read_u8()?;
                }
                _ => {
                    return Err(format!(
                        "unsupported Gen V field-script command 0x{command:04X} at 0x{command_offset:X}"
                    ));
                }
            }
        }
        Err("Gen V field script exceeded the 4096-command frame budget".to_owned())
    }

    fn open_message(&mut self, message_id: u16) -> Result<()> {
        let message = self.messages.get(usize::from(message_id)).ok_or_else(|| {
            format!(
                "field script requested message {message_id}, but the linked bank has {} messages",
                self.messages.len()
            )
        })?;
        tracing::debug!(
            script_id = self.script_id,
            message_id,
            message = %message,
            "field script opened message"
        );
        self.current_message = Some(message.clone());
        Ok(())
    }

    fn value_of(&self, value: u16) -> u32 {
        if is_work_variable(value) {
            self.state.work.get(&value).copied().unwrap_or_default()
        } else {
            u32::from(value)
        }
    }

    fn run_stack_arithmetic(&mut self, command: u16) -> Result<()> {
        let right = self.stack.pop().unwrap_or_default();
        let left = self.stack.pop().unwrap_or_default();
        let value = match command {
            0x000C => left.wrapping_add(right),
            0x000D => left.wrapping_sub(right),
            0x000E => left.wrapping_mul(right),
            0x000F if right != 0 => left / right,
            0x000F => 0,
            _ => unreachable!("only stack arithmetic opcodes reach this helper"),
        };
        self.stack.push(value);
        Ok(())
    }

    fn run_work_operation(&mut self, command: u16) -> Result<()> {
        let variable = self.read_u16()?;
        let operand = self.read_u16()?;
        let left = self.state.work.get(&variable).copied().unwrap_or_default();
        let right = self.value_of(operand);
        let value = match command {
            0x0026 => left.wrapping_add(right),
            0x0027 => left.wrapping_sub(right),
            0x0028 => u32::from(operand),
            0x0029 => self.state.work.get(&operand).copied().unwrap_or_default(),
            0x002A => right,
            0x002B => left.wrapping_mul(right),
            0x002C if right != 0 => left / right,
            0x002C => 0,
            0x002D if right != 0 => left % right,
            0x002D => 0,
            _ => unreachable!("only work-variable opcodes reach this helper"),
        };
        self.state.work.insert(variable, value);
        Ok(())
    }

    fn condition_matches(&self, condition: u8) -> bool {
        match condition {
            0xFF => self.stack_comparison.unwrap_or(false),
            comparison => self
                .last_ordering
                .is_some_and(|ordering| compare_ordering(ordering, u16::from(comparison))),
        }
    }

    fn jump_relative(&mut self, relative: i32) -> Result<()> {
        let target = i64::try_from(self.pc)
            .map_err(|_| "field-script program counter does not fit a signed offset".to_owned())?
            + i64::from(relative);
        let target = usize::try_from(target)
            .map_err(|_| "field-script jump targets a negative offset".to_owned())?;
        if target >= self.bytecode.len() {
            return Err(format!(
                "field-script jump targets 0x{target:X}, past its {} byte bank",
                self.bytecode.len()
            ));
        }
        self.pc = target;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        let value = *self
            .bytecode
            .get(self.pc)
            .ok_or_else(|| "field script read past the end of its bank".to_owned())?;
        self.pc += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_i32(&mut self) -> Result<i32> {
        let bytes = self.read_bytes(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_bytes(&mut self, length: usize) -> Result<&[u8]> {
        let end = self
            .pc
            .checked_add(length)
            .ok_or_else(|| "field script operand range overflowed".to_owned())?;
        let value = self
            .bytecode
            .get(self.pc..end)
            .ok_or_else(|| "field script operand extends past its bank".to_owned())?;
        self.pc = end;
        Ok(value)
    }
}

fn is_work_variable(value: u16) -> bool {
    (0x4000..0xC000).contains(&value)
}

fn compare_ordering(ordering: Ordering, comparison: u16) -> bool {
    match comparison {
        0 => ordering == Ordering::Equal,
        1 => ordering != Ordering::Equal,
        2 => ordering == Ordering::Less,
        3 => ordering != Ordering::Greater,
        4 => ordering == Ordering::Greater,
        5 => ordering != Ordering::Less,
        _ => false,
    }
}
