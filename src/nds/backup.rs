//! The game card's backup (save) memory, served by the ARM7 in hardware.
//!
//! NitroSDK sends card backup requests to the ARM7 over PXI with a shared
//! command block (`CARDiCommandArg`); the ARM7 runs them over SPI. Here the
//! save memory is a file, and each request is applied to it directly.

use std::fs;
use std::path::PathBuf;

/// `CARDiCommandArg` field offsets.
pub const ARG_RESULT: u32 = 0x00;
pub const ARG_ID: u32 = 0x08;
pub const ARG_SOURCE: u32 = 0x0C;
pub const ARG_DESTINATION: u32 = 0x10;
pub const ARG_LENGTH: u32 = 0x14;
/// `CARDBackupSpec::total_size`, the first field of the chip spec.
pub const ARG_TOTAL_SIZE: u32 = 0x18;

/// `CARDRequest` values.
pub const REQ_INIT: u32 = 0;
pub const REQ_IDENTIFY: u32 = 2;
pub const REQ_READ_ID: u32 = 3;
pub const REQ_READ_BACKUP: u32 = 6;
pub const REQ_WRITE_BACKUP: u32 = 7;
pub const REQ_PROGRAM_BACKUP: u32 = 8;
pub const REQ_VERIFY_BACKUP: u32 = 9;
pub const REQ_ERASE_PAGE_BACKUP: u32 = 10;
pub const REQ_ERASE_SECTOR_BACKUP: u32 = 11;
pub const REQ_ERASE_CHIP_BACKUP: u32 = 12;
pub const REQ_ERASE_SUBSECTOR_BACKUP: u32 = 15;

/// `CARDResult` values.
pub const RESULT_SUCCESS: u32 = 0;
pub const RESULT_FAILURE: u32 = 1;

pub struct Backup {
    pub data: Vec<u8>,
    path: PathBuf,
}

impl Backup {
    /// Open the save file, or start with erased memory if there is none.
    pub fn open(path: PathBuf) -> Self {
        let data = fs::read(&path).unwrap_or_default();
        Self { data, path }
    }

    /// Size the memory to what the game's chip spec declares.
    pub fn ensure_size(&mut self, size: usize) {
        if size > self.data.len() && size <= 16 * 1024 * 1024 {
            self.data.resize(size, 0xFF);
        }
    }

    pub fn range(&self, offset: u32, length: u32) -> Option<std::ops::Range<usize>> {
        let start = offset as usize;
        let end = start.checked_add(length as usize)?;
        (end <= self.data.len()).then_some(start..end)
    }

    pub fn erase(&mut self, offset: u32, length: u32) -> bool {
        match self.range(offset, length) {
            Some(range) => {
                self.data[range].fill(0xFF);
                true
            }
            None => false,
        }
    }

    /// Write the save file after a change.
    pub fn flush(&self) -> std::result::Result<(), String> {
        fs::write(&self.path, &self.data)
            .map_err(|error| format!("could not write save '{}': {error}", self.path.display()))
    }
}
