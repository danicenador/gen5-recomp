//! The two 2D graphics engines. Each frame is rendered at the start of
//! vertical blank from the registers, palettes, OAM and VRAM as they are
//! then; mid-frame register changes (scroll effects from HBlank DMA) are not
//! seen. Engine A's BG0 can use the geometry engine's rendered 3D layer.
//! Behaviour per GBATEK.

use super::gpu3d::Pixel;
use super::io::Io;
use super::memory::Memory;
use super::vram::{Region, Vram};
use rayon::prelude::*;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

const POWCNT1: u32 = 0x0400_0304;
const LAYER_OBJ: usize = 4;
const LAYER_BACKDROP: usize = 5;
/// Lines per parallel task when drawing a screen.
const LINES_PER_TASK: usize = 8;

/// Both screens as RGBA8.
#[derive(Clone)]
pub struct Frame {
    pub top: Vec<u8>,
    pub bottom: Vec<u8>,
}

impl Frame {
    pub fn blank() -> Self {
        Self {
            top: vec![0; WIDTH * HEIGHT * 4],
            bottom: vec![0; WIDTH * HEIGHT * 4],
        }
    }
}

pub fn render(io: &Io, memory: &Memory) -> Frame {
    render_with(io, memory, &mut FrameCache::default())
}

/// What display capture and the screens of one frame share: the 3D layer
/// and engine A's lines. Nothing they read changes between the two (capture
/// only writes LCDC banks, which no engine or texture reads).
#[derive(Default)]
pub struct FrameCache {
    three_d: Option<Vec<Pixel>>,
    /// Engine A's lines before master brightness, and whether they were
    /// composed with the rendered 3D layer rather than a blank one.
    engine_a: Option<(bool, Vec<u16>)>,
}

impl FrameCache {
    fn three_d(&mut self, io: &Io, memory: &Memory) -> &[Pixel] {
        self.three_d
            .get_or_insert_with(|| io.geometry.render(io, &memory.vram))
    }

    /// Engine A's lines, composed with the 3D layer or a blank one.
    fn engine_a(&mut self, io: &Io, memory: &Memory, with_three_d: bool, dispcnt: u32) -> &[u16] {
        if self
            .engine_a
            .as_ref()
            .is_none_or(|(three_d, _)| *three_d != with_three_d)
        {
            let blank;
            let three_d = if with_three_d {
                self.three_d(io, memory)
            } else {
                blank = vec![Pixel::default(); WIDTH * HEIGHT];
                &blank
            };
            let lines = Engine::new(io, memory, false, three_d).lines(dispcnt);
            self.engine_a = Some((with_three_d, lines));
        }
        &self.engine_a.as_ref().unwrap().1
    }
}

/// `render`, sharing work with this frame's `capture_with`.
pub fn render_with(io: &Io, memory: &Memory, cache: &mut FrameCache) -> Frame {
    let power = io.get16(POWCNT1);
    let with_three_d = power & (1 << 3) != 0;
    let blank = vec![Pixel::default(); WIDTH * HEIGHT];
    let engine = |cache: &mut FrameCache, b: bool, bit: u16| {
        if power & bit == 0 {
            return vec![0; WIDTH * HEIGHT * 4];
        }
        let engine = Engine::new(io, memory, b, &blank);
        let dispcnt = engine.reg32(0);
        let colors = if !b && engine.display_mode(dispcnt) == 1 && dispcnt & 0x80 == 0 {
            cache.engine_a(io, memory, with_three_d, dispcnt).to_vec()
        } else {
            engine.colors(dispcnt)
        };
        engine.brighten(&colors)
    };
    let a = engine(cache, false, 1 << 1);
    let b = engine(cache, true, 1 << 9);
    // POWCNT1 bit 15 puts engine A on the top screen.
    if power & 0x8000 != 0 {
        Frame { top: a, bottom: b }
    } else {
        Frame { top: b, bottom: a }
    }
}

/// Complete one frame's display capture even when no screen sink is attached.
/// Capture observes engine A before master brightness and writes physical
/// LCDC banks; software can remap the result into either engine next frame.
pub fn capture(io: &mut Io, memory: &mut Memory) {
    capture_with(io, memory, &mut FrameCache::default());
}

/// `capture`, sharing work with this frame's `render_with`.
pub fn capture_with(io: &mut Io, memory: &mut Memory, cache: &mut FrameCache) {
    const CAPTURE: u32 = 0x0400_0064;
    let control = io.get32(CAPTURE);
    if control & 0x8000_0000 == 0 {
        return;
    }
    let bank = ((control >> 16) & 3) as usize;
    let bank_control = memory.vram.control[bank];
    if bank_control & 0x87 != 0x80 {
        io.set32(CAPTURE, control & !0x8000_0000);
        return;
    }
    let (width, height) =
        [(128, 128), (256, 64), (256, 128), (256, 192)][((control >> 20) & 3) as usize];
    let mode = (control >> 29) & 3;
    let mut source_a = vec![0u16; WIDTH * HEIGHT];
    if mode != 1 {
        let dispcnt = io.get32(0x0400_0000);
        if control & (1 << 24) != 0 {
            for (out, p) in source_a.iter_mut().zip(cache.three_d(io, memory)) {
                *out = p.color | if p.alpha != 0 { 0x8000 } else { 0 };
            }
        } else if dispcnt & 0x80 != 0 {
            source_a.fill(0xffff);
        } else {
            let lines = cache.engine_a(io, memory, true, dispcnt);
            for (out, &c) in source_a.iter_mut().zip(lines) {
                *out = c | 0x8000;
            }
        }
    }
    let dispcnt = io.get32(0x0400_0000);
    let read_bank = ((dispcnt >> 18) & 3) as usize * 0x20000;
    let read_offset = if (dispcnt >> 16) & 3 == 2 {
        0
    } else {
        ((control >> 26) & 3) as usize * 0x8000
    };
    let write_offset = ((control >> 18) & 3) as usize * 0x8000;
    let eva = (control & 31).min(16);
    let evb = ((control >> 8) & 31).min(16);
    let mut output = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let a = source_a[y * WIDTH + x];
            // Main-memory display FIFO capture is not implemented yet.
            let b = if control & (1 << 25) == 0 {
                memory.vram.read16(
                    Region::Lcdc,
                    read_bank + ((read_offset + (y * width + x) * 2) & 0x1ffff),
                )
            } else {
                0
            };
            output.push(match mode {
                0 => a,
                1 => b,
                _ => {
                    let fa = if a & 0x8000 != 0 { eva } else { 0 };
                    let fb = if b & 0x8000 != 0 { evb } else { 0 };
                    mix(a, b, fa, fb) | if fa != 0 || fb != 0 { 0x8000 } else { 0 }
                }
            });
        }
    }
    for (i, color) in output.into_iter().enumerate() {
        let at = bank * 0x20000 + ((write_offset + i * 2) & 0x1ffff);
        memory.vram.banks[at..at + 2].copy_from_slice(&color.to_le_bytes());
    }
    io.set32(CAPTURE, control & !0x8000_0000);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BgKind {
    Off,
    Text,
    Affine,
    Extended,
    Large,
    ThreeD,
}

#[derive(Clone, Copy)]
struct ObjPixel {
    color: u16,
    priority: u8,
    semi: bool,
}

#[derive(Clone, Copy)]
struct Layer {
    color: u16,
    layer: usize,
    semi: bool,
    three_d_alpha: Option<u8>,
}

/// One engine's view of the hardware. It holds no `Memory` (whose page
/// tables are raw pointers), so lines can be drawn on several threads.
struct Engine<'a> {
    io: &'a Io,
    vram: &'a Vram,
    palette_ram: &'a [u8],
    oam: &'a [u8],
    /// Engine B (sub) rather than A (main).
    b: bool,
    three_d: &'a [Pixel],
}

impl<'a> Engine<'a> {
    fn new(io: &'a Io, memory: &'a Memory, b: bool, three_d: &'a [Pixel]) -> Self {
        Self {
            io,
            vram: &memory.vram,
            palette_ram: &memory.palette,
            oam: &memory.oam,
            b,
            three_d,
        }
    }

    fn reg16(&self, offset: u32) -> u16 {
        self.io
            .get16(if self.b { 0x0400_1000 } else { 0x0400_0000 } + offset)
    }

    fn reg32(&self, offset: u32) -> u32 {
        u32::from(self.reg16(offset)) | (u32::from(self.reg16(offset + 2)) << 16)
    }

    fn region(&self, obj: bool) -> Region {
        match (self.b, obj) {
            (false, false) => Region::ABg,
            (true, false) => Region::BBg,
            (false, true) => Region::AObj,
            (true, true) => Region::BObj,
        }
    }

    /// Standard palette entry: BG entries 0-255, OBJ entries 256-511.
    fn palette(&self, index: usize) -> u16 {
        let at = if self.b { 0x400 } else { 0 } + index * 2;
        u16::from_le_bytes([self.palette_ram[at], self.palette_ram[at + 1]])
    }

    fn bg_ext_palette(&self, slot: usize, index: usize) -> u16 {
        let region = if self.b {
            Region::BBgExtPalette
        } else {
            Region::ABgExtPalette
        };
        self.vram.read16(region, slot * 0x2000 + index * 2)
    }

    fn obj_ext_palette(&self, index: usize) -> u16 {
        let region = if self.b {
            Region::BObjExtPalette
        } else {
            Region::AObjExtPalette
        };
        self.vram.read16(region, index * 2)
    }

    fn oam16(&self, offset: usize) -> u16 {
        let at = if self.b { 0x400 } else { 0 } + offset;
        u16::from_le_bytes([self.oam[at], self.oam[at + 1]])
    }

    fn display_mode(&self, dispcnt: u32) -> u32 {
        if self.b {
            (dispcnt >> 16) & 1
        } else {
            (dispcnt >> 16) & 3
        }
    }

    /// The screen's colors before master brightness.
    fn colors(&self, dispcnt: u32) -> Vec<u16> {
        let mut colors = vec![0x7FFF_u16; WIDTH * HEIGHT];
        match self.display_mode(dispcnt) {
            // Display off: white.
            0 => {}
            1 if dispcnt & 0x80 != 0 => {}
            1 => colors = self.lines(dispcnt),
            // A VRAM bank in LCDC mode, shown directly.
            2 => {
                let base = ((dispcnt >> 18) & 3) as usize * 0x2_0000;
                for (index, color) in colors.iter_mut().enumerate() {
                    *color = self.vram.read16(Region::Lcdc, base + index * 2);
                }
            }
            // Main memory display (fed by DMA) is not supported.
            _ => colors.fill(0),
        }
        colors
    }

    /// Every line of the backgrounds and sprites. Lines are independent,
    /// so they are drawn in parallel.
    fn lines(&self, dispcnt: u32) -> Vec<u16> {
        let mut colors = vec![0_u16; WIDTH * HEIGHT];
        colors
            .par_chunks_mut(WIDTH * LINES_PER_TASK)
            .enumerate()
            .for_each(|(index, rows)| {
                for (offset, row) in rows.chunks_mut(WIDTH).enumerate() {
                    self.render_line(dispcnt, index * LINES_PER_TASK + offset, row);
                }
            });
        colors
    }

    /// RGBA8 after master brightness.
    fn brighten(&self, colors: &[u16]) -> Vec<u8> {
        let bright = self.reg16(0x6C);
        let factor = u32::from(bright & 0x1F).min(16);
        let mode = bright >> 14;
        let mut rgba = vec![0xFF; WIDTH * HEIGHT * 4];
        for (out, &color) in rgba.chunks_exact_mut(4).zip(colors) {
            let mut channels =
                [color & 0x1F, (color >> 5) & 0x1F, (color >> 10) & 0x1F].map(u32::from);
            match mode {
                1 => channels = channels.map(|c| c + (31 - c) * factor / 16),
                2 => channels = channels.map(|c| c - c * factor / 16),
                _ => {}
            }
            for (out, c) in out.iter_mut().zip(channels) {
                *out = ((c << 3) | (c >> 2)) as u8;
            }
        }
        rgba
    }

    fn bg_kind(&self, dispcnt: u32, index: usize) -> BgKind {
        let mode = dispcnt & 7;
        match (index, mode) {
            (0, _) if !self.b && (dispcnt & 8 != 0 || mode == 6) => BgKind::ThreeD,
            (0, 0..=6) => BgKind::Text,
            (1, 0..=5) => BgKind::Text,
            (2, 0 | 1 | 3) => BgKind::Text,
            (2, 2 | 4) => BgKind::Affine,
            (2, 5) => BgKind::Extended,
            (2, 6) if !self.b => BgKind::Large,
            (3, 0) => BgKind::Text,
            (3, 1 | 2) => BgKind::Affine,
            (3, 3..=5) => BgKind::Extended,
            _ => BgKind::Off,
        }
    }

    fn render_line(&self, dispcnt: u32, line: usize, out: &mut [u16]) {
        let mut bgs: [[Option<u16>; WIDTH]; 4] = [[None; WIDTH]; 4];
        let mut priorities = [0_u8; 4];
        let mut enabled = [false; 4];
        for index in 0..4 {
            if dispcnt & (0x100 << index) == 0 {
                continue;
            }
            let control = self.reg16(8 + 2 * index as u32);
            priorities[index] = (control & 3) as u8;
            enabled[index] = true;
            match self.bg_kind(dispcnt, index) {
                BgKind::Text => self.text_line(dispcnt, index, control, line, &mut bgs[index]),
                BgKind::Affine | BgKind::Extended | BgKind::Large => {
                    let kind = self.bg_kind(dispcnt, index);
                    self.affine_line(dispcnt, index, control, kind, line, &mut bgs[index]);
                }
                BgKind::ThreeD => {
                    let scroll = usize::from(self.reg16(0x10) & 0x1ff);
                    for (x, color) in bgs[index].iter_mut().enumerate() {
                        let sx = (x + scroll) & 0x1ff;
                        if sx < WIDTH {
                            let pixel = self.three_d[line * WIDTH + sx];
                            if pixel.alpha != 0 {
                                *color = Some(pixel.color);
                            }
                        }
                    }
                }
                BgKind::Off => enabled[index] = false,
            }
        }
        let mut objs: [Option<ObjPixel>; WIDTH] = [None; WIDTH];
        let mut obj_window = [false; WIDTH];
        if dispcnt & 0x1000 != 0 {
            self.obj_line(dispcnt, line, &mut objs, &mut obj_window);
        }

        let window_masks = self.window_masks(dispcnt, line, &obj_window);
        let blend = self.reg16(0x50);
        let alpha = self.reg16(0x52);
        let eva = u32::from(alpha & 0x1F).min(16);
        let evb = u32::from((alpha >> 8) & 0x1F).min(16);
        let evy = u32::from(self.reg16(0x54) & 0x1F).min(16);
        let backdrop = self.palette(0);

        for x in 0..WIDTH {
            let mask = window_masks[x];
            let mut top: [Option<Layer>; 2] = [None, None];
            let mut push = |layer: Layer| {
                if top[0].is_none() {
                    top[0] = Some(layer);
                } else if top[1].is_none() {
                    top[1] = Some(layer);
                }
            };
            for priority in 0..4 {
                if let Some(obj) = objs[x]
                    && obj.priority == priority
                    && mask & 0x10 != 0
                {
                    push(Layer {
                        color: obj.color,
                        layer: LAYER_OBJ,
                        semi: obj.semi,
                        three_d_alpha: None,
                    });
                }
                for index in 0..4 {
                    if enabled[index]
                        && priorities[index] == priority
                        && mask & (1 << index) != 0
                        && let Some(color) = bgs[index][x]
                    {
                        push(Layer {
                            color,
                            layer: index,
                            semi: false,
                            three_d_alpha: if index == 0
                                && self.bg_kind(dispcnt, 0) == BgKind::ThreeD
                            {
                                let sx = (x + usize::from(self.reg16(0x10) & 0x1ff)) & 0x1ff;
                                Some(self.three_d[line * WIDTH + sx].alpha)
                            } else {
                                None
                            },
                        });
                    }
                }
            }
            let backdrop_layer = Layer {
                color: backdrop,
                layer: LAYER_BACKDROP,
                semi: false,
                three_d_alpha: None,
            };
            let first = top[0].unwrap_or(backdrop_layer);
            let second = top[1].unwrap_or(backdrop_layer);
            let is_second = blend & (0x100 << second.layer) != 0;
            let is_first = blend & (1 << first.layer) != 0;
            out[x] = if let Some(alpha) = first.three_d_alpha
                && alpha < 31
                && is_second
                && mask & 0x20 != 0
            {
                let a = u32::from(alpha) + 1;
                let c1 = first.color;
                let c2 = second.color;
                let mut result = 0;
                for shift in [0, 5, 10] {
                    let c = (u32::from((c1 >> shift) & 31) * a
                        + u32::from((c2 >> shift) & 31) * (32 - a))
                        / 32;
                    result |= (c as u16) << shift;
                }
                result
            } else if first.semi && is_second && top[0].is_some() {
                mix(first.color, second.color, eva, evb)
            } else if mask & 0x20 == 0 || !is_first {
                first.color
            } else {
                match (blend >> 6) & 3 {
                    1 if is_second && top[0].is_some() => mix(first.color, second.color, eva, evb),
                    2 => map_channels(first.color, |c| c + (31 - c) * evy / 16),
                    3 => map_channels(first.color, |c| c - c * evy / 16),
                    _ => first.color,
                }
            };
        }
    }

    /// Which layers (bits 0-4) and effects (bit 5) each pixel shows.
    fn window_masks(&self, dispcnt: u32, line: usize, obj_window: &[bool; WIDTH]) -> [u16; WIDTH] {
        if dispcnt & 0xE000 == 0 {
            return [0x3F; WIDTH];
        }
        let inside = |low: usize, high: usize, value: usize| {
            if low <= high {
                (low..high).contains(&value)
            } else {
                value >= low || value < high
            }
        };
        let win_in = self.reg16(0x48);
        let win_out = self.reg16(0x4A);
        let mut masks = [win_out & 0x3F; WIDTH];
        for (x, mask) in masks.iter_mut().enumerate() {
            if dispcnt & 0x8000 != 0 && obj_window[x] {
                *mask = (win_out >> 8) & 0x3F;
            }
        }
        for window in (0..2).rev() {
            if dispcnt & (0x2000 << window) == 0 {
                continue;
            }
            let h = self.reg16(0x40 + 2 * window as u32);
            let v = self.reg16(0x44 + 2 * window as u32);
            let (x1, x2) = (usize::from(h >> 8), usize::from(h & 0xFF));
            let (y1, y2) = (usize::from(v >> 8), usize::from(v & 0xFF));
            if !inside(y1, y2, line) {
                continue;
            }
            for (x, mask) in masks.iter_mut().enumerate() {
                if inside(x1, x2, x) {
                    *mask = (win_in >> (8 * window)) & 0x3F;
                }
            }
        }
        masks
    }

    fn bases(&self, dispcnt: u32, control: u16) -> (usize, usize) {
        let mut char_base = usize::from((control >> 2) & 0xF) * 0x4000;
        let mut screen_base = usize::from((control >> 8) & 0x1F) * 0x800;
        if !self.b {
            char_base += ((dispcnt >> 24) & 7) as usize * 0x1_0000;
            screen_base += ((dispcnt >> 27) & 7) as usize * 0x1_0000;
        }
        (char_base, screen_base)
    }

    fn text_line(
        &self,
        dispcnt: u32,
        index: usize,
        control: u16,
        line: usize,
        out: &mut [Option<u16>; WIDTH],
    ) {
        let vram = self.vram;
        let region = self.region(false);
        let hofs = usize::from(self.reg16(0x10 + 4 * index as u32) & 0x1FF);
        let vofs = usize::from(self.reg16(0x12 + 4 * index as u32) & 0x1FF);
        let (char_base, screen_base) = self.bases(dispcnt, control);
        let size = control >> 14;
        let width = if size & 1 != 0 { 512 } else { 256 };
        let height = if size & 2 != 0 { 512 } else { 256 };
        let y = (line + vofs) % height;
        let color256 = control & 0x80 != 0;
        let extended = color256 && dispcnt & (1 << 30) != 0;
        let slot = if index < 2 && control & 0x2000 != 0 {
            index + 2
        } else {
            index
        };
        for (x, pixel) in out.iter_mut().enumerate() {
            let sx = (x + hofs) % width;
            let block = sx / 256 + if width == 512 { (y / 256) * 2 } else { y / 256 };
            let entry_at =
                screen_base + block * 0x800 + ((y % 256) / 8) * 64 + ((sx % 256) / 8) * 2;
            let entry = vram.read16(region, entry_at);
            let tile = usize::from(entry & 0x3FF);
            let mut tx = sx % 8;
            let mut ty = y % 8;
            if entry & 0x400 != 0 {
                tx = 7 - tx;
            }
            if entry & 0x800 != 0 {
                ty = 7 - ty;
            }
            let bank = usize::from(entry >> 12);
            *pixel = if color256 {
                let color = usize::from(vram.read8(region, char_base + tile * 64 + ty * 8 + tx));
                (color != 0).then(|| {
                    if extended {
                        self.bg_ext_palette(slot, bank * 256 + color)
                    } else {
                        self.palette(color)
                    }
                })
            } else {
                let byte = vram.read8(region, char_base + tile * 32 + ty * 4 + tx / 2);
                let color = usize::from(if tx & 1 != 0 { byte >> 4 } else { byte & 0xF });
                (color != 0).then(|| self.palette(bank * 16 + color))
            };
        }
    }

    fn affine_line(
        &self,
        dispcnt: u32,
        index: usize,
        control: u16,
        kind: BgKind,
        line: usize,
        out: &mut [Option<u16>; WIDTH],
    ) {
        let vram = self.vram;
        let region = self.region(false);
        let params = 0x20 + 0x10 * (index as u32 - 2);
        let pa = i32::from(self.reg16(params) as i16);
        let pb = i32::from(self.reg16(params + 2) as i16);
        let pc = i32::from(self.reg16(params + 4) as i16);
        let pd = i32::from(self.reg16(params + 6) as i16);
        let reference = |offset: u32| ((self.reg32(params + offset) << 4) as i32) >> 4;
        let start_x = reference(8) + pb * line as i32;
        let start_y = reference(12) + pd * line as i32;
        let size = usize::from(control >> 14);
        let wrap = control & 0x2000 != 0;
        let (char_base, screen_base) = self.bases(dispcnt, control);
        let bitmap = kind == BgKind::Large || (kind == BgKind::Extended && control & 0x80 != 0);
        let (width, height): (usize, usize) = match kind {
            BgKind::Large if size & 1 == 0 => (512, 1024),
            BgKind::Large => (1024, 512),
            _ if bitmap => [(128, 128), (256, 256), (512, 256), (512, 512)][size],
            _ => (128 << size, 128 << size),
        };
        let bitmap_base = usize::from((control >> 8) & 0x1F) * 0x4000;
        for (x, pixel) in out.iter_mut().enumerate() {
            let mut px = (start_x + pa * x as i32) >> 8;
            let mut py = (start_y + pc * x as i32) >> 8;
            if wrap {
                px = px.rem_euclid(width as i32);
                py = py.rem_euclid(height as i32);
            } else if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
                *pixel = None;
                continue;
            }
            let (px, py) = (px as usize, py as usize);
            *pixel = if kind == BgKind::Large {
                let color = usize::from(vram.read8(region, py * width + px));
                (color != 0).then(|| self.palette(color))
            } else if bitmap && control & 4 != 0 {
                let color = vram.read16(region, bitmap_base + (py * width + px) * 2);
                (color & 0x8000 != 0).then_some(color & 0x7FFF)
            } else if bitmap {
                let color = usize::from(vram.read8(region, bitmap_base + py * width + px));
                (color != 0).then(|| self.palette(color))
            } else if kind == BgKind::Affine {
                let tile =
                    usize::from(vram.read8(region, screen_base + (py / 8) * (width / 8) + px / 8));
                let color =
                    usize::from(vram.read8(region, char_base + tile * 64 + (py % 8) * 8 + px % 8));
                (color != 0).then(|| self.palette(color))
            } else {
                // Extended rotation/scaling: text-style entries, 256 colours.
                let entry_at = screen_base + ((py / 8) * (width / 8) + px / 8) * 2;
                let entry = vram.read16(region, entry_at);
                let tile = usize::from(entry & 0x3FF);
                let mut tx = px % 8;
                let mut ty = py % 8;
                if entry & 0x400 != 0 {
                    tx = 7 - tx;
                }
                if entry & 0x800 != 0 {
                    ty = 7 - ty;
                }
                let color = usize::from(vram.read8(region, char_base + tile * 64 + ty * 8 + tx));
                (color != 0).then(|| {
                    if dispcnt & (1 << 30) != 0 {
                        self.bg_ext_palette(index, usize::from(entry >> 12) * 256 + color)
                    } else {
                        self.palette(color)
                    }
                })
            };
        }
    }

    fn obj_line(
        &self,
        dispcnt: u32,
        line: usize,
        out: &mut [Option<ObjPixel>; WIDTH],
        window: &mut [bool; WIDTH],
    ) {
        const SIZES: [[(i32, i32); 4]; 3] = [
            [(8, 8), (16, 16), (32, 32), (64, 64)],
            [(16, 8), (32, 8), (32, 16), (64, 32)],
            [(8, 16), (8, 32), (16, 32), (32, 64)],
        ];
        let vram = self.vram;
        let region = self.region(true);
        for index in 0..128 {
            let attr0 = self.oam16(index * 8);
            let attr1 = self.oam16(index * 8 + 2);
            let attr2 = self.oam16(index * 8 + 4);
            let affine = attr0 & 0x100 != 0;
            if !affine && attr0 & 0x200 != 0 {
                continue;
            }
            let shape = usize::from(attr0 >> 14);
            if shape == 3 {
                continue;
            }
            let (width, height) = SIZES[shape][usize::from(attr1 >> 14)];
            let double = affine && attr0 & 0x200 != 0;
            let (box_w, box_h) = if double {
                (width * 2, height * 2)
            } else {
                (width, height)
            };
            let y = i32::from(attr0 & 0xFF);
            let ly = (line as i32 - y) & 0xFF;
            if ly >= box_h {
                continue;
            }
            let mut x = i32::from(attr1 & 0x1FF);
            if x >= 256 {
                x -= 512;
            }
            let mode = (attr0 >> 10) & 3;
            let color256 = attr0 & 0x2000 != 0;
            let tile = usize::from(attr2 & 0x3FF);
            let priority = ((attr2 >> 10) & 3) as u8;
            let bank = usize::from(attr2 >> 12);
            let params = if affine {
                let group = usize::from((attr1 >> 9) & 0x1F) * 32;
                [6, 14, 22, 30].map(|offset| i32::from(self.oam16(group + offset) as i16))
            } else {
                [0x100, 0, 0, 0x100]
            };
            for bx in 0..box_w {
                let sx = x + bx;
                if !(0..WIDTH as i32).contains(&sx) {
                    continue;
                }
                let (mut tx, mut ty) = if affine {
                    let (dx, dy) = (bx - box_w / 2, ly - box_h / 2);
                    (
                        ((params[0] * dx + params[1] * dy) >> 8) + width / 2,
                        ((params[2] * dx + params[3] * dy) >> 8) + height / 2,
                    )
                } else {
                    (bx, ly)
                };
                if tx < 0 || ty < 0 || tx >= width || ty >= height {
                    continue;
                }
                if !affine {
                    if attr1 & 0x1000 != 0 {
                        tx = width - 1 - tx;
                    }
                    if attr1 & 0x2000 != 0 {
                        ty = height - 1 - ty;
                    }
                }
                let (tx, ty, w) = (tx as usize, ty as usize, width as usize);
                let color = if mode == 3 {
                    let at = if dispcnt & (1 << 6) != 0 {
                        tile * if dispcnt & (1 << 22) != 0 { 256 } else { 128 } + (ty * w + tx) * 2
                    } else if dispcnt & (1 << 5) != 0 {
                        ((tile >> 5) * 8 + ty) * 512 + ((tile & 0x1F) * 8 + tx) * 2
                    } else {
                        ((tile >> 4) * 8 + ty) * 256 + ((tile & 0xF) * 8 + tx) * 2
                    };
                    let color = vram.read16(region, at);
                    (color & 0x8000 != 0 && bank != 0).then_some(color & 0x7FFF)
                } else {
                    let one_d = dispcnt & (1 << 4) != 0;
                    let (base, row_stride) = if one_d {
                        let tile_bytes = if color256 { 64 } else { 32 };
                        (tile * (32 << ((dispcnt >> 20) & 3)), (w / 8) * tile_bytes)
                    } else {
                        (tile * 32, 1024)
                    };
                    let tile_bytes = if color256 { 64 } else { 32 };
                    let at = base + (ty / 8) * row_stride + (tx / 8) * tile_bytes;
                    if color256 {
                        let color = usize::from(vram.read8(region, at + (ty % 8) * 8 + tx % 8));
                        (color != 0).then(|| {
                            if dispcnt & (1 << 31) != 0 {
                                self.obj_ext_palette(bank * 256 + color)
                            } else {
                                self.palette(256 + color)
                            }
                        })
                    } else {
                        let byte = vram.read8(region, at + (ty % 8) * 4 + (tx % 8) / 2);
                        let color = usize::from(if tx & 1 != 0 { byte >> 4 } else { byte & 0xF });
                        (color != 0).then(|| self.palette(256 + bank * 16 + color))
                    }
                };
                let Some(color) = color else {
                    continue;
                };
                let sx = sx as usize;
                if mode == 2 {
                    window[sx] = true;
                    continue;
                }
                if out[sx].is_none_or(|pixel| priority < pixel.priority) {
                    out[sx] = Some(ObjPixel {
                        color,
                        priority,
                        semi: mode == 1,
                    });
                }
            }
        }
    }
}

fn map_channels(color: u16, f: impl Fn(u32) -> u32) -> u16 {
    let channel = |shift: u16| (f(u32::from((color >> shift) & 0x1F)).min(31) as u16) << shift;
    channel(0) | channel(5) | channel(10)
}

fn mix(first: u16, second: u16, eva: u32, evb: u32) -> u16 {
    let channel = |shift: u16| {
        let a = u32::from((first >> shift) & 0x1F);
        let b = u32::from((second >> shift) & 0x1F);
        (((a * eva + b * evb) / 16).min(31) as u16) << shift
    };
    channel(0) | channel(5) | channel(10)
}

/// Encode RGBA8 pixels as an uncompressed PNG.
pub fn png(width: usize, height: usize, rgba: &[u8]) -> Vec<u8> {
    use crate::formats::nds::Crc32;
    let mut raw = Vec::with_capacity((width * 4 + 1) * height);
    for row in rgba.chunks(width * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib stream of stored deflate blocks.
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xFFFF).collect();
    for (index, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(index + 1 == blocks.len()));
        let length = block.len() as u16;
        zlib.extend_from_slice(&length.to_le_bytes());
        zlib.extend_from_slice(&(!length).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = Crc32::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.finish().to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &zlib);
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_background_draws_a_tile() {
        let mut io = Io::new();
        let mut memory = Memory::new();
        // Engine A: mode 0, BG0 on; bank A as BG VRAM; A on the top screen.
        io.set32(0x0400_0000, 0x0001_0100);
        io.set16(POWCNT1, 0x8003);
        memory.vram.control[0] = 0x81;
        memory.vram.update();
        // BG0: char base 0, screen base 1 (0x800). Tile 1 is colour 1.
        io.set16(0x0400_0008, 1 << 8);
        let banks = &mut memory.vram.banks;
        banks[32..64].fill(0x11);
        banks[0x800] = 1;
        memory.palette[2] = 0x1F;
        let frame = render(&io, &memory);
        assert_eq!(&frame.top[..4], &[0xFF, 0, 0, 0xFF]);
        // Next tile along uses tile 0: transparent, so the backdrop (black).
        assert_eq!(&frame.top[8 * 4..8 * 4 + 4], &[0, 0, 0, 0xFF]);
        // Engine B is off on the bottom screen: black.
        assert_eq!(&frame.bottom[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn png_has_signature_and_chunks() {
        let data = png(2, 2, &[0xFF; 16]);
        assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&data[12..16], b"IHDR");
        assert!(data.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }
}
