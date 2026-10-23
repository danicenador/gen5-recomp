use crate::Result;
use crate::formats::{gen5_script, gen5_text};

/// A deliberately small, resumable Gen V field-script interpreter. Unsupported
/// commands fail with their original opcode and byte offset, so the VM can grow
/// as more retail scripts are brought into the native runtime.
pub struct FieldScriptVm {
    bytecode: Vec<u8>,
    messages: Vec<String>,
    pc: usize,
    current_message: Option<String>,
    waiting_for_input: bool,
    ended: bool,
}

impl FieldScriptVm {
    pub fn start(script_bank: &[u8], script_id: u16, message_bank: &[u8]) -> Result<Self> {
        let pc = gen5_script::script_offset(script_bank, script_id)?;
        let messages = gen5_text::parse_message_bank(message_bank)?;
        let mut vm = Self {
            bytecode: script_bank.to_vec(),
            messages,
            pc,
            current_message: None,
            waiting_for_input: false,
            ended: false,
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

    fn run_until_yield(&mut self) -> Result<()> {
        // Guard against a corrupt jump or accidental tight loop while keeping
        // normal field scripts free to execute many commands in one frame.
        for _ in 0..4096 {
            let command_offset = self.pc;
            let command = self.read_u16()?;
            match command {
                0x0000 | 0x0001 | 0x002E | 0x002F | 0x0030 | 0x0074 => {}
                0x0002 => {
                    self.ended = true;
                    self.current_message = None;
                    return Ok(());
                }
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
        self.current_message = Some(message.clone());
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
