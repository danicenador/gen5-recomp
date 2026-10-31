//! Native format and import foundation for Pokémon Generation V.
//!
//! The first slices provide a ROM explorer/importer and a BW2 desktop field
//! runtime. Game rules remain separate from the platform rendering layer.

pub mod formats;
pub mod game;
pub mod importer;
pub mod nds;
pub mod recomp;
pub mod render;
pub mod runtime;

pub type Result<T> = std::result::Result<T, String>;
