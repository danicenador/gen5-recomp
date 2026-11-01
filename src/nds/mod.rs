//! The Nintendo DS hardware the recompiled ARM9 code runs against.
//!
//! - `backup`: the card's save memory, kept in a file.
//! - `card`: the game card's ROM read commands.
//! - `cpu`: CPU state shared with generated code, banked registers, PSRs.
//! - `memory`: the ARM9 address space as page tables over host buffers.
//! - `io`: I/O registers (display timing, interrupts, DMA, math, IPC).
//! - `interpreter`: fallback for code without a recompiled function.
//! - `threads`: NitroSDK threads as host threads taking turns.
//! - `machine`: boot, call dispatch across overlays, interrupts, BIOS calls.

pub mod backup;
pub mod card;
pub mod cpu;
mod interpreter;
pub mod io;
pub mod machine;
pub mod memory;
mod threads;
