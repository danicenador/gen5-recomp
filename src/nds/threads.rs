//! NitroSDK threads on host threads.
//!
//! The SDK switches threads with `OS_SaveContext` / `OS_LoadContext`, which
//! store the registers in an `OSContext` and later resume by jumping to the
//! saved `pc`. Recompiled code keeps its state on the host stack, so each
//! guest thread runs on its own host thread instead, and exactly one host
//! thread runs at a time (it holds the baton):
//!
//! - `SaveContext` writes the context, marks it resumable and returns 0.
//! - `LoadContext` passes the baton to the context's host thread, starting
//!   one if the context is new (from `OS_CreateThread`), then waits for the
//!   baton to come back. When it does, it restores this thread's saved
//!   registers and returns. In the SDK the code after `OS_LoadContext` is
//!   the same as after `OS_SaveContext` returning 1, so returning from
//!   `LoadContext` resumes the thread where it left off.

use super::cpu::{Cpu, MODE_SVC};
use super::machine;
use crate::recomp::hle::Hle;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

/// `OSContext` layout: CPSR, r0-r14, pc + 4, SVC-mode sp.
const CONTEXT_REGISTERS: u32 = 0x04;
const CONTEXT_PC: u32 = 0x40;
const CONTEXT_SVC_SP: u32 = 0x44;
/// Stored as the pc of a context saved by `SaveContext`; such a context is
/// resumed by waking the host thread that saved it.
const RESUME_PC: u32 = 0xFFFF_FFF0;
const PSR_THUMB: u32 = 1 << 5;
const THREAD_STACK: usize = 64 * 1024 * 1024;

struct Baton {
    /// The token of the host thread allowed to run.
    running: Mutex<u64>,
    turn: Condvar,
}

static BATON: Baton = Baton {
    running: Mutex::new(0),
    turn: Condvar::new(),
};

/// Saved contexts and the token of the host thread waiting to resume them.
static SAVED: Mutex<Option<HashMap<u32, u64>>> = Mutex::new(None);
static NEXT_TOKEN: Mutex<u64> = Mutex::new(1);

thread_local! {
    /// This host thread's baton token; the boot thread has token 0.
    static MY_TOKEN: Cell<u64> = const { Cell::new(0) };
}

fn new_token() -> u64 {
    let mut next = NEXT_TOKEN.lock().unwrap();
    *next += 1;
    *next
}

fn pass_baton(token: u64) {
    *BATON.running.lock().unwrap() = token;
    BATON.turn.notify_all();
}

fn wait_for_baton(token: u64) {
    let mut running = BATON.running.lock().unwrap();
    while *running != token {
        running = BATON.turn.wait(running).unwrap();
    }
}

pub(super) extern "C" fn host_hle(cpu: *mut Cpu, id: u32) {
    match id {
        id if id == Hle::SaveContext as u32 => save_context(cpu),
        id if id == Hle::LoadContext as u32 => load_context(cpu),
        id if id == Hle::IrqThreadSwitch as u32 => irq_thread_switch(cpu),
        _ => unsafe { machine::get(cpu) }.stop(&format!("unknown HLE routine {id}")),
    }
}

/// Write the CPU state to a context and mark it resumable by this host
/// thread. `r0` is the value the context's r0 slot gets.
fn write_context(cpu_ptr: *mut Cpu, context: u32, r0: u32) {
    let machine = unsafe { machine::get(cpu_ptr) };
    let cpu = unsafe { &mut *cpu_ptr };
    let cpsr = machine.banks.cpsr(cpu);
    let svc_sp = machine.banks.banked_sp(cpu, MODE_SVC);
    machine.write32(context, cpsr);
    for index in 0..15 {
        let value = if index == 0 { r0 } else { cpu.r[index] };
        machine.write32(context + CONTEXT_REGISTERS + 4 * index as u32, value);
    }
    machine.write32(context + CONTEXT_PC, RESUME_PC);
    machine.write32(context + CONTEXT_SVC_SP, svc_sp);
    SAVED
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(context, MY_TOKEN.get());
}

fn save_context(cpu_ptr: *mut Cpu) {
    let context = unsafe { (*cpu_ptr).r[0] };
    // r0 = 1: what `SaveContext` returns when the context is resumed.
    write_context(cpu_ptr, context, 1);
    unsafe { (*cpu_ptr).r[0] = 0 };
}

/// Load a context's registers and mode into the CPU.
fn restore(cpu_ptr: *mut Cpu, context: u32) {
    let machine = unsafe { machine::get(cpu_ptr) };
    let cpu = unsafe { &mut *cpu_ptr };
    let cpsr = machine.read32(context);
    machine.banks.set_cpsr(cpu, cpsr, 0xF);
    for index in 0..15 {
        cpu.r[index] = machine.read32(context + CONTEXT_REGISTERS + 4 * index as u32);
    }
    let svc_sp = machine.read32(context + CONTEXT_SVC_SP);
    machine.banks.set_banked_sp(cpu, MODE_SVC, svc_sp);
}

/// The context this host thread last saved.
fn my_context() -> Option<u32> {
    let me = MY_TOKEN.get();
    SAVED.lock().unwrap().as_ref().and_then(|saved| {
        saved
            .iter()
            .find(|(_, token)| **token == me)
            .map(|(context, _)| *context)
    })
}

/// Give the CPU to `target`'s thread and wait until this thread is resumed.
/// Returns false, without waiting, if `target` is this thread's context.
fn hand_off(cpu_ptr: *mut Cpu, target: u32) -> bool {
    let machine = unsafe { machine::get(cpu_ptr) };
    let resumable = machine.read32(target + CONTEXT_PC) == RESUME_PC;
    let parked = SAVED
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|saved| saved.get(&target).copied());
    let me = MY_TOKEN.get();
    let next = match parked {
        Some(token) if resumable => token,
        _ => start_thread(cpu_ptr, target),
    };
    if next == me {
        return false;
    }
    pass_baton(next);
    wait_for_baton(me);
    true
}

fn load_context(cpu_ptr: *mut Cpu) {
    let target = unsafe { (*cpu_ptr).r[0] };
    let mine = my_context();
    if !hand_off(cpu_ptr, target) {
        restore(cpu_ptr, target);
        return;
    }
    // Resumed: continue as this thread, from where it saved its context.
    if let Some(context) = mine {
        restore(cpu_ptr, context);
    }
}

/// NitroSDK `OSThreadInfo` and `OSThread` offsets, as used by the IRQ
/// handler's thread switch.
const INFO_CURRENT: u32 = 0x04;
const INFO_LIST: u32 = 0x08;
const INFO_SWITCH_CALLBACK: u32 = 0x0C;
const THREAD_STATE: u32 = 0x64;
const THREAD_NEXT: u32 = 0x68;
const STATE_READY: u32 = 1;

/// The IRQ handler found a thread switch is due. Clear the request, return
/// from the handler (pop the `lr` it saved), and switch threads once the
/// interrupt is over.
fn irq_thread_switch(cpu_ptr: *mut Cpu) {
    let machine = unsafe { machine::get(cpu_ptr) };
    let cpu = unsafe { &mut *cpu_ptr };
    let info = cpu.r[12];
    machine.write16(info, 0);
    cpu.r[13] = cpu.r[13].wrapping_add(4);
    machine.pending_switch = Some(info);
}

/// After an interrupt: switch to the highest-priority ready thread if it is
/// not the interrupted one. The interrupted thread's whole state is kept on
/// this host thread and restored when it is scheduled again.
pub(super) fn reschedule_after_irq(cpu_ptr: *mut Cpu, info: u32) {
    let machine = unsafe { machine::get(cpu_ptr) };
    // The list is sorted by priority, so the first ready thread wins.
    let mut next = machine.read32(info + INFO_LIST);
    while next != 0 && machine.read16(next + THREAD_STATE) != STATE_READY {
        next = machine.read32(next + THREAD_NEXT);
    }
    let current = machine.read32(info + INFO_CURRENT);
    if next == 0 || next == current {
        return;
    }
    let snapshot = Snapshot::take(cpu_ptr);
    let callback = machine.read32(info + INFO_SWITCH_CALLBACK);
    if callback != 0 {
        let cpu = unsafe { &mut *cpu_ptr };
        cpu.r[0] = current;
        cpu.r[1] = next;
        cpu.r[14] = SWITCH_CALLBACK_RETURN;
        machine::call_any(cpu_ptr, callback);
        snapshot.restore(cpu_ptr);
    }
    let machine = unsafe { machine::get(cpu_ptr) };
    machine.write32(info + INFO_CURRENT, next);
    // The thread's context is the start of its OSThread.
    write_context(cpu_ptr, current, snapshot.r[0]);
    hand_off(cpu_ptr, next);
    snapshot.restore(cpu_ptr);
}

const SWITCH_CALLBACK_RETURN: u32 = 0xFFFF_0004;

/// The full register state of an interrupted thread.
struct Snapshot {
    r: [u32; 16],
    flags: [u32; 5],
    cpsr: u32,
}

impl Snapshot {
    fn take(cpu_ptr: *mut Cpu) -> Self {
        let machine = unsafe { machine::get(cpu_ptr) };
        let cpu = unsafe { &*cpu_ptr };
        Self {
            r: cpu.r,
            flags: [cpu.n, cpu.z, cpu.c, cpu.v, cpu.q],
            cpsr: machine.banks.cpsr(cpu),
        }
    }

    fn restore(&self, cpu_ptr: *mut Cpu) {
        let machine = unsafe { machine::get(cpu_ptr) };
        let cpu = unsafe { &mut *cpu_ptr };
        machine.banks.set_cpsr(cpu, self.cpsr, 0xF);
        cpu.r = self.r;
        [cpu.n, cpu.z, cpu.c, cpu.v, cpu.q] = self.flags;
    }
}

/// Start a host thread for a fresh context (from `OS_CreateThread`); it
/// waits for the baton, then runs the thread function.
fn start_thread(cpu_ptr: *mut Cpu, context: u32) -> u64 {
    let token = new_token();
    if let Some(saved) = SAVED.lock().unwrap().as_mut() {
        saved.remove(&context);
    }
    let cpu_address = cpu_ptr as usize;
    let spawned = std::thread::Builder::new()
        .name(format!("guest-thread-{context:08X}"))
        .stack_size(THREAD_STACK)
        .spawn(move || {
            MY_TOKEN.set(token);
            wait_for_baton(token);
            let cpu = cpu_address as *mut Cpu;
            restore(cpu, context);
            let machine = unsafe { machine::get(cpu) };
            let cpsr = machine.read32(context);
            let entry = machine.read32(context + CONTEXT_PC).wrapping_sub(4);
            let thumb = u32::from(cpsr & PSR_THUMB != 0);
            // The thread function returns to `lr` (OS_ExitThread).
            let exit = unsafe { (*cpu).r[14] };
            machine::call_any(cpu, entry | thumb);
            machine::call_any(cpu, exit);
            unsafe { machine::get(cpu) }.stop(&format!(
                "thread with context 0x{context:08X} returned past its exit routine"
            ));
        });
    if let Err(error) = spawned {
        unsafe { machine::get(cpu_ptr) }.stop(&format!("could not start a host thread: {error}"));
    }
    token
}

/// For reports: each parked thread's context with `lr`, `sp`, and the
/// return addresses found on its stack (a rough call chain).
pub(super) fn describe(read32: &mut dyn FnMut(u32) -> u32) -> Vec<String> {
    let contexts: Vec<u32> = SAVED
        .lock()
        .unwrap()
        .as_ref()
        .map(|saved| saved.keys().copied().collect())
        .unwrap_or_default();
    let mut lines = Vec::new();
    for context in contexts {
        let lr = read32(context + CONTEXT_REGISTERS + 4 * 14);
        let sp = read32(context + CONTEXT_REGISTERS + 4 * 13);
        let returns: Vec<String> = (0..96)
            .map(|slot| read32(sp + 4 * slot))
            .filter(|&word| {
                (0x0200_4000..0x0230_0000).contains(&word)
                    || (0x01FF_8000..0x0200_0000).contains(&word)
            })
            .take(16)
            .map(|word| format!("{word:08X}"))
            .collect();
        lines.push(format!(
            "context 0x{context:08X}: lr 0x{lr:08X} sp 0x{sp:08X}; stack code pointers {}",
            returns.join(" ")
        ));
    }
    lines
}

/// Run the boot code on a host thread with a large stack (generated code
/// uses far more stack than the guest), as baton holder 0.
pub(super) fn run_boot(run: impl FnOnce() + Send + 'static) {
    let handle = std::thread::Builder::new()
        .name("guest-boot".to_owned())
        .stack_size(THREAD_STACK)
        .spawn(run)
        .expect("could not start the boot thread");
    let _ = handle.join();
}
