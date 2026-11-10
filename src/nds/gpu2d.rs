//! The two 2D graphics engines. Each frame is rendered at the start of
//! vertical blank from the registers, palettes, OAM and VRAM as they are
//! then; mid-frame register changes (scroll effects from HBlank DMA) are not
//! seen. Engine A's BG0 can use the geometry engine's rendered 3D layer.
//! Behaviour per GBATEK.

use super::gpu3d::Pixel;
pub use super::gpu3d::WIDE_EXTRA;
use super::io::Io;
use super::memory::Memory;
use super::vram::{ColorMap, Region, Vram, WideCapture};
use rayon::prelude::*;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;

const POWCNT1: u32 = 0x0400_0304;
const LAYER_OBJ: usize = 4;
const LAYER_BACKDROP: usize = 5;
/// Lines per parallel task when drawing a screen.
const LINES_PER_TASK: usize = 8;

/// Both screens as RGBA8, or, for a GPU renderer at a higher resolution,
/// what it needs to draw them (`top` and `bottom` are then empty).
#[derive(Clone)]
pub struct Frame {
    pub top: Vec<u8>,
    pub bottom: Vec<u8>,
    pub scaled: Option<Box<ScaledFrame>>,
}

impl Frame {
    pub fn blank() -> Self {
        Self {
            top: vec![0; WIDTH * HEIGHT * 4],
            bottom: vec![0; WIDTH * HEIGHT * 4],
            scaled: None,
        }
    }
}

/// A frame for a GPU renderer: both screens composed at native resolution
/// with the 3D layer left open, and the 3D list to draw at any resolution.
/// 2D layers scale by pixel repetition; only the 3D layer gains detail.
///
/// In widescreen (`extra` is `WIDE_EXTRA`) the top screen is `extra`
/// columns wider on each side. Backgrounds and sprites stay in the middle
/// 256 columns; where the screen shows 3D, the extra ones show the 3D layer
/// with the same blending and brightness, over what is under the 3D at the
/// end of the line (`side_of`), and black elsewhere. The bottom screen
/// keeps its 256 columns, centered.
#[derive(Clone)]
pub struct ScaledFrame {
    /// `(256 + 2 * extra)` x 384 `Composed::pack` pixels, top screen
    /// first. The bottom screen's extra columns are never shown.
    pub pixels: Vec<[u32; 4]>,
    /// Columns added on each side of the top screen: 0 or `WIDE_EXTRA`.
    pub extra: usize,
    /// MASTER_BRIGHT of the top and bottom screens.
    pub brightness: [u16; 2],
    pub three_d: Option<ThreeDLayer>,
}

/// Engine A's 3D layer: the screen it is on (0 top, 1 bottom), its BG0
/// horizontal scroll, and the list.
#[derive(Clone)]
pub struct ThreeDLayer {
    pub screen: usize,
    pub scroll: u32,
    pub scene: super::gpu3d::Scene,
}

/// Both screens for a GPU renderer, the top one widened when the geometry
/// engine is in widescreen (`Geometry::wide`).
pub fn render_scaled(io: &Io, memory: &Memory) -> ScaledFrame {
    let power = io.get16(POWCNT1);
    let blank = vec![Pixel::default(); WIDTH * HEIGHT];
    let mut three_d = None;
    // Each engine's screen, and what its widescreen columns show.
    let wide = io.geometry.wide;
    type Screen = (Vec<Composed>, Composed, u16, Option<Vec<Composed>>);
    let mut engine = |b: bool, bit: u16| -> Screen {
        if power & bit == 0 {
            return (
                vec![Composed::fixed(0); WIDTH * HEIGHT],
                Composed::fixed(0),
                0,
                None,
            );
        }
        let engine = Engine::new(io, memory, b, &blank);
        let dispcnt = engine.reg32(0);
        let brightness = engine.reg16(0x6C);
        if b || engine.display_mode(dispcnt) != 1 || dispcnt & 0x80 != 0 {
            let colors = engine
                .colors(dispcnt)
                .into_iter()
                .map(Composed::fixed)
                .collect();
            return (colors, Composed::fixed(0), brightness, None);
        }
        let mut composed = vec![Composed::default(); WIDTH * HEIGHT];
        let scroll = std::sync::atomic::AtomicUsize::new(0);
        composed
            .par_chunks_mut(WIDTH * LINES_PER_TASK)
            .enumerate()
            .for_each(|(index, rows)| {
                for (offset, row) in rows.chunks_mut(WIDTH).enumerate() {
                    let line = index * LINES_PER_TASK + offset;
                    let s = engine.compose_line(dispcnt, line, row);
                    scroll.store(s, std::sync::atomic::Ordering::Relaxed);
                }
            });
        let uses_three_d = dispcnt & 0x100 != 0 && engine.bg_kind(dispcnt, 0) == BgKind::ThreeD;
        if power & (1 << 3) != 0 && uses_three_d {
            three_d = Some((
                scroll.into_inner() as u32,
                io.geometry.scene(io, &memory.vram),
            ));
        }
        // Captured screens shown as bitmaps fill the sides line by line.
        let captured = wide
            .then(|| engine.capture_sides(dispcnt, &memory.vram))
            .flatten();
        (composed, engine.side(dispcnt), brightness, captured)
    };
    let (a, a_side, a_bright, a_captured) = engine(false, 1 << 1);
    let (b, b_side, b_bright, _) = engine(true, 1 << 9);
    // POWCNT1 bit 15 puts engine A on the top screen.
    let a_on_top = power & 0x8000 != 0;
    let (top, side, bottom, brightness, captured) = if a_on_top {
        (a, a_side, b, [a_bright, b_bright], a_captured)
    } else {
        (b, b_side, a, [b_bright, a_bright], None)
    };
    let extra = if io.geometry.wide { WIDE_EXTRA } else { 0 };
    let mut pixels = Vec::with_capacity((WIDTH + 2 * extra) * 2 * HEIGHT);
    for (screen, side, captured) in [
        (&top, side, captured.as_deref()),
        (&bottom, Composed::fixed(0), None),
    ] {
        let cover = (extra > 0)
            .then(|| widescreen_cover(screen, side))
            .flatten();
        for (line, row) in screen.chunks(WIDTH).enumerate() {
            if let Some(cover) = cover {
                pixels.extend(std::iter::repeat_n(cover.pack(), extra));
                pixels.extend(row.iter().map(Composed::pack));
                pixels.extend(std::iter::repeat_n(cover.pack(), extra));
                continue;
            }
            if let Some(captured) = captured.filter(|_| extra > 0) {
                let sides = &captured[line * 2 * WIDE_EXTRA..][..2 * WIDE_EXTRA];
                pixels.extend(sides[..WIDE_EXTRA].iter().map(Composed::pack));
                pixels.extend(row.iter().map(Composed::pack));
                pixels.extend(sides[WIDE_EXTRA..].iter().map(Composed::pack));
                continue;
            }
            // Each side mirrors the line's end: column -1 - i shows
            // column i, column 256 + i column 255 - i.
            pixels.extend(
                (0..extra)
                    .rev()
                    .map(|i| side_of(side, row[i], row[0]).pack()),
            );
            pixels.extend(row.iter().map(Composed::pack));
            pixels.extend(
                (0..extra).map(|i| side_of(side, row[WIDTH - 1 - i], row[WIDTH - 1]).pack()),
            );
        }
    }
    ScaledFrame {
        pixels,
        extra,
        brightness,
        three_d: three_d.map(|(scroll, scene)| ThreeDLayer {
            screen: usize::from(!a_on_top),
            scroll,
            scene,
        }),
    }
}

/// What a column of widescreen's extra columns shows for `mirrored`, the
/// screen's pixel as far from that end of the line on the other side of
/// it. Where the screen shows 3D (`side` takes the 3D layer) and the 3D
/// layer is on top there, the 2D layers under it are mirrored into the
/// extra columns, so a background behind the 3D (Juniper's gradient in the
/// introduction, the rival's backdrop) carries on instead of the backdrop
/// color. Layers in front of 3D stay inside the DS screen, including
/// translucent dialogue boxes and their text. Where 2D is in front at the mirrored
/// column (a text box near the end of the line), the line's last pixel
/// `outer` carries on instead, if it shows the 3D layer; failing that, they
/// show `side`.
fn side_of(side: Composed, mirrored: Composed, outer: Composed) -> Composed {
    let shows_three_d = |pixel: &Composed| matches!(pixel.with, With::First { .. });
    if !matches!(side.with, With::First { .. }) {
        side
    } else if shows_three_d(&mirrored) {
        mirrored
    } else if shows_three_d(&outer) {
        outer
    } else {
        side
    }
}

/// A uniform foreground covering the entire screen is a screen effect (the
/// season banner's black sheet, for example). Carry its color/blend into the
/// margins without reflecting any lettering. Requiring the whole perimeter
/// excludes dialogue boxes, HUDs and partial overlays.
fn widescreen_cover(screen: &[Composed], side: Composed) -> Option<Composed> {
    if !matches!(side.with, With::First { .. }) || screen.len() != WIDTH * HEIGHT {
        return None;
    }
    let cover = screen[0];
    if matches!(cover.with, With::First { .. }) {
        return None;
    }
    let same =
        |pixel: &Composed| pixel.with == cover.with && pixel.coefficients == cover.coefficients;
    let horizontal = screen[..WIDTH]
        .iter()
        .chain(&screen[(HEIGHT - 1) * WIDTH..]);
    let vertical = screen
        .chunks_exact(WIDTH)
        .flat_map(|row| [&row[0], &row[WIDTH - 1]]);
    horizontal.chain(vertical).all(same).then_some(cover)
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
        Frame {
            top: a,
            bottom: b,
            scaled: None,
        }
    } else {
        Frame {
            top: b,
            bottom: a,
            scaled: None,
        }
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
    let start = bank * 0x20000 + write_offset;
    memory
        .vram
        .wide_captures
        .retain(|wide| wide.start / 0x20000 != bank);
    let sides = (io.geometry.wide
        && mode == 0
        && width == WIDTH
        && write_offset + width * height * 2 <= 0x20000)
        .then(|| capture_sides(io, memory, control & (1 << 24) != 0, height));
    for (i, color) in output.iter().enumerate() {
        let at = bank * 0x20000 + ((write_offset + i * 2) & 0x1ffff);
        memory.vram.banks[at..at + 2].copy_from_slice(&color.to_le_bytes());
    }
    if let Some(sides) = sides {
        memory.vram.wide_captures.push(WideCapture {
            start,
            written: output,
            sides,
            extra: WIDE_EXTRA,
        });
    }
    io.set32(CAPTURE, control & !0x8000_0000);
}

/// What widescreen showed beside the screen as display capture source A
/// saw it: the 3D layer alone (`three_d_only`) or engine A's picture, per
/// line `WIDE_EXTRA` pixels left of the screen and then right of it, with
/// bit 15 set where opaque.
fn capture_sides(io: &Io, memory: &Memory, three_d_only: bool, height: usize) -> Vec<u16> {
    let columns = WIDTH + 2 * WIDE_EXTRA;
    let dispcnt = io.get32(0x0400_0000);
    let shows_three_d = three_d_only || io.get16(POWCNT1) & (1 << 3) != 0;
    let wide = if shows_three_d {
        io.geometry.render_wide(io, &memory.vram)
    } else {
        vec![Pixel::default(); columns * HEIGHT]
    };
    let at = |y: usize, x: usize| wide[y * columns + x];
    let mut sides = Vec::with_capacity(height * 2 * WIDE_EXTRA);
    if three_d_only {
        for y in 0..height {
            for x in (0..WIDE_EXTRA).chain(WIDE_EXTRA + WIDTH..columns) {
                let pixel = at(y, x);
                sides.push(pixel.color | if pixel.alpha != 0 { 0x8000 } else { 0 });
            }
        }
        return sides;
    }
    if dispcnt & 0x80 != 0 {
        return vec![0xFFFF; height * 2 * WIDE_EXTRA];
    }
    // Engine A's picture: its sides as the screen showed them, with the
    // wide 3D layer.
    let blank = vec![Pixel::default(); WIDTH * HEIGHT];
    let engine = Engine::new(io, memory, false, &blank);
    let side = engine.side(dispcnt);
    let captured = engine.capture_sides(dispcnt, &memory.vram);
    let mut screen = vec![Composed::default(); WIDTH * HEIGHT];
    screen
        .par_chunks_mut(WIDTH)
        .enumerate()
        .for_each(|(y, row)| {
            engine.compose_line(dispcnt, y, row);
        });
    let cover = widescreen_cover(&screen, side);
    for (y, row) in screen.chunks_exact(WIDTH).take(height).enumerate() {
        for x in (0..WIDE_EXTRA).chain(WIDE_EXTRA + WIDTH..columns) {
            let column = if x < WIDE_EXTRA { x } else { x - WIDTH };
            let composed = match (cover, &captured) {
                (Some(cover), _) => cover,
                (_, Some(pixels)) => pixels[y * 2 * WIDE_EXTRA + column],
                // Mirrored, as `render_scaled` shows them.
                (_, None) if x < WIDE_EXTRA => side_of(side, row[WIDE_EXTRA - 1 - x], row[0]),
                (_, None) => side_of(
                    side,
                    row[WIDTH - 1 - (x - WIDE_EXTRA - WIDTH)],
                    row[WIDTH - 1],
                ),
            };
            let pixel = at(y, x);
            sides.push(composed.resolve((pixel.alpha != 0).then_some(pixel)) | 0x8000);
        }
    }
    sides
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
    /// Engine A's BG0 showing the 3D layer; its color is the 3D pixel's.
    three_d: bool,
}

/// BLDALPHA's EVA and EVB and BLDY's EVY, each at most 16.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Coefficients {
    pub eva: u8,
    pub evb: u8,
    pub evy: u8,
}

/// What a special effect does to the 3D layer when it is the top layer and
/// does not blend with the layer below by its own alpha. Never EVA/EVB
/// alpha blending: over a second target the 3D layer always blends by its
/// own alpha instead. The values are the GPU compositor's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None = 0,
    Brighten = 2,
    Darken = 3,
}

/// How a pixel combines with the 3D pixel under it, when there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum With {
    /// The 3D layer is hidden, or below the top two layers.
    Fixed(u16),
    /// The 3D layer is on top, over `second`. A 3D pixel blends with it by
    /// its own alpha when `alpha_blend` (an opaque one then shows as it
    /// is); otherwise `effect` applies.
    First {
        second: u16,
        alpha_blend: bool,
        effect: Effect,
    },
    /// The 3D layer is second, and `first` alpha-blends with it.
    Second { first: u16 },
}

/// One pixel of a screen with the 3D layer left open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Composed {
    /// The color where the 3D layer is transparent.
    pub without: u16,
    pub with: With,
    pub coefficients: Coefficients,
}

impl Default for Composed {
    fn default() -> Self {
        Self::fixed(0)
    }
}

impl Composed {
    /// A pixel that does not depend on the 3D layer.
    pub fn fixed(color: u16) -> Self {
        Self {
            without: color,
            with: With::Fixed(color),
            coefficients: Coefficients::default(),
        }
    }

    /// The final color, given the visible 3D pixel here, if any.
    pub fn resolve(&self, three_d: Option<Pixel>) -> u16 {
        let Some(pixel) = three_d else {
            return self.without;
        };
        let Coefficients { eva, evb, evy } = self.coefficients;
        let (eva, evb, evy) = (u32::from(eva), u32::from(evb), u32::from(evy));
        match self.with {
            With::Fixed(color) => color,
            With::First {
                second,
                alpha_blend,
                effect,
            } => {
                if alpha_blend {
                    let a = u32::from(pixel.alpha) + 1;
                    let mut result = 0;
                    for shift in [0, 5, 10] {
                        let c = (u32::from((pixel.color >> shift) & 31) * a
                            + u32::from((second >> shift) & 31) * (32 - a))
                            / 32;
                        result |= (c as u16) << shift;
                    }
                    return result;
                }
                match effect {
                    Effect::None => pixel.color,
                    Effect::Brighten => map_channels(pixel.color, |c| c + (31 - c) * evy / 16),
                    Effect::Darken => map_channels(pixel.color, |c| c - c * evy / 16),
                }
            }
            With::Second { first } => mix(first, pixel.color, eva, evb),
        }
    }

    /// Packed for the GPU compositor: the color without 3D and the kind
    /// (bits 16-17: fixed, 3D first, 3D second); the other color; the
    /// alpha-blend flag (bit 0), effect (bits 1-2) and EVA, EVB, EVY
    /// (bits 8, 16, 24).
    pub fn pack(&self) -> [u32; 4] {
        let (kind, other, flags) = match self.with {
            With::Fixed(color) => (0, color, 0),
            With::First {
                second,
                alpha_blend,
                effect,
            } => (1, second, u32::from(alpha_blend) | (effect as u32) << 1),
            With::Second { first } => (2, first, 0),
        };
        let c = self.coefficients;
        [
            u32::from(self.without) | kind << 16,
            u32::from(other),
            flags | u32::from(c.eva) << 8 | u32::from(c.evb) << 16 | u32::from(c.evy) << 24,
            0,
        ]
    }
}

/// The blend registers for one pixel.
struct BlendState {
    /// BLDCNT.
    blend: u16,
    /// The window lets special effects through.
    effects: bool,
    coefficients: Coefficients,
}

impl BlendState {
    fn first_target(&self, layer: usize) -> bool {
        self.blend & (1 << layer) != 0
    }

    fn second_target(&self, layer: usize) -> bool {
        self.blend & (0x100 << layer) != 0
    }

    fn mode(&self) -> u16 {
        (self.blend >> 6) & 3
    }

    /// The color of the top two 2D layers (`first` is None when only the
    /// backdrop shows).
    fn fixed(&self, first: Option<Layer>, second: Layer, backdrop: Layer) -> u16 {
        let top = first.is_some();
        let first = first.unwrap_or(backdrop);
        let Coefficients { eva, evb, evy } = self.coefficients;
        let (eva, evb, evy) = (u32::from(eva), u32::from(evb), u32::from(evy));
        let is_second = self.second_target(second.layer);
        if first.semi && is_second && top {
            mix(first.color, second.color, eva, evb)
        } else if !self.effects || !self.first_target(first.layer) {
            first.color
        } else {
            match self.mode() {
                1 if is_second && top => mix(first.color, second.color, eva, evb),
                2 => map_channels(first.color, |c| c + (31 - c) * evy / 16),
                3 => map_channels(first.color, |c| c - c * evy / 16),
                _ => first.color,
            }
        }
    }

    /// A pixel from the top three of its layers (the 3D layer marked by
    /// `Layer::three_d`), with `backdrop` under them.
    fn compose(&self, stack: &[Option<Layer>; 3], backdrop: Layer) -> Composed {
        let mut flat = stack.iter().flatten().filter(|layer| !layer.three_d);
        let (first, second) = (flat.next().copied(), flat.next().copied());
        let without = self.fixed(first, second.unwrap_or(backdrop), backdrop);
        let position = stack
            .iter()
            .position(|layer| layer.is_some_and(|layer| layer.three_d));
        let with = match position {
            Some(0) => self.three_d_first(stack[1].unwrap_or(backdrop)),
            Some(1) => self.three_d_second(stack[0].unwrap()),
            _ => With::Fixed(without),
        };
        Composed {
            without,
            with,
            coefficients: self.coefficients,
        }
    }

    /// The 3D layer on top of `second`. Over a second target the hardware
    /// blends 3D pixels by their own alpha whatever BLDCNT's mode and first
    /// targets say, so EVA and EVB never apply: Black 2 and White 2's
    /// introduction sets alpha blending with EVA and EVB at 0 after Juniper
    /// sends out Minccino, and both still show.
    fn three_d_first(&self, second: Layer) -> With {
        let is_second = self.second_target(second.layer);
        let effect = if !self.effects || !self.first_target(0) {
            Effect::None
        } else {
            match self.mode() {
                2 => Effect::Brighten,
                3 => Effect::Darken,
                _ => Effect::None,
            }
        };
        With::First {
            second: second.color,
            alpha_blend: is_second && self.effects,
            effect,
        }
    }

    /// `first` over the 3D layer.
    fn three_d_second(&self, first: Layer) -> With {
        let blends = self.second_target(0);
        if first.semi && blends {
            return With::Second { first: first.color };
        }
        if !self.effects || !self.first_target(first.layer) {
            return With::Fixed(first.color);
        }
        let Coefficients { evy, .. } = self.coefficients;
        let evy = u32::from(evy);
        match self.mode() {
            1 if blends => With::Second { first: first.color },
            2 => With::Fixed(map_channels(first.color, |c| c + (31 - c) * evy / 16)),
            3 => With::Fixed(map_channels(first.color, |c| c - c * evy / 16)),
            _ => With::Fixed(first.color),
        }
    }
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
        let mut composed = [Composed::default(); WIDTH];
        let scroll = self.compose_line(dispcnt, line, &mut composed);
        for (x, (out, composed)) in out.iter_mut().zip(&composed).enumerate() {
            let sx = (x + scroll) & 0x1ff;
            let three_d = (sx < WIDTH)
                .then(|| self.three_d[line * WIDTH + sx])
                .filter(|pixel| pixel.alpha != 0);
            *out = composed.resolve(three_d);
        }
    }

    /// Compose one line, leaving the 3D layer (engine A's BG0) open: each
    /// pixel says what it shows without a 3D pixel and how it combines with
    /// one. Returns the 3D layer's horizontal scroll.
    fn compose_line(&self, dispcnt: u32, line: usize, out: &mut [Composed]) -> usize {
        let mut bgs: [[Option<u16>; WIDTH]; 4] = [[None; WIDTH]; 4];
        let mut priorities = [0_u8; 4];
        let mut enabled = [false; 4];
        let mut three_d = false;
        let scroll = usize::from(self.reg16(0x10) & 0x1ff);
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
                BgKind::ThreeD => three_d = true,
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
        let coefficients = Coefficients {
            eva: (alpha & 0x1F).min(16) as u8,
            evb: ((alpha >> 8) & 0x1F).min(16) as u8,
            evy: (self.reg16(0x54) & 0x1F).min(16) as u8,
        };
        let backdrop = Layer {
            color: self.palette(0),
            layer: LAYER_BACKDROP,
            semi: false,
            three_d: false,
        };
        let effects = |mask: u16| BlendState {
            blend,
            effects: mask & 0x20 != 0,
            coefficients,
        };

        for (x, out) in out.iter_mut().enumerate() {
            let mask = window_masks[x];
            // The top three candidates: enough to know the top two with and
            // without the 3D pixel.
            let mut stack: [Option<Layer>; 3] = [None; 3];
            let mut push = |layer: Layer| {
                if let Some(slot) = stack.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(layer);
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
                        three_d: false,
                    });
                }
                for index in 0..4 {
                    if !enabled[index] || priorities[index] != priority || mask & (1 << index) == 0
                    {
                        continue;
                    }
                    if index == 0 && three_d {
                        if (x + scroll) & 0x1ff < WIDTH {
                            push(Layer {
                                color: 0,
                                layer: 0,
                                semi: false,
                                three_d: true,
                            });
                        }
                    } else if let Some(color) = bgs[index][x] {
                        push(Layer {
                            color,
                            layer: index,
                            semi: false,
                            three_d: false,
                        });
                    }
                }
            }
            *out = effects(mask).compose(&stack, backdrop);
        }
        scroll
    }

    /// Widescreen's extra columns, line by line (`WIDE_EXTRA` left of the
    /// screen, then right of it), where the screen shows a captured screen
    /// as a direct-color bitmap background, from a capture made in
    /// widescreen (`WideCapture`) that is still in VRAM: the bitmap carries
    /// on into them, through its own affine transform, from VRAM where it
    /// maps to the 256 captured columns and from the capture's sides beside
    /// them, under the 3D layer if the screen shows it. Sprites and other
    /// backgrounds stay in the middle. `None` when no such background shows.
    fn capture_sides(&self, dispcnt: u32, vram: &Vram) -> Option<Vec<Composed>> {
        if self.b || vram.wide_captures.is_empty() {
            return None;
        }
        let region = self.region(false);
        // The captured bitmaps shown: index, control, base, capture.
        let mut bitmaps = Vec::new();
        for index in 2..4 {
            let control = self.reg16(8 + 2 * index as u32);
            if dispcnt & (0x100 << index) == 0
                || self.bg_kind(dispcnt, index) != BgKind::Extended
                || control & 0x84 != 0x84
                || control >> 14 != 1
            {
                continue;
            }
            let base = usize::from((control >> 8) & 0x1F) * 0x4000;
            let Some(start) = vram.offset(region, base) else {
                continue;
            };
            if let Some((capture, changes)) = vram
                .wide_captures
                .iter()
                .filter(|capture| capture.start == start)
                .find_map(|capture| Some((capture, capture.changes(vram)?)))
            {
                bitmaps.push((index, control, base, capture, changes));
            }
        }
        if bitmaps.is_empty() {
            return None;
        }
        let mask = if dispcnt & 0xE000 == 0 {
            0x3F
        } else {
            self.reg16(0x4A) & 0x3F
        };
        let three_d = dispcnt & 0x100 != 0 && self.bg_kind(dispcnt, 0) == BgKind::ThreeD;
        let three_d_priority = (self.reg16(8) & 3) as u8;
        let alpha = self.reg16(0x52);
        let state = BlendState {
            blend: self.reg16(0x50),
            effects: mask & 0x20 != 0,
            coefficients: Coefficients {
                eva: (alpha & 0x1F).min(16) as u8,
                evb: ((alpha >> 8) & 0x1F).min(16) as u8,
                evy: (self.reg16(0x54) & 0x1F).min(16) as u8,
            },
        };
        let backdrop = Layer {
            color: self.palette(0),
            layer: LAYER_BACKDROP,
            semi: false,
            three_d: false,
        };
        let sample = |index: usize,
                      control: u16,
                      base: usize,
                      capture: &WideCapture,
                      changes: ColorMap,
                      line: usize,
                      x: i32| {
            let params = 0x20 + 0x10 * (index as u32 - 2);
            let pa = i32::from(self.reg16(params) as i16);
            let pb = i32::from(self.reg16(params + 2) as i16);
            let pc = i32::from(self.reg16(params + 4) as i16);
            let pd = i32::from(self.reg16(params + 6) as i16);
            let reference = |offset: u32| ((self.reg32(params + offset) << 4) as i32) >> 4;
            let mut px = (reference(8) + pb * line as i32 + pa * x) >> 8;
            let mut py = (reference(12) + pd * line as i32 + pc * x) >> 8;
            if control & 0x2000 != 0 {
                px = px.rem_euclid(256);
                py = py.rem_euclid(256);
            }
            if !(0..256).contains(&py) {
                return None;
            }
            let color = if (0..256).contains(&px) {
                vram.read16(region, base + (py as usize * 256 + px as usize) * 2)
            } else if (py as usize) < capture.height() {
                changes.apply(capture.side(px, py as usize)?)
            } else {
                return None;
            };
            (color & 0x8000 != 0).then_some(color & 0x7FFF)
        };
        let columns = (-(WIDE_EXTRA as i32)..0).chain(WIDTH as i32..(WIDTH + WIDE_EXTRA) as i32);
        let sides = (0..HEIGHT)
            .flat_map(|line| columns.clone().map(move |x| (line, x)))
            .map(|(line, x)| {
                let mut stack: [Option<Layer>; 3] = [None; 3];
                let mut push = |layer: Layer| {
                    if let Some(slot) = stack.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(layer);
                    }
                };
                for priority in 0..4 {
                    if three_d && three_d_priority == priority && mask & 1 != 0 {
                        push(Layer {
                            color: 0,
                            layer: 0,
                            semi: false,
                            three_d: true,
                        });
                    }
                    for &(index, control, base, capture, changes) in &bitmaps {
                        if (control & 3) as u8 != priority || mask & (1 << index) == 0 {
                            continue;
                        }
                        if let Some(color) = sample(index, control, base, capture, changes, line, x)
                        {
                            push(Layer {
                                color,
                                layer: index,
                                semi: false,
                                three_d: false,
                            });
                        }
                    }
                }
                state.compose(&stack, backdrop)
            })
            .collect();
        Some(sides)
    }

    /// What widescreen's extra columns show: they are outside every
    /// window and have no backgrounds or sprites, so the 3D layer, if the
    /// screen shows it, over the backdrop, or black.
    fn side(&self, dispcnt: u32) -> Composed {
        let mask = if dispcnt & 0xE000 == 0 {
            0x3F
        } else {
            self.reg16(0x4A) & 0x3F
        };
        if dispcnt & 0x100 == 0 || self.bg_kind(dispcnt, 0) != BgKind::ThreeD || mask & 1 == 0 {
            return Composed::fixed(0);
        }
        let alpha = self.reg16(0x52);
        let coefficients = Coefficients {
            eva: (alpha & 0x1F).min(16) as u8,
            evb: ((alpha >> 8) & 0x1F).min(16) as u8,
            evy: (self.reg16(0x54) & 0x1F).min(16) as u8,
        };
        let backdrop = Layer {
            color: self.palette(0),
            layer: LAYER_BACKDROP,
            semi: false,
            three_d: false,
        };
        let state = BlendState {
            blend: self.reg16(0x50),
            effects: mask & 0x20 != 0,
            coefficients,
        };
        Composed {
            without: state.fixed(None, backdrop, backdrop),
            with: state.three_d_first(backdrop),
            coefficients,
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
    fn three_d_over_a_second_target_blends_by_its_own_alpha() {
        // BG0 (3D) first target, BG3 second, alpha blending with EVA and
        // EVB at 0, as in White 2's introduction once Minccino is out.
        let state = BlendState {
            blend: 0x0941,
            effects: true,
            coefficients: Coefficients::default(),
        };
        let bg3 = Layer {
            color: 0x001F,
            layer: 3,
            semi: false,
            three_d: false,
        };
        let composed = Composed {
            without: bg3.color,
            with: state.three_d_first(bg3),
            coefficients: state.coefficients,
        };
        let opaque = Pixel {
            color: 0x7C00,
            alpha: 31,
        };
        assert_eq!(composed.resolve(Some(opaque)), 0x7C00);
        let half = Pixel {
            color: 0x7C00,
            alpha: 15,
        };
        assert_eq!(composed.resolve(Some(half)), 0x3C0F);
        assert_eq!(composed.resolve(None), 0x001F);
        // Brightness still applies to 3D over a layer that is no second
        // target.
        let brighten = BlendState {
            blend: 0x0081,
            effects: true,
            coefficients: Coefficients {
                evy: 16,
                ..Default::default()
            },
        };
        let composed = Composed {
            without: bg3.color,
            with: brighten.three_d_first(bg3),
            coefficients: brighten.coefficients,
        };
        assert_eq!(composed.resolve(Some(opaque)), 0x7FFF);
    }

    #[test]
    fn widescreen_sides_carry_on_what_is_under_the_3d_at_the_edges() {
        let state = BlendState {
            blend: 0,
            effects: true,
            coefficients: Coefficients::default(),
        };
        let layer = |color, layer| Layer {
            color,
            layer,
            semi: false,
            three_d: false,
        };
        let backdrop = layer(0x551D, LAYER_BACKDROP);
        let side = Composed {
            without: backdrop.color,
            with: state.three_d_first(backdrop),
            coefficients: state.coefficients,
        };
        let gradient = Composed {
            without: 0x6F7B,
            with: state.three_d_first(layer(0x6F7B, 3)),
            coefficients: state.coefficients,
        };
        assert_eq!(side_of(side, gradient, gradient), gradient);
        // A text box in front of the 3D at the mirrored column lets the end
        // of the line carry on; in front of the 3D there too, or on a screen
        // without 3D, the sides show what they did before.
        let text_box = Composed::fixed(0x7FFF);
        assert_eq!(side_of(side, text_box, gradient), gradient);
        assert_eq!(side_of(side, text_box, text_box), side);
        // Translucent dialogue backgrounds and glyphs are foreground UI too.
        let fading = Composed {
            without: 0,
            with: With::Second { first: 0 },
            coefficients: state.coefficients,
        };
        assert_eq!(side_of(side, fading, gradient), gradient);
        assert_eq!(side_of(side, fading, fading), side);
        let glyph = Composed {
            with: With::Second { first: 0x7fff },
            ..fading
        };
        assert_eq!(side_of(side, glyph, fading), side);
        assert_eq!(
            side_of(Composed::fixed(0), gradient, gradient),
            Composed::fixed(0)
        );
    }

    #[test]
    fn full_screen_fades_cover_the_margins_without_copying_their_lettering() {
        let mut io = Io::new();
        let mut memory = Memory::new();
        io.geometry.wide = true;
        io.set16(POWCNT1, 0x800a);
        io.set32(0x0400_0000, 0x10308); // BG0 3D below text BG1
        io.set16(0x0400_0008, 1);
        io.set16(0x0400_000a, 1 << 8);
        memory.vram.control[0] = 0x81;
        memory.vram.update();
        memory.vram.banks[32..64].fill(0x11); // opaque black tile
        memory.vram.banks[64..96].fill(0x22); // white lettering
        memory.palette[4..6].copy_from_slice(&0x7fffu16.to_le_bytes());
        for tile in memory.vram.banks[0x800..0x1000].chunks_exact_mut(2) {
            tile.copy_from_slice(&1u16.to_le_bytes());
        }
        memory.vram.banks[0x800 + (10 * 32 + 15) * 2] = 2;
        let columns = WIDTH + 2 * WIDE_EXTRA;
        for blend in [0, 0x0142] {
            io.set16(0x0400_0050, blend);
            io.set16(0x0400_0052, 0x0808);
            let frame = render_scaled(&io, &memory);
            let expected = if blend == 0 {
                Composed {
                    coefficients: Coefficients {
                        eva: 8,
                        evb: 8,
                        evy: 0,
                    },
                    ..Composed::fixed(0)
                }
            } else {
                Composed {
                    without: 0,
                    with: With::Second { first: 0 },
                    coefficients: Coefficients {
                        eva: 8,
                        evb: 8,
                        evy: 0,
                    },
                }
            };
            for y in 0..HEIGHT {
                for x in (0..WIDE_EXTRA).chain(WIDE_EXTRA + WIDTH..columns) {
                    assert_eq!(frame.pixels[y * columns + x], expected.pack(), "{x},{y}");
                }
            }
            // Lettering remains only in the center.
            assert_ne!(
                frame.pixels[80 * columns + WIDE_EXTRA + 120],
                expected.pack()
            );
            assert!(
                capture_sides(&io, &memory, false, HEIGHT)
                    .iter()
                    .all(|&p| p == 0x8000)
            );
        }
        // A dialogue box covering only the bottom of the screen is UI, so
        // its background and glyphs must not extend into the margins.
        memory.vram.banks[0x800..0x800 + 19 * 32 * 2].fill(0);
        let frame = render_scaled(&io, &memory);
        assert_eq!((frame.pixels[180 * columns][0] >> 16) & 3, 1);
    }

    #[test]
    fn a_captured_screen_shown_as_a_bitmap_fills_the_sides() {
        let mut io = Io::new();
        let mut memory = Memory::new();
        io.geometry.wide = true;
        // Bank D as engine A's backgrounds; mode 5 with BG2 a 256x256
        // direct-color bitmap at 0, untransformed; A on the top screen.
        memory.vram.control[3] = 0x81;
        memory.vram.update();
        io.set16(POWCNT1, 0x8003);
        io.set32(0x0400_0000, 0x0001_0405);
        io.set16(0x0400_000C, 0x4084);
        io.set16(0x0400_0020, 0x100);
        io.set16(0x0400_0026, 0x100);
        // Captured red, which the game has since halved in place, with green
        // beside it.
        let start = 3 * 0x20000;
        let fill = |memory: &mut Memory, color: u16| {
            for at in (start..start + 256 * 192 * 2).step_by(2) {
                memory.vram.banks[at..at + 2].copy_from_slice(&color.to_le_bytes());
            }
        };
        fill(&mut memory, 0x800F);
        memory.vram.wide_captures.push(WideCapture {
            start,
            written: vec![0x801F; 256 * 192],
            sides: vec![0x83E0; 192 * 2 * WIDE_EXTRA],
            extra: WIDE_EXTRA,
        });
        let columns = WIDTH + 2 * WIDE_EXTRA;
        let without = |frame: &ScaledFrame, x: usize, y: usize| {
            frame.pixels[y * columns + x][0] as u16 & 0x7FFF
        };
        let frame = render_scaled(&io, &memory);
        assert_eq!(without(&frame, 0, 10), 0x03E0);
        assert_eq!(without(&frame, columns - 1, 100), 0x03E0);
        assert_eq!(without(&frame, WIDE_EXTRA, 10), 0x000F);
        // Zoomed in by two around the middle, as through a door, the sides
        // show the bitmap itself.
        io.set16(0x0400_0020, 0x80);
        io.set32(0x0400_0028, 64 << 8);
        let frame = render_scaled(&io, &memory);
        assert_eq!(without(&frame, 0, 10), 0x000F);
        io.set16(0x0400_0020, 0x100);
        io.set32(0x0400_0028, 0);
        // Other data in the bank is not the captured screen any more.
        for at in (start..start + 256 * 96 * 2).step_by(2) {
            memory.vram.banks[at..at + 2].copy_from_slice(&0x8123_u16.to_le_bytes());
        }
        let frame = render_scaled(&io, &memory);
        assert_eq!(without(&frame, 0, 10), 0);
    }

    #[test]
    fn png_has_signature_and_chunks() {
        let data = png(2, 2, &[0xFF; 16]);
        assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&data[12..16], b"IHDR");
        assert!(data.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }
}
