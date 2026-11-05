//! Video RAM: nine banks (A-I) that VRAMCNT maps into the engines' BG and
//! OBJ regions, the LCDC region, extended palettes and the 3D engine's
//! texture slots. The CPU and the renderers both go through the same maps.
//! Layout per GBATEK; where banks overlap, the later bank wins instead of
//! the hardware's mix.

/// Offsets of banks A-I in `Vram::banks` (the same as in the LCDC region).
const BANK_BASE: [usize; 9] = [
    0x0_0000, 0x2_0000, 0x4_0000, 0x6_0000, 0x8_0000, 0x9_0000, 0x9_4000, 0x9_8000, 0xA_0000,
];
const BANK_SIZE: [usize; 9] = [
    0x2_0000, 0x2_0000, 0x2_0000, 0x2_0000, 0x1_0000, 0x4000, 0x4000, 0x8000, 0x4000,
];
pub const TOTAL: usize = 0xA_4000;
/// Granularity of the maps: the smallest mappable piece (a BG or OBJ
/// extended palette slot is 8 KiB).
const UNIT: usize = 0x2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    ABg,
    BBg,
    AObj,
    BObj,
    Lcdc,
    Texture,
    TexturePalette,
    ABgExtPalette,
    BBgExtPalette,
    AObjExtPalette,
    BObjExtPalette,
}

const REGIONS: usize = 11;

fn region_size(region: Region) -> usize {
    match region {
        Region::ABg => 0x8_0000,
        Region::BBg | Region::BObj => 0x2_0000,
        Region::AObj => 0x4_0000,
        Region::Lcdc => TOTAL,
        Region::Texture => 0x8_0000,
        Region::TexturePalette => 0x2_0000,
        Region::ABgExtPalette | Region::BBgExtPalette => 0x8000,
        Region::AObjExtPalette | Region::BObjExtPalette => 0x2000,
    }
}

pub struct Vram {
    pub banks: Vec<u8>,
    /// VRAMCNT_A..I.
    pub control: [u8; 9],
    /// For each region, the bank offset of every UNIT-sized piece.
    maps: [Vec<Option<usize>>; REGIONS],
}

impl Vram {
    pub fn new() -> Self {
        let mut vram = Self {
            banks: vec![0; TOTAL],
            control: [0; 9],
            maps: std::array::from_fn(|_| Vec::new()),
        };
        vram.update();
        vram
    }

    /// Rebuild the maps after a VRAMCNT write.
    pub fn update(&mut self) {
        let all = [
            Region::ABg,
            Region::BBg,
            Region::AObj,
            Region::BObj,
            Region::Lcdc,
            Region::Texture,
            Region::TexturePalette,
            Region::ABgExtPalette,
            Region::BBgExtPalette,
            Region::AObjExtPalette,
            Region::BObjExtPalette,
        ];
        for region in all {
            self.maps[region as usize] = vec![None; region_size(region) / UNIT];
        }
        for bank in 0..9 {
            let control = self.control[bank];
            if control & 0x80 == 0 {
                continue;
            }
            let mst = control & 7;
            let offset = usize::from((control >> 3) & 3);
            let size = BANK_SIZE[bank];
            let mut place = |region: Region, at: usize, length: usize| {
                let map = &mut self.maps[region as usize];
                for unit in 0..length / UNIT {
                    if let Some(slot) = map.get_mut(at / UNIT + unit) {
                        *slot = Some(BANK_BASE[bank] + unit * UNIT);
                    }
                }
            };
            match (bank, mst) {
                (_, 0) => place(Region::Lcdc, BANK_BASE[bank], size),
                // A-D: 128 KiB banks.
                (0..=3, 1) => place(Region::ABg, 0x2_0000 * offset, size),
                (0 | 1, 2) => place(Region::AObj, 0x2_0000 * (offset & 1), size),
                (0..=3, 3) => place(Region::Texture, 0x2_0000 * offset, size),
                (2, 4) => place(Region::BBg, 0, size),
                (3, 4) => place(Region::BObj, 0, size),
                // E: 64 KiB.
                (4, 1) => place(Region::ABg, 0, size),
                (4, 2) => place(Region::AObj, 0, size),
                (4, 3) => place(Region::TexturePalette, 0, size),
                (4, 4) => place(Region::ABgExtPalette, 0, 0x8000),
                // F, G: 16 KiB.
                (5 | 6, 1) => place(
                    Region::ABg,
                    0x4000 * (offset & 1) + 0x1_0000 * (offset >> 1),
                    size,
                ),
                (5 | 6, 2) => place(
                    Region::AObj,
                    0x4000 * (offset & 1) + 0x1_0000 * (offset >> 1),
                    size,
                ),
                (5 | 6, 3) => place(
                    Region::TexturePalette,
                    0x4000 * ((offset & 1) + 4 * (offset >> 1)),
                    size,
                ),
                (5 | 6, 4) => place(Region::ABgExtPalette, 0x4000 * (offset & 1), size),
                (5 | 6, 5) => place(Region::AObjExtPalette, 0, 0x2000),
                // H: 32 KiB.
                (7, 1) => place(Region::BBg, 0, size),
                (7, 2) => place(Region::BBgExtPalette, 0, size),
                // I: 16 KiB.
                (8, 1) => place(Region::BBg, 0x8000, size),
                (8, 2) => place(Region::BObj, 0, size),
                (8, 3) => place(Region::BObjExtPalette, 0, 0x2000),
                _ => {}
            }
        }
    }

    /// The bank offset behind an ARM9 address in 0x06000000-0x06FFFFFF.
    pub fn cpu_offset(&self, address: u32) -> Option<usize> {
        let (region, offset) = match (address >> 20) & 0xF {
            0x0 | 0x1 => (Region::ABg, address as usize % 0x8_0000),
            0x2 | 0x3 => (Region::BBg, address as usize % 0x2_0000),
            0x4 | 0x5 => (Region::AObj, address as usize % 0x4_0000),
            0x6 | 0x7 => (Region::BObj, address as usize % 0x2_0000),
            _ => (Region::Lcdc, address as usize % 0x10_0000),
        };
        self.offset(region, offset)
    }

    pub fn offset(&self, region: Region, offset: usize) -> Option<usize> {
        let base = (*self.maps[region as usize].get(offset / UNIT)?)?;
        Some(base + offset % UNIT)
    }

    /// A region as one linear buffer, unmapped parts zero.
    pub fn snapshot(&self, region: Region) -> Vec<u8> {
        let mut out = vec![0; region_size(region)];
        for (unit, chunk) in out.chunks_mut(UNIT).enumerate() {
            if let Some(Some(base)) = self.maps[region as usize].get(unit) {
                chunk.copy_from_slice(&self.banks[*base..*base + UNIT]);
            }
        }
        out
    }

    pub fn read8(&self, region: Region, offset: usize) -> u8 {
        self.offset(region, offset).map_or(0, |at| self.banks[at])
    }

    pub fn read16(&self, region: Region, offset: usize) -> u16 {
        self.offset(region, offset & !1).map_or(0, |at| {
            u16::from_le_bytes([self.banks[at], self.banks[at + 1]])
        })
    }
}

impl Default for Vram {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banks_follow_vramcnt() {
        let mut vram = Vram::new();
        assert_eq!(vram.cpu_offset(0x0600_0000), None);
        // A as engine A BG at offset 1, C as engine B BG, F as BG ext palette.
        vram.control[0] = 0x80 | 1 | (1 << 3);
        vram.control[2] = 0x80 | 4;
        vram.control[5] = 0x80 | 4;
        vram.update();
        assert_eq!(vram.cpu_offset(0x0602_0010), Some(0x10));
        assert_eq!(vram.cpu_offset(0x0620_0004), Some(0x4_0004));
        assert_eq!(vram.offset(Region::ABgExtPalette, 0x2002), Some(0x9_2002));
        assert_eq!(vram.offset(Region::ABgExtPalette, 0x4000), None);
        // Mirrors: engine A BG repeats every 512 KiB.
        assert_eq!(vram.cpu_offset(0x060A_0010), Some(0x10));
        // LCDC: H at its fixed address.
        vram.control[7] = 0x80;
        vram.update();
        assert_eq!(vram.cpu_offset(0x0689_8000), Some(0x9_8000));
    }

    #[test]
    fn snapshot_follows_the_region_map() {
        let mut vram = Vram::new();
        vram.control[1] = 0x80 | 3 | (1 << 3); // B as texture slot 1
        vram.update();
        vram.banks[0x2_0000 + 5] = 7;
        let texture = vram.snapshot(Region::Texture);
        assert_eq!(texture.len(), 0x8_0000);
        assert_eq!((texture[0x2_0005], texture[5]), (7, 0));
    }
}
