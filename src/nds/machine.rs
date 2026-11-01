//! Runs the recompiled ARM9 code: loads the game library, sets up memory as
//! the DS boot process leaves it, and services every `rt_*` hook.
//!
//! Generated code calls back into the host while host code calls into
//! generated code, so the machine is reached through the raw pointer stored
//! in `Cpu::runtime`. Functions that call into generated code take the CPU
//! pointer and only borrow the machine between those calls.

use super::backup::{self, Backup};
use super::card::{self, Card};
use super::cpu::{Banks, Cpu, MODE_IRQ, MODE_SVC, MODE_SYSTEM};
use super::firmware::{self, Firmware};
use super::io::{self, Effect, Io};
use super::memory::Memory;
use crate::Result;
use crate::importer::code::CodeManifest;
use crate::recomp::program::{ModuleId, Program};
use libloading::Library;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

type GeneratedFunction = unsafe extern "C" fn(*mut Cpu);

/// Must match `Gen5FunctionEntry` in `gen5rt.h`.
#[repr(C)]
struct FunctionEntry {
    module: u16,
    thumb: u16,
    address: u32,
    function: GeneratedFunction,
}

/// Must match `Gen5Host` in `gen5host.c`.
#[repr(C)]
struct Host {
    read8: extern "C" fn(*mut Cpu, u32) -> u32,
    read16: extern "C" fn(*mut Cpu, u32) -> u32,
    read32: extern "C" fn(*mut Cpu, u32) -> u32,
    write8: extern "C" fn(*mut Cpu, u32, u32),
    write16: extern "C" fn(*mut Cpu, u32, u32),
    write32: extern "C" fn(*mut Cpu, u32, u32),
    call: extern "C" fn(*mut Cpu, u32),
    jump: extern "C" fn(*mut Cpu, u32, u32),
    swi: extern "C" fn(*mut Cpu, u32, u32),
    mcr: extern "C" fn(*mut Cpu, u32, u32, u32, u32, u32, u32),
    mrc: extern "C" fn(*mut Cpu, u32, u32, u32, u32, u32) -> u32,
    read_psr: extern "C" fn(*mut Cpu, u32) -> u32,
    write_psr: extern "C" fn(*mut Cpu, u32, u32, u32),
    exception_return: extern "C" fn(*mut Cpu, u32),
    undefined: extern "C" fn(*mut Cpu, u32, u32),
    hle: extern "C" fn(*mut Cpu, u32),
    tick: extern "C" fn(*mut Cpu),
}

#[derive(Debug, Clone, Copy)]
struct Target {
    module: u16,
    thumb: bool,
    function: GeneratedFunction,
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Call {
        target: u32,
        lr: u32,
    },
    Jump {
        target: u32,
        from: u32,
    },
    Swi(u32),
    Irq(u32),
    Read {
        address: u32,
        value: u32,
    },
    Write {
        address: u32,
        value: u32,
    },
    Cp15 {
        crn: u32,
        crm: u32,
        opc2: u32,
        value: u32,
    },
    Psr {
        value: u32,
        fields: u32,
    },
    FifoSend(u32),
    Interpret {
        target: u32,
        lr: u32,
    },
    /// A request the ARM7 stand-in acknowledged without doing anything.
    Unhandled {
        tag: u32,
        command: u32,
    },
    Backup {
        command: u32,
        offset: u32,
        length: u32,
        result: u32,
    },
}

pub struct Options {
    /// Stop after this many frames (vertical blanks).
    pub max_frames: u64,
    /// Stop with a report after this much wall-clock time.
    pub seconds: u64,
}

pub struct Machine {
    cpu: *mut Cpu,
    pub memory: Memory,
    pub io: Io,
    pub banks: Banks,
    cp15_control: u32,
    cp15_dtcm: u32,
    cp15_itcm: u32,
    functions: HashMap<u32, Vec<Target>>,
    overlays: HashMap<u16, (u32, Vec<u8>)>,
    events: VecDeque<Event>,
    calls: u64,
    poll: (u32, u64),
    /// Set by the IRQ handler's thread switch; done after the interrupt.
    pub(super) pending_switch: Option<u32>,
    /// Runtime callbacks since boot; display time advances with them.
    activity: u64,
    card: Card,
    /// Every word the ARM9 sent through the IPC FIFO, for reports.
    fifo_sent: Vec<(u32, u32)>,
    /// Replies the ARM7 stand-in sent, as (tag, data), for reports.
    fifo_replies: Vec<(u32, u32)>,
    /// Command of the PM packet being received.
    arm7_pm_command: u32,
    arm7_tp_command: u32,
    /// Firmware flash packet being received.
    nvram_command: u32,
    nvram_bytes: Vec<u8>,
    /// The shared card backup command block, set by the INIT request.
    backup_block: u32,
    backup_expects_block: bool,
    backup: Backup,
    firmware: Firmware,
    /// The ARM7 stand-in's IPCSYNC output nibble.
    arm7_sync_output: u32,
    /// Progress through the shared-RAM init handshake at 0x027FFFF0.
    arm7_init_phase: u8,
    options: Options,
    _library: Library,
}

const EVENT_HISTORY: usize = 400;
const ACTIVITY_PER_LINE: u64 = 32;
/// Backward branches between `rt_tick` calls.
const LOOP_BUDGET: u32 = 2000;
const PXI_TAG_PM: u32 = 8;
const PXI_TAG_TOUCHPANEL: u32 = 6;
const PXI_TAG_NVRAM: u32 = 4;
const PXI_TAG_RTC: u32 = 5;
/// Where the ARM7 leaves the RTC date and time (BCD) for the SDK.
const RTC_DATE: u32 = 0x027F_FDE8;
const RTC_TIME: u32 = 0x027F_FDEC;
/// Card backup requests travel on the FS tag.
const PXI_TAG_FS: u32 = 11;

/// Shared-RAM words of the ARM9/ARM7 boot handshake.
const ARM7_BOOT_COUNTER: u32 = 0x027F_FC26;
const ARM7_BOOT_PROGRESS: u32 = 0x027F_FC28;
const ARM7_INIT_SYNC: u32 = 0x027F_FFF0;
/// Bit mask of PXI FIFO tags the ARM7 has registered handlers for.
const ARM7_PXI_READY: u32 = 0x027F_FF8C;
/// Where the firmware's user settings are copied at boot.
const USER_SETTINGS_COPY: u32 = 0x027F_FC80;
const POLL_LIMIT: u64 = 20_000_000;
const WAIT_LIMIT_FRAMES: u64 = 600;

/// SAFETY: `cpu.runtime` points at the boxed machine for the whole run.
unsafe fn machine<'a>(cpu: *mut Cpu) -> &'a mut Machine {
    unsafe { &mut *((*cpu).runtime as *mut Machine) }
}

/// The machine a CPU belongs to, for the interpreter.
///
/// # Safety
/// `cpu` must be the CPU of a booted machine; the borrow must end before
/// control passes to generated code.
pub(super) unsafe fn get<'a>(cpu: *mut Cpu) -> &'a mut Machine {
    unsafe { machine(cpu) }
}

impl Machine {
    fn record(&mut self, event: Event) {
        if self.events.len() == EVENT_HISTORY {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn cpu(&mut self) -> &mut Cpu {
        // SAFETY: the CPU outlives the machine and is only touched from the
        // single thread running the game.
        unsafe { &mut *self.cpu }
    }

    // --- Memory access for host code (BIOS functions, DMA) -------------

    pub fn read8(&mut self, address: u32) -> u32 {
        match self.memory.byte(address, false) {
            // SAFETY: mapped pages point into live buffers.
            Some(byte) => u32::from(unsafe { *byte }),
            None => self.read_slow(address, 1),
        }
    }

    pub fn read16(&mut self, address: u32) -> u32 {
        let address = address & !1;
        match self.memory.byte(address, false) {
            Some(byte) => u32::from(unsafe { (byte as *const u16).read_unaligned() }),
            None => self.read_slow(address, 2),
        }
    }

    pub fn read32(&mut self, address: u32) -> u32 {
        let address = address & !3;
        match self.memory.byte(address, false) {
            Some(byte) => unsafe { (byte as *const u32).read_unaligned() },
            None => self.read_slow(address, 4),
        }
    }

    pub fn write8(&mut self, address: u32, value: u32) {
        match self.memory.byte(address, true) {
            Some(byte) => unsafe { *byte = value as u8 },
            None => self.write_slow(address, 1, value),
        }
    }

    pub fn write16(&mut self, address: u32, value: u32) {
        let address = address & !1;
        match self.memory.byte(address, true) {
            Some(byte) => unsafe { (byte as *mut u16).write_unaligned(value as u16) },
            None => self.write_slow(address, 2, value),
        }
    }

    pub fn write32(&mut self, address: u32, value: u32) {
        let address = address & !3;
        match self.memory.byte(address, true) {
            Some(byte) => unsafe { (byte as *mut u32).write_unaligned(value) },
            None => self.write_slow(address, 4, value),
        }
    }

    fn is_io(address: u32) -> bool {
        (0x0400_0000..0x0400_2000).contains(&address)
            || (io::IPCFIFORECV..io::IPCFIFORECV + 4).contains(&address)
            || (card::DATA..card::DATA + 4).contains(&address)
    }

    fn read_slow(&mut self, address: u32, size: u32) -> u32 {
        if address & !3 == card::DATA {
            let word = self.card.next_word();
            if !self.card.has_data() {
                self.finish_card_transfer();
            }
            return word;
        }
        if Self::is_io(address) {
            self.tick();
            let value = self.io.read(address, size);
            if self.poll.0 == address {
                self.poll.1 += 1;
                if self.poll.1 > POLL_LIMIT {
                    self.stop(&format!(
                        "stuck polling I/O register 0x{address:08X} (last value 0x{value:X})"
                    ));
                }
            } else {
                self.arm7_tick();
                self.poll = (address, 1);
                self.record(Event::Read { address, value });
            }
            return value;
        }
        // GBA slot and unmapped space read as open bus.
        if (0x0800_0000..0x0B00_0000).contains(&address) {
            return if size == 1 {
                0xFF
            } else {
                0xFFFF_FFFF >> (32 - 8 * size)
            };
        }
        0
    }

    /// Advance display time with runtime activity, so code that busy-waits
    /// without polling the display still sees lines and frames go by.
    pub(super) fn tick(&mut self) {
        self.activity += 1;
        if self.activity.is_multiple_of(ACTIVITY_PER_LINE) {
            let frame = self.io.frame;
            self.io.set_line(self.io.line + 1);
            if self.io.frame != frame && self.io.frame >= self.options.max_frames {
                let frames = self.io.frame;
                self.stop(&format!("reached frame {frames}"));
            }
        }
    }

    fn write_slow(&mut self, address: u32, size: u32, value: u32) {
        if !Self::is_io(address) {
            return;
        }
        self.poll = (0, 0);
        self.record(Event::Write { address, value });
        match self.io.write(address, size, value) {
            Some(Effect::Dma(channel)) => self.run_dma(channel),
            Some(Effect::FifoSend(word)) => {
                self.record(Event::FifoSend(word));
                self.arm7_fifo(word);
            }
            Some(Effect::Sync(value)) => self.arm7_sync(value),
            None => {}
        }
        if address & !3 == card::ROMCTRL && size == 4 && value & card::ROMCTRL_BUSY != 0 {
            self.start_card_transfer(value);
        }
    }

    fn start_card_transfer(&mut self, control: u32) {
        let command: [u8; 8] =
            std::array::from_fn(|index| self.io.get8(card::COMMAND + index as u32));
        if let Err(unsupported) = self.card.start(command, control) {
            self.stop(&format!("unsupported {unsupported}"));
        }
        if !self.card.has_data() {
            self.finish_card_transfer();
            return;
        }
        self.io.set32(card::ROMCTRL, control | card::ROMCTRL_READY);
        // A DMA channel waiting on the card takes the whole block.
        for channel in 0..4 {
            let base = io::DMA_BASE + 12 * channel;
            let dma = self.io.get32(base + 8);
            if dma & 0x8000_0000 == 0 || (dma >> 27) & 7 != 5 {
                continue;
            }
            let mut destination = self.io.get32(base + 4);
            let step = match (dma >> 21) & 3 {
                0 | 3 => 4_u32,
                1 => 4_u32.wrapping_neg(),
                _ => 0,
            };
            while self.card.has_data() {
                let word = self.card.next_word();
                self.write32(destination, word);
                destination = destination.wrapping_add(step);
            }
            if dma & (1 << 25) == 0 {
                self.io.set32(base + 8, dma & !0x8000_0000);
            }
            if dma & (1 << 30) != 0 {
                self.io.raise(1 << (8 + channel));
            }
            self.finish_card_transfer();
            return;
        }
    }

    fn finish_card_transfer(&mut self) {
        let control = self.io.get32(card::ROMCTRL) & !(card::ROMCTRL_BUSY | card::ROMCTRL_READY);
        self.io.set32(card::ROMCTRL, control);
        // AUXSPICNT bit 14: transfer-complete interrupt.
        if self.io.get16(0x0400_01A0) & 0x4000 != 0 {
            self.io.raise(1 << 19);
        }
    }

    fn run_dma(&mut self, channel: usize) {
        let base = io::DMA_BASE + 12 * channel as u32;
        let mut source = self.io.get32(base);
        let mut destination = self.io.get32(base + 4);
        let control = self.io.get32(base + 8);
        let words = control & (1 << 26) != 0;
        let unit: i32 = if words { 4 } else { 2 };
        let mut count = control & 0x1F_FFFF;
        if count == 0 {
            count = 0x20_0000;
        }
        let step = |mode: u32| match mode {
            0 | 3 => unit,
            1 => -unit,
            _ => 0,
        };
        let destination_step = step((control >> 21) & 3);
        let source_step = step((control >> 23) & 3);
        for _ in 0..count {
            if words {
                let value = self.read32(source);
                self.write32(destination, value);
            } else {
                let value = self.read16(source);
                self.write16(destination, value);
            }
            source = source.wrapping_add(source_step as u32);
            destination = destination.wrapping_add(destination_step as u32);
        }
        self.io.set32(base + 8, control & !0x8000_0000);
        if control & (1 << 30) != 0 {
            self.io.raise(1 << (8 + channel));
        }
    }

    // --- ARM7 stand-in -----------------------------------------------

    /// The ARM7 is not run; answer the handshakes the ARM9 SDK expects.
    fn arm7_sync(&mut self, value: u32) {
        // PXI start-up handshake: the ARM9 echoes the ARM7's IPCSYNC output,
        // and the ARM7 counts down each time it sees its value echoed. The
        // ARM9 is done after several changes once the value reaches 0.
        if value == self.arm7_sync_output {
            self.arm7_sync_output = (self.arm7_sync_output + 15) & 0xF;
        }
        self.io.set_sync_input(self.arm7_sync_output);
    }

    fn arm7_fifo(&mut self, word: u32) {
        let lr = self.cpu().r[14];
        self.fifo_sent.push((word, lr));
        let tag = word & 0x1F;
        let data = word >> 6;
        match tag {
            PXI_TAG_PM => {
                // Power management packets: bit 25 marks the first word
                // (command in bits 8-14), bit 24 the last. The ARM7 answers
                // with the command and a result byte, 0 for success.
                if data & (1 << 25) != 0 {
                    self.arm7_pm_command = (data >> 8) & 0x7F;
                }
                if data & (1 << 24) != 0 {
                    self.arm7_reply(PXI_TAG_PM, self.arm7_pm_command << 8, false);
                }
            }
            PXI_TAG_FS => self.card_backup_message(data),
            PXI_TAG_TOUCHPANEL => {
                // Touch panel packets are framed like PM's; commands are
                // answered with the command and result 0. Sampled touch
                // data would arrive as command 0x10; none is sent, which
                // reads as "not touched".
                if data & (1 << 25) != 0 {
                    self.arm7_tp_command = (data >> 8) & 0x7F;
                }
                if data & (1 << 24) != 0 {
                    // Replies are framed as one-word packets (first and last
                    // bits set); the callback ignores them otherwise.
                    let reply = (1 << 25) | (1 << 24) | (self.arm7_tp_command << 8);
                    self.arm7_reply(PXI_TAG_TOUCHPANEL, reply, false);
                }
            }
            PXI_TAG_RTC => {
                // RTC commands are single words; the ARM7 refreshes the
                // date and time in shared RAM and answers with the command
                // and result 0.
                self.write_rtc_time();
                let command = (data >> 8) & 0x7F;
                self.arm7_reply(PXI_TAG_RTC, command << 8, false);
            }
            PXI_TAG_NVRAM => {
                // Firmware flash packets: bit 25 marks the first word (command
                // in bits 8-15, then one argument byte), later words carry two
                // bytes each, and bit 24 marks the last word.
                if data & (1 << 25) != 0 {
                    self.nvram_command = (data >> 8) & 0xFF;
                    self.nvram_bytes = vec![data as u8];
                } else {
                    self.nvram_bytes.push((data >> 8) as u8);
                    self.nvram_bytes.push(data as u8);
                }
                if data & (1 << 24) != 0 {
                    self.nvram_request();
                }
            }
            _ => {}
        }
    }

    /// Run a complete firmware flash request against the firmware image.
    fn nvram_request(&mut self) {
        let bytes = std::mem::take(&mut self.nvram_bytes);
        let command = self.nvram_command;
        let field = |range: std::ops::Range<usize>| {
            bytes.get(range).map(|slice| {
                slice
                    .iter()
                    .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
            })
        };
        // Transfers: a 24-bit flash address, the size (4 bytes for reads,
        // 2 for writes), then the 32-bit address of the ARM9 buffer.
        let transfer = || {
            let size = 3..bytes.len().checked_sub(4)?;
            Some((
                field(0..3)?,
                field(size)?,
                field(bytes.len() - 4..bytes.len())?,
            ))
        };
        match command {
            firmware::READ | firmware::FAST_READ => {
                let Some((address, size, destination)) = transfer() else {
                    self.stop(&format!("short firmware read request {bytes:02X?}"));
                };
                for (offset, index) in self.firmware.range(address, size).enumerate() {
                    let byte = self.firmware.data[index];
                    self.write8(destination + offset as u32, u32::from(byte));
                }
            }
            firmware::PAGE_WRITE | firmware::PAGE_PROGRAM => {
                let Some((address, size, source)) = transfer() else {
                    self.stop(&format!("short firmware write request {bytes:02X?}"));
                };
                for (offset, index) in self.firmware.range(address, size).enumerate() {
                    let byte = self.read8(source + offset as u32) as u8;
                    let cell = &mut self.firmware.data[index];
                    // Programming can only clear bits; a page write erases
                    // first.
                    *cell = if command == firmware::PAGE_PROGRAM {
                        *cell & byte
                    } else {
                        byte
                    };
                }
                self.flush_firmware();
            }
            firmware::PAGE_ERASE | firmware::SECTOR_ERASE => {
                let address = field(0..3).unwrap_or(0);
                self.firmware
                    .erase(address, command == firmware::SECTOR_ERASE);
                self.flush_firmware();
            }
            // The status register: never busy, writes enabled.
            firmware::READ_STATUS => {
                let destination = field(0..4).unwrap_or(0);
                if destination != 0 {
                    self.write8(destination, 0x02);
                }
            }
            firmware::WRITE_ENABLE | firmware::WRITE_DISABLE => {}
            _ => self.record(Event::Unhandled {
                tag: PXI_TAG_NVRAM,
                command,
            }),
        }
        self.arm7_reply(PXI_TAG_NVRAM, command << 8, false);
    }

    fn flush_firmware(&mut self) {
        if let Err(error) = self.firmware.flush() {
            self.stop(&error);
        }
    }

    /// Write the host's local date and time where the SDK reads the RTC.
    fn write_rtc_time(&mut self) {
        let bcd = |value: u32| ((value / 10) << 4) | (value % 10);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let days = now / 86_400;
        let seconds = (now % 86_400) as u32;
        let (year, month, day) = civil_from_days(days as i64);
        let date = bcd((year % 100) as u32) | (bcd(month) << 8) | (bcd(day) << 16);
        let time = bcd(seconds / 3600) | (bcd(seconds / 60 % 60) << 8) | (bcd(seconds % 60) << 16);
        self.write32(RTC_DATE, date);
        self.write32(RTC_TIME, time);
    }

    /// Card backup (save memory) requests, which the ARM7 serves over SPI.
    /// The ARM9 sends a command number; INIT is followed by the address of
    /// the shared command block, whose first word receives the result.
    fn card_backup_message(&mut self, data: u32) {
        if self.backup_expects_block {
            self.backup_expects_block = false;
            self.backup_block = data;
            let size = self.read32(data + backup::ARG_TOTAL_SIZE);
            self.backup.ensure_size(size as usize);
            self.finish_backup_command(backup::REQ_INIT, backup::RESULT_SUCCESS);
            return;
        }
        if data == backup::REQ_INIT {
            self.backup_expects_block = true;
            return;
        }
        let result = self.run_backup_command(data);
        self.finish_backup_command(data, result);
    }

    fn run_backup_command(&mut self, command: u32) -> u32 {
        let block = self.backup_block;
        let source = self.read32(block + backup::ARG_SOURCE);
        let destination = self.read32(block + backup::ARG_DESTINATION);
        let length = self.read32(block + backup::ARG_LENGTH);
        let mut changed = false;
        let result = match command {
            backup::REQ_IDENTIFY => backup::RESULT_SUCCESS,
            backup::REQ_READ_ID => {
                // A Macronix FLASH ID; games only check that reads work.
                self.write32(block + backup::ARG_ID, 0x0013_20C2);
                backup::RESULT_SUCCESS
            }
            backup::REQ_READ_BACKUP => match self.backup.range(source, length) {
                Some(range) => {
                    let bytes = self.backup.data[range].to_vec();
                    for (index, byte) in bytes.into_iter().enumerate() {
                        self.write8(destination + index as u32, u32::from(byte));
                    }
                    backup::RESULT_SUCCESS
                }
                None => backup::RESULT_FAILURE,
            },
            backup::REQ_WRITE_BACKUP | backup::REQ_PROGRAM_BACKUP => {
                match self.backup.range(destination, length) {
                    Some(range) => {
                        let program = command == backup::REQ_PROGRAM_BACKUP;
                        for (index, offset) in range.enumerate() {
                            let byte = self.read8(source + index as u32) as u8;
                            let slot = &mut self.backup.data[offset];
                            // Programming FLASH can only clear bits.
                            *slot = if program { *slot & byte } else { byte };
                        }
                        changed = true;
                        backup::RESULT_SUCCESS
                    }
                    None => backup::RESULT_FAILURE,
                }
            }
            backup::REQ_VERIFY_BACKUP => match self.backup.range(destination, length) {
                Some(range) => {
                    let matches = range.enumerate().all(|(index, offset)| {
                        self.read8(source + index as u32) as u8 == self.backup.data[offset]
                    });
                    if matches {
                        backup::RESULT_SUCCESS
                    } else {
                        backup::RESULT_FAILURE
                    }
                }
                None => backup::RESULT_FAILURE,
            },
            backup::REQ_ERASE_PAGE_BACKUP
            | backup::REQ_ERASE_SECTOR_BACKUP
            | backup::REQ_ERASE_SUBSECTOR_BACKUP => {
                changed = true;
                if self.backup.erase(destination, length) {
                    backup::RESULT_SUCCESS
                } else {
                    backup::RESULT_FAILURE
                }
            }
            backup::REQ_ERASE_CHIP_BACKUP => {
                changed = true;
                self.backup.data.fill(0xFF);
                backup::RESULT_SUCCESS
            }
            _ => {
                let words: Vec<String> = (0..8)
                    .map(|index| format!("{:08X}", self.read32(block + 4 * index)))
                    .collect();
                self.stop(&format!(
                    "unhandled card backup command {command} (block 0x{block:08X}: {})",
                    words.join(" ")
                ));
            }
        };
        self.record(Event::Backup {
            command,
            offset: if command == backup::REQ_READ_BACKUP {
                source
            } else {
                destination
            },
            length,
            result,
        });
        if changed && let Err(error) = self.backup.flush() {
            self.stop(&error);
        }
        result
    }

    fn finish_backup_command(&mut self, command: u32, result: u32) {
        self.write32(self.backup_block + backup::ARG_RESULT, result);
        self.arm7_reply(PXI_TAG_FS, command, true);
    }

    /// Queue a PXI message from the ARM7 to the ARM9.
    fn arm7_reply(&mut self, tag: u32, data: u32, flag: bool) {
        self.fifo_replies.push((tag, data));
        let flag = if flag { 0x20 } else { 0 };
        self.io.push_from_arm7((data << 6) | flag | tag);
    }

    /// Let the ARM7 make progress while the ARM9 waits on it. At boot, the
    /// two CPUs count up halfwords in shared RAM until each sees the other
    /// move, and the ARM9 then waits for the ARM7's boot progress word.
    pub(super) fn arm7_tick(&mut self) {
        let counter = Memory::main_offset(ARM7_BOOT_COUNTER);
        let main = &mut self.memory.main;
        let value = u16::from_le_bytes([main[counter], main[counter + 1]]).wrapping_add(1);
        main[counter..counter + 2].copy_from_slice(&value.to_le_bytes());
        // The stand-in has a handler registered for every PXI service.
        let ready = Memory::main_offset(ARM7_PXI_READY);
        main[ready..ready + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let progress = Memory::main_offset(ARM7_BOOT_PROGRESS);
        if main[progress] < 4 {
            main[progress] = 4;
        }
        // Init handshake in four bytes: message, echo, done, acknowledge.
        // First the ARM9 sends and the ARM7 echoes and sets done; then the
        // roles swap until the ARM9's echoes add up, and the ARM7 acks.
        let init = Memory::main_offset(ARM7_INIT_SYNC);
        let [message, echo, done, ack] =
            [main[init], main[init + 1], main[init + 2], main[init + 3]];
        match self.arm7_init_phase {
            0 if ack == 1 => {
                // Roles swap; the done flag is now the ARM9's to set.
                main[init + 2] = 0;
                self.arm7_init_phase = 1;
            }
            0 if message & 0x80 != 0 => {
                main[init + 1] = message;
                main[init + 2] = 1;
            }
            1 if done == 1 => {
                main[init + 3] = 1;
                self.arm7_init_phase = 2;
            }
            1 if echo == message => main[init] = 0x80 | (message.wrapping_add(1) & 0xF),
            _ => {}
        }
    }

    // --- CP15 and PSR ------------------------------------------------

    fn mcr(&mut self, crn: u32, crm: u32, opc2: u32, value: u32) -> bool {
        self.record(Event::Cp15 {
            crn,
            crm,
            opc2,
            value,
        });
        match (crn, crm, opc2) {
            (1, 0, 0) => {
                self.cp15_control = value;
                self.memory.dtcm_enabled = value & (1 << 16) != 0;
                self.memory.itcm_enabled = value & (1 << 18) != 0;
                self.memory.remap();
            }
            (9, 1, 0) => {
                self.cp15_dtcm = value;
                self.memory.dtcm_base = value & 0xFFFF_F000;
                self.memory.dtcm_size = 512 << ((value >> 1) & 0x1F);
                self.memory.remap();
            }
            (9, 1, 1) => {
                self.cp15_itcm = value;
                self.memory.itcm_end = (512_u32 << ((value >> 1) & 0x1F)).min(0x0200_0000);
                self.memory.remap();
            }
            // Wait for interrupt.
            (7, 0, 4) | (7, 8, 2) => return true,
            _ => {}
        }
        false
    }

    fn mrc(&self, crn: u32, crm: u32, opc2: u32) -> u32 {
        match (crn, crm, opc2) {
            (0, 0, 0) => 0x4105_9461,
            (0, 0, 1) => 0x0F0D_2112,
            (1, 0, 0) => self.cp15_control,
            (9, 1, 0) => self.cp15_dtcm,
            (9, 1, 1) => self.cp15_itcm,
            _ => 0,
        }
    }

    // --- Dispatch ----------------------------------------------------

    /// Whether an overlay's code is what is currently in RAM at `address`.
    fn overlay_loaded(&mut self, module: u16, address: u32) -> bool {
        let Some((base, image)) = self.overlays.get(&module) else {
            return false;
        };
        let offset = (address - base) as usize;
        let expected: Vec<u8> = image[offset..(offset + 16).min(image.len())].to_vec();
        expected
            .iter()
            .enumerate()
            .all(|(index, &byte)| self.read8(address + index as u32) == u32::from(byte))
    }

    fn resolve(&mut self, target: u32) -> Option<Target> {
        let address = target & !1;
        let candidates = self.functions.get(&address)?.clone();
        if let Some(static_target) = candidates.iter().find(|target| target.module >= 0xFF00) {
            return Some(*static_target);
        }
        candidates
            .into_iter()
            .find(|candidate| self.overlay_loaded(candidate.module, address))
    }

    // --- Reports -----------------------------------------------------

    pub fn stop(&mut self, reason: &str) -> ! {
        let mut report = String::new();
        let _ = writeln!(report, "\n=== stopped: {reason}");
        let mode = self.banks.mode;
        let calls = self.calls;
        let frame = self.io.frame;
        let cpu = self.cpu();
        let _ = writeln!(
            report,
            "frame {frame}, {calls} runtime calls, mode 0x{mode:02X}, flags nzcv={}{}{}{}",
            cpu.n, cpu.z, cpu.c, cpu.v
        );
        for (index, value) in cpu.r.iter().enumerate() {
            let _ = write!(report, "r{index}={value:08X} ");
            if index % 8 == 7 {
                report.push('\n');
            }
        }
        report.push_str("recent events (oldest first):\n");
        for event in &self.events {
            let _ = writeln!(report, "  {}", describe(event));
        }
        report.push_str("IPC FIFO messages sent to the ARM7:\n");
        for ((word, lr), count) in runs(&self.fifo_sent) {
            let _ = writeln!(
                report,
                "  tag {:2} ({}) data 0x{:07X}{} (lr 0x{lr:08X}){}",
                word & 0x1F,
                pxi_tag_name(word & 0x1F),
                word >> 6,
                if word & 0x20 != 0 {
                    " (error flag)"
                } else {
                    ""
                },
                repeated(count)
            );
        }
        report.push_str("IPC FIFO replies from the ARM7 stand-in:\n");
        for ((tag, data), count) in runs(&self.fifo_replies) {
            let _ = writeln!(
                report,
                "  tag {tag:2} ({}) data 0x{data:07X}{}",
                pxi_tag_name(tag),
                repeated(count)
            );
        }
        report.push_str("I/O registers touched (address: reads/writes):\n");
        for (address, access) in &self.io.touched {
            let _ = writeln!(
                report,
                "  0x{address:08X}: {}/{}",
                access.reads, access.writes
            );
        }
        // The running thread's call chain, from its system-mode stack.
        let sp = {
            let cpu = unsafe { &*self.cpu };
            self.banks.banked_sp(cpu, super::cpu::MODE_SYSTEM)
        };
        let chain: Vec<String> = (0..128)
            .map(|slot| self.read32(sp.wrapping_add(4 * slot)))
            .filter(|&word| (0x0200_4000..0x0230_0000).contains(&word))
            .take(20)
            .map(|word| format!("{word:08X}"))
            .collect();
        let _ = writeln!(
            report,
            "running thread (sp 0x{sp:08X}) stack code pointers: {}",
            chain.join(" ")
        );
        let mut modules: Vec<(u16, u32)> =
            self.overlays.iter().map(|(&id, (base, _))| (id, *base)).collect();
        modules.sort_unstable();
        let loaded: Vec<String> = modules
            .into_iter()
            .filter(|&(id, base)| self.overlay_loaded(id, base))
            .map(|(id, base)| format!("ov{id:03} at 0x{base:08X}"))
            .collect();
        let _ = writeln!(report, "overlays in RAM: {}", loaded.join(", "));
        report.push_str("parked guest threads:\n");
        for line in super::threads::describe(&mut |address| self.read32(address)) {
            let _ = writeln!(report, "  {line}");
        }
        // GEN5_DUMP=address:length[,address:length...] dumps memory words.
        if let Ok(spec) = std::env::var("GEN5_DUMP") {
            for range in spec.split(',') {
                let parse =
                    |text: &str| u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok();
                let Some((start, length)) = range
                    .split_once(':')
                    .and_then(|(start, length)| Some((parse(start)?, parse(length)?)))
                else {
                    continue;
                };
                let _ = writeln!(report, "memory 0x{start:08X}:");
                for row in (0..length).step_by(16) {
                    let _ = write!(report, "  {:08X}:", start + row);
                    for word in (row..(row + 16).min(length)).step_by(4) {
                        let value = self.read32(start + word);
                        let _ = write!(report, " {value:08X}");
                    }
                    report.push('\n');
                }
            }
        }
        print!("{report}");
        std::process::exit(0);
    }
}

/// (year, month, day) for days since 1970-01-01 (Howard Hinnant's
/// civil-from-days algorithm). The date is UTC, which is close enough for
/// the game's clock.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Consecutive equal items, counted.
fn runs<T: Copy + PartialEq>(items: &[T]) -> Vec<(T, usize)> {
    let mut runs: Vec<(T, usize)> = Vec::new();
    for &item in items {
        match runs.last_mut() {
            Some((last, count)) if *last == item => *count += 1,
            _ => runs.push((item, 1)),
        }
    }
    runs
}

fn repeated(count: usize) -> String {
    if count > 1 {
        format!(" x{count}")
    } else {
        String::new()
    }
}

/// NitroSDK PXI FIFO tags.
fn pxi_tag_name(tag: u32) -> &'static str {
    match tag {
        0 => "EX",
        1 => "USER_0",
        2 => "USER_1",
        3 => "SYSTEM",
        4 => "NVRAM",
        5 => "RTC",
        6 => "TOUCHPANEL",
        7 => "SOUND",
        8 => "PM",
        9 => "MIC",
        10 => "WM",
        11 => "FS",
        12 => "OS",
        13 => "CTRDG",
        14 => "CARD",
        15 => "WVR",
        16 => "CTRDG_Ex",
        17 => "CTRDG_PHI",
        _ => "?",
    }
}

fn describe(event: &Event) -> String {
    match *event {
        Event::Call { target, lr } => format!("call 0x{target:08X} (lr 0x{lr:08X})"),
        Event::Jump { target, from } => format!("jump 0x{target:08X} from 0x{from:08X}"),
        Event::Swi(number) => format!("swi 0x{number:02X}"),
        Event::Irq(flags) => format!("irq flags 0x{flags:08X}"),
        Event::Read { address, value } => format!("read  0x{address:08X} = 0x{value:X}"),
        Event::Write { address, value } => format!("write 0x{address:08X} = 0x{value:X}"),
        Event::Cp15 {
            crn,
            crm,
            opc2,
            value,
        } => format!("cp15 c{crn},c{crm},{opc2} = 0x{value:08X}"),
        Event::Psr { value, fields } => format!("msr fields {fields:X} = 0x{value:08X}"),
        Event::FifoSend(word) => format!("ipc fifo send 0x{word:08X}"),
        Event::Unhandled { tag, command } => format!(
            "ARM7 stand-in acknowledged {} command 0x{command:02X} without action",
            pxi_tag_name(tag)
        ),
        Event::Backup {
            command,
            offset,
            length,
            result,
        } => format!(
            "card backup command {command} at 0x{offset:X} length 0x{length:X}: result {result}"
        ),
        Event::Interpret { target, lr } => {
            format!("interpret 0x{target:08X} (lr 0x{lr:08X})")
        }
    }
}

// --- Calls into generated code ---------------------------------------------

fn call_target(cpu: *mut Cpu, target: u32) {
    call_any(cpu, target);
}

/// Call `target` (bit 0 selects Thumb): the recompiled function if one is
/// loaded there, otherwise the interpreter. `lr` must already be set.
pub(super) fn call_any(cpu: *mut Cpu, target: u32) {
    let function = {
        // SAFETY: see `machine`; the borrow ends before generated code runs.
        let machine = unsafe { machine(cpu) };
        machine.calls += 1;
        match machine.resolve(target) {
            Some(resolved) if resolved.thumb == (target & 1 != 0) || target & 1 == 0 => {
                Some(resolved.function)
            }
            Some(_) => machine.stop(&format!(
                "call to 0x{target:08X} in Thumb state, but the function there is ARM"
            )),
            None => {
                let lr = unsafe { (*cpu).r[14] };
                machine.record(Event::Interpret { target, lr });
                None
            }
        }
    };
    match function {
        // SAFETY: the function comes from the game library's table.
        Some(function) => unsafe { function(cpu) },
        None => super::interpreter::run(cpu, target),
    }
}

/// Deliver pending interrupts as the ARM9 BIOS does: switch to IRQ mode and
/// call the handler stored at the end of DTCM.
fn deliver_interrupts(cpu: *mut Cpu) -> bool {
    let (handler, saved) = {
        let machine = unsafe { machine(cpu) };
        let pending = machine.io.pending();
        if pending == 0 {
            return false;
        }
        machine.record(Event::Irq(pending));
        let handler = machine.read32(machine.memory.dtcm_base + 0x3FFC);
        let cpu = unsafe { &mut *cpu };
        let saved = [cpu.r[0], cpu.r[1], cpu.r[2], cpu.r[3], cpu.r[12]];
        machine.banks.enter(cpu, MODE_IRQ);
        cpu.r[14] = 0xFFFF_0000;
        (handler, saved)
    };
    call_target(cpu, handler);
    let machine = unsafe { machine(cpu) };
    let cpu = unsafe { &mut *cpu };
    machine.banks.leave(cpu);
    [cpu.r[0], cpu.r[1], cpu.r[2], cpu.r[3], cpu.r[12]] = saved;
    if let Some(info) = machine.pending_switch.take() {
        super::threads::reschedule_after_irq(cpu, info);
    }
    true
}

/// Halt until an interrupt is pending, then service it.
fn halt(cpu: *mut Cpu) {
    if !deliver_interrupts(cpu) {
        advance_frame(cpu);
        deliver_interrupts(cpu);
    }
}

fn advance_frame(cpu: *mut Cpu) {
    let machine = unsafe { machine(cpu) };
    machine.io.advance_to_vblank();
    if machine.io.frame >= machine.options.max_frames {
        let frames = machine.io.frame;
        machine.stop(&format!("reached frame {frames}"));
    }
}

/// BIOS IntrWait: wait until the IRQ handler sets one of `mask` in the
/// check flags at the end of DTCM.
fn interrupt_wait(cpu: *mut Cpu, discard: bool, mask: u32) {
    let flags_address = {
        let machine = unsafe { machine(cpu) };
        let address = machine.memory.dtcm_base + 0x3FF8;
        if discard {
            let flags = machine.read32(address) & !mask;
            machine.write32(address, flags);
        }
        address
    };
    let start = unsafe { machine(cpu) }.io.frame;
    loop {
        if !deliver_interrupts(cpu) {
            advance_frame(cpu);
            deliver_interrupts(cpu);
        }
        let machine = unsafe { machine(cpu) };
        let flags = machine.read32(flags_address);
        if flags & mask != 0 {
            machine.write32(flags_address, flags & !mask);
            return;
        }
        if machine.io.frame - start > WAIT_LIMIT_FRAMES {
            machine.stop(&format!(
                "IntrWait for 0x{mask:X} never satisfied (IE 0x{:X}, IME {})",
                machine.io.get32(io::IE),
                machine.io.get32(io::IME)
            ));
        }
    }
}

// --- BIOS calls ----------------------------------------------------------------

fn swi(cpu: *mut Cpu, number: u32) {
    let machine = unsafe { machine(cpu) };
    machine.record(Event::Swi(number));
    let r = unsafe { (*cpu).r };
    match number {
        // WaitByLoop: the caller is usually waiting on the ARM7.
        0x03 => machine.arm7_tick(),
        0x04 => interrupt_wait(cpu, r[0] != 0, r[1]),
        0x05 => interrupt_wait(cpu, true, io::IRQ_VBLANK),
        0x06 => halt(cpu),
        0x09 => {
            let (numerator, denominator) = (r[0] as i32, r[1] as i32);
            if denominator == 0 {
                machine.stop("BIOS Div by zero");
            }
            let quotient = numerator.wrapping_div(denominator);
            let cpu = unsafe { &mut *cpu };
            cpu.r[0] = quotient as u32;
            cpu.r[1] = numerator.wrapping_rem(denominator) as u32;
            cpu.r[3] = quotient.unsigned_abs();
        }
        0x0B => machine.cpu_set(r[0], r[1], r[2], false),
        0x0C => machine.cpu_set(r[0], r[1], r[2], true),
        0x0D => unsafe { (*cpu).r[0] = r[0].isqrt() },
        0x0E => {
            let mut crc = r[0] & 0xFFFF;
            for index in 0..r[2] {
                crc ^= machine.read8(r[1] + index);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xA001
                    } else {
                        crc >> 1
                    };
                }
            }
            unsafe { (*cpu).r[0] = crc };
        }
        0x0F => unsafe { (*cpu).r[0] = 0 },
        0x11 | 0x12 => machine.lz77(r[0], r[1]),
        0x14 | 0x15 => machine.run_length(r[0], r[1]),
        _ => machine.stop(&format!("unimplemented BIOS call 0x{number:02X}")),
    }
}

impl Machine {
    fn cpu_set(&mut self, source: u32, destination: u32, control: u32, fast: bool) {
        let fill = control & (1 << 24) != 0;
        let words = fast || control & (1 << 26) != 0;
        let mut count = control & 0x1F_FFFF;
        if fast {
            count = count.div_ceil(8) * 8;
        }
        let unit: u32 = if words { 4 } else { 2 };
        let fill_value = if words {
            self.read32(source)
        } else {
            self.read16(source)
        };
        for index in 0..count {
            let from = source + if fill { 0 } else { index * unit };
            let to = destination + index * unit;
            if words {
                let value = if fill { fill_value } else { self.read32(from) };
                self.write32(to, value);
            } else {
                let value = if fill { fill_value } else { self.read16(from) };
                self.write16(to, value);
            }
        }
    }

    fn lz77(&mut self, mut source: u32, destination: u32) {
        let header = self.read32(source);
        let size = header >> 8;
        source += 4;
        let mut written = 0;
        while written < size {
            let flags = self.read8(source);
            source += 1;
            for bit in (0..8).rev() {
                if written >= size {
                    break;
                }
                if flags & (1 << bit) == 0 {
                    let byte = self.read8(source);
                    source += 1;
                    self.write8(destination + written, byte);
                    written += 1;
                } else {
                    let (high, low) = (self.read8(source), self.read8(source + 1));
                    source += 2;
                    let length = (high >> 4) + 3;
                    let distance = (((high & 0xF) << 8) | low) + 1;
                    for _ in 0..length {
                        if written >= size {
                            break;
                        }
                        let byte = self.read8(destination + written - distance);
                        self.write8(destination + written, byte);
                        written += 1;
                    }
                }
            }
        }
    }

    fn run_length(&mut self, mut source: u32, destination: u32) {
        let header = self.read32(source);
        let size = header >> 8;
        source += 4;
        let mut written = 0;
        while written < size {
            let flag = self.read8(source);
            source += 1;
            if flag & 0x80 != 0 {
                let byte = self.read8(source);
                source += 1;
                for _ in 0..(flag & 0x7F) + 3 {
                    self.write8(destination + written, byte);
                    written += 1;
                }
            } else {
                for _ in 0..(flag & 0x7F) + 1 {
                    let byte = self.read8(source);
                    source += 1;
                    self.write8(destination + written, byte);
                    written += 1;
                }
            }
        }
    }
}

// --- Host callbacks ------------------------------------------------------------

extern "C" fn host_read8(cpu: *mut Cpu, address: u32) -> u32 {
    unsafe { machine(cpu) }.read_slow(address, 1)
}
extern "C" fn host_read16(cpu: *mut Cpu, address: u32) -> u32 {
    unsafe { machine(cpu) }.read_slow(address, 2)
}
extern "C" fn host_read32(cpu: *mut Cpu, address: u32) -> u32 {
    unsafe { machine(cpu) }.read_slow(address, 4)
}
extern "C" fn host_write8(cpu: *mut Cpu, address: u32, value: u32) {
    unsafe { machine(cpu) }.write_slow(address, 1, value & 0xFF);
    interrupt_if_enabled(cpu);
}
extern "C" fn host_write16(cpu: *mut Cpu, address: u32, value: u32) {
    unsafe { machine(cpu) }.write_slow(address, 2, value & 0xFFFF);
    interrupt_if_enabled(cpu);
}
extern "C" fn host_write32(cpu: *mut Cpu, address: u32, value: u32) {
    unsafe { machine(cpu) }.write_slow(address, 4, value);
    interrupt_if_enabled(cpu);
}

extern "C" fn host_call(cpu: *mut Cpu, target: u32) {
    let lr = unsafe { (*cpu).r[14] };
    let machine = unsafe { machine(cpu) };
    machine.record(Event::Call { target, lr });
    machine.tick();
    call_target(cpu, target);
}

extern "C" fn host_jump(cpu: *mut Cpu, target: u32, from: u32) {
    unsafe { machine(cpu) }.record(Event::Jump { target, from });
    // A jump into the middle of a function is interpreted until it returns.
    call_any(cpu, target);
}

pub(super) extern "C" fn host_swi(cpu: *mut Cpu, comment: u32, thumb: u32) {
    let number = if thumb != 0 {
        comment & 0xFF
    } else {
        (comment >> 16) & 0xFF
    };
    swi(cpu, number);
}

pub(super) extern "C" fn host_mcr(
    cpu: *mut Cpu,
    cp: u32,
    _opc1: u32,
    crn: u32,
    crm: u32,
    opc2: u32,
    value: u32,
) {
    let machine = unsafe { machine(cpu) };
    if cp != 15 {
        machine.stop(&format!("MCR to coprocessor {cp}"));
    }
    if machine.mcr(crn, crm, opc2, value) {
        halt(cpu);
    }
}

pub(super) extern "C" fn host_mrc(
    cpu: *mut Cpu,
    _cp: u32,
    _opc1: u32,
    crn: u32,
    crm: u32,
    opc2: u32,
) -> u32 {
    unsafe { machine(cpu) }.mrc(crn, crm, opc2)
}

pub(super) extern "C" fn host_read_psr(cpu: *mut Cpu, spsr: u32) -> u32 {
    let machine = unsafe { machine(cpu) };
    if spsr != 0 {
        machine.banks.spsr()
    } else {
        machine.banks.cpsr(unsafe { &*cpu })
    }
}

pub(super) extern "C" fn host_write_psr(cpu: *mut Cpu, spsr: u32, fields: u32, value: u32) {
    let machine = unsafe { machine(cpu) };
    machine.record(Event::Psr { value, fields });
    machine.tick();
    if spsr != 0 {
        machine.banks.set_spsr(value, fields);
    } else {
        machine.banks.set_cpsr(unsafe { &mut *cpu }, value, fields);
        interrupt_if_enabled(cpu);
    }
}

/// Take a pending interrupt now if the CPU accepts interrupts. Called where
/// the game re-enables them, which is where hardware would take one.
fn interrupt_if_enabled(cpu: *mut Cpu) {
    let machine = unsafe { machine(cpu) };
    if !machine.banks.irq_disabled && machine.io.pending() != 0 {
        deliver_interrupts(cpu);
    }
}

/// A loop ran out of budget: let time pass, let the ARM7 act, and take a
/// pending interrupt.
extern "C" fn host_tick(cpu: *mut Cpu) {
    let machine = unsafe { machine(cpu) };
    unsafe { (*cpu).budget = LOOP_BUDGET };
    machine.arm7_tick();
    machine.tick();
    interrupt_if_enabled(cpu);
}

extern "C" fn host_exception_return(cpu: *mut Cpu, _target: u32) {
    let machine = unsafe { machine(cpu) };
    machine.banks.leave(unsafe { &mut *cpu });
}

pub(super) extern "C" fn host_undefined(cpu: *mut Cpu, address: u32, raw: u32) {
    unsafe { machine(cpu) }.stop(&format!(
        "undefined instruction 0x{raw:08X} at 0x{address:08X}"
    ));
}

// --- Boot ------------------------------------------------------------------------

/// Load the game library and run the game from its ARM9 entry point.
pub fn boot(cache_root: &Path, rom: &Path, options: Options) -> Result<()> {
    let card = Card::open(rom)?;
    let code_dir = cache_root.join("code");
    let manifest_text = fs::read_to_string(code_dir.join("code.json"))
        .map_err(|error| format!("could not read code manifest: {error}"))?;
    let manifest: CodeManifest = serde_json::from_str(&manifest_text)
        .map_err(|error| format!("could not parse code manifest: {error}"))?;
    let arm9 = fs::read(code_dir.join(&manifest.arm9.binary.file))
        .map_err(|error| format!("could not read ARM9 binary: {error}"))?;
    let header = fs::read(code_dir.join("header.bin"))
        .map_err(|error| format!("could not read ROM header (re-run `import`): {error}"))?;
    let program = Program::load(&code_dir)?;

    let library_path = cache_root
        .join("recomp")
        .join("build")
        .join(crate::recomp::compile::LIBRARY_NAME);
    // SAFETY: the library is the game code built by `build`; it runs no
    // initialisers beyond the C runtime's.
    let library = unsafe { Library::new(&library_path) }.map_err(|error| {
        format!(
            "could not load '{}': {error} (run `build` first)",
            library_path.display()
        )
    })?;
    let (set_host, table) = unsafe {
        let set_host = *library
            .get::<unsafe extern "C" fn(*const Host)>(b"gen5_set_host\0")
            .map_err(|error| format!("game library has no gen5_set_host: {error}"))?;
        let table_function = *library
            .get::<unsafe extern "C" fn(*mut u32) -> *const FunctionEntry>(b"gen5_function_table\0")
            .map_err(|error| format!("game library has no gen5_function_table: {error}"))?;
        let mut count = 0;
        let entries = table_function(&mut count);
        (
            set_host,
            std::slice::from_raw_parts(entries, count as usize),
        )
    };

    let mut functions: HashMap<u32, Vec<Target>> = HashMap::new();
    for entry in table {
        functions.entry(entry.address).or_default().push(Target {
            module: entry.module,
            thumb: entry.thumb != 0,
            function: entry.function,
        });
    }
    let overlays = program
        .modules
        .iter()
        .filter_map(|module| match module.id {
            ModuleId::Overlay(id) => Some((id, (module.base, module.bytes.clone()))),
            _ => None,
        })
        .collect();

    let cpu = Box::into_raw(Box::new(Cpu {
        r: [0; 16],
        n: 0,
        z: 0,
        c: 0,
        v: 0,
        q: 0,
        read_pages: std::ptr::null_mut(),
        write_pages: std::ptr::null_mut(),
        runtime: std::ptr::null_mut(),
        budget: LOOP_BUDGET,
    }));
    let mut machine = Box::new(Machine {
        cpu,
        memory: Memory::new(),
        io: Io::new(),
        banks: Banks::new(MODE_SVC),
        cp15_control: 0x0005_2078,
        cp15_dtcm: 0x0080_000A,
        cp15_itcm: 0x0000_0020,
        functions,
        overlays,
        events: VecDeque::new(),
        calls: 0,
        poll: (0, 0),
        activity: 0,
        pending_switch: None,
        card,
        fifo_sent: Vec::new(),
        fifo_replies: Vec::new(),
        arm7_pm_command: 0,
        arm7_tp_command: 0,
        nvram_command: 0,
        nvram_bytes: Vec::new(),
        backup_block: 0,
        backup_expects_block: false,
        backup: Backup::open(cache_root.join("save.bin")),
        firmware: Firmware::open(cache_root.join("firmware.bin")),
        arm7_sync_output: 7,
        arm7_init_phase: 0,
        options,
        _library: library,
    });

    // Memory as the firmware leaves it: the ARM9 binary at its load
    // address and the cartridge header copy at 0x027FFE00.
    let load = Memory::main_offset(manifest.arm9.binary.load_address);
    machine.memory.main[load..load + arm9.len()].copy_from_slice(&arm9);
    let header_offset = Memory::main_offset(0x027F_FE00);
    machine.memory.main[header_offset..header_offset + header.len()].copy_from_slice(&header);
    // The ARM7 boot code copies the user settings to shared RAM.
    let settings = Memory::main_offset(USER_SETTINGS_COPY);
    machine.memory.main[settings..settings + firmware::USER_SETTINGS_LENGTH]
        .copy_from_slice(machine.firmware.user_settings());
    // Boot medium: card.
    machine.memory.main[Memory::main_offset(0x027F_FC40)] = 1;
    // The firmware records the boot card's chip ID; the SDK compares later
    // reads against it to detect a pulled-out card.
    for address in [0x027F_F800, 0x027F_FC00] {
        let offset = Memory::main_offset(address);
        machine.memory.main[offset..offset + 4].copy_from_slice(&card::CHIP_ID.to_le_bytes());
    }

    {
        let cpu = unsafe { &mut *cpu };
        cpu.read_pages = machine.memory.read_pages.as_mut_ptr();
        cpu.write_pages = machine.memory.write_pages.as_mut_ptr();
        cpu.runtime = (&mut *machine as *mut Machine).cast::<c_void>();
        machine.banks.set_banked_sp(cpu, MODE_IRQ, 0x0080_3FA0);
        machine.banks.set_banked_sp(cpu, MODE_SYSTEM, 0x0080_3EC0);
        cpu.r[13] = 0x0080_3FC0;
    }

    let host = Host {
        read8: host_read8,
        read16: host_read16,
        read32: host_read32,
        write8: host_write8,
        write16: host_write16,
        write32: host_write32,
        call: host_call,
        jump: host_jump,
        swi: host_swi,
        mcr: host_mcr,
        mrc: host_mrc,
        read_psr: host_read_psr,
        write_psr: host_write_psr,
        exception_return: host_exception_return,
        undefined: host_undefined,
        hle: super::threads::host_hle,
        tick: host_tick,
    };
    unsafe { set_host(&host) };

    let entry = manifest.arm9.binary.entry;
    println!(
        "Booting ARM9 entry 0x{entry:08X} with {} functions",
        table.len()
    );
    // The machine is reached through `cpu.runtime` from here on.
    let machine = Box::leak(machine);
    // Generated code that loops without calling the runtime cannot be
    // interrupted; after the wall-clock limit, report from another thread.
    // The report reads state the game thread is changing, so it is only a
    // snapshot for diagnosis, and the process exits right after it.
    let watched = machine as *mut Machine as usize;
    let seconds = machine.options.seconds;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(seconds));
        // SAFETY: the machine is leaked, so the pointer stays valid; the
        // racy read is accepted for a diagnostic snapshot before exiting.
        let machine = unsafe { &mut *(watched as *mut Machine) };
        machine.stop(&format!(
            "wall-clock limit of {seconds}s (likely a loop in recompiled code that never calls the runtime)"
        ));
    });
    let cpu_address = cpu as usize;
    super::threads::run_boot(move || {
        let cpu = cpu_address as *mut Cpu;
        call_target(cpu, entry);
        unsafe { get(cpu) }.stop("the entry function returned");
    });
    Ok(())
}
