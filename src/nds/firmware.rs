//! The console's firmware flash (NVRAM), served by the ARM7 over SPI.
//!
//! Games read the user settings and the Nintendo Wi-Fi Connection settings
//! from it, and DWC writes its user ID back. There is no dump of a real
//! console's firmware, so the stand-in builds a freshly set up DS: a header
//! that points at the settings, three empty Wi-Fi slots and one set of user
//! settings, each with a valid CRC. The image is kept in a file so what the
//! game writes survives restarts. Layout per GBATEK.

use std::fs;
use std::path::PathBuf;

pub const SIZE: usize = 256 * 1024;
const PAGE: usize = 256;
const SECTOR: usize = 64 * 1024;
/// Where the user settings live (header word 0x20 holds this / 8).
const USER_SETTINGS: usize = 0x3_FE00;
pub const USER_SETTINGS_LENGTH: usize = 0x70;
/// Nintendo Wi-Fi Connection slots 1-3.
const WFC_SLOTS: usize = 0x3_FA00;
/// Wireless calibration data in the header: CRC, length, then the data.
const WIFI_CRC: usize = 0x2A;
const WIFI_LENGTH: usize = 0x138;
/// A MAC address in Nintendo's range. Black 2 and White 2 refuse to run
/// parts of the game on a console whose MAC is all zeros or the well-known
/// emulator default below, which earlier firmware images used.
const MAC: [u8; 6] = [0x00, 0x09, 0xBF, 0x4E, 0x2A, 0x71];
const EMULATOR_MAC: [u8; 6] = [0x00, 0x09, 0xBF, 0x12, 0x34, 0x56];
const MAC_OFFSET: usize = 0x36;

/// NitroSDK NVRAM PXI commands (SPI flash instructions).
pub const WRITE_ENABLE: u32 = 0x20;
pub const WRITE_DISABLE: u32 = 0x21;
pub const READ_STATUS: u32 = 0x22;
pub const READ: u32 = 0x23;
pub const FAST_READ: u32 = 0x24;
pub const PAGE_WRITE: u32 = 0x25;
pub const PAGE_PROGRAM: u32 = 0x26;
pub const PAGE_ERASE: u32 = 0x27;
pub const SECTOR_ERASE: u32 = 0x28;

pub struct Firmware {
    pub data: Vec<u8>,
    path: PathBuf,
}

/// CRC-16 as the BIOS and firmware compute it (reflected 0x8005).
pub fn crc16(initial: u16, bytes: &[u8]) -> u16 {
    let mut crc = initial;
    for &byte in bytes {
        crc ^= u16::from(byte);
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

fn put16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

impl Firmware {
    /// Open the firmware file, or build a fresh one if there is none.
    pub fn open(path: PathBuf) -> Self {
        let mut data = fs::read(&path)
            .ok()
            .filter(|data| data.len() == SIZE)
            .unwrap_or_else(fresh);
        if data[MAC_OFFSET..MAC_OFFSET + 6] == EMULATOR_MAC {
            set_mac(&mut data, MAC);
        }
        Self { data, path }
    }

    /// The console's MAC address, which the ARM7 copies to shared RAM.
    pub fn mac(&self) -> [u8; 6] {
        std::array::from_fn(|index| self.data[MAC_OFFSET + index])
    }

    /// The current user settings, as the ARM7 copies them to shared RAM.
    pub fn user_settings(&self) -> &[u8] {
        // Two copies; the valid one with the higher update counter wins.
        let copy = |index: usize| {
            let base = USER_SETTINGS + index * 0x100;
            let block = &self.data[base..base + 0x100];
            let stored = u16::from_le_bytes([block[0x72], block[0x73]]);
            let valid = crc16(0xFFFF, &block[..USER_SETTINGS_LENGTH]) == stored;
            valid.then(|| (u16::from_le_bytes([block[0x70], block[0x71]]), base))
        };
        let base = match (copy(0), copy(1)) {
            (Some((first, a)), Some((second, b))) => {
                // The counter wraps at 0x80.
                if (second.wrapping_sub(first) & 0x7F) == 1 { b } else { a }
            }
            (Some((_, a)), None) => a,
            (_, Some((_, b))) => b,
            (None, None) => USER_SETTINGS,
        };
        &self.data[base..base + USER_SETTINGS_LENGTH]
    }

    pub fn range(&self, address: u32, length: u32) -> std::ops::Range<usize> {
        let start = (address as usize) % SIZE;
        start..(start + length as usize).min(SIZE)
    }

    /// Erase the page or sector holding `address`.
    pub fn erase(&mut self, address: u32, sector: bool) {
        let unit = if sector { SECTOR } else { PAGE };
        let start = (address as usize % SIZE) & !(unit - 1);
        self.data[start..start + unit].fill(0xFF);
    }

    /// Write the firmware file after a change.
    pub fn flush(&self) -> std::result::Result<(), String> {
        fs::write(&self.path, &self.data).map_err(|error| {
            format!(
                "could not write firmware '{}': {error}",
                self.path.display()
            )
        })
    }
}

/// Store `mac` in the wireless calibration block and update its CRC.
fn set_mac(data: &mut [u8], mac: [u8; 6]) {
    data[MAC_OFFSET..MAC_OFFSET + 6].copy_from_slice(&mac);
    let crc = crc16(0, &data[WIFI_CRC + 2..WIFI_CRC + 2 + WIFI_LENGTH]);
    put16(data, WIFI_CRC, crc);
}

/// A freshly set up DS: English, no nickname, no Wi-Fi connections.
fn fresh() -> Vec<u8> {
    let mut data = vec![0xFF; SIZE];
    // Header: console type (original DS), the user settings pointer and
    // the wireless calibration block with the console's MAC address.
    data[..0x200].fill(0);
    data[0x1D] = 0xFF;
    put16(&mut data, 0x20, (USER_SETTINGS / 8) as u16);
    put16(&mut data, WIFI_CRC + 2, WIFI_LENGTH as u16);
    set_mac(&mut data, MAC);

    // Wi-Fi Connection slots: not configured, no user ID yet.
    for slot in 0..3 {
        let base = WFC_SLOTS + slot * 0x100;
        let block = &mut data[base..base + 0x100];
        block.fill(0);
        block[0xE7] = 0xFF;
        block[0xEF] = 1 << slot;
        let crc = crc16(0, &block[..0xFE]);
        put16(block, 0xFE, crc);
    }

    // User settings, twice; the second copy is the newer one.
    let mut settings = [0_u8; 0x100];
    put16(&mut settings, 0x00, 5);
    settings[0x03] = 1;
    settings[0x04] = 1;
    // Touch screen calibration: raw ADC values are pixels * 16.
    put16(&mut settings, 0x58, 0);
    put16(&mut settings, 0x5A, 0);
    put16(&mut settings, 0x5E, 255 << 4);
    put16(&mut settings, 0x60, 191 << 4);
    settings[0x62] = 255;
    settings[0x63] = 191;
    // Language English.
    put16(&mut settings, 0x64, 1);
    settings[0x74..].fill(0xFF);
    for (index, counter) in [(0, 0_u16), (1, 1)] {
        put16(&mut settings, 0x70, counter);
        let crc = crc16(0xFFFF, &settings[..USER_SETTINGS_LENGTH]);
        put16(&mut settings, 0x72, crc);
        let base = USER_SETTINGS + index * 0x100;
        data[base..base + 0x100].copy_from_slice(&settings);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_image_has_valid_settings() {
        let firmware = Firmware {
            data: fresh(),
            path: PathBuf::new(),
        };
        let pointer = u16::from_le_bytes([firmware.data[0x20], firmware.data[0x21]]);
        assert_eq!(usize::from(pointer) * 8, USER_SETTINGS);
        let settings = firmware.user_settings();
        assert_eq!(settings[0x64] & 7, 1);
        let slot = &firmware.data[WFC_SLOTS..WFC_SLOTS + 0x100];
        assert_eq!(crc16(0, &slot[..0xFE]), u16::from_le_bytes([slot[0xFE], slot[0xFF]]));
    }

    #[test]
    fn emulator_default_mac_is_replaced() {
        let path = std::env::temp_dir().join("gen5-firmware-mac-test.bin");
        let mut data = fresh();
        set_mac(&mut data, EMULATOR_MAC);
        fs::write(&path, &data).unwrap();
        let firmware = Firmware::open(path.clone());
        let _ = fs::remove_file(path);
        assert_eq!(firmware.mac(), MAC);
        let stored = u16::from_le_bytes([firmware.data[WIFI_CRC], firmware.data[WIFI_CRC + 1]]);
        assert_eq!(
            crc16(0, &firmware.data[WIFI_CRC + 2..WIFI_CRC + 2 + WIFI_LENGTH]),
            stored
        );
    }

    #[test]
    fn crc16_matches_known_value() {
        // CRC-16/ARC of "123456789".
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }
}
