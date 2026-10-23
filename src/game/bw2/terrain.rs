use crate::Result;
use crate::formats::narc::NarcArchive;
use crate::game::bw2::map::ZoneMap;
use std::fs;
use std::path::Path;

const MODEL_ARCHIVE: &str = "a/0/0/8";

/// The terrain code used by Aspertia City's broad, traversable ground.
/// Other codes remain blocked until their Gen V movement permissions are mapped.
pub const ASPERTIA_WALKABLE_TYPE: u32 = 0x0081_0001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainTile {
    pub shape: u8,
    pub slope_index: u16,
    pub height_index: u16,
    pub tile_type: u32,
    pub diagonal: bool,
}

impl TerrainTile {
    pub fn is_walkable(self) -> bool {
        self.tile_type == ASPERTIA_WALKABLE_TYPE
    }
}

#[derive(Debug, Clone)]
pub struct TerrainLayer {
    pub width: u16,
    pub height: u16,
    pub tiles: Vec<TerrainTile>,
}

impl TerrainLayer {
    pub fn tile(&self, x: usize, z: usize) -> Option<TerrainTile> {
        if x >= usize::from(self.width) || z >= usize::from(self.height) {
            return None;
        }
        self.tiles.get(z * usize::from(self.width) + x).copied()
    }
}

/// Read the first height-terrain layer for a single-cell BW2 zone.
pub fn load_zone_terrain(cache_root: &Path, zone: &ZoneMap) -> Result<TerrainLayer> {
    if zone.model_ids.len() != 1 || zone.width != 1 || zone.height != 1 {
        return Err(format!(
            "field movement currently supports one-cell maps; zone {} is {}x{}",
            zone.zone_id, zone.width, zone.height
        ));
    }
    let model_id =
        zone.model_ids[0].ok_or_else(|| format!("zone {} has no map model", zone.zone_id))?;
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let archive_bytes = fs::read(romfs.join(MODEL_ARCHIVE))
        .map_err(|error| format!("could not read BW2 map-model archive: {error}"))?;
    let archive = NarcArchive::parse(&archive_bytes)?;
    let member_index = usize::try_from(model_id)
        .map_err(|_| format!("map model ID {model_id} does not fit this platform"))?;
    let chunk = archive.member(member_index)?;
    let layers = parse_terrain_layers(chunk)?;
    layers
        .into_iter()
        .next()
        .ok_or_else(|| format!("map model {model_id} contains no terrain layer"))
}

fn parse_terrain_layers(chunk: &[u8]) -> Result<Vec<TerrainLayer>> {
    require(chunk, 0, 8, "map chunk header")?;
    let magic = &chunk[..2];
    let offsets = match magic {
        b"WB" | b"RD" => vec![u32_at(chunk, 8)? as usize, u32_at(chunk, 12)? as usize],
        b"GC" => vec![
            u32_at(chunk, 8)? as usize,
            u32_at(chunk, 12)? as usize,
            u32_at(chunk, 16)? as usize,
        ],
        b"NG" => return Ok(Vec::new()),
        _ => {
            return Err(format!(
                "map model has unsupported chunk type {:?}",
                String::from_utf8_lossy(magic)
            ));
        }
    };
    let mut ranges = Vec::new();
    for pair in offsets.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if start >= end || end > chunk.len() || start < 20 {
            return Err(format!(
                "map terrain section has invalid bounds 0x{start:X}..0x{end:X}"
            ));
        }
        ranges.push(start..end);
    }
    if magic == b"RD" {
        return Err("RD map terrain uses direct planes and is not supported yet".to_owned());
    }

    ranges
        .iter()
        .enumerate()
        .map(|(index, range)| parse_height_layer(chunk, range.start, range.end, index))
        .collect()
}

fn parse_height_layer(
    chunk: &[u8],
    start: usize,
    end: usize,
    layer: usize,
) -> Result<TerrainLayer> {
    require(chunk, start, 4, "terrain dimensions")?;
    let width = u16_at(chunk, start)?;
    let height = u16_at(chunk, start + 2)?;
    if width == 0 || height == 0 {
        return Err(format!("terrain layer {layer} has zero dimensions"));
    }
    let count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or_else(|| format!("terrain layer {layer} tile count overflowed"))?;
    let records_start = start + 4;
    let records_size = count
        .checked_mul(8)
        .ok_or_else(|| format!("terrain layer {layer} record size overflowed"))?;
    let splits_start = records_start
        .checked_add(records_size)
        .ok_or_else(|| format!("terrain layer {layer} split offset overflowed"))?;
    if splits_start > end || !(end - splits_start).is_multiple_of(8) {
        return Err(format!(
            "terrain layer {layer} size does not match its {width}x{height} dimensions"
        ));
    }
    let split_count = (end - splits_start) / 8;
    let mut tiles = Vec::with_capacity(count);
    for index in 0..count {
        let offset = records_start + index * 8;
        let shape_slope = u16_at(chunk, offset)?;
        let height_index = u16_at(chunk, offset + 2)?;
        let flags = u32_at(chunk, offset + 4)?;
        let shape = (shape_slope & 3) as u8;
        let slope_index = shape_slope >> 2;
        if shape == 2 && usize::from(height_index) >= split_count {
            return Err(format!(
                "terrain layer {layer} tile {index} references missing split {}",
                height_index
            ));
        }
        if shape != 0 && shape != 2 {
            return Err(format!(
                "terrain layer {layer} tile {index} has unsupported shape {shape}"
            ));
        }
        tiles.push(TerrainTile {
            shape,
            slope_index,
            height_index,
            tile_type: flags & 0x7FFF_FFFF,
            diagonal: flags >> 31 != 0,
        });
    }
    Ok(TerrainLayer {
        width,
        height,
        tiles,
    })
}

fn require(bytes: &[u8], start: usize, length: usize, what: &str) -> Result<()> {
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{what} range overflowed"))?;
    if end > bytes.len() {
        return Err(format!("{what} extends past its containing file"));
    }
    Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    require(bytes, offset, 2, "16-bit field")?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    require(bytes, offset, 4, "32-bit field")?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}
