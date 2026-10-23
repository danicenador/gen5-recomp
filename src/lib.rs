//! Native format and import foundation for Pokémon Generation V.
//!
//! The first vertical slice is intentionally an asset importer and explorer. It
//! reads the original NDS container directly and writes an immutable, file-level
//! cache; game execution and asset conversion sit on top of these boundaries.

pub mod formats;
pub mod game;
pub mod importer;

pub type Result<T> = std::result::Result<T, String>;
