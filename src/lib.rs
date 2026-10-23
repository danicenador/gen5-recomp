//! Native format and import foundation for Pokémon Generation V.
//!
//! The first slices provide a ROM explorer/importer and a BW2 map geometry
//! preview. Runtime simulation and full asset conversion build on these layers.

pub mod formats;
pub mod game;
pub mod importer;
pub mod render;

pub type Result<T> = std::result::Result<T, String>;
