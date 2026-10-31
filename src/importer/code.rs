//! Export of the ROM's executable code for the static recompiler.
//!
//! Writes `code/` inside an import cache:
//!
//! - `arm9.bin`: the decompressed ARM9 static binary as loaded at
//!   `load_address`, autoload payloads included at their image offsets.
//! - `arm7.bin`: the ARM7 static binary.
//! - `overlay9/overlay_NNNN.bin` (and `overlay7/`): decompressed overlays.
//! - `code.json`: addresses, autoload blocks, overlay tables and CRC-32s.

use crate::Result;
use crate::formats::nds::{AutoloadBlock, Cpu, Crc32, ModuleParams, NdsRom, OverlayEntry};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

pub const CODE_FORMAT: &str = "gen5-code-v1";

#[derive(Debug, Serialize, Deserialize)]
pub struct CodeManifest {
    pub format: String,
    pub arm9: Arm9Binary,
    pub arm7: Binary,
    pub overlays9: Vec<OverlayFile>,
    pub overlays7: Vec<OverlayFile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Binary {
    pub file: String,
    pub load_address: u32,
    pub entry: u32,
    pub size: u32,
    pub crc32: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Arm9Binary {
    #[serde(flatten)]
    pub binary: Binary,
    pub was_compressed: bool,
    /// Bytes from the start of `arm9.bin` that stay at `load_address`.
    pub main_size: u32,
    pub module_params: ModuleParams,
    pub autoloads: Vec<AutoloadBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverlayFile {
    pub file: String,
    pub crc32: String,
    #[serde(flatten)]
    pub entry: OverlayEntry,
}

/// Write the code export into `destination`, which must not exist yet.
/// Write `header.bin`: the cartridge header the firmware copies to RAM.
pub fn export_header(rom: &mut NdsRom, destination: &Path) -> Result<()> {
    let header = rom.read_at(0, 0x200)?;
    write_file(&destination.join("header.bin"), &header).map(|_| ())
}

pub fn export_code(rom: &mut NdsRom, destination: &Path) -> Result<CodeManifest> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "could not create code directory '{}': {error}",
            destination.display()
        )
    })?;

    export_header(rom, destination)?;
    let arm9 = rom.arm9_image()?;
    let arm9_crc = write_file(&destination.join("arm9.bin"), &arm9.bytes)?;
    let arm7 = rom.arm7_binary()?;
    let arm7_crc = write_file(&destination.join("arm7.bin"), &arm7)?;

    let overlays9 = export_overlays(rom, Cpu::Arm9, destination, "overlay9")?;
    let overlays7 = export_overlays(rom, Cpu::Arm7, destination, "overlay7")?;

    let manifest = CodeManifest {
        format: CODE_FORMAT.to_owned(),
        arm9: Arm9Binary {
            binary: Binary {
                file: "arm9.bin".to_owned(),
                load_address: arm9.load_address,
                entry: arm9.entry,
                size: arm9.bytes.len() as u32,
                crc32: format!("{arm9_crc:08X}"),
            },
            was_compressed: arm9.was_compressed,
            main_size: arm9.main_len() as u32,
            module_params: arm9.params,
            autoloads: arm9.autoloads,
        },
        arm7: Binary {
            file: "arm7.bin".to_owned(),
            load_address: rom.header.arm7_load,
            entry: rom.header.arm7_entry,
            size: arm7.len() as u32,
            crc32: format!("{arm7_crc:08X}"),
        },
        overlays9,
        overlays7,
    };
    let manifest_file = File::create(destination.join("code.json"))
        .map_err(|error| format!("could not create code manifest: {error}"))?;
    serde_json::to_writer_pretty(&manifest_file, &manifest)
        .map_err(|error| format!("could not write code manifest: {error}"))?;
    manifest_file
        .sync_all()
        .map_err(|error| format!("could not flush code manifest: {error}"))?;
    Ok(manifest)
}

fn export_overlays(
    rom: &mut NdsRom,
    cpu: Cpu,
    destination: &Path,
    directory: &str,
) -> Result<Vec<OverlayFile>> {
    let table = rom.overlay_table(cpu)?;
    if table.is_empty() {
        return Ok(Vec::new());
    }
    fs::create_dir_all(destination.join(directory))
        .map_err(|error| format!("could not create '{directory}' directory: {error}"))?;
    table
        .into_iter()
        .map(|entry| {
            let file = format!("{directory}/overlay_{:04}.bin", entry.id);
            let bytes = rom.overlay_image(&entry)?;
            let crc = write_file(&destination.join(&file), &bytes)?;
            Ok(OverlayFile {
                file,
                crc32: format!("{crc:08X}"),
                entry,
            })
        })
        .collect()
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<u32> {
    let mut file = File::create(path)
        .map_err(|error| format!("could not create '{}': {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write '{}': {error}", path.display()))?;
    let mut crc = Crc32::new();
    crc.update(bytes);
    Ok(crc.finish())
}
