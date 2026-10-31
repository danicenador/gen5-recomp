//! Executable code in an NDS ROM: the ARM9/ARM7 static binaries, the ARM9
//! autoload blocks (ITCM/DTCM) and the overlay tables.
//!
//! This is the input to static recompilation. Everything is returned as the
//! bytes that sit in RAM after the boot loader and decompression have run.

use super::NdsRom;
use crate::Result;
use crate::formats::blz;
use serde::{Deserialize, Serialize};

const OVERLAY_ENTRY_SIZE: usize = 32;
const NITROCODE_LE: u32 = 0x2106_C0DE;
const NITROCODE_BE: u32 = 0xDEC0_0621;
/// The ARM9 module parameters end with the two nitrocode markers.
const MODULE_PARAMS_SIZE: usize = 0x24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cpu {
    Arm9,
    Arm7,
}

/// One entry of an ARM9 or ARM7 overlay table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayEntry {
    pub id: u32,
    pub ram_address: u32,
    pub ram_size: u32,
    pub bss_size: u32,
    /// Static initializer function pointer table, as RAM addresses.
    pub sinit_start: u32,
    pub sinit_end: u32,
    pub file_id: u32,
    /// Size of the BLZ-compressed file, when `compressed` is set.
    pub compressed_size: u32,
    pub compressed: bool,
    pub authenticated: bool,
}

impl OverlayEntry {
    pub fn ram_end(&self) -> u32 {
        self.ram_address
            .wrapping_add(self.ram_size)
            .wrapping_add(self.bss_size)
    }
}

/// Fields of the ARM9 `_start_ModuleParams` block that the boot code uses.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModuleParams {
    /// RAM offset of the parameters inside the decompressed ARM9 binary.
    pub offset: u32,
    pub autoload_list_start: u32,
    pub autoload_list_end: u32,
    pub autoload_start: u32,
    pub static_bss_start: u32,
    pub static_bss_end: u32,
    /// RAM address where the compressed static image ends, or 0 when the
    /// ARM9 binary is stored uncompressed.
    pub compressed_static_end: u32,
    pub sdk_version: u32,
}

/// A block the ARM9 boot code copies out of the static binary, usually ITCM
/// and DTCM contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoloadBlock {
    pub ram_address: u32,
    pub size: u32,
    pub bss_size: u32,
    /// Third word of a 16-byte TWL SDK entry. In BW2 it equals
    /// `ram_address`; it is kept until its meaning is confirmed.
    pub twl_extra: Option<u32>,
    /// Offset of this block's bytes inside the decompressed ARM9 image.
    pub image_offset: u32,
}

/// The decompressed ARM9 static binary with its layout.
#[derive(Debug, Clone)]
pub struct Arm9Image {
    pub load_address: u32,
    pub entry: u32,
    /// Decompressed binary. The compressed-end field in the module params is
    /// cleared, matching RAM after the boot code has decompressed it.
    pub bytes: Vec<u8>,
    pub was_compressed: bool,
    pub params: ModuleParams,
    pub autoloads: Vec<AutoloadBlock>,
}

impl Arm9Image {
    /// The part of the binary that stays at `load_address` (before autoloads).
    pub fn main_len(&self) -> usize {
        self.autoloads
            .first()
            .map_or(self.bytes.len(), |block| block.image_offset as usize)
    }
}

impl NdsRom {
    pub fn arm9_image(&mut self) -> Result<Arm9Image> {
        let header = self.header.clone();
        let raw = self.read_at(u64::from(header.arm9.offset), header.arm9.size as usize)?;
        let footer_offset = u64::from(header.arm9.offset) + u64::from(header.arm9.size);
        let footer = if footer_offset + 12 <= self.byte_len {
            self.read_at(footer_offset, 12)?
        } else {
            Vec::new()
        };
        let params_offset = find_module_params(&raw, &footer)?;
        let raw_params = parse_module_params(&raw, params_offset);

        let (mut bytes, was_compressed) = if raw_params.compressed_static_end == 0 {
            (raw, false)
        } else {
            let end = raw_params
                .compressed_static_end
                .checked_sub(header.arm9_load)
                .map(|value| value as usize)
                .filter(|&value| value <= raw.len())
                .ok_or_else(|| {
                    format!(
                        "ARM9 compressed end 0x{:08X} lies outside the binary",
                        raw_params.compressed_static_end
                    )
                })?;
            let mut decompressed = blz::decompress(&raw[..end])
                .map_err(|error| format!("could not decompress ARM9: {error}"))?;
            decompressed.extend_from_slice(&raw[end..]);
            (decompressed, true)
        };
        // Clear compressed_static_end like the boot code does after decoding.
        put_u32(&mut bytes, params_offset + 0x14, 0);
        let params = parse_module_params(&bytes, params_offset);
        // DSi-capable titles use the TWL SDK's 16-byte autoload records.
        let record_size = if header.unit_code == 0 { 12 } else { 16 };
        let autoloads = parse_autoloads(&bytes, header.arm9_load, &params, record_size)?;

        Ok(Arm9Image {
            load_address: header.arm9_load,
            entry: header.arm9_entry,
            bytes,
            was_compressed,
            params,
            autoloads,
        })
    }

    pub fn arm7_binary(&mut self) -> Result<Vec<u8>> {
        let section = self.header.arm7;
        self.read_at(u64::from(section.offset), section.size as usize)
    }

    pub fn overlay_table(&mut self, cpu: Cpu) -> Result<Vec<OverlayEntry>> {
        let section = match cpu {
            Cpu::Arm9 => self.header.arm9_overlay,
            Cpu::Arm7 => self.header.arm7_overlay,
        };
        if section.size == 0 {
            return Ok(Vec::new());
        }
        if !(section.size as usize).is_multiple_of(OVERLAY_ENTRY_SIZE) {
            return Err(format!(
                "{} size 0x{:X} is not a multiple of {OVERLAY_ENTRY_SIZE}",
                section.name, section.size
            ));
        }
        let table = self.read_at(u64::from(section.offset), section.size as usize)?;
        let file_count = self.filesystem.allocations.len();
        table
            .chunks_exact(OVERLAY_ENTRY_SIZE)
            .enumerate()
            .map(|(index, record)| {
                let flags = u32_at(record, 28);
                let entry = OverlayEntry {
                    id: u32_at(record, 0),
                    ram_address: u32_at(record, 4),
                    ram_size: u32_at(record, 8),
                    bss_size: u32_at(record, 12),
                    sinit_start: u32_at(record, 16),
                    sinit_end: u32_at(record, 20),
                    file_id: u32_at(record, 24),
                    compressed_size: flags & 0x00FF_FFFF,
                    compressed: flags & 0x0100_0000 != 0,
                    authenticated: flags & 0x0200_0000 != 0,
                };
                if entry.id as usize != index {
                    return Err(format!(
                        "{} entry {index} declares overlay ID {}",
                        section.name, entry.id
                    ));
                }
                if entry.file_id as usize >= file_count {
                    return Err(format!(
                        "overlay {} uses file {}, beyond the {file_count}-entry FAT",
                        entry.id, entry.file_id
                    ));
                }
                Ok(entry)
            })
            .collect()
    }

    /// Read an overlay as it appears in RAM, without its zeroed BSS.
    pub fn overlay_image(&mut self, entry: &OverlayEntry) -> Result<Vec<u8>> {
        let (start, end) = self.filesystem.allocations[entry.file_id as usize];
        let raw = self.read_at(u64::from(start), (end - start) as usize)?;
        let bytes = if entry.compressed {
            let compressed_size = entry.compressed_size as usize;
            if compressed_size > raw.len() {
                return Err(format!(
                    "overlay {} declares 0x{compressed_size:X} compressed bytes in a 0x{:X}-byte file",
                    entry.id,
                    raw.len()
                ));
            }
            blz::decompress(&raw[..compressed_size])
                .map_err(|error| format!("could not decompress overlay {}: {error}", entry.id))?
        } else {
            raw
        };
        if bytes.len() != entry.ram_size as usize {
            return Err(format!(
                "overlay {} is 0x{:X} bytes, but its table entry declares 0x{:X}",
                entry.id,
                bytes.len(),
                entry.ram_size
            ));
        }
        Ok(bytes)
    }
}

/// Find the module params. Retail ROMs store a 12-byte footer right after the
/// ARM9 binary whose second word is the params offset; otherwise scan for the
/// nitrocode markers.
fn find_module_params(arm9: &[u8], footer: &[u8]) -> Result<usize> {
    let is_params = |offset: usize| {
        offset + MODULE_PARAMS_SIZE <= arm9.len()
            && u32_at(arm9, offset + 0x1C) == NITROCODE_BE
            && u32_at(arm9, offset + 0x20) == NITROCODE_LE
    };
    if footer.len() >= 8 && u32_at(footer, 0) == NITROCODE_BE {
        let offset = u32_at(footer, 4) as usize;
        if is_params(offset) {
            return Ok(offset);
        }
    }
    (0..arm9.len().saturating_sub(MODULE_PARAMS_SIZE))
        .step_by(4)
        .find(|&offset| is_params(offset))
        .ok_or_else(|| "ARM9 binary has no module params (nitrocode marker)".to_owned())
}

fn parse_module_params(arm9: &[u8], offset: usize) -> ModuleParams {
    ModuleParams {
        offset: offset as u32,
        autoload_list_start: u32_at(arm9, offset),
        autoload_list_end: u32_at(arm9, offset + 0x04),
        autoload_start: u32_at(arm9, offset + 0x08),
        static_bss_start: u32_at(arm9, offset + 0x0C),
        static_bss_end: u32_at(arm9, offset + 0x10),
        compressed_static_end: u32_at(arm9, offset + 0x14),
        sdk_version: u32_at(arm9, offset + 0x18),
    }
}

fn parse_autoloads(
    arm9: &[u8],
    load_address: u32,
    params: &ModuleParams,
    record_size: usize,
) -> Result<Vec<AutoloadBlock>> {
    let to_offset = |address: u32, what: &str| {
        address
            .checked_sub(load_address)
            .map(|value| value as usize)
            .filter(|&value| value <= arm9.len())
            .ok_or_else(|| format!("ARM9 {what} 0x{address:08X} lies outside the binary"))
    };
    let list_start = to_offset(params.autoload_list_start, "autoload list")?;
    let list_end = to_offset(params.autoload_list_end, "autoload list end")?;
    let mut data_offset = to_offset(params.autoload_start, "autoload data")?;
    if list_end < list_start || (list_end - list_start) % record_size != 0 {
        return Err("ARM9 autoload list has an invalid size".to_owned());
    }

    let mut blocks = Vec::new();
    for record in arm9[list_start..list_end].chunks_exact(record_size) {
        let (bss_size, twl_extra) = if record_size == 16 {
            (u32_at(record, 12), Some(u32_at(record, 8)))
        } else {
            (u32_at(record, 8), None)
        };
        let block = AutoloadBlock {
            ram_address: u32_at(record, 0),
            size: u32_at(record, 4),
            bss_size,
            twl_extra,
            image_offset: data_offset as u32,
        };
        data_offset = data_offset
            .checked_add(block.size as usize)
            .filter(|&end| end <= arm9.len())
            .ok_or_else(|| {
                format!(
                    "ARM9 autoload block for 0x{:08X} runs past the binary",
                    block.ram_address
                )
            })?;
        blocks.push(block);
    }
    Ok(blocks)
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params_block(compressed_end: u32) -> Vec<u8> {
        let mut bytes = vec![0; MODULE_PARAMS_SIZE];
        put_u32(&mut bytes, 0x00, 0x0200_0100);
        put_u32(&mut bytes, 0x04, 0x0200_0118);
        put_u32(&mut bytes, 0x08, 0x0200_00C0);
        put_u32(&mut bytes, 0x14, compressed_end);
        put_u32(&mut bytes, 0x1C, NITROCODE_BE);
        put_u32(&mut bytes, 0x20, NITROCODE_LE);
        bytes
    }

    #[test]
    fn parses_module_params_and_autoloads() {
        let mut arm9 = vec![0_u8; 0x118];
        arm9[0x40..0x40 + MODULE_PARAMS_SIZE].copy_from_slice(&params_block(0));
        // ITCM: 0x20 bytes, DTCM: 0x10 bytes + 0x40 BSS.
        put_u32(&mut arm9, 0x100, 0x0100_0000);
        put_u32(&mut arm9, 0x104, 0x20);
        put_u32(&mut arm9, 0x10C, 0x027E_0000);
        put_u32(&mut arm9, 0x110, 0x10);
        put_u32(&mut arm9, 0x114, 0x40);

        let offset = find_module_params(&arm9, &[]).unwrap();
        assert_eq!(offset, 0x40);
        let params = parse_module_params(&arm9, offset);
        let blocks = parse_autoloads(&arm9, 0x0200_0000, &params, 12).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].image_offset, 0xC0);
        assert_eq!(blocks[1].image_offset, 0xE0);
        assert_eq!(blocks[1].bss_size, 0x40);
    }

    #[test]
    fn parses_twl_autoload_records() {
        let mut arm9 = vec![0_u8; 0x120];
        arm9[0x40..0x40 + MODULE_PARAMS_SIZE].copy_from_slice(&params_block(0));
        put_u32(&mut arm9, 0x04 + 0x40, 0x0200_0120);
        put_u32(&mut arm9, 0x100, 0x027E_0000);
        put_u32(&mut arm9, 0x104, 0x10);
        put_u32(&mut arm9, 0x108, 0x027E_0000);
        put_u32(&mut arm9, 0x10C, 0x20);
        // A second record: 0x20 bytes of payload, ending exactly at 0xF0.
        put_u32(&mut arm9, 0x110, 0x0240_0000);
        put_u32(&mut arm9, 0x114, 0x20);
        let params = parse_module_params(&arm9, 0x40);
        let blocks = parse_autoloads(&arm9, 0x0200_0000, &params, 16).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].bss_size, 0x20);
        assert_eq!(blocks[0].twl_extra, Some(0x027E_0000));
        assert_eq!(blocks[1].image_offset, 0xD0);
    }

    #[test]
    fn rejects_autoload_past_binary() {
        let mut arm9 = vec![0_u8; 0x118];
        arm9[0x40..0x40 + MODULE_PARAMS_SIZE].copy_from_slice(&params_block(0));
        put_u32(&mut arm9, 0x100, 0x0100_0000);
        put_u32(&mut arm9, 0x104, 0x1000);
        let params = parse_module_params(&arm9, 0x40);
        assert!(parse_autoloads(&arm9, 0x0200_0000, &params, 12).is_err());
    }
}
