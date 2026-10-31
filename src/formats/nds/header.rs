use crate::Result;

const NDS_HEADER_MIN_SIZE: usize = 0x200;

#[derive(Debug, Clone, Copy)]
pub struct RomSection {
    pub name: &'static str,
    pub offset: u32,
    pub size: u32,
}

impl RomSection {
    fn validate(self, rom_size: u64) -> Result<()> {
        if self.offset == 0 && self.size == 0 {
            return Ok(());
        }
        if self.offset == 0 || self.size == 0 {
            return Err(format!(
                "{} section has only one of offset/size set (0x{:X}, 0x{:X})",
                self.name, self.offset, self.size
            ));
        }
        let end = u64::from(self.offset)
            .checked_add(u64::from(self.size))
            .ok_or_else(|| format!("{} section range overflowed", self.name))?;
        if end > rom_size {
            return Err(format!(
                "{} section ends at 0x{end:X}, beyond ROM size 0x{rom_size:X}",
                self.name
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NdsHeader {
    pub title: String,
    pub game_code: [u8; 4],
    pub maker_code: [u8; 2],
    pub unit_code: u8,
    pub version: u8,
    pub arm9: RomSection,
    pub arm9_entry: u32,
    pub arm9_load: u32,
    pub arm7: RomSection,
    pub arm7_entry: u32,
    pub arm7_load: u32,
    pub fnt: RomSection,
    pub fat: RomSection,
    pub arm9_overlay: RomSection,
    pub arm7_overlay: RomSection,
    pub banner_offset: u32,
    pub declared_rom_size: u32,
    pub declared_header_size: u32,
    pub stored_logo_crc16: u16,
    pub calculated_logo_crc16: u16,
    pub stored_header_crc16: u16,
    pub calculated_header_crc16: u16,
}

impl NdsHeader {
    pub fn parse(bytes: &[u8], rom_size: u64) -> Result<Self> {
        if bytes.len() < NDS_HEADER_MIN_SIZE {
            return Err("NDS header buffer is shorter than 0x200 bytes".to_owned());
        }

        let title = decode_header_text(&bytes[0x00..0x0C]);
        let game_code = bytes[0x0C..0x10]
            .try_into()
            .map_err(|_| "invalid game code in NDS header".to_owned())?;
        let maker_code = bytes[0x10..0x12]
            .try_into()
            .map_err(|_| "invalid maker code in NDS header".to_owned())?;

        let arm9 = section(bytes, 0x20, 0x2C, "ARM9");
        let arm7 = section(bytes, 0x30, 0x3C, "ARM7");
        let fnt = section(bytes, 0x40, 0x44, "FNT");
        let fat = section(bytes, 0x48, 0x4C, "FAT");
        let arm9_overlay = section(bytes, 0x50, 0x54, "ARM9 overlay table");
        let arm7_overlay = section(bytes, 0x58, 0x5C, "ARM7 overlay table");

        for section in [arm9, arm7, fnt, fat, arm9_overlay, arm7_overlay] {
            section.validate(rom_size)?;
        }
        if fnt.size < 8 {
            return Err("NDS filename table is missing or too small".to_owned());
        }
        if fnt.size > 16 * 1024 * 1024 {
            return Err(format!(
                "NDS filename table is implausibly large: {} bytes",
                fnt.size
            ));
        }
        if fat.size == 0 || fat.size % 8 != 0 {
            return Err(format!(
                "NDS allocation table has invalid size 0x{:X} (expected a non-zero multiple of 8)",
                fat.size
            ));
        }
        if fat.size > 512 * 1024 {
            return Err(format!(
                "NDS allocation table is implausibly large: {} bytes",
                fat.size
            ));
        }
        if title.is_empty() {
            return Err("NDS header does not contain a game title".to_owned());
        }

        let stored_logo_crc16 = u16_at(bytes, 0x15C);
        let calculated_logo_crc16 = super::crc16_nintendo(&bytes[0x0C0..0x15C]);
        let stored_header_crc16 = u16_at(bytes, 0x15E);
        let calculated_header_crc16 = super::crc16_nintendo(&bytes[..0x15E]);
        if stored_logo_crc16 != calculated_logo_crc16 {
            return Err(format!(
                "Nintendo logo checksum mismatch (header 0x{stored_logo_crc16:04X}, calculated 0x{calculated_logo_crc16:04X})"
            ));
        }
        if stored_header_crc16 != calculated_header_crc16 {
            return Err(format!(
                "header checksum mismatch (header 0x{stored_header_crc16:04X}, calculated 0x{calculated_header_crc16:04X})"
            ));
        }

        let declared_rom_size = u32_at(bytes, 0x80);
        let declared_header_size = u32_at(bytes, 0x84);
        if declared_rom_size == 0 {
            return Err("header declares an empty ROM".to_owned());
        }
        if u64::from(declared_rom_size) > rom_size {
            return Err(format!(
                "header declares ROM size 0x{declared_rom_size:X}, larger than file size 0x{rom_size:X}"
            ));
        }
        if declared_header_size < 0x160 || u64::from(declared_header_size) > rom_size {
            return Err(format!(
                "header declares invalid header size 0x{declared_header_size:X}"
            ));
        }

        Ok(Self {
            title,
            game_code,
            maker_code,
            unit_code: bytes[0x12],
            version: bytes[0x1E],
            arm9,
            arm9_entry: u32_at(bytes, 0x24),
            arm9_load: u32_at(bytes, 0x28),
            arm7,
            arm7_entry: u32_at(bytes, 0x34),
            arm7_load: u32_at(bytes, 0x38),
            fnt,
            fat,
            arm9_overlay,
            arm7_overlay,
            banner_offset: u32_at(bytes, 0x68),
            declared_rom_size,
            declared_header_size,
            stored_logo_crc16,
            calculated_logo_crc16,
            stored_header_crc16,
            calculated_header_crc16,
        })
    }
}

fn section(bytes: &[u8], offset_at: usize, size_at: usize, name: &'static str) -> RomSection {
    RomSection {
        name,
        offset: u32_at(bytes, offset_at),
        size: u32_at(bytes, size_at),
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

fn decode_header_text(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0 && *byte != b' ')
        .map_or(0, |index| index + 1);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}
