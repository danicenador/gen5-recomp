//! The Nintendo DS hardware the recompiled ARM9 code runs against.
//!
//! - `backup`: the card's save memory, kept in a file.
//! - `card`: the game card's ROM read commands.
//! - `cpu`: CPU state shared with generated code, banked registers, PSRs.
//! - `firmware`: the console's firmware flash (user and Wi-Fi settings).
//! - `memory`: the ARM9 address space as page tables over host buffers.
//! - `vram`: the VRAM banks and their VRAMCNT mapping.
//! - `io`: I/O registers (display timing, interrupts, DMA, math, IPC).
//! - `gpu2d`: the two 2D engines, rendering a frame for each screen.
//! - `interpreter`: fallback for code without a recompiled function.
//! - `threads`: NitroSDK threads as host threads taking turns.
//! - `machine`: boot, call dispatch across overlays, interrupts, BIOS calls.

pub mod backup;
pub mod card;
pub mod cpu;
pub mod firmware;
pub mod gpu2d;
mod interpreter;
pub mod io;
pub mod machine;
pub mod memory;
mod threads;
pub mod vram;
