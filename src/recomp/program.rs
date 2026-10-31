//! The ARM9 program as separately loadable modules.
//!
//! Overlays share address ranges, so an address alone does not name code. A
//! [`ModuleId`] plus an address does: `ov042:0x021D0000` and
//! `ov117:0x021D0000` are different functions.

use crate::Result;
use crate::importer::code::{CODE_FORMAT, CodeManifest};
use serde::{Serialize, Serializer};
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ModuleId {
    /// The ARM9 static binary that stays at its load address.
    Main,
    /// A block the boot code copies elsewhere (ITCM, DTCM, ...), by index.
    Autoload(u8),
    Overlay(u16),
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleId::Main => f.write_str("main"),
            ModuleId::Autoload(index) => write!(f, "autoload{index}"),
            ModuleId::Overlay(id) => write!(f, "ov{id:03}"),
        }
    }
}

impl FromStr for ModuleId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || format!("invalid module '{text}' (expected main, autoloadN or ovN)");
        if text == "main" {
            Ok(ModuleId::Main)
        } else if let Some(index) = text.strip_prefix("autoload") {
            index.parse().map(ModuleId::Autoload).map_err(|_| invalid())
        } else if let Some(id) = text.strip_prefix("ov") {
            id.parse().map(ModuleId::Overlay).map_err(|_| invalid())
        } else {
            Err(invalid())
        }
    }
}

impl Serialize for ModuleId {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

pub struct Module {
    pub id: ModuleId,
    pub base: u32,
    pub bytes: Vec<u8>,
    pub bss_size: u32,
    /// Static initializer pointer table (overlays only), as RAM addresses.
    pub sinit: Option<(u32, u32)>,
}

impl Module {
    pub fn end(&self) -> u32 {
        self.base + self.bytes.len() as u32
    }

    pub fn contains(&self, address: u32) -> bool {
        address >= self.base && address < self.end()
    }

    /// Whether the module's whole loaded range (BSS included) overlaps another's.
    pub fn overlaps(&self, other: &Module) -> bool {
        let end = self.end() + self.bss_size;
        let other_end = other.end() + other.bss_size;
        self.base < other_end && other.base < end
    }

    pub fn u16_at(&self, address: u32) -> Option<u16> {
        let offset = address.checked_sub(self.base)? as usize;
        let bytes = self.bytes.get(offset..offset + 2)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub fn u32_at(&self, address: u32) -> Option<u32> {
        let offset = address.checked_sub(self.base)? as usize;
        let bytes = self.bytes.get(offset..offset + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

pub struct Program {
    pub modules: Vec<Module>,
    pub entry: u32,
}

impl Program {
    /// Load the `code/` directory written by the importer.
    pub fn load(code_dir: &Path) -> Result<Self> {
        let manifest_path = code_dir.join("code.json");
        let text = fs::read_to_string(&manifest_path).map_err(|error| {
            format!(
                "could not read '{}': {error} (run `import` to create the code export)",
                manifest_path.display()
            )
        })?;
        let manifest: CodeManifest = serde_json::from_str(&text)
            .map_err(|error| format!("could not parse '{}': {error}", manifest_path.display()))?;
        if manifest.format != CODE_FORMAT {
            return Err(format!(
                "code export format '{}' is not supported (expected {CODE_FORMAT})",
                manifest.format
            ));
        }

        let read = |file: &str| {
            fs::read(code_dir.join(file))
                .map_err(|error| format!("could not read code file '{file}': {error}"))
        };
        let arm9_bytes = read(&manifest.arm9.binary.file)?;
        let main_size = manifest.arm9.main_size as usize;
        if main_size > arm9_bytes.len() {
            return Err("ARM9 main size exceeds the ARM9 binary".to_owned());
        }
        let static_bss = manifest
            .arm9
            .module_params
            .static_bss_end
            .saturating_sub(manifest.arm9.module_params.static_bss_start);

        let mut modules = vec![Module {
            id: ModuleId::Main,
            base: manifest.arm9.binary.load_address,
            bytes: arm9_bytes[..main_size].to_vec(),
            bss_size: static_bss,
            sinit: None,
        }];
        for (index, block) in manifest.arm9.autoloads.iter().enumerate() {
            let start = block.image_offset as usize;
            let bytes = arm9_bytes
                .get(start..start + block.size as usize)
                .ok_or_else(|| format!("autoload {index} lies outside the ARM9 binary"))?;
            modules.push(Module {
                id: ModuleId::Autoload(index as u8),
                base: block.ram_address,
                bytes: bytes.to_vec(),
                bss_size: block.bss_size,
                sinit: None,
            });
        }
        for overlay in &manifest.overlays9 {
            let bytes = read(&overlay.file)?;
            let entry = &overlay.entry;
            modules.push(Module {
                id: ModuleId::Overlay(entry.id as u16),
                base: entry.ram_address,
                bytes,
                bss_size: entry.bss_size,
                sinit: (entry.sinit_start != 0).then_some((entry.sinit_start, entry.sinit_end)),
            });
        }
        Ok(Self {
            modules,
            entry: manifest.arm9.binary.entry,
        })
    }

    pub fn module(&self, id: ModuleId) -> Option<&Module> {
        self.modules.iter().find(|module| module.id == id)
    }

    /// Modules (other than `from`) that may hold `address` while `from` is
    /// loaded: static modules first, then overlays that do not overlap `from`.
    pub fn resolve(&self, from: ModuleId, address: u32) -> Resolution {
        let Some(caller) = self.module(from) else {
            return Resolution::Outside;
        };
        if caller.contains(address) {
            return Resolution::Module(from);
        }
        if let Some(module) = self
            .modules
            .iter()
            .find(|module| !matches!(module.id, ModuleId::Overlay(_)) && module.contains(address))
        {
            return Resolution::Module(module.id);
        }
        let candidates: Vec<ModuleId> = self
            .modules
            .iter()
            .filter(|module| matches!(module.id, ModuleId::Overlay(_)))
            .filter(|module| module.contains(address))
            .filter(|module| !matches!(from, ModuleId::Overlay(_)) || !module.overlaps(caller))
            .map(|module| module.id)
            .collect();
        match candidates.len() {
            0 => Resolution::Outside,
            1 => Resolution::Module(candidates[0]),
            _ => Resolution::Ambiguous(candidates),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Module(ModuleId),
    /// Several overlays could be loaded at the address.
    Ambiguous(Vec<ModuleId>),
    /// No loaded code covers the address (BIOS, I/O, or a bad pointer).
    Outside,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(id: ModuleId, base: u32, size: usize) -> Module {
        Module {
            id,
            base,
            bytes: vec![0; size],
            bss_size: 0,
            sinit: None,
        }
    }

    #[test]
    fn module_ids_round_trip() {
        for id in [ModuleId::Main, ModuleId::Autoload(1), ModuleId::Overlay(42)] {
            assert_eq!(id.to_string().parse::<ModuleId>().unwrap(), id);
        }
        assert_eq!(ModuleId::Overlay(7).to_string(), "ov007");
        assert!("ovx".parse::<ModuleId>().is_err());
    }

    #[test]
    fn resolves_overlapping_overlays() {
        let program = Program {
            entry: 0,
            modules: vec![
                module(ModuleId::Main, 0x0200_0000, 0x1000),
                module(ModuleId::Overlay(0), 0x0210_0000, 0x1000),
                module(ModuleId::Overlay(1), 0x0210_0000, 0x2000),
                module(ModuleId::Overlay(2), 0x0220_0000, 0x1000),
                module(ModuleId::Overlay(3), 0x0220_0000, 0x1000),
            ],
        };
        assert_eq!(
            program.resolve(ModuleId::Overlay(0), 0x0200_0010),
            Resolution::Module(ModuleId::Main)
        );
        assert_eq!(
            program.resolve(ModuleId::Overlay(0), 0x0210_0010),
            Resolution::Module(ModuleId::Overlay(0))
        );
        assert_eq!(
            program.resolve(ModuleId::Overlay(2), 0x0210_1800),
            Resolution::Module(ModuleId::Overlay(1))
        );
        // Overlay 1 overlaps overlay 0, so it cannot be loaded alongside it.
        assert_eq!(
            program.resolve(ModuleId::Overlay(0), 0x0210_1800),
            Resolution::Outside
        );
        assert_eq!(
            program.resolve(ModuleId::Main, 0x0220_0000),
            Resolution::Ambiguous(vec![ModuleId::Overlay(2), ModuleId::Overlay(3)])
        );
        assert_eq!(
            program.resolve(ModuleId::Main, 0xFFFF_0000),
            Resolution::Outside
        );
    }
}
