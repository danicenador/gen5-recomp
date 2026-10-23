use crate::Result;
use crate::formats::narc::NarcArchive;
use std::fs;
use std::path::{Path, PathBuf};

pub const ASPERTIA_CITY_ZONE: u32 = 427;
const ZONE_RECORD_SIZE: usize = 0x30;
const ZONE_ARCHIVE: &str = "a/0/1/2";
const MATRIX_ARCHIVE: &str = "a/0/0/9";

#[derive(Debug, Clone)]
pub struct ZoneMap {
    pub zone_id: u32,
    pub area_id: u16,
    pub matrix_id: u16,
    pub scripts_bank: u16,
    pub init_scripts_bank: u16,
    pub text_bank: u16,
    pub event_bank: u16,
    pub width: u16,
    pub height: u16,
    pub model_ids: Vec<Option<u32>>,
    pub cell_zones: Option<Vec<Option<u32>>>,
}

pub fn load_zone(cache_root: &Path, zone_id: u32) -> Result<ZoneMap> {
    let romfs = romfs_root(cache_root);
    let zone_archive = fs::read(romfs.join(ZONE_ARCHIVE))
        .map_err(|error| format!("could not read BW2 zone archive: {error}"))?;
    let zone_narc = NarcArchive::parse(&zone_archive)?;
    let zone_table = zone_narc.member(0)?;
    if zone_table.len() % ZONE_RECORD_SIZE != 0 {
        return Err(format!(
            "BW2 zone table length {} is not a multiple of 0x{ZONE_RECORD_SIZE:X}",
            zone_table.len()
        ));
    }
    let zone_count = zone_table.len() / ZONE_RECORD_SIZE;
    let zone_index =
        usize::try_from(zone_id).map_err(|_| "zone index does not fit this platform".to_owned())?;
    let zone_start = zone_index
        .checked_mul(ZONE_RECORD_SIZE)
        .ok_or_else(|| "zone record offset overflowed".to_owned())?;
    let record = zone_table
        .get(zone_start..zone_start + ZONE_RECORD_SIZE)
        .ok_or_else(|| format!("zone {zone_id} is outside the {zone_count} BW2 zone records"))?;
    let area_id = u16_at(record, 2);
    let matrix_id = u16_at(record, 4);
    let scripts_bank = u16_at(record, 6);
    let init_scripts_bank = u16_at(record, 8);
    let text_bank = u16_at(record, 10);
    let event_bank = u16_at(record, 22);

    let matrix_archive = fs::read(romfs.join(MATRIX_ARCHIVE))
        .map_err(|error| format!("could not read BW2 map-matrix archive: {error}"))?;
    let matrix_narc = NarcArchive::parse(&matrix_archive)?;
    let matrix_bytes = matrix_narc.member(usize::from(matrix_id))?;
    parse_matrix(
        zone_id,
        area_id,
        matrix_id,
        scripts_bank,
        init_scripts_bank,
        text_bank,
        event_bank,
        matrix_bytes,
    )
}

impl ZoneMap {
    pub fn display_name(&self) -> &'static str {
        match self.zone_id {
            ASPERTIA_CITY_ZONE => "Aspertia City",
            _ => "Unknown BW2 zone",
        }
    }

    pub fn model_at(&self, x: usize, z: usize) -> Option<u32> {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        if x >= width || z >= height {
            return None;
        }
        self.model_ids.get(z * width + x).copied().flatten()
    }
}

fn parse_matrix(
    zone_id: u32,
    area_id: u16,
    matrix_id: u16,
    scripts_bank: u16,
    init_scripts_bank: u16,
    text_bank: u16,
    event_bank: u16,
    bytes: &[u8],
) -> Result<ZoneMap> {
    if bytes.len() < 8 {
        return Err(format!(
            "matrix {matrix_id} is shorter than its 8-byte header"
        ));
    }
    let has_zones = u16_at(bytes, 0);
    let reserved = u16_at(bytes, 2);
    let width = u16_at(bytes, 4);
    let height = u16_at(bytes, 6);
    if has_zones > 1 || reserved != 0 || width == 0 || height == 0 {
        return Err(format!(
            "matrix {matrix_id} has an invalid header (zones={has_zones}, reserved={reserved}, size={width}x{height})"
        ));
    }
    let cell_count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| format!("matrix {matrix_id} cell count overflowed"))?;
    let grids = if has_zones == 1 { 2 } else { 1 };
    let expected_size = cell_count
        .checked_mul(4)
        .and_then(|size| size.checked_mul(grids))
        .and_then(|size| size.checked_add(8))
        .ok_or_else(|| format!("matrix {matrix_id} byte size overflowed"))?;
    if bytes.len() != expected_size {
        return Err(format!(
            "matrix {matrix_id} has {} bytes, expected {expected_size} for a {width}x{height} grid",
            bytes.len()
        ));
    }
    let model_ids = parse_grid(bytes, 8, cell_count);
    let cell_zones = (has_zones == 1).then(|| parse_grid(bytes, 8 + cell_count * 4, cell_count));
    Ok(ZoneMap {
        zone_id,
        area_id,
        matrix_id,
        scripts_bank,
        init_scripts_bank,
        text_bank,
        event_bank,
        width,
        height,
        model_ids,
        cell_zones,
    })
}

fn parse_grid(bytes: &[u8], offset: usize, count: usize) -> Vec<Option<u32>> {
    (0..count)
        .map(|index| {
            let value = u32_at(bytes, offset + index * 4);
            (value != u32::MAX).then_some(value)
        })
        .collect()
}

fn romfs_root(cache_root: &Path) -> PathBuf {
    let extracted = cache_root.join("romfs");
    if extracted.is_dir() {
        extracted
    } else {
        cache_root.to_path_buf()
    }
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
