use super::{NdsHeader, NdsRom};
use crate::Result;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone)]
pub struct RomFile {
    pub id: u16,
    /// A safe, portable path decoded from the NDS filename table.
    pub path: String,
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct RomFileSystem {
    pub files: Vec<RomFile>,
    /// Raw FAT ranges indexed by file ID, including unnamed overlay files.
    pub allocations: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, Copy)]
struct DirectoryRecord {
    entries_offset: u32,
    first_file_id: u16,
}

impl RomFileSystem {
    pub(super) fn read(file: &mut File, header: &NdsHeader, rom_size: u64) -> Result<Self> {
        let fnt = read_range(file, header.fnt.offset, header.fnt.size)?;
        let fat = read_range(file, header.fat.offset, header.fat.size)?;
        let fat_entries = parse_fat(&fat, rom_size)?;
        let directories = parse_directory_records(&fnt)?;

        let mut filesystem = Self {
            files: Vec::new(),
            allocations: Vec::new(),
        };
        let mut visited = HashSet::new();
        let mut seen_file_ids = HashSet::new();
        walk_directory(
            0,
            "",
            &fnt,
            &directories,
            &fat_entries,
            &mut visited,
            &mut seen_file_ids,
            &mut filesystem.files,
        )?;

        let mut seen_paths = HashSet::new();
        for entry in &filesystem.files {
            if !seen_paths.insert(entry.path.to_ascii_lowercase()) {
                return Err(format!(
                    "duplicate ROM path '{}': FNT is ambiguous",
                    entry.path
                ));
            }
        }
        filesystem
            .files
            .sort_by(|left, right| left.path.cmp(&right.path));
        filesystem.allocations = fat_entries;
        Ok(filesystem)
    }
}

impl NdsRom {
    pub fn files_under(&self, prefix: &str) -> Vec<&RomFile> {
        let normalized = prefix.trim_matches('/').replace('\\', "/");
        if normalized.is_empty() {
            return self.filesystem.files.iter().collect();
        }
        let prefix = normalized.to_ascii_lowercase();
        self.filesystem
            .files
            .iter()
            .filter(|file| {
                let path = file.path.to_ascii_lowercase();
                path == prefix || path.starts_with(&format!("{prefix}/"))
            })
            .collect()
    }
}

fn read_range(file: &mut File, offset: u32, length: u32) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(u64::from(offset)))
        .map_err(|error| format!("could not seek to NDS table at 0x{offset:X}: {error}"))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("could not read NDS table at 0x{offset:X}: {error}"))?;
    Ok(bytes)
}

fn parse_fat(bytes: &[u8], rom_size: u64) -> Result<Vec<(u32, u32)>> {
    if bytes.len() % 8 != 0 {
        return Err("NDS FAT length is not a multiple of 8 bytes".to_owned());
    }
    let mut entries = Vec::with_capacity(bytes.len() / 8);
    for (id, record) in bytes.chunks_exact(8).enumerate() {
        let start = u32_at(record, 0);
        let end = u32_at(record, 4);
        if end < start {
            return Err(format!("FAT file {id} has end offset before start offset"));
        }
        if u64::from(end) > rom_size {
            return Err(format!(
                "FAT file {id} ends at 0x{end:X}, beyond ROM size 0x{rom_size:X}"
            ));
        }
        entries.push((start, end));
    }
    Ok(entries)
}

fn parse_directory_records(bytes: &[u8]) -> Result<Vec<DirectoryRecord>> {
    if bytes.len() < 8 {
        return Err("NDS FNT does not contain its root directory record".to_owned());
    }
    let directory_count = usize::from(u16_at(bytes, 6));
    if directory_count == 0 || directory_count > 0x1000 {
        return Err(format!(
            "NDS FNT declares invalid directory count {directory_count}"
        ));
    }
    let records_size = directory_count
        .checked_mul(8)
        .ok_or_else(|| "NDS FNT directory record size overflowed".to_owned())?;
    if records_size > bytes.len() {
        return Err(format!(
            "NDS FNT directory records need {records_size} bytes, table has {}",
            bytes.len()
        ));
    }
    let mut records = Vec::with_capacity(directory_count);
    for record in bytes[..records_size].chunks_exact(8) {
        let entries_offset = u32_at(record, 0);
        if (entries_offset as usize) < records_size || (entries_offset as usize) >= bytes.len() {
            return Err(format!(
                "NDS FNT subtable offset 0x{entries_offset:X} is outside the data region"
            ));
        }
        records.push(DirectoryRecord {
            entries_offset,
            first_file_id: u16_at(record, 4),
        });
    }
    Ok(records)
}

fn walk_directory(
    directory_id: usize,
    prefix: &str,
    fnt: &[u8],
    directories: &[DirectoryRecord],
    fat: &[(u32, u32)],
    visited: &mut HashSet<usize>,
    seen_file_ids: &mut HashSet<usize>,
    files: &mut Vec<RomFile>,
) -> Result<()> {
    if prefix.matches('/').count() > 512 {
        return Err("FNT directory nesting exceeds the supported safety limit".to_owned());
    }
    let directory = directories
        .get(directory_id)
        .ok_or_else(|| format!("FNT references missing directory {directory_id}"))?;
    if !visited.insert(directory_id) {
        return Err(format!(
            "FNT directory graph contains a cycle at {directory_id}"
        ));
    }

    let mut cursor = directory.entries_offset as usize;
    let mut next_file_id = usize::from(directory.first_file_id);
    let mut child_directories = Vec::new();
    loop {
        let code = *fnt
            .get(cursor)
            .ok_or_else(|| format!("FNT subtable for directory {directory_id} is unterminated"))?;
        cursor += 1;
        if code == 0 {
            break;
        }
        let is_directory = code & 0x80 != 0;
        let name_length = usize::from(code & 0x7F);
        if name_length == 0 {
            return Err(format!(
                "FNT directory {directory_id} contains an empty name"
            ));
        }
        let name_end = cursor
            .checked_add(name_length)
            .ok_or_else(|| "FNT filename range overflowed".to_owned())?;
        let name_bytes = fnt.get(cursor..name_end).ok_or_else(|| {
            format!("FNT filename extends past end of directory {directory_id} subtable")
        })?;
        let name = safe_component(name_bytes)?;
        cursor = name_end;

        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if is_directory {
            let child_id = usize::from(u16_at(fnt, cursor));
            cursor += 2;
            if child_id & 0xF000 != 0xF000 {
                return Err(format!(
                    "FNT subdirectory reference 0x{child_id:04X} is invalid"
                ));
            }
            child_directories.push((child_id & 0x0FFF, path));
        } else {
            if next_file_id > usize::from(u16::MAX) {
                return Err("FNT file ID exceeds the NDS file ID range".to_owned());
            }
            let (offset, end) = fat
                .get(next_file_id)
                .copied()
                .ok_or_else(|| format!("FNT file ID {next_file_id} has no matching FAT record"))?;
            if !seen_file_ids.insert(next_file_id) {
                return Err(format!("FNT references duplicate file ID {next_file_id}"));
            }
            files.push(RomFile {
                id: next_file_id as u16,
                path,
                offset,
                size: end - offset,
            });
            next_file_id += 1;
            if next_file_id > usize::from(u16::MAX) + 1 {
                return Err("FNT file ID exceeds the NDS file ID range".to_owned());
            }
        }
    }

    for (child_id, path) in child_directories {
        walk_directory(
            child_id,
            &path,
            fnt,
            directories,
            fat,
            visited,
            seen_file_ids,
            files,
        )?;
    }
    Ok(())
}

fn safe_component(bytes: &[u8]) -> Result<String> {
    if bytes == b"." || bytes == b".." {
        return Err("FNT contains a path traversal component".to_owned());
    }
    let mut output = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b' ' | b'-' | b'_' | b'.') {
            output.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut output, "%{byte:02X}")
                .map_err(|_| "could not encode NDS filename".to_owned())?;
        }
    }
    Ok(output)
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
