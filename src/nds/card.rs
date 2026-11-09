//! The game card as the ARM9 sees it in normal (KEY2) mode: an 8-byte
//! command in 0x040001A8, a transfer started from ROMCTRL, and data words
//! read from 0x04100010 (by the CPU or a card-triggered DMA), and the SPI
//! bus to the card's backup chip through AUXSPICNT and AUXSPIDATA.

use crate::Result;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const ROMCTRL: u32 = 0x0400_01A4;
pub const COMMAND: u32 = 0x0400_01A8;
pub const DATA: u32 = 0x0410_0010;

/// ROMCTRL: data word ready, and block busy (also the start bit).
pub const ROMCTRL_READY: u32 = 1 << 23;
pub const ROMCTRL_BUSY: u32 = 1 << 31;

/// 1 Gbit-class Macronix mask ROM; only the low byte is checked by games.
pub const CHIP_ID: u32 = 0x0000_7FC2;

pub const AUXSPICNT: u32 = 0x0400_01A0;
pub const AUXSPIDATA: u32 = 0x0400_01A2;
/// AUXSPICNT: keep chip select after this byte, SPI (not ROM) mode, enable.
pub const AUXSPICNT_HOLD: u16 = 1 << 6;
pub const AUXSPICNT_SPI: u16 = 1 << 13;
pub const AUXSPICNT_ENABLE: u16 = 1 << 15;

/// Infrared chip commands: pass the rest of the transfer through to the
/// backup flash, and read the chip's ID, which is always 0xAA.
const IR_PASS_THROUGH: u8 = 0x00;
const IR_ID: u8 = 0x08;
const IR_ID_REPLY: u8 = 0xAA;

pub struct Card {
    rom: File,
    rom_size: u64,
    /// The mask ROM's capacity: addresses wrap at this size, as the chip
    /// ignores the address lines above it.
    chip_size: u64,
    pending: VecDeque<u32>,
    pub spi: BackupSpi,
}

impl Card {
    pub fn open(path: &Path) -> Result<Self> {
        let mut rom = File::open(path)
            .map_err(|error| format!("could not open ROM '{}': {error}", path.display()))?;
        let rom_size = rom
            .metadata()
            .map_err(|error| format!("could not read ROM size: {error}"))?
            .len();
        // Header byte 0x14: device capacity, 128 KiB << n.
        let mut capacity = [0u8; 1];
        rom.seek(SeekFrom::Start(0x14))
            .and_then(|_| rom.read_exact(&mut capacity))
            .map_err(|error| format!("could not read the ROM header: {error}"))?;
        let chip_size = (0x2_0000u64 << capacity[0].min(15)).max(rom_size.next_power_of_two());
        let mut game_code = [0u8; 4];
        rom.seek(SeekFrom::Start(0x0C))
            .and_then(|_| rom.read_exact(&mut game_code))
            .map_err(|error| format!("could not read the ROM header: {error}"))?;
        Ok(Self {
            rom,
            rom_size,
            chip_size,
            pending: VecDeque::new(),
            spi: BackupSpi::new(game_code),
        })
    }

    /// Start a transfer for `command` with the ROMCTRL value written.
    /// Returns a description of an unsupported command.
    pub fn start(&mut self, command: [u8; 8], control: u32) -> std::result::Result<(), String> {
        self.pending.clear();
        let length = match (control >> 24) & 7 {
            0 => 0,
            7 => 4,
            size => 0x100 << size,
        };
        match command[0] {
            0xB7 => {
                let mut address =
                    u32::from_be_bytes([command[1], command[2], command[3], command[4]]);
                // The secure area cannot be read in this mode; retail cards
                // return data from 0x8000 onward instead.
                // Black 2 and White 2 check both behaviours with a read of
                // 0x20001000, which a flash cartridge answers differently.
                address = (u64::from(address) % self.chip_size) as u32;
                if address < 0x8000 {
                    address = 0x8000 + (address & 0x1FF);
                }
                let mut bytes = vec![0xFF; length];
                if u64::from(address) < self.rom_size {
                    let available =
                        (self.rom_size - u64::from(address)).min(length as u64) as usize;
                    self.rom
                        .seek(SeekFrom::Start(u64::from(address)))
                        .and_then(|_| self.rom.read_exact(&mut bytes[..available]))
                        .map_err(|error| format!("card read at 0x{address:08X} failed: {error}"))?;
                }
                self.pending.extend(
                    bytes
                        .chunks_exact(4)
                        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]])),
                );
            }
            0xB8 => self
                .pending
                .extend(std::iter::repeat_n(CHIP_ID, length / 4)),
            other => return Err(format!("card command 0x{other:02X} ({command:02X?})")),
        }
        Ok(())
    }

    pub fn has_data(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn next_word(&mut self) -> u32 {
        self.pending.pop_front().unwrap_or(0xFFFF_FFFF)
    }
}

/// The SPI bus from AUXSPIDATA to the card's backup chip, one byte per
/// write. Saves do not use it: the ARM7 stand-in serves backup requests.
/// What does is the check that the card is genuine: cards whose game code
/// starts with 'I' (HeartGold and SoulSilver, Black and White, Black 2 and
/// White 2) put an infrared chip in front of the flash, and Black 2 and
/// White 2 ask it for its ID before awarding experience. Without the
/// expected 0xAA, Pokemon gain no EXP after a battle.
pub struct BackupSpi {
    infrared: bool,
    /// Bytes sent since chip select was last released.
    sent: usize,
    /// The infrared command: the first byte of the transfer.
    command: u8,
}

impl BackupSpi {
    pub fn new(game_code: [u8; 4]) -> Self {
        Self {
            infrared: game_code[0] == b'I',
            sent: 0,
            command: 0,
        }
    }

    /// Send `value` and return the byte read back. Without `hold`, chip
    /// select is released after it, ending the transfer.
    pub fn transfer(&mut self, value: u8, hold: bool) -> u8 {
        let reply = if !self.infrared {
            // No flash is modelled on the bus: the line floats high.
            0xFF
        } else if self.sent == 0 {
            self.command = value;
            0x00
        } else {
            match self.command {
                IR_ID => IR_ID_REPLY,
                IR_PASS_THROUGH => 0xFF,
                // Infrared send and receive: nothing in range.
                _ => 0x00,
            }
        };
        self.sent = if hold { self.sent + 1 } else { 0 };
        reply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The genuine-card check in Black 2 and White 2's overlay 338: hold
    /// chip select, send the ID command 0x08, then release it on a dummy
    /// byte, which must read back 0xAA or no Pokemon gains EXP.
    #[test]
    fn infrared_chip_answers_its_id_command() {
        let mut spi = BackupSpi::new(*b"IRDO");
        assert_eq!(spi.transfer(IR_ID, true), 0x00);
        assert_eq!(spi.transfer(0x00, false), IR_ID_REPLY);
        // The next transfer starts over with a new command byte.
        assert_eq!(spi.transfer(IR_PASS_THROUGH, true), 0x00);
        assert_eq!(spi.transfer(0x05, false), 0xFF);
        assert_eq!(spi.transfer(IR_ID, true), 0x00);
        assert_eq!(spi.transfer(0x00, true), IR_ID_REPLY);
        assert_eq!(spi.transfer(0x00, false), IR_ID_REPLY);
    }

    #[test]
    fn cards_without_infrared_have_no_id_chip() {
        let mut spi = BackupSpi::new(*b"ADAE");
        assert_eq!(spi.transfer(IR_ID, true), 0xFF);
        assert_eq!(spi.transfer(0x00, false), 0xFF);
    }
}
