use crate::Result;
use std::ops::Range;

/// A validated view of a Nitro Archive (NARC) container.
///
/// The archive borrows its input bytes and keeps member ranges, so callers can
/// inspect large ROM archives without copying every entry into memory.
#[derive(Debug, Clone)]
pub struct NarcArchive<'a> {
    bytes: &'a [u8],
    members: Vec<Range<usize>>,
}

impl<'a> NarcArchive<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        if bytes.len() < 16 || &bytes[..4] != b"NARC" {
            return Err("NARC file has an invalid or truncated header".to_owned());
        }
        if u16_at(bytes, 4) != 0xFFFE {
            return Err("NARC byte order mark is not little-endian".to_owned());
        }
        let declared_size = usize::try_from(u32_at(bytes, 8))
            .map_err(|_| "NARC declared size does not fit this platform".to_owned())?;
        let header_size = usize::from(u16_at(bytes, 12));
        let chunk_count = usize::from(u16_at(bytes, 14));
        if header_size != 16 || declared_size > bytes.len() || declared_size < header_size {
            return Err(format!(
                "NARC header has invalid sizes (header {header_size}, file {declared_size}, available {})",
                bytes.len()
            ));
        }

        let archive = &bytes[..declared_size];
        let mut cursor = header_size;
        let mut fat_records: Option<Vec<(u32, u32)>> = None;
        let mut data_range: Option<Range<usize>> = None;
        for _ in 0..chunk_count {
            let chunk_header_end = cursor
                .checked_add(8)
                .ok_or_else(|| "NARC chunk header offset overflowed".to_owned())?;
            if chunk_header_end > archive.len() {
                return Err("NARC chunk header extends past the archive".to_owned());
            }
            let magic = &archive[cursor..cursor + 4];
            let chunk_size = usize::try_from(u32_at(archive, cursor + 4))
                .map_err(|_| "NARC chunk size does not fit this platform".to_owned())?;
            let chunk_end = cursor
                .checked_add(chunk_size)
                .ok_or_else(|| "NARC chunk range overflowed".to_owned())?;
            if chunk_size < 8 || chunk_end > archive.len() {
                return Err(format!(
                    "NARC chunk at 0x{cursor:X} has invalid size 0x{chunk_size:X}"
                ));
            }

            match magic {
                b"BTAF" => {
                    if fat_records.is_some() {
                        return Err("NARC contains more than one BTAF file table".to_owned());
                    }
                    if chunk_size < 12 {
                        return Err("NARC BTAF chunk is too short".to_owned());
                    }
                    let count = usize::from(u16_at(archive, cursor + 8));
                    let records_size =
                        count
                            .checked_mul(8)
                            .and_then(|size| size.checked_add(12))
                            .ok_or_else(|| "NARC BTAF record size overflowed".to_owned())?;
                    if records_size > chunk_size {
                        return Err(format!(
                            "NARC BTAF declares {count} members, but its chunk is too short"
                        ));
                    }
                    let mut records = Vec::with_capacity(count);
                    let mut entry_cursor = cursor + 12;
                    for _ in 0..count {
                        records.push((
                            u32_at(archive, entry_cursor),
                            u32_at(archive, entry_cursor + 4),
                        ));
                        entry_cursor += 8;
                    }
                    fat_records = Some(records);
                }
                b"GMIF" => {
                    if data_range.is_some() {
                        return Err("NARC contains more than one GMIF data chunk".to_owned());
                    }
                    data_range = Some(cursor + 8..chunk_end);
                }
                // BTNF stores names. NitroFS paths are already available from
                // the ROM FNT, so the archive reader only needs its FAT and data.
                b"BTNF" => {}
                _ => {
                    return Err(format!(
                        "NARC contains unsupported chunk {:?}",
                        String::from_utf8_lossy(magic)
                    ));
                }
            }
            cursor = chunk_end;
        }
        if cursor != archive.len() {
            return Err("NARC chunk count does not cover the declared archive size".to_owned());
        }

        let records = fat_records.ok_or_else(|| "NARC has no BTAF file table".to_owned())?;
        let data = data_range.ok_or_else(|| "NARC has no GMIF data chunk".to_owned())?;
        let data_size = data.end - data.start;
        let mut members = Vec::with_capacity(records.len());
        for (index, (start, end)) in records.into_iter().enumerate() {
            let start = usize::try_from(start)
                .map_err(|_| format!("NARC member {index} start does not fit this platform"))?;
            let end = usize::try_from(end)
                .map_err(|_| format!("NARC member {index} end does not fit this platform"))?;
            if end < start || end > data_size {
                return Err(format!(
                    "NARC member {index} range 0x{start:X}..0x{end:X} is outside GMIF data"
                ));
            }
            members.push(data.start + start..data.start + end);
        }

        Ok(Self {
            bytes: archive,
            members,
        })
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn member(&self, index: usize) -> Result<&'a [u8]> {
        let range = self
            .members
            .get(index)
            .ok_or_else(|| format!("NARC member {index} is outside the {} members", self.len()))?;
        Ok(&self.bytes[range.clone()])
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
