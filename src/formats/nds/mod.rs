mod filesystem;
mod header;

pub use filesystem::{RomFile, RomFileSystem};
pub use header::{NdsHeader, RomSection};

use crate::Result;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// An opened Nintendo DS ROM with its validated header and NitroFS index.
pub struct NdsRom {
    file: File,
    pub header: NdsHeader,
    pub filesystem: RomFileSystem,
    pub byte_len: u64,
}

impl NdsRom {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)
            .map_err(|error| format!("could not open ROM '{}': {error}", path.display()))?;
        let byte_len = file
            .metadata()
            .map_err(|error| format!("could not read ROM metadata: {error}"))?
            .len();

        let mut raw_header = [0_u8; 0x200];
        file.read_exact(&mut raw_header)
            .map_err(|error| format!("file is too short to contain an NDS header: {error}"))?;
        let header = NdsHeader::parse(&raw_header, byte_len)?;
        let filesystem = RomFileSystem::read(&mut file, &header, byte_len)?;

        Ok(Self {
            file,
            header,
            filesystem,
            byte_len,
        })
    }

    /// Read a validated ROM range. The caller cannot request bytes outside the file.
    pub fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(length as u64)
            .ok_or_else(|| "ROM read range overflowed".to_owned())?;
        if end > self.byte_len {
            return Err(format!(
                "ROM read at 0x{offset:X} of {length} bytes exceeds file size 0x{:X}",
                self.byte_len
            ));
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|error| format!("could not seek in ROM: {error}"))?;
        let mut bytes = vec![0; length];
        self.file
            .read_exact(&mut bytes)
            .map_err(|error| format!("could not read ROM data at 0x{offset:X}: {error}"))?;
        Ok(bytes)
    }

    /// Calculate a stable identity for this exact ROM image using CRC-32.
    pub fn crc32(&mut self) -> Result<u32> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| format!("could not seek to start of ROM: {error}"))?;
        let mut crc = Crc32::new();
        let mut buffer = [0_u8; 1024 * 1024];
        loop {
            let count = self
                .file
                .read(&mut buffer)
                .map_err(|error| format!("could not read ROM for checksum: {error}"))?;
            if count == 0 {
                break;
            }
            crc.update(&buffer[..count]);
        }
        Ok(crc.finish())
    }
}

/// Small dependency-free CRC-32 implementation (IEEE 802.3).
#[derive(Clone, Copy)]
pub(crate) struct Crc32(u32);

const fn make_crc32_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xEDB8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = make_crc32_table();

impl Crc32 {
    pub(crate) const fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            let index = ((self.0 ^ u32::from(*byte)) & 0xFF) as usize;
            self.0 = (self.0 >> 8) ^ CRC32_TABLE[index];
        }
    }

    pub(crate) const fn finish(self) -> u32 {
        !self.0
    }
}

pub(crate) fn crc16_nintendo(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}
