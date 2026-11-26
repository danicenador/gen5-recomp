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
    /// What display captures in widescreen saw beside the DS's columns,
    /// at most one per bank.
    pub wide_captures: Vec<WideCapture>,
}

/// A display capture made in widescreen, with what the screen showed in
/// the extra columns beside the 256 captured ones. The game shows captured
/// screens as bitmap backgrounds in transitions (a door, a battle starting,
/// an item received); these columns let them fill the sides as well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WideCapture {
    /// Where the first captured pixel is in `Vram::banks`.
    pub start: usize,
    /// The 256 x height pixels the capture wrote, to tell whether they are
    /// still there.
    pub written: Vec<u16>,
    /// Per line, `extra` pixels left of the captured ones, then `extra`
    /// right of them; bit 15 is set where they are opaque.
    pub sides: Vec<u16>,
    pub extra: usize,
}

/// A per-channel change of 15-bit colors (`WideCapture::changes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorMap([[u8; 32]; 3]);

impl ColorMap {
    /// The color changed, keeping bit 15.
    pub fn apply(&self, color: u16) -> u16 {
        let mut out = color & 0x8000;
        for (channel, table) in self.0.iter().enumerate() {
            let shift = 5 * channel;
            out |= u16::from(table[usize::from((color >> shift) & 31)]) << shift;
        }
        out
    }
}

impl WideCapture {
    pub fn height(&self) -> usize {
        self.written.len() / 256
    }

    /// How the captured pixels at `start` have changed since: the game may
    /// darken or tint a captured screen in place (Black 2 and White 2 halve
    /// it as a battle starts). `None` unless every channel's value maps to
    /// one new value throughout (all but 1% of the pixels), so that
    /// something else drawn there, or other data, is not taken for it.
    pub fn changes(&self, vram: &Vram) -> Option<ColorMap> {
        let end = self.start + self.written.len() * 2;
        let bytes = vram.banks.get(self.start..end)?;
        let mut seen = [[None::<u8>; 32]; 3];
        let mut conflicts = 0;
        for (&pair, &old) in bytes.as_chunks::<2>().0.iter().zip(&self.written) {
            let new = u16::from_le_bytes(pair);
            let mut conflict = false;
            for (channel, seen) in seen.iter_mut().enumerate() {
                let shift = 5 * channel;
                let (from, to) = (((old >> shift) & 31) as usize, ((new >> shift) & 31) as u8);
                match seen[from] {
                    None => seen[from] = Some(to),
                    Some(value) if value != to => conflict = true,
                    _ => {}
                }
            }
            conflicts += usize::from(conflict);
        }
        if conflicts * 100 > self.written.len() {
            return None;
        }
        // Values the captured pixels never had follow their neighbours.
        Some(ColorMap(seen.map(|seen| {
            std::array::from_fn(|value| {
                let below = (0..=value).rev().find_map(|v| seen[v].map(|to| (v, to)));
                let above = (value..32).find_map(|v| seen[v].map(|to| (v, to)));
                match (below, above) {
                    (Some((_, to)), _) if seen[value].is_some() => to,
                    (Some((v0, t0)), Some((v1, t1))) => {
                        let (v0, t0, v1, t1) = (v0 as i32, t0 as i32, v1 as i32, t1 as i32);
                        (t0 + (t1 - t0) * (value as i32 - v0) / (v1 - v0)) as u8
                    }
                    (Some((v, to)), None) | (None, Some((v, to))) => {
                        (value as i32 + to as i32 - v as i32).clamp(0, 31) as u8
                    }
                    (None, None) => value as u8,
                }
            })
        })))
    }

    /// The pixel at column `x` (negative left of the captured columns, 256
    /// and on right of them) of line `y`, if it is beside them.
    pub fn side(&self, x: i32, y: usize) -> Option<u16> {
        let extra = self.extra as i32;
        let column = if (-extra..0).contains(&x) {
            x + extra
        } else if (256..256 + extra).contains(&x) {
            x - 256 + extra
        } else {
            return None;
        };
        self.sides
            .get(y * 2 * self.extra + column as usize)
            .copied()
    }
}

impl Vram {
    pub fn new() -> Self {
        let mut vram = Self {
            banks: vec![0; TOTAL],
            control: [0; 9],
            maps: std::array::from_fn(|_| Vec::new()),
            wide_captures: Vec::new(),
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
    fn wide_captures_follow_the_captured_pixels_as_the_game_changes_them() {
        let mut vram = Vram::new();
        let written: Vec<u16> = (0..256 * 4).map(|i| 0x8000 | (i % 32) as u16).collect();
        let capture = WideCapture {
            start: 0x2_0000,
            written: written.clone(),
            sides: (0..4 * 2 * 43).map(|i| (i % 32) as u16 | 0x8000).collect(),
            extra: 43,
        };
        assert_eq!(capture.height(), 4);
        assert_eq!(capture.side(-43, 0), Some(0x8000));
        assert_eq!(capture.side(-1, 0), Some(0x8000 | 10));
        assert_eq!(capture.side(256, 1), Some(0x8000 | ((86 + 43) as u16 % 32)));
        assert_eq!(capture.side(0, 0), None);
        assert_eq!(capture.side(299, 0), None);
        let store = |vram: &mut Vram, colors: &[u16]| {
            for (i, color) in colors.iter().enumerate() {
                let at = 0x2_0000 + i * 2;
                vram.banks[at..at + 2].copy_from_slice(&color.to_le_bytes());
            }
        };
        // Unchanged, and halved in place, as a battle starting does.
        store(&mut vram, &written);
        assert_eq!(capture.changes(&vram).unwrap().apply(0x801F), 0x801F);
        let halved: Vec<u16> = written.iter().map(|c| 0x8000 | (c & 31) >> 1).collect();
        store(&mut vram, &halved);
        let changes = capture.changes(&vram).unwrap();
        assert_eq!(changes.apply(0x801E), 0x800F);
        // Green never appeared in the capture: it follows its neighbours.
        assert_eq!(changes.apply(0x03E0), 0x03E0);
        // Something else drawn there is not the capture.
        let other: Vec<u16> = (0..256 * 4).map(|i| (i / 32 % 32) as u16).collect();
        store(&mut vram, &other);
        assert_eq!(capture.changes(&vram), None);
    }

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
