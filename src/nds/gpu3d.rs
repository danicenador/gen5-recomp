//! DS geometry commands and a frame-based software rasterizer.
//!
//! Register formats and matrix conventions follow GBATEK's DS 3D chapters:
//! https://mgba-emu.github.io/gbatek/#ds-3d-video
//! Commands execute synchronously, except SWAP_BUFFERS which waits for vblank.
//! This is a functional renderer, not a cycle-accurate FIFO or rasterizer.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;

use super::io::Io;
use super::vram::{Region, Vram};

const WIDTH: usize = 256;
const HEIGHT: usize = 192;
type Matrix = [i32; 16];

/// Columns added on each side of the 3D layer and the top screen in
/// widescreen: 256 + 2 * 43 = 342 by 192 is 16:9.
pub const WIDE_EXTRA: usize = 43;

/// How far the left and right clip planes reach, as a multiple of w, for a
/// polygon drawn through `viewport`. In widescreen, a full-width viewport
/// reaches `WIDE_EXTRA` columns further on each side: the camera sees
/// more to the left and right, with the same vertical view and everything
/// the same size on screen, instead of the picture being stretched.
/// Viewports narrower than the screen keep their edges.
fn reach(viewport: [u8; 4], wide: bool) -> f64 {
    if wide && viewport[0] == 0 && viewport[2] == 255 {
        (128 + WIDE_EXTRA) as f64 / 128.0
    } else {
        1.0
    }
}

fn identity() -> Matrix {
    let mut m = [0; 16];
    for i in 0..4 {
        m[i * 5] = 4096;
    }
    m
}

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| {
        let (r, c) = (i / 4, i % 4);
        let sum = (0..4).fold(0i64, |sum, k| {
            sum.wrapping_add(i64::from(a[r * 4 + k]) * i64::from(b[k * 4 + c]))
        });
        (sum >> 12) as i32
    })
}

fn transform(v: [f64; 4], m: Matrix) -> [f64; 4] {
    // Dividing by 4096 is exact, so multiplying by its inverse is too.
    const SCALE: f64 = 1.0 / 4096.0;
    std::array::from_fn(|c| {
        v[0] * (f64::from(m[c]) * SCALE)
            + v[1] * (f64::from(m[4 + c]) * SCALE)
            + v[2] * (f64::from(m[8 + c]) * SCALE)
            + v[3] * (f64::from(m[12 + c]) * SCALE)
    })
}

fn signed10(v: u32) -> f64 {
    f64::from((v as i32) << 22 >> 22)
}
fn vector(v: u32) -> [f64; 4] {
    [
        signed10(v) / 512.0,
        signed10(v >> 10) / 512.0,
        signed10(v >> 20) / 512.0,
        0.0,
    ]
}
fn rgb(v: u32) -> [f64; 3] {
    std::array::from_fn(|i| f64::from((v >> (i * 5)) & 31))
}
fn pack(c: [f64; 3]) -> u16 {
    c.into_iter().enumerate().fold(0, |v, (i, c)| {
        v | ((c.round().clamp(0.0, 31.0) as u16) << (i * 5))
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pixel {
    pub color: u16,
    pub alpha: u8,
}

#[derive(Clone, Copy, Debug)]
struct Vertex {
    clip: [f64; 4],
    color: [f64; 3],
    uv: [f64; 2],
}

impl Vertex {
    fn lerp(self, other: Self, t: f64) -> Self {
        Self {
            clip: std::array::from_fn(|i| self.clip[i] + (other.clip[i] - self.clip[i]) * t),
            color: std::array::from_fn(|i| self.color[i] + (other.color[i] - self.color[i]) * t),
            uv: std::array::from_fn(|i| self.uv[i] + (other.uv[i] - self.uv[i]) * t),
        }
    }
}

#[derive(Clone)]
struct Polygon {
    vertices: Vec<Vertex>,
    attr: u32,
    texture: u32,
    palette: u32,
    viewport: [u8; 4],
    id: PolygonId,
}

/// What a polygon looks like, and how many polygons that look the same were
/// submitted before it in its list, culled ones included. The same object
/// drawn in two consecutive lists gets the same id, so its polygons can be
/// paired up for interpolation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct PolygonId {
    texture: u32,
    palette: u32,
    /// POLYGON_ATTR without the alpha, which games fade.
    attr: u32,
    vertices: u8,
    occurrence: u32,
}

/// The attr bits that identify a polygon: everything but its alpha.
const ID_ATTR_MASK: u32 = !(31 << 16);

/// A swapped list: the polygons the rasterizer draws until the next swap.
#[derive(Clone, Default)]
pub struct DisplayList {
    polygons: Vec<Polygon>,
    render_w: bool,
    /// SWAP_BUFFERS bit 0: translucent polygons keep submission order
    /// instead of being sorted by Y.
    manual_sort: bool,
    /// The swap that published the list (`Geometry::swaps` after it).
    pub swap: u64,
}

/// How many of a list's polygons were paired with the next list.
#[derive(Clone, Copy, Debug, Default)]
pub struct Matched {
    pub matched: usize,
    pub total: usize,
}

/// A paired polygon that moves further than this many DS pixels between
/// lists is drawn where it is now: it was a cut or a warp, not motion.
const SNAP_PIXELS: f64 = 48.0;

impl DisplayList {
    /// This list's polygons moved `alpha` of the way to where the same
    /// polygons are in `next`: 0 is this list as it is. Only positions and
    /// vertex colors move; what is drawn, and how, is this list's, so it
    /// always matches the textures and registers of the frame shown.
    fn interpolated(
        &self,
        next: Option<&DisplayList>,
        alpha: f64,
    ) -> (Cow<'_, [Polygon]>, Matched) {
        let total = self.polygons.len();
        let Some(next) = next.filter(|_| alpha > 0.0) else {
            return (Cow::Borrowed(&self.polygons), Matched { matched: 0, total });
        };
        let alpha = alpha.min(1.0);
        let later: HashMap<PolygonId, &Polygon> = next
            .polygons
            .iter()
            .map(|polygon| (polygon.id, polygon))
            .collect();
        let mut matched = 0;
        let polygons = self
            .polygons
            .iter()
            .map(|polygon| {
                let mut polygon = polygon.clone();
                if let Some(after) = later.get(&polygon.id)
                    && pairs_with(&polygon, after)
                {
                    matched += 1;
                    for (vertex, after) in polygon.vertices.iter_mut().zip(&after.vertices) {
                        let moved = vertex.lerp(*after, alpha);
                        vertex.clip = moved.clip;
                        vertex.color = moved.color;
                    }
                }
                polygon
            })
            .collect();
        (Cow::Owned(polygons), Matched { matched, total })
    }
}

/// Whether `before` can be drawn part way to `after`: the same shape, in
/// front of the camera both times, and not too far apart on screen.
fn pairs_with(before: &Polygon, after: &Polygon) -> bool {
    if before.vertices.len() != after.vertices.len() {
        return false;
    }
    before.vertices.iter().zip(&after.vertices).all(|(a, b)| {
        let (wa, wb) = (a.clip[3], b.clip[3]);
        if wa <= 0.0 || wb <= 0.0 {
            return false;
        }
        let dx = (a.clip[0] / wa - b.clip[0] / wb) * 128.0;
        let dy = (a.clip[1] / wa - b.clip[1] / wb) * 96.0;
        dx.abs() <= SNAP_PIXELS && dy.abs() <= SNAP_PIXELS
    })
}

pub struct Geometry {
    matrices: [Matrix; 4], // projection, position, direction, texture
    /// Position times projection, recomputed when either changes.
    clip: Option<Matrix>,
    position_stack: [(Matrix, Matrix); 32],
    projection_stack: Matrix,
    texture_stack: Matrix,
    position_sp: u8,
    projection_sp: u8,
    texture_sp: u8,
    stack_error: bool,
    mode: usize,
    commands: VecDeque<u8>,
    parameters: Vec<u32>,
    direct: Option<u8>,
    deferred: VecDeque<(u32, u32)>,
    swap: Option<u32>,
    /// Whether the previous polygon of the current strip was stored, so the
    /// next one shares its vertices in vertex RAM.
    strip_stored: bool,
    building: Vec<Polygon>,
    /// Polygons submitted to the list being built, by look (`PolygonId`
    /// without the occurrence), culled ones included.
    occurrences: HashMap<PolygonId, u32>,
    displayed: Arc<DisplayList>,
    vertices: Vec<Vertex>,
    vertex_count: usize,
    /// Polygons of the list being built that count against polygon RAM:
    /// all but those only widescreen shows.
    stored_polygons: usize,
    /// Polygons kept only for widescreen's extra columns. They have polygon
    /// and vertex RAM of their own, so the DS's view never loses polygons
    /// to them.
    side_polygons: usize,
    side_vertices: usize,
    /// Widescreen: keep and draw what lies beyond the left and right edges
    /// of the screen (see `reach`).
    pub wide: bool,
    primitive: Option<u32>,
    strip_index: usize,
    position: [f64; 4],
    color: [f64; 3],
    uv: [f64; 2],
    raw_uv: [f64; 2],
    attr: u32,
    next_attr: u32,
    texture: u32,
    palette: u32,
    viewport: [u8; 4],
    diffuse: [f64; 3],
    ambient: [f64; 3],
    specular: [f64; 3],
    emission: [f64; 3],
    lights: [[f64; 4]; 4],
    light_colors: [[f64; 3]; 4],
    shininess: [u8; 128],
    use_shininess: bool,
    position_result: [i32; 4],
    vector_result: [i16; 3],
    box_result: bool,
    pub swaps: u64,
    pub submitted_polygons: u64,
    /// The most polygons and vertices one swapped list kept.
    pub peak_polygons: usize,
    pub peak_vertices: usize,
    /// Swapped lists that dropped visible polygons because polygon or vertex
    /// RAM was full.
    pub overflowed_lists: u64,
    overflowed: bool,
    /// The most polygons one list submitted, culled ones included.
    pub peak_submitted: u64,
    list_start: u64,
    /// GEN5_3D_TRACE: describe every polygon and box test of the next list.
    pub trace_next: bool,
    trace: Option<String>,
    pub traced: Option<String>,
    /// Host time spent rendering, in nanoseconds.
    render_nanos: AtomicU64,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            matrices: [identity(); 4],
            clip: None,
            position_stack: [(identity(), identity()); 32],
            projection_stack: identity(),
            texture_stack: identity(),
            position_sp: 0,
            projection_sp: 0,
            texture_sp: 0,
            stack_error: false,
            mode: 0,
            commands: VecDeque::new(),
            parameters: Vec::new(),
            direct: None,
            deferred: VecDeque::new(),
            swap: None,
            strip_stored: false,
            building: Vec::new(),
            occurrences: HashMap::new(),
            displayed: Arc::default(),
            vertices: Vec::new(),
            vertex_count: 0,
            stored_polygons: 0,
            side_polygons: 0,
            side_vertices: 0,
            wide: false,
            primitive: None,
            strip_index: 0,
            position: [0.0, 0.0, 0.0, 1.0],
            color: [31.0; 3],
            uv: [0.0; 2],
            raw_uv: [0.0; 2],
            attr: 0,
            next_attr: 0,
            texture: 0,
            palette: 0,
            viewport: [0, 0, 255, 191],
            diffuse: [0.0; 3],
            ambient: [0.0; 3],
            specular: [0.0; 3],
            emission: [0.0; 3],
            lights: [[0.0; 4]; 4],
            light_colors: [[0.0; 3]; 4],
            shininess: [0; 128],
            use_shininess: false,
            position_result: [0; 4],
            vector_result: [0; 3],
            box_result: false,
            swaps: 0,
            submitted_polygons: 0,
            peak_polygons: 0,
            peak_vertices: 0,
            overflowed_lists: 0,
            overflowed: false,
            peak_submitted: 0,
            list_start: 0,
            trace_next: false,
            trace: None,
            traced: None,
            render_nanos: AtomicU64::new(0),
        }
    }
}

fn parameter_count(cmd: u8) -> usize {
    match cmd {
        0x10
        | 0x12..=0x14
        | 0x20..=0x22
        | 0x24..=0x2b
        | 0x30..=0x33
        | 0x40
        | 0x50
        | 0x60
        | 0x72 => 1,
        0x16 | 0x18 => 16,
        0x17 | 0x19 => 12,
        0x1a => 9,
        0x1b | 0x1c | 0x70 => 3,
        0x23 | 0x71 => 2,
        0x34 => 32,
        _ => 0,
    }
}

impl Geometry {
    pub fn write(&mut self, address: u32, value: u32) {
        if self.swap.is_some() {
            self.deferred.push_back((address, value));
            return;
        }
        if address < 0x0400_0440 {
            if self.commands.is_empty() {
                self.commands.extend(value.to_le_bytes());
            } else {
                self.parameters.push(value);
            }
            self.drain_commands();
        } else {
            let cmd = ((address - 0x0400_0400) / 4) as u8;
            if self.direct != Some(cmd) {
                self.parameters.clear();
                self.direct = Some(cmd);
            }
            if parameter_count(cmd) != 0 {
                self.parameters.push(value);
            }
            if self.parameters.len() == parameter_count(cmd) {
                self.execute_parameters(cmd);
                self.direct = None;
            }
        }
    }

    fn drain_commands(&mut self) {
        while self.swap.is_none() {
            let Some(&cmd) = self.commands.front() else {
                break;
            };
            if self.parameters.len() < parameter_count(cmd) {
                break;
            }
            self.commands.pop_front();
            self.execute_parameters(cmd);
        }
    }

    /// Run `cmd` with the parameters gathered for it, keeping the buffer.
    fn execute_parameters(&mut self, cmd: u8) {
        let mut params = std::mem::take(&mut self.parameters);
        self.execute(cmd, &params);
        params.clear();
        self.parameters = params;
    }

    /// Publish the completed list at vblank, retaining it until the next swap.
    pub fn vblank(&mut self) {
        if let Some(flags) = self.swap.take() {
            self.peak_polygons = self.peak_polygons.max(self.stored_polygons);
            self.peak_vertices = self.peak_vertices.max(self.vertex_count);
            self.peak_submitted = self
                .peak_submitted
                .max(self.submitted_polygons - self.list_start);
            self.list_start = self.submitted_polygons;
            self.overflowed_lists += u64::from(std::mem::take(&mut self.overflowed));
            self.swaps += 1;
            self.displayed = Arc::new(DisplayList {
                polygons: std::mem::take(&mut self.building),
                render_w: flags & 2 != 0,
                manual_sort: flags & 1 != 0,
                swap: self.swaps,
            });
            self.occurrences.clear();
            self.vertex_count = 0;
            self.stored_polygons = 0;
            self.side_polygons = 0;
            self.side_vertices = 0;
            if let Some(text) = self.trace.take() {
                self.traced = Some(text);
            }
            if std::mem::take(&mut self.trace_next) {
                self.trace = Some(String::new());
            }
            self.drain_commands();
            while self.swap.is_none() {
                let Some((address, value)) = self.deferred.pop_front() else {
                    break;
                };
                self.write(address, value);
            }
        }
    }

    /// Polygons in the list being displayed.
    pub fn displayed_polygons(&self) -> usize {
        self.displayed.polygons.len()
    }

    pub fn status(&self) -> u32 {
        (1 << 25)
            | (1 << 26)
            | (u32::from(self.position_sp & 31) << 8)
            | (u32::from(self.projection_sp & 1) << 13)
            | (u32::from(self.stack_error) << 15)
            | (u32::from(self.box_result) << 1)
    }

    pub fn reset_stack_error(&mut self) {
        self.stack_error = false;
        self.projection_sp = 0;
    }

    pub fn read(&self, address: u32) -> Option<u32> {
        Some(match address & !3 {
            0x0400_0604 => (self.stored_polygons as u32) | ((self.vertex_count as u32) << 16),
            a @ 0x0400_0620..=0x0400_062c => {
                self.position_result[((a - 0x0400_0620) / 4) as usize] as u32
            }
            0x0400_0630 => {
                u32::from(self.vector_result[0] as u16)
                    | (u32::from(self.vector_result[1] as u16) << 16)
            }
            0x0400_0634 => u32::from(self.vector_result[2] as u16),
            a @ 0x0400_0640..=0x0400_067c => multiply(self.matrices[1], self.matrices[0])
                [((a - 0x0400_0640) / 4) as usize]
                as u32,
            a @ 0x0400_0680..=0x0400_06a0 => {
                let i = ((a - 0x0400_0680) / 4) as usize;
                self.matrices[2][i / 3 * 4 + i % 3] as u32
            }
            _ => return None,
        })
    }

    fn stack(&mut self, cmd: u8, value: u32) {
        let position = matches!(self.mode, 1 | 2);
        let sp = if self.mode == 3 {
            self.texture_sp
        } else if position {
            self.position_sp
        } else {
            self.projection_sp
        };
        let index = match cmd {
            0x11 => sp as usize,
            0x12 => {
                let offset = if position {
                    (value as i32) << 26 >> 26
                } else {
                    1
                };
                let next = (i32::from(sp) - offset) & if position { 63 } else { 1 };
                if self.mode == 3 {
                    self.texture_sp = next as u8;
                } else if position {
                    self.position_sp = next as u8;
                } else {
                    self.projection_sp = next as u8;
                }
                next as usize
            }
            _ => {
                if position {
                    value as usize & 31
                } else {
                    0
                }
            }
        };
        if self.mode != 3 && index >= if position { 31 } else { 1 } {
            self.stack_error = true;
        }
        self.clip = None;
        if matches!(cmd, 0x11 | 0x13) {
            match self.mode {
                0 => self.projection_stack = self.matrices[0],
                1 | 2 => self.position_stack[index & 31] = (self.matrices[1], self.matrices[2]),
                _ => self.texture_stack = self.matrices[3],
            }
        } else {
            match self.mode {
                0 => self.matrices[0] = self.projection_stack,
                1 | 2 => {
                    (self.matrices[1], self.matrices[2]) = self.position_stack[index & 31];
                }
                _ => self.matrices[3] = self.texture_stack,
            }
        }
        if cmd == 0x11 {
            if self.mode == 3 {
                self.texture_sp = (sp + 1) & 1;
            } else if position {
                self.position_sp = (sp + 1) & 63;
            } else {
                self.projection_sp = (sp + 1) & 1;
            }
        }
    }

    fn matrix(&mut self, cmd: u8, p: &[u32]) {
        let mut m = identity();
        match cmd {
            0x16 | 0x18 => {
                for i in 0..16 {
                    m[i] = p[i] as i32;
                }
            }
            0x17 | 0x19 | 0x1a => {
                for (i, &v) in p.iter().enumerate() {
                    m[i / 3 * 4 + i % 3] = v as i32;
                }
            }
            0x1b => {
                for i in 0..3 {
                    m[i * 5] = p[i] as i32;
                }
            }
            0x1c => {
                for i in 0..3 {
                    m[12 + i] = p[i] as i32;
                }
            }
            _ => {}
        }
        let index = [0, 1, 1, 3][self.mode];
        let load = matches!(cmd, 0x15..=0x17);
        if index != 3 {
            self.clip = None;
        }
        self.matrices[index] = if load {
            m
        } else {
            multiply(m, self.matrices[index])
        };
        if self.mode == 2 && cmd != 0x1b {
            self.matrices[2] = if load {
                m
            } else {
                multiply(m, self.matrices[2])
            };
        }
    }

    fn texture_transform(&mut self, source: [f64; 4], mode: u32) {
        let t = transform(source, self.matrices[3]);
        self.uv = if mode == 1 {
            [t[0], t[1]]
        } else {
            [t[0] + self.raw_uv[0], t[1] + self.raw_uv[1]]
        };
    }

    fn normal(&mut self, value: u32) {
        let raw = vector(value);
        if self.texture >> 30 == 2 {
            self.texture_transform(raw, 2);
        }
        let n = transform(raw, self.matrices[2]);
        self.color = self.emission;
        for light in 0..4 {
            if self.attr & (1 << light) == 0 {
                continue;
            }
            let l = self.lights[light];
            let diffuse = (-(0..3).map(|i| n[i] * l[i]).sum::<f64>()).clamp(0.0, 1.0);
            let half = [l[0] / 2.0, l[1] / 2.0, (l[2] - 1.0) / 2.0];
            let mut specular = (-(0..3).map(|i| n[i] * half[i]).sum::<f64>())
                .clamp(0.0, 1.0)
                .powi(2);
            if self.use_shininess {
                specular =
                    f64::from(self.shininess[(specular * 128.0).min(127.0) as usize]) / 256.0;
            }
            for i in 0..3 {
                self.color[i] += self.light_colors[light][i] / 31.0
                    * (self.ambient[i] + self.diffuse[i] * diffuse + self.specular[i] * specular);
            }
        }
        self.color = self.color.map(|c| c.clamp(0.0, 31.0));
    }

    fn clip_matrix(&mut self) -> Matrix {
        *self
            .clip
            .get_or_insert_with(|| multiply(self.matrices[1], self.matrices[0]))
    }

    fn vertex(&mut self) {
        let Some(primitive) = self.primitive else {
            return;
        };
        if self.texture >> 30 == 3 {
            let mut p = self.position;
            p[3] = 0.0;
            self.texture_transform(p, 3);
        }
        let clip = transform(self.position, self.clip_matrix());
        self.vertices.push(Vertex {
            clip,
            color: self.color,
            uv: self.uv,
        });
        let count = if primitive == 1 || primitive == 3 {
            4
        } else {
            3
        };
        if self.vertices.len() < count {
            return;
        }
        let mut vertices = self.vertices.clone();
        if primitive == 2 && self.strip_index & 1 != 0 {
            vertices.swap(0, 1);
        }
        // Quad strips arrive as alternating pairs, not perimeter order.
        if primitive == 3 {
            vertices.swap(2, 3);
        }
        // Polygon and vertex RAM hold 2048 polygons and 6144 vertices, but
        // only polygons that survive culling and clipping are stored: a
        // wide scene submits far more than it keeps.
        self.submitted_polygons += 1;
        let mut id = PolygonId {
            texture: self.texture,
            palette: self.palette,
            attr: self.attr & ID_ATTR_MASK,
            vertices: count as u8,
            occurrence: 0,
        };
        let seen = self.occurrences.entry(id).or_default();
        id.occurrence = *seen;
        *seen += 1;
        let reach = reach(self.viewport, self.wide);
        let verdict = self.visible(&vertices, reach);
        // Only widescreen's extra columns show this polygon.
        let side = reach > 1.0 && verdict.is_ok() && self.visible(&vertices, 1.0).is_err();
        let mut traced = None;
        let stored = if verdict.is_ok() {
            let shared = if self.strip_stored && matches!(primitive, 2 | 3) {
                count - 2
            } else {
                count
            };
            let (polygons, vertex_count) = if side {
                (&mut self.side_polygons, &mut self.side_vertices)
            } else {
                (&mut self.stored_polygons, &mut self.vertex_count)
            };
            let fits = *polygons < 2048 && *vertex_count + shared <= 6144;
            if fits {
                *polygons += 1;
                *vertex_count += shared;
                self.building.push(Polygon {
                    vertices: if self.trace.is_some() {
                        vertices.clone()
                    } else {
                        std::mem::take(&mut vertices)
                    },
                    attr: self.attr,
                    texture: self.texture,
                    palette: self.palette,
                    viewport: self.viewport,
                    id,
                });
            }
            self.overflowed |= !fits && !side;
            if !fits {
                traced = Some("full");
            }
            fits
        } else {
            false
        };
        if let Some(trace) = &mut self.trace {
            use std::fmt::Write;
            let status = verdict
                .err()
                .or(traced)
                .unwrap_or(if side { "side" } else { "kept" });
            let _ = write!(
                trace,
                "{status:6} attr {:08X} tex {:08X} pal {:04X}:",
                self.attr, self.texture, self.palette
            );
            for v in &vertices {
                let w = v.clip[3];
                let _ = write!(
                    trace,
                    " ({:.1},{:.1} z{:.3} w{:.3})",
                    (v.clip[0] / w + 1.0) * 128.0,
                    (1.0 - v.clip[1] / w) * 96.0,
                    v.clip[2] / w,
                    w
                );
            }
            trace.push('\n');
        }
        self.strip_stored = stored;
        match primitive {
            2 => {
                self.vertices.remove(0);
            }
            3 => {
                self.vertices.drain(..2);
            }
            _ => self.vertices.clear(),
        }
        self.strip_index += 1;
    }

    /// Whether the hardware would store this polygon: it is not entirely
    /// outside one clip plane (the left and right ones at `reach`), it does
    /// not cross the far plane unless the polygon asks for that, and its
    /// facing is drawn.
    fn visible(&self, vertices: &[Vertex], reach: f64) -> Result<(), &'static str> {
        if (0..6).any(|plane| {
            vertices
                .iter()
                .all(|v| plane_distance(v.clip, plane, reach) < 0.0)
        }) {
            return Err("out");
        }
        if self.attr & (1 << 12) == 0 && vertices.iter().any(|v| v.clip[2] > v.clip[3]) {
            return Err("far");
        }
        if vertices.iter().all(|v| v.clip[3] > 0.0) {
            // Facing, in the same screen orientation the rasterizer uses
            // (y grows downwards).
            let screen = |v: &Vertex| [v.clip[0] / v.clip[3], -v.clip[1] / v.clip[3]];
            let mut area = 0.0;
            for i in 0..vertices.len() {
                let (a, b) = (
                    screen(&vertices[i]),
                    screen(&vertices[(i + 1) % vertices.len()]),
                );
                area += a[0] * b[1] - b[0] * a[1];
            }
            if area != 0.0 {
                let front = area < 0.0;
                if self.attr & (if front { 1 << 7 } else { 1 << 6 }) == 0 {
                    return Err("facing");
                }
            }
        }
        Ok(())
    }

    fn execute(&mut self, cmd: u8, p: &[u32]) {
        let v = p.first().copied().unwrap_or(0);
        match cmd {
            0x10 => self.mode = (v & 3) as usize,
            0x11..=0x14 => self.stack(cmd, v),
            0x15..=0x1c => self.matrix(cmd, p),
            0x20 => self.color = rgb(v),
            0x21 => self.normal(v),
            0x22 => {
                self.raw_uv = [
                    f64::from(v as i16) / 16.0,
                    f64::from((v >> 16) as i16) / 16.0,
                ];
                self.uv = self.raw_uv;
                if self.texture >> 30 == 1 {
                    self.texture_transform(
                        [self.raw_uv[0], self.raw_uv[1], 1.0 / 16.0, 1.0 / 16.0],
                        1,
                    );
                }
            }
            0x23..=0x28 => {
                match cmd {
                    0x23 => {
                        self.position = [
                            f64::from(v as i16) / 4096.0,
                            f64::from((v >> 16) as i16) / 4096.0,
                            f64::from(p[1] as i16) / 4096.0,
                            1.0,
                        ]
                    }
                    0x24 => {
                        for i in 0..3 {
                            self.position[i] = signed10(v >> (i * 10)) / 64.0;
                        }
                    }
                    0x25..=0x27 => {
                        let indices = match cmd {
                            0x25 => [0, 1],
                            0x26 => [0, 2],
                            _ => [1, 2],
                        };
                        for (i, &index) in indices.iter().enumerate() {
                            self.position[index] = f64::from((v >> (i * 16)) as i16) / 4096.0;
                        }
                    }
                    _ => {
                        for i in 0..3 {
                            self.position[i] += signed10(v >> (i * 10)) / 4096.0;
                        }
                    }
                }
                self.vertex();
            }
            0x29 => self.next_attr = v,
            0x2a => self.texture = v,
            0x2b => self.palette = v & 0x1fff,
            0x30 => {
                self.diffuse = rgb(v);
                self.ambient = rgb(v >> 16);
                if v & 0x8000 != 0 {
                    self.color = self.diffuse;
                }
            }
            0x31 => {
                self.specular = rgb(v);
                self.emission = rgb(v >> 16);
                self.use_shininess = v & 0x8000 != 0;
            }
            0x32 => self.lights[(v >> 30) as usize] = transform(vector(v), self.matrices[2]),
            0x33 => self.light_colors[(v >> 30) as usize] = rgb(v),
            0x34 => {
                for (i, &word) in p.iter().enumerate() {
                    self.shininess[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
            }
            0x40 => {
                self.primitive = Some(v & 3);
                self.strip_stored = false;
                self.vertices.clear();
                self.strip_index = 0;
                self.attr = self.next_attr;
            }
            0x41 => {
                self.primitive = None;
                self.vertices.clear();
            }
            0x50 => self.swap = Some(v),
            0x60 => self.viewport = v.to_le_bytes(),
            0x70 => {
                let values: Vec<f64> = p
                    .iter()
                    .flat_map(|v| {
                        [
                            f64::from(*v as i16) / 4096.0,
                            f64::from((v >> 16) as i16) / 4096.0,
                        ]
                    })
                    .collect();
                let clip = self.clip_matrix();
                let corners: Vec<_> = (0..8)
                    .map(|n| {
                        transform(
                            [
                                values[0] + if n & 1 != 0 { values[3] } else { 0.0 },
                                values[1] + if n & 2 != 0 { values[4] } else { 0.0 },
                                values[2] + if n & 4 != 0 { values[5] } else { 0.0 },
                                1.0,
                            ],
                            clip,
                        )
                    })
                    .collect();
                // Widescreen counts boxes beside the screen as visible, so
                // objects the game culls with box tests appear there too.
                let reach = reach(self.viewport, self.wide);
                self.box_result = !(0..6).any(|plane| {
                    corners
                        .iter()
                        .all(|c| plane_distance(*c, plane, reach) < 0.0)
                });
                if let Some(trace) = &mut self.trace {
                    use std::fmt::Write;
                    let _ = writeln!(trace, "box    {values:?} -> {}", self.box_result);
                }
            }
            0x71 => {
                let clip = self.clip_matrix();
                self.position_result = transform(
                    [
                        f64::from(v as i16) / 4096.0,
                        f64::from((v >> 16) as i16) / 4096.0,
                        f64::from(p[1] as i16) / 4096.0,
                        1.0,
                    ],
                    clip,
                )
                .map(|x| (x * 4096.0) as i32);
            }
            0x72 => {
                let r = transform(vector(v), self.matrices[2]);
                self.vector_result = std::array::from_fn(|i| (r[i] * 4096.0) as i16);
            }
            _ => {}
        }
    }

    pub fn render(&self, io: &Io, vram: &Vram) -> Vec<Pixel> {
        let start = std::time::Instant::now();
        let pixels = self.render_list(io, vram);
        self.render_nanos
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        pixels
    }

    /// Host time spent rendering since the last call, for the frame-time report.
    pub fn take_render_time(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.render_nanos.swap(0, Ordering::Relaxed))
    }

    fn render_list(&self, io: &Io, vram: &Vram) -> Vec<Pixel> {
        let clear = io.get32(0x0400_0350);
        let mut pixels = vec![
            Pixel {
                color: clear as u16 & 0x7fff,
                alpha: ((clear >> 16) & 31) as u8
            };
            WIDTH * HEIGHT
        ];
        let clear_depth = f64::from(io.get16(0x0400_0354) & 0x7fff) / 32767.0;
        let list = &*self.displayed;
        let mut depths = vec![
            if list.render_w {
                clear_depth * 4096.0
            } else {
                clear_depth
            };
            WIDTH * HEIGHT
        ];
        // Only a previously drawn translucent polygon suppresses another
        // translucent polygon with the same ID; opaque IDs do not.
        let mut ids = vec![64; WIDTH * HEIGHT];
        let state = RenderState::read(io, list.render_w);
        // The DS's own view: display capture and native-resolution screens
        // show exactly the screen's 256 columns.
        let triangles = triangles(&list.polygons, list.manual_sort, false);
        if triangles.is_empty() {
            return pixels;
        }
        // Every pixel only depends on the polygons drawn over it, in order,
        // so bands of rows render in parallel with the same result.
        pixels
            .par_chunks_mut(BAND_ROWS * WIDTH)
            .zip(depths.par_chunks_mut(BAND_ROWS * WIDTH))
            .zip(ids.par_chunks_mut(BAND_ROWS * WIDTH))
            .enumerate()
            .for_each(|(index, ((pixels, depths), ids))| {
                let top = index * BAND_ROWS;
                let bottom = top + pixels.len() / WIDTH;
                for triangle in &triangles {
                    let rows = triangle.min_y.max(top)..triangle.max_y.min(bottom);
                    if !rows.is_empty() {
                        triangle.draw(rows, top, &state, vram, pixels, depths, ids);
                    }
                }
            });
        pixels
    }

    /// The displayed list for a GPU rasterizer, with the registers and
    /// texture memory it reads.
    pub fn scene(&self, io: &Io, vram: &Vram) -> Scene {
        let state = RenderState::read(io, self.displayed.render_w);
        let clear = io.get32(0x0400_0350);
        Scene {
            list: self.displayed.clone(),
            clear_color: clear as u16 & 0x7fff,
            clear_alpha: ((clear >> 16) & 31) as u8,
            clear_depth: f32::from(io.get16(0x0400_0354) & 0x7fff) / 32767.0,
            control: state.control,
            alpha_ref: state.alpha_ref,
            toon: state.toon,
            w_buffer: state.w_buffer,
            texture: vram.snapshot(Region::Texture),
            texture_palette: vram.snapshot(Region::TexturePalette),
        }
    }
}

/// A list's polygons as triangles in drawing order: clipped, culled, and
/// sorted as the hardware sorts polygons. `wide` clips them to the
/// widescreen view (see `reach`) instead of the DS's.
fn triangles(polygons: &[Polygon], manual_sort: bool, wide: bool) -> Vec<Triangle<'_>> {
    // Opaque polygons precede translucent ones. The hardware sorts
    // opaque polygons, and translucent ones unless SWAP_BUFFERS asked
    // for manual order, by their lowest then highest screen Y. The sort
    // is stable, like submission order within equal keys.
    let mut triangles = Vec::new();
    for translucent in [false, true] {
        let mut group: Vec<&Polygon> = polygons
            .iter()
            .filter(|polygon| {
                let alpha = (polygon.attr >> 16) & 31;
                let format = (polygon.texture >> 26) & 7;
                (alpha < 31 || format == 1 || format == 6) == translucent
            })
            .collect();
        if !translucent || !manual_sort {
            group.sort_by_cached_key(|polygon| sort_key(polygon));
        }
        for polygon in group {
            let vertices = clip_polygon(&polygon.vertices, reach(polygon.viewport, wide));
            if vertices.len() < 3 {
                continue;
            }
            for i in 1..vertices.len() - 1 {
                triangles.extend(Triangle::new(
                    [vertices[0], vertices[i], vertices[i + 1]],
                    polygon,
                    translucent,
                ));
            }
        }
    }
    triangles
}

/// Triangles for the GPU rasterizer, with the list's polygons part way to
/// the next list (see `DisplayList::interpolated`).
#[derive(Clone, Default)]
pub struct Triangles {
    pub vertices: Vec<SceneVertex>,
    pub batches: Vec<Batch>,
    pub matched: Matched,
}

impl DisplayList {
    /// The list's triangles in drawing order, in runs that share render
    /// state, moved `alpha` of the way to `next`. `wide` keeps what lies in
    /// widescreen's extra columns, at DS x coordinates from `-WIDE_EXTRA`
    /// to `256 + WIDE_EXTRA`.
    pub fn build(&self, next: Option<&DisplayList>, alpha: f64, wide: bool) -> Triangles {
        let (polygons, matched) = self.interpolated(next, alpha);
        let mut vertices = Vec::new();
        let mut batches: Vec<Batch> = Vec::new();
        for triangle in triangles(&polygons, self.manual_sort, wide) {
            let attr = triangle.polygon.attr;
            let batch = Batch {
                range: 0..0,
                translucent: triangle.translucent,
                depth_equal: attr & (1 << 14) != 0,
                depth_write: !triangle.translucent || attr & (1 << 11) != 0,
                id: if triangle.translucent {
                    ((attr >> 24) & 63) as u8
                } else {
                    64
                },
            };
            let start = vertices.len() as u32;
            match batches.last_mut() {
                Some(last) if last.same_state(&batch) => {}
                _ => batches.push(Batch {
                    range: start..start,
                    ..batch
                }),
            }
            for (i, vertex) in triangle.vertices.iter().enumerate() {
                let w = vertex.clip[3];
                vertices.push(SceneVertex {
                    position: [
                        triangle.screen[i][0] as f32,
                        triangle.screen[i][1] as f32,
                        (triangle.depth_w[i] * 0.5 + 0.5) as f32,
                        w as f32,
                    ],
                    color: vertex.color.map(|c| c as f32),
                    uv: vertex.uv.map(|c| c as f32),
                    attr,
                    texture: triangle.polygon.texture,
                    palette: triangle.polygon.palette,
                });
            }
            batches.last_mut().unwrap().range.end = vertices.len() as u32;
        }
        Triangles {
            vertices,
            batches,
            matched,
        }
    }
}

/// One frame's 3D list for a GPU rasterizer, and what the rasterizer reads.
/// `DisplayList::build` turns the list into triangles.
#[derive(Clone)]
pub struct Scene {
    pub list: Arc<DisplayList>,
    pub clear_color: u16,
    pub clear_alpha: u8,
    pub clear_depth: f32,
    /// DISP3DCNT.
    pub control: u16,
    pub alpha_ref: u8,
    pub toon: [u16; 32],
    pub w_buffer: bool,
    /// Texture image and palette memory as mapped now (512 and 128 KiB).
    pub texture: Vec<u8>,
    pub texture_palette: Vec<u8>,
}

/// A triangle corner: DS screen position (pixels, y down), depth as the
/// Z-buffer stores it, w, and the per-polygon attributes.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SceneVertex {
    pub position: [f32; 4],
    pub color: [f32; 3],
    pub uv: [f32; 2],
    pub attr: u32,
    pub texture: u32,
    pub palette: u32,
}

/// Consecutive triangles drawn with the same blending, depth and stencil
/// state. `id` is the translucent polygon ID (64 for opaque polygons).
#[derive(Clone, Debug)]
pub struct Batch {
    pub range: std::ops::Range<u32>,
    pub translucent: bool,
    pub depth_equal: bool,
    pub depth_write: bool,
    pub id: u8,
}

impl Batch {
    fn same_state(&self, other: &Self) -> bool {
        (
            self.translucent,
            self.depth_equal,
            self.depth_write,
            self.id,
        ) == (
            other.translucent,
            other.depth_equal,
            other.depth_write,
            other.id,
        )
    }
}

/// Rows per band of the parallel rasterizer.
const BAND_ROWS: usize = 8;

/// The display registers the rasterizer reads.
struct RenderState {
    /// DISP3DCNT.
    control: u16,
    alpha_ref: u8,
    toon: [u16; 32],
    w_buffer: bool,
}

impl RenderState {
    fn read(io: &Io, w_buffer: bool) -> Self {
        Self {
            control: io.get16(0x0400_0060),
            alpha_ref: io.get8(0x0400_0340),
            toon: std::array::from_fn(|i| io.get16(0x0400_0380 + i as u32 * 2)),
            w_buffer,
        }
    }
}

/// The hardware's Y sort key: the bottom-most, then the top-most vertex.
fn sort_key(polygon: &Polygon) -> (i32, i32) {
    let height = f64::from(i32::from(polygon.viewport[3]) - i32::from(polygon.viewport[1]) + 1);
    let ys = polygon
        .vertices
        .iter()
        .filter(|v| v.clip[3] > 0.0)
        .map(|v| {
            let y = 191.0
                - ((v.clip[1] / v.clip[3] + 1.0) * height / 2.0 + f64::from(polygon.viewport[1]));
            y.clamp(-1024.0, 1024.0) as i32
        });
    let (low, high) = ys.fold((i32::MIN, i32::MAX), |(low, high), y| {
        (low.max(y), high.min(y))
    });
    (low, high)
}

/// How far inside clip plane `plane` a point is (negative: outside). The
/// left and right planes (0 and 1) are at `reach` times w.
fn plane_distance(c: [f64; 4], plane: usize, reach: f64) -> f64 {
    let w = if plane < 2 { c[3] * reach } else { c[3] };
    w + if plane & 1 == 0 {
        c[plane / 2]
    } else {
        -c[plane / 2]
    }
}

fn clip_polygon(input: &[Vertex], reach: f64) -> Vec<Vertex> {
    let mut vertices = input.to_vec();
    for plane in 0..6 {
        let old = std::mem::take(&mut vertices);
        if old.is_empty() {
            break;
        }
        let mut previous = *old.last().unwrap();
        let mut pd = plane_distance(previous.clip, plane, reach);
        for current in old {
            let cd = plane_distance(current.clip, plane, reach);
            if (pd < 0.0) != (cd < 0.0) {
                vertices.push(previous.lerp(current, pd / (pd - cd)));
            }
            if cd >= 0.0 {
                vertices.push(current);
            }
            previous = current;
            pd = cd;
        }
    }
    vertices
}

fn texture_coordinate(value: f64, size: usize, repeat: bool, flip: bool) -> usize {
    let n = value.floor() as i32;
    if !repeat {
        return n.clamp(0, size as i32 - 1) as usize;
    }
    let period = size as i32 * if flip { 2 } else { 1 };
    let n = n.rem_euclid(period) as usize;
    if n >= size { size * 2 - 1 - n } else { n }
}

fn texture_sample(vram: &Vram, param: u32, palette: u32, uv: [f64; 2]) -> Pixel {
    let format = (param >> 26) & 7;
    if format == 0 {
        return Pixel {
            color: 0x7fff,
            alpha: 31,
        };
    }
    let width = 8 << ((param >> 20) & 7);
    let height = 8 << ((param >> 23) & 7);
    let s = texture_coordinate(uv[0], width, param & (1 << 16) != 0, param & (1 << 18) != 0);
    let t = texture_coordinate(
        uv[1],
        height,
        param & (1 << 17) != 0,
        param & (1 << 19) != 0,
    );
    let index = t * width + s;
    let base = (param as usize & 0xffff) * 8;
    let palette = palette as usize * if format == 2 { 8 } else { 16 };
    let pal = |index| vram.read16(Region::TexturePalette, palette + index * 2) & 0x7fff;
    let byte = |at| vram.read8(Region::Texture, at);
    let mut alpha = 31;
    let color = match format {
        7 => {
            let c = vram.read16(Region::Texture, base + index * 2);
            alpha = if c & 0x8000 != 0 { 31 } else { 0 };
            c & 0x7fff
        }
        5 => {
            let block = (t / 4) * (width / 4) + s / 4;
            let at = base + block * 4;
            let texel = (byte(at + t % 4) >> (2 * (s % 4))) & 3;
            let descriptor = vram.read16(
                Region::Texture,
                0x20000 + (at & 0x1ffff) / 2 + if at & 0x40000 != 0 { 0x10000 } else { 0 },
            );
            let offset = usize::from(descriptor & 0x3fff) * 4;
            let color = |n| vram.read16(Region::TexturePalette, palette + offset + n * 2) & 0x7fff;
            let mode = descriptor >> 14;
            match (mode, texel) {
                (0 | 1, 3) => {
                    alpha = 0;
                    0
                }
                (1, 2) | (3, 2 | 3) => {
                    let weight = if mode == 1 {
                        4.0
                    } else if texel == 2 {
                        5.0
                    } else {
                        3.0
                    };
                    let a = rgb(u32::from(color(0)));
                    let b = rgb(u32::from(color(1)));
                    pack(std::array::from_fn(|i| {
                        ((a[i] * weight + b[i] * (8.0 - weight)) / 8.0).floor()
                    }))
                }
                _ => color(texel as usize),
            }
        }
        _ => {
            let (color, a) = match format {
                1 => {
                    let b = byte(base + index);
                    let a = b >> 5;
                    (b & 31, a * 4 + a / 2)
                }
                2 => ((byte(base + index / 4) >> ((index % 4) * 2)) & 3, 31),
                3 => ((byte(base + index / 2) >> ((index % 2) * 4)) & 15, 31),
                4 => (byte(base + index), 31),
                6 => {
                    let b = byte(base + index);
                    (b & 7, b >> 3)
                }
                _ => unreachable!(),
            };
            alpha = if matches!(format, 2..=4) && color == 0 && param & (1 << 29) != 0 {
                0
            } else {
                a
            };
            pal(color as usize)
        }
    };
    Pixel { color, alpha }
}

/// A triangle of a clipped polygon, set up once for every band it covers.
struct Triangle<'a> {
    polygon: &'a Polygon,
    /// Drawn in the translucent group.
    translucent: bool,
    vertices: [Vertex; 3],
    screen: [[f64; 2]; 3],
    area: f64,
    inverse_area: f64,
    /// Edges (1,2), (2,0) and (0,1) that the half-open fill rule leaves out.
    excluded: [bool; 3],
    /// Per vertex: 1/w, z/w, and color and texture coordinates over w.
    inverse_w: [f64; 3],
    depth_w: [f64; 3],
    attributes_w: [[f64; 5]; 3],
    min_x: usize,
    max_x: usize,
    min_y: usize,
    max_y: usize,
}

const EDGES: [(usize, usize); 3] = [(1, 2), (2, 0), (0, 1)];

impl<'a> Triangle<'a> {
    /// None if nothing of the triangle is drawn.
    fn new(vertices: [Vertex; 3], polygon: &'a Polygon, translucent: bool) -> Option<Self> {
        if vertices.iter().any(|v| v.clip[3] <= 0.0) {
            return None;
        }
        let polygon_alpha = (polygon.attr >> 16) & 31;
        // Wireframe and shadow polygons need edge/stencil rasterization.
        if polygon_alpha == 0 || (polygon.attr >> 4) & 3 == 3 {
            return None;
        }
        let vp = polygon.viewport;
        let width = i32::from(vp[2]) - i32::from(vp[0]) + 1;
        let height = i32::from(vp[3]) - i32::from(vp[1]) + 1;
        let screen = vertices.map(|v| {
            [
                (v.clip[0] / v.clip[3] + 1.0) * f64::from(width) / 2.0 + f64::from(vp[0]),
                191.0
                    - ((v.clip[1] / v.clip[3] + 1.0) * f64::from(height) / 2.0 + f64::from(vp[1])),
            ]
        });
        let area = edge(screen[0], screen[1], screen[2]);
        if area.abs() < 1e-10 {
            return None;
        }
        let front = area < 0.0;
        if polygon.attr & (if front { 1 << 7 } else { 1 << 6 }) == 0 {
            return None;
        }
        // Half-open triangle edges prevent a quad's shared diagonal from
        // blending twice (or hiding the far half of a translucent polygon).
        let excluded = EDGES.map(|(a, b)| {
            let (a, b) = if area > 0.0 {
                (screen[a], screen[b])
            } else {
                (screen[b], screen[a])
            };
            !(b[1] < a[1] || b[1] == a[1] && b[0] > a[0])
        });
        let bound = |axis: usize, limit: f64| {
            let low = screen.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
            let high = screen
                .iter()
                .map(|p| p[axis])
                .fold(f64::NEG_INFINITY, f64::max);
            (
                low.floor().clamp(0.0, limit) as usize,
                high.ceil().clamp(0.0, limit) as usize,
            )
        };
        let (min_x, max_x) = bound(0, WIDTH as f64);
        let (min_y, max_y) = bound(1, HEIGHT as f64);
        let inverse_w = vertices.map(|v| 1.0 / v.clip[3]);
        Some(Self {
            polygon,
            translucent,
            vertices,
            screen,
            area,
            inverse_area: 1.0 / area,
            excluded,
            inverse_w,
            depth_w: std::array::from_fn(|i| vertices[i].clip[2] / vertices[i].clip[3]),
            attributes_w: std::array::from_fn(|i| {
                let v = vertices[i];
                [v.color[0], v.color[1], v.color[2], v.uv[0], v.uv[1]].map(|a| a * inverse_w[i])
            }),
            min_x,
            max_x,
            min_y,
            max_y,
        })
    }

    /// The columns of row `y` that can be inside the triangle, with a pixel
    /// of margin either side; the per-pixel test decides.
    fn span(&self, y: usize) -> std::ops::Range<usize> {
        let py = y as f64 + 0.5;
        let sign = self.area.signum();
        let tolerance = 1e-10 * self.area.abs();
        let (mut low, mut high) = (self.min_x as f64, self.max_x as f64);
        for (a, b) in EDGES {
            let (a, b) = (self.screen[a], self.screen[b]);
            // sign * edge(a, b, (px, py)) = constant - slope * px, which
            // must not fall below -tolerance.
            let slope = sign * (b[1] - a[1]);
            let constant = sign * ((b[0] - a[0]) * (py - a[1]) + (b[1] - a[1]) * a[0]);
            if slope == 0.0 {
                if constant < -tolerance {
                    return 0..0;
                }
                continue;
            }
            let px = (constant + tolerance) / slope - 0.5;
            if slope > 0.0 {
                high = high.min(px.ceil() + 2.0);
            } else {
                low = low.max(px.floor() - 1.0);
            }
        }
        let low = low.clamp(self.min_x as f64, self.max_x as f64) as usize;
        let high = high.clamp(self.min_x as f64, self.max_x as f64) as usize;
        low..high.max(low)
    }

    /// Draw `rows` into a band of the frame that starts at row `top`.
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        rows: std::ops::Range<usize>,
        top: usize,
        state: &RenderState,
        vram: &Vram,
        pixels: &mut [Pixel],
        depths: &mut [f64],
        ids: &mut [u32],
    ) {
        let polygon = self.polygon;
        let screen = self.screen;
        let control = state.control;
        let w_buffer = state.w_buffer;
        let id = (polygon.attr >> 24) & 63;
        let polygon_alpha = ((polygon.attr >> 16) & 31) as u8;
        let mode = (polygon.attr >> 4) & 3;
        let depth_equal = polygon.attr & (1 << 14) != 0;
        let translucent_depth = polygon.attr & (1 << 11) != 0;
        let textured = control & 1 != 0;
        for y in rows {
            for x in self.span(y) {
                let point = [x as f64 + 0.5, y as f64 + 0.5];
                let weights = [
                    edge(screen[1], screen[2], point) * self.inverse_area,
                    edge(screen[2], screen[0], point) * self.inverse_area,
                    edge(screen[0], screen[1], point) * self.inverse_area,
                ];
                if weights.iter().any(|w| *w < -1e-10) {
                    continue;
                }
                if (0..3).any(|i| self.excluded[i] && weights[i].abs() <= 1e-10) {
                    continue;
                }
                let reciprocal_w = weights[0] * self.inverse_w[0]
                    + weights[1] * self.inverse_w[1]
                    + weights[2] * self.inverse_w[2];
                let depth = if w_buffer {
                    1.0 / reciprocal_w
                } else {
                    (weights[0] * self.depth_w[0]
                        + weights[1] * self.depth_w[1]
                        + weights[2] * self.depth_w[2])
                        * 0.5
                        + 0.5
                };
                let at = (y - top) * WIDTH + x;
                if depth_equal {
                    if (depth - depths[at]).abs() > if w_buffer { 0.125 } else { 0.000125 } {
                        continue;
                    }
                } else if depth >= depths[at] {
                    continue;
                }
                let w = 1.0 / reciprocal_w;
                let a = &self.attributes_w;
                let interpolated: [f64; 5] = std::array::from_fn(|i| {
                    (weights[0] * a[0][i] + weights[1] * a[1][i] + weights[2] * a[2][i]) * w
                });
                let vertex_color = [interpolated[0], interpolated[1], interpolated[2]];
                let tex = if textured {
                    texture_sample(
                        vram,
                        polygon.texture,
                        polygon.palette,
                        [interpolated[3], interpolated[4]],
                    )
                } else {
                    Pixel {
                        color: 0x7fff,
                        alpha: 31,
                    }
                };
                let alpha = if mode == 1 {
                    polygon_alpha
                } else {
                    (((u32::from(tex.alpha) + 1) * (u32::from(polygon_alpha) + 1) - 1) / 32) as u8
                };
                if alpha == 0 || control & 4 != 0 && alpha <= state.alpha_ref {
                    continue;
                }
                let texture_color = rgb(u32::from(tex.color));
                let color = match mode {
                    1 => std::array::from_fn(|i| {
                        (texture_color[i] * f64::from(tex.alpha)
                            + vertex_color[i] * f64::from(31 - tex.alpha))
                            / 31.0
                    }),
                    2 => {
                        let toon = rgb(u32::from(
                            state.toon[vertex_color[0].clamp(0.0, 31.0) as usize],
                        ));
                        std::array::from_fn(|i| {
                            if control & 2 == 0 {
                                texture_color[i] * toon[i] / 31.0
                            } else {
                                (texture_color[i] * vertex_color[i] / 31.0 + toon[i]).min(31.0)
                            }
                        })
                    }
                    _ => std::array::from_fn(|i| texture_color[i] * vertex_color[i] / 31.0),
                };
                if alpha < 31 {
                    if ids[at] == id {
                        continue;
                    }
                    if control & 8 != 0 && pixels[at].alpha != 0 {
                        let old = rgb(u32::from(pixels[at].color));
                        pixels[at].color = pack(std::array::from_fn(|i| {
                            (color[i] * f64::from(alpha + 1) + old[i] * f64::from(31 - alpha))
                                / 32.0
                        }));
                        pixels[at].alpha = pixels[at].alpha.max(alpha);
                    } else {
                        pixels[at] = Pixel {
                            color: pack(color),
                            alpha,
                        };
                    }
                    ids[at] = id;
                } else {
                    pixels[at] = Pixel {
                        color: pack(color),
                        alpha,
                    };
                    ids[at] = 64;
                }
                if alpha == 31 || translucent_depth {
                    depths[at] = depth;
                }
            }
        }
    }
}

fn edge(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(g: &mut Geometry, cmd: u8, parameters: &[u32]) {
        let address = 0x0400_0400 + u32::from(cmd) * 4;
        if parameters.is_empty() {
            g.write(address, 0);
        }
        for &p in parameters {
            g.write(address, p);
        }
    }

    fn vertex(g: &mut Geometry, x: i16, y: i16, z: i16) {
        command(
            g,
            0x23,
            &[
                u32::from(x as u16) | (u32::from(y as u16) << 16),
                u32::from(z as u16),
            ],
        );
    }

    fn triangle(g: &mut Geometry, color: u32, z: i16, alpha: u32, id: u32) {
        command(g, 0x29, &[(alpha << 16) | 0xc0 | (id << 24)]);
        command(g, 0x40, &[0]);
        command(g, 0x20, &[color]);
        vertex(g, -2048, -2048, z);
        vertex(g, 2048, -2048, z);
        vertex(g, 0, 2048, z);
        command(g, 0x41, &[]);
    }

    fn io() -> Io {
        let mut io = Io::new();
        io.set16(0x0400_0354, 0x7fff);
        io
    }

    #[test]
    fn packed_commands_wait_for_all_parameters_and_execute_zero_parameter_commands() {
        let mut g = Geometry::default();
        // MODE, IDENTITY, TRANSLATE, PUSH in one packed command word.
        g.write(0x0400_0400, 0x111c1510);
        g.write(0x0400_0400, 2);
        g.write(0x0400_0400, 4096);
        g.write(0x0400_0400, 8192);
        assert_eq!(g.position_sp, 0);
        g.write(0x0400_0400, 12288);
        assert_eq!(g.position_sp, 1);
        assert_eq!(&g.matrices[1][12..15], &[4096, 8192, 12288]);
        assert_eq!(g.matrices[1], g.matrices[2]);
        assert!(g.commands.is_empty());
    }

    #[test]
    fn matrices_premultiply_and_mode_one_restores_both_stacks() {
        let mut g = Geometry::default();
        command(&mut g, 0x10, &[2]);
        command(&mut g, 0x1c, &[4096, 8192, 0]);
        command(&mut g, 0x11, &[]);
        command(&mut g, 0x1b, &[8192, 12288, 4096]);
        let result = transform([1.0, 1.0, 0.0, 1.0], g.matrices[1]);
        assert_eq!(result, [3.0, 5.0, 0.0, 1.0]);
        assert_eq!(g.matrices[2][0], 4096); // scale never affects directions
        command(&mut g, 0x10, &[1]);
        command(&mut g, 0x15, &[]);
        command(&mut g, 0x12, &[1]);
        assert_eq!(g.matrices[1], g.matrices[2]);
        assert_eq!(g.read(0x0400_0670), Some(4096));
        assert_eq!(g.position_sp, 0);
    }

    #[test]
    fn swap_waits_for_vblank_and_defers_following_list() {
        let mut g = Geometry::default();
        triangle(&mut g, 31, 0, 31, 0);
        command(&mut g, 0x50, &[0]);
        assert!(g.displayed.polygons.is_empty());
        triangle(&mut g, 31 << 5, 0, 31, 0);
        command(&mut g, 0x50, &[0]);
        g.vblank();
        assert_eq!(g.displayed.polygons.len(), 1);
        assert_eq!(g.displayed.polygons[0].vertices[0].color, [31.0, 0.0, 0.0]);
        assert_eq!(g.building.len(), 1);
        g.vblank();
        assert_eq!(g.displayed.polygons[0].vertices[0].color, [0.0, 31.0, 0.0]);
        g.vblank();
        assert_eq!(g.displayed.polygons.len(), 1);
        assert_eq!(g.swaps, 2);
    }

    #[test]
    fn depth_buffer_and_translucency_work_over_opaque_polygon_with_same_id() {
        let mut g = Geometry::default();
        triangle(&mut g, 31, 1024, 31, 0);
        triangle(&mut g, 31 << 10, 2048, 31, 1); // farther: hidden
        triangle(&mut g, 31 << 5, 0, 15, 0); // same ID as opaque: must blend
        command(&mut g, 0x50, &[0]);
        g.vblank();
        let mut io = io();
        io.set16(0x0400_0060, 8);
        let pixels = g.render(&io, &Vram::new());
        let pixel = pixels[96 * WIDTH + 128];
        assert_eq!(pixel.color, 16 | (16 << 5));
        assert_eq!(pixel.alpha, 31);
        assert_eq!(pixels[0].alpha, 0);
    }

    #[test]
    fn near_plane_clipping_keeps_visible_piece() {
        let mut g = Geometry::default();
        command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
        command(&mut g, 0x40, &[0]);
        command(&mut g, 0x20, &[31]);
        vertex(&mut g, -2048, -2048, 0);
        vertex(&mut g, 2048, -2048, 0);
        vertex(&mut g, 0, 2048, -8192);
        command(&mut g, 0x50, &[0]);
        g.vblank();
        let pixels = g.render(&io(), &Vram::new());
        assert_eq!(pixels[125 * WIDTH + 128].alpha, 31);
        assert_eq!(pixels[60 * WIDTH + 128].alpha, 0);
    }

    fn texture_vram() -> Vram {
        let mut v = Vram::new();
        v.control[0] = 0x83;
        v.control[1] = 0x8b; // texture slots 0 and 1
        v.control[4] = 0x83; // palette
        v.update();
        for (i, c) in [31u16, 31 << 5, 31 << 10, 0x7fff].into_iter().enumerate() {
            v.banks[0x80000 + i * 2..0x80002 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        v
    }

    #[test]
    fn palette_alpha_and_direct_texture_formats() {
        let mut v = texture_vram();
        v.banks[0] = 0b1110_0100;
        for (x, expected) in [31, 31 << 5, 31 << 10, 0x7fff].into_iter().enumerate() {
            assert_eq!(
                texture_sample(&v, 2 << 26, 0, [x as f64, 0.0]).color,
                expected
            );
        }
        assert_eq!(
            texture_sample(&v, (2 << 26) | (1 << 29), 0, [0.0, 0.0]).alpha,
            0
        );
        v.banks[0] = 0xa1;
        assert_eq!(texture_sample(&v, 1 << 26, 0, [0.0, 0.0]).alpha, 22);
        assert_eq!(texture_sample(&v, 6 << 26, 0, [0.0, 0.0]).alpha, 20);
        v.banks[0..2].copy_from_slice(&0x801fu16.to_le_bytes());
        let p = texture_sample(&v, 7 << 26, 0, [0.0, 0.0]);
        assert_eq!((p.color, p.alpha), (31, 31));
    }

    #[test]
    fn compressed_texture_modes_and_slot_two_descriptors() {
        let mut v = texture_vram();
        v.banks[0] = 0b1110_0100;
        for mode in 0..4u16 {
            v.banks[0x20000..0x20002].copy_from_slice(&(mode << 14).to_le_bytes());
            let p2 = texture_sample(&v, 5 << 26, 0, [2.0, 0.0]);
            let p3 = texture_sample(&v, 5 << 26, 0, [3.0, 0.0]);
            assert_eq!(p3.alpha, if mode < 2 { 0 } else { 31 });
            assert_eq!(
                p2.color,
                match mode {
                    1 => 15 | (15 << 5),
                    3 => 19 | (11 << 5),
                    _ => 31 << 10,
                }
            );
        }
        v.control[2] = 0x93;
        v.update();
        v.banks[0x40000] = 0b0101_0101;
        v.banks[0x30000..0x30002].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            texture_sample(&v, (5 << 26) | 0x8000, 0, [0.0, 0.0]).color,
            31 << 5
        );
    }

    #[test]
    fn texture_clamp_repeat_and_mirror_include_negative_coordinates() {
        assert_eq!(texture_coordinate(-1.0, 8, false, false), 0);
        assert_eq!(texture_coordinate(-1.0, 8, true, false), 7);
        assert_eq!(texture_coordinate(-1.0, 8, true, true), 0);
        assert_eq!(texture_coordinate(8.0, 8, true, true), 7);
        assert_eq!(texture_coordinate(16.0, 8, true, true), 0);
    }

    #[test]
    fn textured_triangles_interpolate_uv_with_reciprocal_w() {
        let mut g = Geometry::default();
        Arc::make_mut(&mut g.displayed).polygons.push(Polygon {
            vertices: vec![
                Vertex {
                    clip: [-1.0, -1.0, 0.0, 1.0],
                    uv: [0.0, 0.0],
                    color: [31.0; 3],
                },
                Vertex {
                    clip: [2.0, -2.0, 0.0, 2.0],
                    uv: [8.0, 0.0],
                    color: [31.0; 3],
                },
                Vertex {
                    clip: [-1.0, 1.0, 0.0, 1.0],
                    uv: [0.0, 0.0],
                    color: [31.0; 3],
                },
            ],
            attr: (31 << 16) | 0xc0,
            texture: 2 << 26,
            palette: 0,
            viewport: [0, 0, 255, 191],
            id: PolygonId::default(),
        });
        let mut v = texture_vram();
        v.banks[0] = 0xe4;
        v.banks[1] = 0xe4;
        let mut io = io();
        io.set16(0x0400_0060, 1);
        let pixels = g.render(&io, &v);
        // Affine interpolation would sample texel 4 (red), while correct
        // perspective interpolation samples texel 2 (blue).
        assert_eq!(pixels[130 * WIDTH + 128].color, 31 << 10);
    }

    /// A swapped list of the test triangle drawn at these x offsets (in
    /// clip units, 4096 = 1.0), all looking the same.
    fn list_at(offsets: &[i32]) -> Arc<DisplayList> {
        let mut g = Geometry::default();
        command(&mut g, 0x10, &[2]);
        for &x in offsets {
            command(&mut g, 0x15, &[]);
            command(&mut g, 0x1c, &[x as u32, 0, 0]);
            triangle(&mut g, 31, 0, 31, 0);
        }
        command(&mut g, 0x50, &[0]);
        g.vblank();
        g.displayed.clone()
    }

    /// The leftmost screen x of each triangle `build` makes.
    fn left_edges(triangles: &Triangles) -> Vec<f32> {
        triangles
            .vertices
            .chunks(3)
            .map(|t| {
                t.iter()
                    .map(|v| v.position[0])
                    .fold(f32::INFINITY, f32::min)
            })
            .collect()
    }

    #[test]
    fn interpolation_starts_at_the_shown_list_and_moves_toward_the_next() {
        let (shown, next) = (list_at(&[0]), list_at(&[410]));
        let alone = shown.build(None, 0.0, false);
        let start = shown.build(Some(&next), 0.0, false);
        assert_eq!(
            bytemuck::cast_slice::<_, u8>(&alone.vertices),
            bytemuck::cast_slice::<_, u8>(&start.vertices)
        );
        // -0.5 is x 64; 0.1 further right is 12.8 pixels.
        assert_eq!(left_edges(&alone), [64.0]);
        let half = shown.build(Some(&next), 0.5, false);
        assert!(
            (left_edges(&half)[0] - 70.4).abs() < 0.1,
            "{:?}",
            left_edges(&half)
        );
        assert_eq!((half.matched.matched, half.matched.total), (1, 1));
        let end = left_edges(&shown.build(Some(&next), 1.0, false))[0];
        assert!((end - 76.8).abs() < 0.1, "{end}");
    }

    #[test]
    fn a_polygon_culled_in_one_list_does_not_shift_the_pairs_after_it() {
        // The first triangle leaves the view (3.0 is off screen) in the
        // next list; the second still pairs with itself, not the first.
        let shown = list_at(&[0, 819]);
        let next = list_at(&[12288, 1024]);
        assert_eq!(next.polygons.len(), 1);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!((half.matched.matched, half.matched.total), (1, 2));
        let mut edges = left_edges(&half);
        edges.sort_by(f32::total_cmp);
        // The first stays at 64; the second moves from 0.2 to 0.25, so
        // from 89.6 halfway to 96.
        assert_eq!(edges[0], 64.0);
        assert!((edges[1] - 92.8).abs() < 0.1, "{edges:?}");
    }

    #[test]
    fn a_polygon_that_jumps_is_drawn_where_it_is() {
        let (shown, next) = (list_at(&[0]), list_at(&[4096]));
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 0);
        assert_eq!(left_edges(&half), [64.0]);
    }

    #[test]
    fn texture_stack_does_not_change_projection_stack_pointer() {
        let mut g = Geometry::default();
        command(&mut g, 0x10, &[3]);
        command(&mut g, 0x11, &[]);
        assert_eq!(g.status() & (1 << 13), 0);
        command(&mut g, 0x12, &[1]);
        assert_eq!(g.texture_sp, 0);
    }

    #[test]
    fn quad_and_triangle_strips_keep_consistent_winding() {
        for primitive in [2, 3] {
            let mut g = Geometry::default();
            command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
            command(&mut g, 0x40, &[primitive]);
            for (x, y) in [(-2048, -2048), (2048, -2048), (-2048, 2048), (2048, 2048)] {
                vertex(&mut g, x, y, 0);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            let pixels = g.render(&io(), &Vram::new());
            assert_eq!(pixels.iter().filter(|p| p.alpha != 0).count(), 128 * 96);
        }
    }

    #[test]
    fn normal_uses_material_and_transformed_light() {
        let mut g = Geometry::default();
        command(&mut g, 0x32, &[512 << 20]); // -Z
        command(&mut g, 0x33, &[0x7fff]);
        command(&mut g, 0x30, &[31]); // diffuse red
        command(&mut g, 0x29, &[(31 << 16) | 0xc1]);
        command(&mut g, 0x40, &[0]);
        command(&mut g, 0x21, &[511 << 20]); // +Z
        assert!(g.color[0] > 30.0);
        assert_eq!(g.color[1], 0.0);
    }

    #[test]
    fn io_routes_fifo_dma_and_matrix_result_subword_reads() {
        let mut io = Io::new();
        io.write(0x0400_0440, 4, 1);
        io.write(0x0400_0470, 4, 0x12345678);
        io.write(0x0400_0470, 4, 0);
        io.write(0x0400_0470, 4, 0);
        assert_eq!(io.read(0x0400_0670, 4), 0x12345678);
        assert_eq!(io.read(0x0400_0672, 2), 0x1234);
        assert!(matches!(
            io.write(super::super::io::DMA_BASE + 8, 4, 0xbc000004),
            Some(super::super::io::Effect::Dma(0))
        ));
    }

    #[test]
    fn three_d_bg0_composes_with_two_d_backdrop_and_brightness() {
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        triangle(&mut io.geometry, 31, 0, 15, 1);
        command(&mut io.geometry, 0x50, &[0]);
        io.advance_to_vblank();
        io.set16(0x0400_0304, 0x800a); // A on top, 2D + 3D render enabled
        io.set32(0x0400_0000, 0x10108); // normal display, BG0 3D
        io.set16(0x0400_0050, 1 << 13); // backdrop second target
        memory.palette[0..2].copy_from_slice(&(31u16 << 10).to_le_bytes());
        let frame = super::super::gpu2d::render(&io, &memory);
        let at = (96 * WIDTH + 128) * 4;
        assert_eq!(&frame.top[at..at + 4], &[123, 0, 123, 255]);
        io.set16(0x0400_006c, 0x8010); // master brightness black
        let frame = super::super::gpu2d::render(&io, &memory);
        assert_eq!(&frame.top[at..at + 4], &[0, 0, 0, 255]);
    }

    #[test]
    fn gpu_renderer_matches_software_and_scales_the_3d_layer() {
        let Ok(mut gpu) = crate::runtime::scaled::Headless::new() else {
            eprintln!("no graphics adapter; skipping");
            return;
        };
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        // An opaque red triangle, and a translucent blue one over it and
        // over the backdrop, with a semi-transparent 2D backdrop target.
        triangle(&mut io.geometry, 31, 1024, 31, 0);
        command(&mut io.geometry, 0x29, &[(15 << 16) | 0xc0 | (1 << 24)]);
        command(&mut io.geometry, 0x40, &[0]);
        command(&mut io.geometry, 0x20, &[31 << 10]);
        vertex(&mut io.geometry, -4096, -1024, 0);
        vertex(&mut io.geometry, 1024, -1024, 0);
        vertex(&mut io.geometry, 1024, 3072, 0);
        command(&mut io.geometry, 0x50, &[0]);
        io.advance_to_vblank();
        io.set16(0x0400_0304, 0x800a);
        io.set32(0x0400_0000, 0x10108);
        io.set16(0x0400_0060, 8); // alpha blending
        io.set16(0x0400_0050, 1 << 13);
        memory.palette[0..2].copy_from_slice(&(31u16 << 5).to_le_bytes());
        let software = super::super::gpu2d::render(&io, &memory);
        let scaled = super::super::gpu2d::render_scaled(&io, &memory);
        let (width, height, pixels) = gpu.render(&scaled, 1).unwrap();
        assert_eq!((width, height), (256, 384));
        // Blends that land on a half may round either way (one step of 31);
        // nothing may differ by more.
        let differing = software
            .top
            .chunks(4)
            .zip(pixels.chunks(4))
            .filter(|(a, b)| (0..3).any(|i| a[i].abs_diff(b[i]) > 9))
            .count();
        assert_eq!(differing, 0);
        let (width, _, doubled) = gpu.render(&scaled, 2).unwrap();
        assert_eq!(width, 512);
        for (x, y) in [(128, 96), (100, 130), (20, 20)] {
            let one = &pixels[(y * 256 + x) * 4..][..4];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let two = &doubled[((2 * y + dy) * 512 + 2 * x + dx) * 4..][..4];
                assert_eq!(one, two, "pixel {x},{y}");
            }
        }
    }

    /// A red triangle right of the screen, x 1.05 to 1.3, which only
    /// widescreen's extra columns (up to x 1.336) show: DS columns 262 to
    /// 294, rows 47 to 143.
    fn beside_the_screen(g: &mut Geometry) {
        command(g, 0x29, &[(31 << 16) | 0xc0]);
        command(g, 0x40, &[0]);
        command(g, 0x20, &[31]);
        vertex(g, 4301, -2048, 0);
        vertex(g, 5325, -2048, 0);
        vertex(g, 4813, 2048, 0);
        command(g, 0x41, &[]);
    }

    #[test]
    fn widescreen_keeps_what_is_beside_the_screen_outside_polygon_ram() {
        for wide in [false, true] {
            let mut g = Geometry {
                wide,
                ..Default::default()
            };
            triangle(&mut g, 31, 0, 31, 0);
            beside_the_screen(&mut g);
            // The game sees only the polygon on the DS's own screen.
            assert_eq!(g.read(0x0400_0604), Some(1 | (3 << 16)));
            // A box test beside the screen passes only in widescreen.
            command(
                &mut g,
                0x70,
                &[
                    4301 | (u32::from(-2048i16 as u16) << 16),
                    1024 << 16,
                    4096 | (1024 << 16),
                ],
            );
            assert_eq!(g.status() & 2 != 0, wide);
            command(&mut g, 0x50, &[0]);
            g.vblank();
            assert_eq!(g.displayed.polygons.len(), if wide { 2 } else { 1 });
            // A 4:3 build clips it away; a widescreen one draws it beyond
            // column 256.
            let right = |triangles: &Triangles| {
                triangles
                    .vertices
                    .iter()
                    .map(|v| v.position[0])
                    .fold(f32::NEG_INFINITY, f32::max)
            };
            assert_eq!(right(&g.displayed.build(None, 0.0, false)), 192.0);
            let widened = g.displayed.build(None, 0.0, true);
            if wide {
                assert!((right(&widened) - 294.4).abs() < 0.1);
            } else {
                assert_eq!(right(&widened), 192.0);
            }
        }
    }

    #[test]
    fn widescreen_widens_the_top_screen_around_the_same_picture() {
        let Ok(mut gpu) = crate::runtime::scaled::Headless::new() else {
            eprintln!("no graphics adapter; skipping");
            return;
        };
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        io.geometry.wide = true;
        triangle(&mut io.geometry, 31 << 10, 1024, 31, 0);
        beside_the_screen(&mut io.geometry);
        command(&mut io.geometry, 0x50, &[0]);
        io.advance_to_vblank();
        io.set16(0x0400_0304, 0x800a); // A on top, 2D + 3D render enabled
        io.set32(0x0400_0000, 0x10108); // normal display, BG0 3D
        memory.palette[0..2].copy_from_slice(&(31u16 << 5).to_le_bytes());
        let software = super::super::gpu2d::render(&io, &memory);
        let scaled = super::super::gpu2d::render_scaled(&io, &memory);
        assert_eq!(scaled.extra, WIDE_EXTRA);
        let (width, height, pixels) = gpu.render(&scaled, 1).unwrap();
        assert_eq!((width, height), (342, 384));
        let at = |x: usize, y: usize| &pixels[(y * 342 + x) * 4..][..4];
        // The middle of the top screen is the DS's picture.
        for y in 0..192 {
            for x in 0..256 {
                let ds = &software.top[(y * 256 + x) * 4..][..4];
                assert_eq!(at(x + 43, y), ds, "pixel {x},{y}");
            }
        }
        assert_eq!(at(43 + 128, 96), [0, 0, 255, 255]);
        // Beside it, the top screen shows the 3D layer over the backdrop;
        // the bottom screen has nothing there.
        assert_eq!(at(43 + 278, 96), [255, 0, 0, 255]);
        assert_eq!(at(5, 96), [0, 255, 0, 255]);
        assert_eq!(at(5, 192 + 96), [0, 0, 0, 255]);
        assert_eq!(at(341, 192 + 96), [0, 0, 0, 255]);
    }

    #[test]
    fn display_capture_wraps_destination_and_ignores_master_brightness() {
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        memory.vram.control[2] = 0x80;
        memory.vram.update();
        memory.palette[0..2].copy_from_slice(&31u16.to_le_bytes());
        io.set32(0x0400_0000, 0x10000);
        io.set16(0x0400_006c, 0x8010);
        // Capture engine A into bank C, starting at 96 KiB, 256x192.
        io.set32(0x0400_0064, 0x8000_0000 | (3 << 20) | (3 << 18) | (2 << 16));
        super::super::gpu2d::capture(&mut io, &mut memory);
        assert_eq!(memory.vram.read16(Region::Lcdc, 0x58000), 0x801f);
        assert_eq!(memory.vram.read16(Region::Lcdc, 0x40000), 0x801f);
        assert_eq!(io.get32(0x0400_0064) >> 31, 0);
    }

    #[test]
    fn display_capture_blends_vram_and_three_d_with_transparent_alpha() {
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        memory.vram.control[0] = 0x80;
        memory.vram.control[1] = 0x80;
        memory.vram.update();
        for pixel in memory.vram.banks[..0x20000].chunks_mut(2) {
            pixel.copy_from_slice(&0xfc00u16.to_le_bytes());
        }
        io.set32(0x0400_0350, 31 | (31 << 16)); // opaque red 3D clear
        io.set32(0x0400_0064, 0xc101_0808); // A=3D + B=bank A -> bank B, half each
        super::super::gpu2d::capture(&mut io, &mut memory);
        assert_eq!(
            memory.vram.read16(Region::Lcdc, 0x20000),
            0x8000 | 15 | (15 << 10)
        );
        io.set32(0x0400_0350, 31); // transparent red must not contribute
        io.set32(0x0400_0064, 0xc101_0808);
        super::super::gpu2d::capture(&mut io, &mut memory);
        assert_eq!(
            memory.vram.read16(Region::Lcdc, 0x20000),
            0x8000 | (15 << 10)
        );
    }
}
