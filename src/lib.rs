//! Native format and import foundation for Pokémon Generation V.
//!
//! A ROM explorer/importer, a static recompiler for the ARM9 code, and the
//! Nintendo DS runtime the recompiled code runs on.

pub mod formats;
pub mod importer;
pub mod nds;
pub mod recomp;
pub mod runtime;

pub type Result<T> = std::result::Result<T, String>;
