use crate::Result;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct NitroTexture {
    pub name: String,
    pub width: u16,
    pub height: u16,
    pub rgba8: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TextureEntry {
    offset: usize,
    parameters: u16,
    width: usize,
    height: usize,
    name: String,
}

#[derive(Debug, Clone, Copy)]
struct PaletteEntry {
    offset: usize,
}

/// Decode the common tiled texture formats used by BW2 map texture sets.
/// Unsupported or malformed individual images are skipped so one bad texture
/// cannot prevent the rest of a map from loading.
pub fn parse_texture_archive(bytes: &[u8]) -> Result<Vec<NitroTexture>> {
    if bytes.len() < 20 || &bytes[..4] != b"BTX0" {
        return Err("Nitro texture member has no BTX0 header".to_owned());
    }
    let tex0_offset = usize_at_u32(bytes, 16)?;
    require(bytes, tex0_offset, 0x3C, "TEX0 header")?;
    if &bytes[tex0_offset..tex0_offset + 4] != b"TEX0" {
        return Err("BTX0 does not point to a TEX0 section".to_owned());
    }
    let section_size = usize_at_u32(bytes, tex0_offset + 4)?;
    require(bytes, tex0_offset, section_size, "TEX0 section")?;
    let tex0_end = tex0_offset + section_size;
    let texture_dict_offset = tex0_offset
        .checked_add(usize::from(u16_at(bytes, tex0_offset + 14)?))
        .ok_or_else(|| "TEX0 texture dictionary offset overflowed".to_owned())?;
    let texture_data_offset = tex0_offset
        .checked_add(usize_at_u32(bytes, tex0_offset + 20)?)
        .ok_or_else(|| "TEX0 texture data offset overflowed".to_owned())?;
    let palette_dict_offset = tex0_offset
        .checked_add(usize_at_u32(bytes, tex0_offset + 52)?)
        .ok_or_else(|| "TEX0 palette dictionary offset overflowed".to_owned())?;
    let palette_data_offset = tex0_offset
        .checked_add(usize_at_u32(bytes, tex0_offset + 56)?)
        .ok_or_else(|| "TEX0 palette data offset overflowed".to_owned())?;

    let texture_entries = read_texture_dictionary(bytes, texture_dict_offset, tex0_end)?;
    let palette_entries = read_palette_dictionary(bytes, palette_dict_offset, tex0_end)?;
    let palettes = palette_entries
        .into_iter()
        .map(|(name, entry)| (name, entry))
        .collect::<HashMap<_, _>>();

    let mut decoded = Vec::with_capacity(texture_entries.len());
    for entry in texture_entries {
        let format = usize::from((entry.parameters >> 10) & 7);
        let Some(bits_per_pixel) = [0, 8, 2, 4, 8, 2, 8, 16].get(format).copied() else {
            continue;
        };
        if bits_per_pixel == 0 || format == 5 {
            continue;
        }
        let pixel_count = entry
            .width
            .checked_mul(entry.height)
            .ok_or_else(|| format!("texture '{}' dimensions overflow", entry.name))?;
        let data_size = pixel_count
            .checked_mul(bits_per_pixel)
            .and_then(|bits| bits.checked_add(7))
            .map(|bits| bits / 8)
            .ok_or_else(|| format!("texture '{}' data size overflow", entry.name))?;
        let data_start = texture_data_offset
            .checked_add(entry.offset)
            .ok_or_else(|| format!("texture '{}' offset overflow", entry.name))?;
        require(bytes, data_start, data_size, "TEX0 texture pixels")?;
        if data_start + data_size > tex0_end {
            return Err(format!("texture '{}' extends past TEX0", entry.name));
        }

        let palette_name = format!("{}_pl", entry.name);
        let palette_start = if let Some(palette) = palettes.get(&palette_name) {
            palette_data_offset
                .checked_add(palette.offset)
                .ok_or_else(|| format!("texture '{}' palette offset overflow", entry.name))?
        } else {
            palette_data_offset
        };
        let rgba8 = match decode_pixels(
            bytes,
            data_start,
            data_size,
            entry.width,
            entry.height,
            format,
            entry.parameters & (1 << 13) != 0,
            palette_start,
            tex0_end,
        ) {
            Ok(rgba8) => rgba8,
            Err(error) => {
                tracing::warn!(
                    texture = %entry.name,
                    %error,
                    "skipping undecodable Nitro texture"
                );
                continue;
            }
        };
        decoded.push(NitroTexture {
            name: entry.name,
            width: u16::try_from(entry.width)
                .map_err(|_| "Nitro texture width exceeds 16 bits".to_owned())?,
            height: u16::try_from(entry.height)
                .map_err(|_| "Nitro texture height exceeds 16 bits".to_owned())?,
            rgba8,
        });
    }
    Ok(decoded)
}

fn read_texture_dictionary(bytes: &[u8], start: usize, limit: usize) -> Result<Vec<TextureEntry>> {
    require(bytes, start, 16, "TEX0 texture dictionary")?;
    let count = usize::from(bytes[start + 1]);
    let entries_start = start
        .checked_add(16 + count * 4)
        .ok_or_else(|| "TEX0 texture dictionary size overflowed".to_owned())?;
    let names_start = entries_start
        .checked_add(count * 8)
        .ok_or_else(|| "TEX0 texture names offset overflowed".to_owned())?;
    let end = names_start
        .checked_add(count * 16)
        .ok_or_else(|| "TEX0 texture dictionary end overflowed".to_owned())?;
    if end > limit {
        return Err("TEX0 texture dictionary extends past its section".to_owned());
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let record = entries_start + index * 8;
        let name_start = names_start + index * 16;
        let raw_name = &bytes[name_start..name_start + 16];
        let name_length = raw_name.iter().position(|byte| *byte == 0).unwrap_or(16);
        let name = String::from_utf8_lossy(&raw_name[..name_length]).into_owned();
        let parameters = u16_at(bytes, record + 2)?;
        let width = 8usize << ((parameters >> 4) & 7);
        let height = 8usize << ((parameters >> 7) & 7);
        entries.push(TextureEntry {
            offset: usize::from(u16_at(bytes, record)?) * 8,
            parameters,
            width,
            height,
            name,
        });
    }
    Ok(entries)
}

fn read_palette_dictionary(
    bytes: &[u8],
    start: usize,
    limit: usize,
) -> Result<Vec<(String, PaletteEntry)>> {
    require(bytes, start, 16, "TEX0 palette dictionary")?;
    let count = usize::from(bytes[start + 1]);
    let entries_start = start
        .checked_add(16 + count * 4)
        .ok_or_else(|| "TEX0 palette dictionary size overflowed".to_owned())?;
    let names_start = entries_start
        .checked_add(count * 4)
        .ok_or_else(|| "TEX0 palette names offset overflowed".to_owned())?;
    let end = names_start
        .checked_add(count * 16)
        .ok_or_else(|| "TEX0 palette dictionary end overflowed".to_owned())?;
    if end > limit {
        return Err("TEX0 palette dictionary extends past its section".to_owned());
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let record = entries_start + index * 4;
        let name_start = names_start + index * 16;
        let raw_name = &bytes[name_start..name_start + 16];
        let name_length = raw_name.iter().position(|byte| *byte == 0).unwrap_or(16);
        entries.push((
            String::from_utf8_lossy(&raw_name[..name_length]).into_owned(),
            PaletteEntry {
                offset: usize::from(u16_at(bytes, record)?) * 8,
            },
        ));
    }
    Ok(entries)
}

#[allow(clippy::too_many_arguments)]
fn decode_pixels(
    bytes: &[u8],
    data_start: usize,
    data_size: usize,
    width: usize,
    height: usize,
    format: usize,
    transparent_zero: bool,
    palette_start: usize,
    limit: usize,
) -> Result<Vec<u8>> {
    let pixel_count = width
        .checked_mul(height)
        .ok_or_else(|| "texture pixel count overflowed".to_owned())?;
    let output_size = pixel_count
        .checked_mul(4)
        .ok_or_else(|| "decoded texture size overflowed".to_owned())?;
    let mut output = vec![0; output_size];
    for y in 0..height {
        for x in 0..width {
            let tiled_index = tiled_pixel_index(x, y, width);
            let (color, alpha) = match format {
                1 => {
                    let packed = *bytes
                        .get(data_start + tiled_index)
                        .ok_or_else(|| "A3I5 texture pixel extends past its data".to_owned())?;
                    let index = usize::from(packed & 0x1F);
                    let alpha = u32::from(packed >> 5) * 255 / 7;
                    (
                        palette_color(bytes, palette_start, index, limit)?,
                        alpha as u8,
                    )
                }
                2 => {
                    let packed = packed_bits(bytes, data_start, data_size, tiled_index, 2)?;
                    let color = palette_color(bytes, palette_start, packed as usize, limit)?;
                    let alpha = if transparent_zero && packed == 0 {
                        0
                    } else {
                        255
                    };
                    (color, alpha)
                }
                3 => {
                    let packed = packed_bits(bytes, data_start, data_size, tiled_index, 4)?;
                    let color = palette_color(bytes, palette_start, packed as usize, limit)?;
                    let alpha = if transparent_zero && packed == 0 {
                        0
                    } else {
                        255
                    };
                    (color, alpha)
                }
                4 => {
                    let packed = *bytes
                        .get(data_start + tiled_index)
                        .ok_or_else(|| "indexed texture pixel extends past its data".to_owned())?;
                    let color = palette_color(bytes, palette_start, usize::from(packed), limit)?;
                    let alpha = if transparent_zero && packed == 0 {
                        0
                    } else {
                        255
                    };
                    (color, alpha)
                }
                6 => {
                    let packed = *bytes
                        .get(data_start + tiled_index)
                        .ok_or_else(|| "A5I3 texture pixel extends past its data".to_owned())?;
                    let index = usize::from(packed & 0x07);
                    let alpha = u32::from(packed >> 3) * 255 / 31;
                    (
                        palette_color(bytes, palette_start, index, limit)?,
                        alpha as u8,
                    )
                }
                7 => {
                    let offset = data_start + tiled_index * 2;
                    require(bytes, offset, 2, "direct-color texture pixel")?;
                    let value = u16_at(bytes, offset)?;
                    (rgb555(value), if value & 0x8000 != 0 { 255 } else { 0 })
                }
                _ => return Err(format!("unsupported Nitro texture format {format}")),
            };
            let output_index = (y * width + x) * 4;
            output[output_index..output_index + 4]
                .copy_from_slice(&[color[0], color[1], color[2], alpha]);
        }
    }
    Ok(output)
}

fn packed_bits(
    bytes: &[u8],
    data_start: usize,
    data_size: usize,
    index: usize,
    bits: usize,
) -> Result<u8> {
    let per_byte = 8 / bits;
    let byte_offset = index / per_byte;
    if byte_offset >= data_size {
        return Err("packed texture pixel extends past its data".to_owned());
    }
    let packed = bytes[data_start + byte_offset];
    let shift = (index % per_byte) * bits;
    Ok((packed >> shift) & ((1 << bits) - 1))
}

fn tiled_pixel_index(x: usize, y: usize, width: usize) -> usize {
    let tiles_per_row = width / 8;
    let tile_index = (y / 8) * tiles_per_row + (x / 8);
    tile_index * 64 + (y % 8) * 8 + (x % 8)
}

fn palette_color(
    bytes: &[u8],
    palette_start: usize,
    index: usize,
    limit: usize,
) -> Result<[u8; 3]> {
    let offset = palette_start
        .checked_add(index * 2)
        .ok_or_else(|| "palette color offset overflowed".to_owned())?;
    require(bytes, offset, 2, "TEX0 palette color")?;
    if offset + 2 > limit {
        return Err("TEX0 palette color extends past its section".to_owned());
    }
    Ok(rgb555(u16_at(bytes, offset)?))
}

fn rgb555(value: u16) -> [u8; 3] {
    let expand = |shift| (((value >> shift) & 31u16) * 255u16 / 31u16) as u8;
    [expand(0), expand(5), expand(10)]
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

fn usize_at_u32(bytes: &[u8], offset: usize) -> Result<usize> {
    usize::try_from(u32_at(bytes, offset)?)
        .map_err(|_| "32-bit offset does not fit this platform".to_owned())
}
