use crate::Result;
use crate::formats::narc::NarcArchive;
use crate::game::bw2::map::ZoneMap;
use std::fs;
use std::path::Path;

const MODEL_ARCHIVE: &str = "a/0/0/8";

/// Gen V movement permission values documented as passable for ordinary foot travel.
/// Values not in this set remain blocked until their movement rules are implemented.
const WALKABLE_MOVEMENT_CODES: [u16; 7] = [0, 2, 3, 4, 6, 10, 14];

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
        WALKABLE_MOVEMENT_CODES.contains(&(self.tile_type as u16))
    }

    pub fn movement_code(self) -> u16 {
        self.tile_type as u16
    }

    pub fn height_world(self) -> f32 {
        if self.shape == 2 {
            0.0
        } else {
            f32::from(self.height_index) / 256.0
        }
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

/// Combine the first height-terrain layer from each model cell into a map grid.
pub fn load_zone_terrain(cache_root: &Path, zone: &ZoneMap) -> Result<TerrainLayer> {
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let archive_bytes = fs::read(romfs.join(MODEL_ARCHIVE))
        .map_err(|error| format!("could not read BW2 map-model archive: {error}"))?;
    let archive = NarcArchive::parse(&archive_bytes)?;
    let chunk_width = 32usize;
    let chunk_height = 32usize;
    let width = usize::from(zone.width)
        .checked_mul(chunk_width)
        .ok_or_else(|| "zone terrain width overflowed".to_owned())?;
    let height = usize::from(zone.height)
        .checked_mul(chunk_height)
        .ok_or_else(|| "zone terrain height overflowed".to_owned())?;
    let tile_count = width
        .checked_mul(height)
        .ok_or_else(|| "zone terrain tile count overflowed".to_owned())?;
    let blocked = TerrainTile {
        shape: 0,
        slope_index: 0,
        height_index: 0,
        tile_type: u32::MAX,
        diagonal: false,
    };
    let mut output = TerrainLayer {
        width: u16::try_from(width)
            .map_err(|_| "zone terrain width exceeds the supported grid size".to_owned())?,
        height: u16::try_from(height)
            .map_err(|_| "zone terrain height exceeds the supported grid size".to_owned())?,
        tiles: vec![blocked; tile_count],
    };

    for cell_z in 0..usize::from(zone.height) {
        for cell_x in 0..usize::from(zone.width) {
            let Some(model_id) = zone.model_at(cell_x, cell_z) else {
                continue;
            };
            let member_index = usize::try_from(model_id)
                .map_err(|_| format!("map model ID {model_id} does not fit this platform"))?;
            let chunk = archive.member(member_index)?;
            let Some(layer) = parse_terrain_layers(chunk)?.into_iter().next() else {
                continue;
            };
            if usize::from(layer.width) != chunk_width || usize::from(layer.height) != chunk_height
            {
                return Err(format!(
                    "map model {model_id} has terrain size {}x{}, expected {chunk_width}x{chunk_height}",
                    layer.width, layer.height
                ));
            }
            for local_z in 0..chunk_height {
                let source_start = local_z * chunk_width;
                let target_start = (cell_z * chunk_height + local_z) * width + cell_x * chunk_width;
                output.tiles[target_start..target_start + chunk_width]
                    .copy_from_slice(&layer.tiles[source_start..source_start + chunk_width]);
            }
        }
    }
    Ok(output)
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
