//! The game card's backup (save) memory, served by the ARM7 in hardware.
//!
//! NitroSDK sends card backup requests to the ARM7 over PXI with a shared
//! command block (`CARDiCommandArg`); the ARM7 runs them over SPI. Here the
//! save memory is a file, and each request is applied to it directly.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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
    pub history: Option<crate::debug::History>,
}

impl Backup {
    /// Open the save file, or start with erased memory if there is none.
    ///
    /// Any other read error is returned: starting empty would overwrite the
    /// real save on the game's next write.
    pub fn open(path: PathBuf) -> std::result::Result<Self, String> {
        let data = read_if_present(&path)
            .map_err(|error| format!("could not read save '{}': {error}", path.display()))?
            .unwrap_or_default();
        Ok(Self {
            data,
            path,
            history: None,
        })
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
        replace_file(&self.path, &self.data)
            .map_err(|error| format!("could not write save '{}': {error}", self.path.display()))
    }
}

/// The file's contents, or `None` if it does not exist.
pub(super) fn read_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Replace the file's contents in one step: write a sibling temporary file,
/// then rename it over the file. The process can exit at any moment (a
/// watchdog, a closed window, a crash), so writing in place could leave a
/// truncated save.
pub(super) fn replace_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, data)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("gen5-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn missing_save_starts_empty_and_flush_replaces_it() {
        let directory = scratch("missing");
        let path = directory.join("save.bin");
        let mut backup = Backup::open(path.clone()).unwrap();
        assert!(backup.data.is_empty());
        backup.ensure_size(16);
        backup.data[0] = 0x12;
        backup.flush().unwrap();
        backup.data[0] = 0x34;
        backup.flush().unwrap();
        let mut expected = vec![0xFF; 16];
        expected[0] = 0x34;
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert!(!directory.join("save.bin.tmp").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn unreadable_save_is_an_error_not_an_empty_card() {
        // A directory where the file should be cannot be read as one.
        let directory = scratch("unreadable");
        let path = directory.join("save.bin");
        fs::create_dir(&path).unwrap();
        let error = Backup::open(path).err().unwrap();
        assert!(error.starts_with("could not read save"), "{error}");
        let _ = fs::remove_dir_all(directory);
    }
}
