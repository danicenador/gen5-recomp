//! Runs the recompiled ARM9 code: loads the game library, sets up memory as
//! the DS boot process leaves it, and services every `rt_*` hook.
//!
//! Generated code calls back into the host while host code calls into
//! generated code, so the machine is reached through the raw pointer stored
//! in `Cpu::runtime`. Functions that call into generated code take the CPU
//! pointer and only borrow the machine between those calls.

use super::card::{self, Card};
use super::cpu::{Banks, Cpu, MODE_IRQ, MODE_SVC, MODE_SYSTEM};
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
    card: Card,
    /// Every word the ARM9 sent through the IPC FIFO, for reports.
    fifo_sent: Vec<u32>,
    /// The ARM7 stand-in's IPCSYNC output nibble.
    arm7_sync_output: u32,
    /// Progress through the shared-RAM init handshake at 0x027FFFF0.
    arm7_init_phase: u8,
    options: Options,
    _library: Library,
}

const EVENT_HISTORY: usize = 48;
/// Shared-RAM words of the ARM9/ARM7 boot handshake.
const ARM7_BOOT_COUNTER: u32 = 0x027F_FC26;
const ARM7_BOOT_PROGRESS: u32 = 0x027F_FC28;
const ARM7_INIT_SYNC: u32 = 0x027F_FFF0;
/// Bit mask of PXI FIFO tags the ARM7 has registered handlers for.
const ARM7_PXI_READY: u32 = 0x027F_FF8C;
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
        self.fifo_sent.push(word);
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
        for word in &self.fifo_sent {
            let _ = writeln!(
                report,
                "  tag {:2} ({}) data 0x{:07X}{}",
                word & 0x1F,
                pxi_tag_name(word & 0x1F),
                word >> 6,
                if word & 0x20 != 0 {
                    " (error flag)"
                } else {
                    ""
                }
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
        print!("{report}");
        std::process::exit(0);
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
}
extern "C" fn host_write16(cpu: *mut Cpu, address: u32, value: u32) {
    unsafe { machine(cpu) }.write_slow(address, 2, value & 0xFFFF);
}
extern "C" fn host_write32(cpu: *mut Cpu, address: u32, value: u32) {
    unsafe { machine(cpu) }.write_slow(address, 4, value);
}

extern "C" fn host_call(cpu: *mut Cpu, target: u32) {
    let lr = unsafe { (*cpu).r[14] };
    unsafe { machine(cpu) }.record(Event::Call { target, lr });
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
    if spsr != 0 {
        machine.banks.set_spsr(value, fields);
    } else {
        machine.banks.set_cpsr(unsafe { &mut *cpu }, value, fields);
    }
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
        card,
        fifo_sent: Vec::new(),
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
    // Boot medium: card.
    machine.memory.main[Memory::main_offset(0x027F_FC40)] = 1;

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
    call_target(cpu, entry);
    machine.stop("the entry function returned");
}
