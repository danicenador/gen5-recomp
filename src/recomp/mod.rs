//! Static recompilation of the ARM9 code.
//!
//! - `arm`: ARMv5TE ARM/Thumb decoder and disassembler.
//! - `program`: the ARM9 static binary, autoload blocks and overlays as
//!   modules; overlays that share addresses stay distinct.
//! - `analysis`: function discovery and control-flow recovery.

pub mod analysis;
pub mod arm;
pub mod codegen;
pub mod compile;
pub mod program;
