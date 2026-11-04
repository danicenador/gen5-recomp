//! DS geometry commands and a frame-based software rasterizer.
//!
//! Register formats and matrix conventions follow GBATEK's DS 3D chapters:
//! https://mgba-emu.github.io/gbatek/#ds-3d-video
//! Commands execute synchronously, except SWAP_BUFFERS which waits for vblank.
//! This is a functional renderer, not a cycle-accurate FIFO or rasterizer.

use std::collections::VecDeque;

use super::io::Io;
use super::vram::{Region, Vram};

const WIDTH: usize = 256;
const HEIGHT: usize = 192;
type Matrix = [i32; 16];

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
    std::array::from_fn(|c| {
        (0..4)
            .map(|r| v[r] * f64::from(m[r * 4 + c]) / 4096.0)
            .sum()
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
}

pub struct Geometry {
    matrices: [Matrix; 4], // projection, position, direction, texture
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
    render_w: bool,
    /// SWAP_BUFFERS bit 0: translucent polygons keep submission order
    /// instead of being sorted by Y.
    manual_sort: bool,
    /// Whether the previous polygon of the current strip was stored, so the
    /// next one shares its vertices in vertex RAM.
    strip_stored: bool,
    building: Vec<Polygon>,
    displayed: Vec<Polygon>,
    vertices: Vec<Vertex>,
    vertex_count: usize,
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
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            matrices: [identity(); 4],
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
            render_w: false,
            manual_sort: false,
            strip_stored: false,
            building: Vec::new(),
            displayed: Vec::new(),
            vertices: Vec::new(),
            vertex_count: 0,
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
                let params = std::mem::take(&mut self.parameters);
                self.execute(cmd, &params);
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
            let params = std::mem::take(&mut self.parameters);
            self.execute(cmd, &params);
        }
    }

    /// Publish the completed list at vblank, retaining it until the next swap.
    pub fn vblank(&mut self) {
        if let Some(flags) = self.swap.take() {
            self.render_w = flags & 2 != 0;
            self.manual_sort = flags & 1 != 0;
            self.peak_polygons = self.peak_polygons.max(self.building.len());
            self.peak_vertices = self.peak_vertices.max(self.vertex_count);
            self.peak_submitted = self.peak_submitted.max(self.submitted_polygons - self.list_start);
            self.list_start = self.submitted_polygons;
            self.overflowed_lists += u64::from(std::mem::take(&mut self.overflowed));
            self.displayed = std::mem::take(&mut self.building);
            self.vertex_count = 0;
            if let Some(text) = self.trace.take() {
                self.traced = Some(text);
            }
            if std::mem::take(&mut self.trace_next) {
                self.trace = Some(String::new());
            }
            self.swaps += 1;
            self.drain_commands();
            while self.swap.is_none() {
                let Some((address, value)) = self.deferred.pop_front() else {
                    break;
                };
                self.write(address, value);
            }
        }
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
            0x0400_0604 => (self.building.len() as u32) | ((self.vertex_count as u32) << 16),
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

    fn vertex(&mut self) {
        let Some(primitive) = self.primitive else {
            return;
        };
        if self.texture >> 30 == 3 {
            let mut p = self.position;
            p[3] = 0.0;
            self.texture_transform(p, 3);
        }
        self.vertices.push(Vertex {
            clip: transform(self.position, multiply(self.matrices[1], self.matrices[0])),
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
        let verdict = self.visible(&vertices);
        let mut traced = None;
        let stored = if verdict.is_ok() {
            let shared = if self.strip_stored && matches!(primitive, 2 | 3) {
                count - 2
            } else {
                count
            };
            let fits = self.building.len() < 2048 && self.vertex_count + shared <= 6144;
            if fits {
                self.vertex_count += shared;
                self.building.push(Polygon {
                    vertices: if self.trace.is_some() { vertices.clone() } else { std::mem::take(&mut vertices) },
                    attr: self.attr,
                    texture: self.texture,
                    palette: self.palette,
                    viewport: self.viewport,
                });
            }
            self.overflowed |= !fits;
            if !fits {
                traced = Some("full");
            }
            fits
        } else {
            false
        };
        if let Some(trace) = &mut self.trace {
            use std::fmt::Write;
            let status = verdict.err().or(traced).unwrap_or("kept");
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
    /// outside one clip plane, it does not cross the far plane unless the
    /// polygon asks for that, and its facing is drawn.
    fn visible(&self, vertices: &[Vertex]) -> Result<(), &'static str> {
        if (0..6).any(|plane| vertices.iter().all(|v| plane_distance(v.clip, plane) < 0.0)) {
            return Err("out");
        }
        if self.attr & (1 << 12) == 0 && vertices.iter().any(|v| v.clip[2] > v.clip[3]) {
            return Err("far");
        }
        if vertices.iter().all(|v| v.clip[3] > 0.0) {
            // Facing, in the same screen orientation the rasterizer uses
            // (y grows downwards).
            let screen: Vec<[f64; 2]> = vertices
                .iter()
                .map(|v| [v.clip[0] / v.clip[3], -v.clip[1] / v.clip[3]])
                .collect();
            let mut area = 0.0;
            for i in 0..screen.len() {
                let (a, b) = (screen[i], screen[(i + 1) % screen.len()]);
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
                let clip = multiply(self.matrices[1], self.matrices[0]);
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
                self.box_result =
                    !(0..6).any(|plane| corners.iter().all(|c| plane_distance(*c, plane) < 0.0));
                if let Some(trace) = &mut self.trace {
                    use std::fmt::Write;
                    let _ = writeln!(trace, "box    {values:?} -> {}", self.box_result);
                }
            }
            0x71 => {
                self.position_result = transform(
                    [
                        f64::from(v as i16) / 4096.0,
                        f64::from((v >> 16) as i16) / 4096.0,
                        f64::from(p[1] as i16) / 4096.0,
                        1.0,
                    ],
                    multiply(self.matrices[1], self.matrices[0]),
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
        let clear = io.get32(0x0400_0350);
        let mut pixels = vec![
            Pixel {
                color: clear as u16 & 0x7fff,
                alpha: ((clear >> 16) & 31) as u8
            };
            WIDTH * HEIGHT
        ];
        let clear_depth = f64::from(io.get16(0x0400_0354) & 0x7fff) / 32767.0;
        let mut depths = vec![
            if self.render_w {
                clear_depth * 4096.0
            } else {
                clear_depth
            };
            WIDTH * HEIGHT
        ];
        // Only a previously drawn translucent polygon suppresses another
        // translucent polygon with the same ID; opaque IDs do not.
        let mut ids = vec![64; WIDTH * HEIGHT];
        // Opaque polygons precede translucent ones. Preserve submission order
        // within each group; hardware's automatic Y sort is not modeled yet.
        for translucent in [false, true] {
            let mut group: Vec<&Polygon> = self
                .displayed
                .iter()
                .filter(|polygon| {
                    let alpha = (polygon.attr >> 16) & 31;
                    let format = (polygon.texture >> 26) & 7;
                    (alpha < 31 || format == 1 || format == 6) == translucent
                })
                .collect();
            // The hardware sorts opaque polygons, and translucent ones
            // unless SWAP_BUFFERS asked for manual order, by their lowest
            // then highest screen Y. The sort is stable, like submission
            // order within equal keys.
            if !translucent || !self.manual_sort {
                group.sort_by_cached_key(|polygon| sort_key(polygon));
            }
            for polygon in group {
                let vertices = clip_polygon(&polygon.vertices);
                if vertices.len() < 3 {
                    continue;
                }
                for i in 1..vertices.len() - 1 {
                    rasterize(
                        [vertices[0], vertices[i], vertices[i + 1]],
                        polygon,
                        io,
                        vram,
                        self.render_w,
                        &mut pixels,
                        &mut depths,
                        &mut ids,
                    );
                }
            }
        }
        pixels
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
    let (low, high) = ys.fold((i32::MIN, i32::MAX), |(low, high), y| (low.max(y), high.min(y)));
    (low, high)
}

fn plane_distance(c: [f64; 4], plane: usize) -> f64 {
    c[3] + if plane & 1 == 0 {
        c[plane / 2]
    } else {
        -c[plane / 2]
    }
}

fn clip_polygon(input: &[Vertex]) -> Vec<Vertex> {
    let mut vertices = input.to_vec();
    for plane in 0..6 {
        let old = std::mem::take(&mut vertices);
        if old.is_empty() {
            break;
        }
        let mut previous = *old.last().unwrap();
        let mut pd = plane_distance(previous.clip, plane);
        for current in old {
            let cd = plane_distance(current.clip, plane);
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

#[allow(clippy::too_many_arguments)]
fn rasterize(
    vertices: [Vertex; 3],
    polygon: &Polygon,
    io: &Io,
    vram: &Vram,
    w_buffer: bool,
    pixels: &mut [Pixel],
    depths: &mut [f64],
    ids: &mut [u32],
) {
    if vertices.iter().any(|v| v.clip[3] <= 0.0) {
        return;
    }
    let vp = polygon.viewport;
    let width = i32::from(vp[2]) - i32::from(vp[0]) + 1;
    let height = i32::from(vp[3]) - i32::from(vp[1]) + 1;
    let screen = vertices.map(|v| {
        [
            (v.clip[0] / v.clip[3] + 1.0) * f64::from(width) / 2.0 + f64::from(vp[0]),
            191.0 - ((v.clip[1] / v.clip[3] + 1.0) * f64::from(height) / 2.0 + f64::from(vp[1])),
        ]
    });
    let edge = |a: [f64; 2], b: [f64; 2], p: [f64; 2]| {
        (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
    };
    let area = edge(screen[0], screen[1], screen[2]);
    if area.abs() < 1e-10 {
        return;
    }
    let front = area < 0.0;
    if polygon.attr & (if front { 1 << 7 } else { 1 << 6 }) == 0 {
        return;
    }
    let min_x = screen
        .iter()
        .map(|p| p[0])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .clamp(0.0, 256.0) as usize;
    let max_x = screen
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .clamp(0.0, 256.0) as usize;
    let min_y = screen
        .iter()
        .map(|p| p[1])
        .fold(f64::INFINITY, f64::min)
        .floor()
        .clamp(0.0, 192.0) as usize;
    let max_y = screen
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .clamp(0.0, 192.0) as usize;
    let control = io.get16(0x0400_0060);
    let id = (polygon.attr >> 24) & 63;
    let polygon_alpha = ((polygon.attr >> 16) & 31) as u8;
    // Wireframe and shadow polygons need edge/stencil rasterization.
    if polygon_alpha == 0 || (polygon.attr >> 4) & 3 == 3 {
        return;
    }
    for y in min_y..max_y {
        for x in min_x..max_x {
            let point = [x as f64 + 0.5, y as f64 + 0.5];
            let weights = [
                edge(screen[1], screen[2], point) / area,
                edge(screen[2], screen[0], point) / area,
                edge(screen[0], screen[1], point) / area,
            ];
            if weights.iter().any(|w| *w < -1e-10) {
                continue;
            }
            // Half-open triangle edges prevent a quad's shared diagonal from
            // blending twice (or hiding the far half of a translucent polygon).
            let mut on_excluded_edge = false;
            for (i, &(a, b)) in [(1, 2), (2, 0), (0, 1)].iter().enumerate() {
                if weights[i].abs() > 1e-10 {
                    continue;
                }
                let (a, b) = if area > 0.0 {
                    (screen[a], screen[b])
                } else {
                    (screen[b], screen[a])
                };
                if !(b[1] < a[1] || b[1] == a[1] && b[0] > a[0]) {
                    on_excluded_edge = true;
                }
            }
            if on_excluded_edge {
                continue;
            }
            let reciprocal_w: f64 = (0..3).map(|i| weights[i] / vertices[i].clip[3]).sum();
            let depth = if w_buffer {
                1.0 / reciprocal_w
            } else {
                (0..3)
                    .map(|i| weights[i] * vertices[i].clip[2] / vertices[i].clip[3])
                    .sum::<f64>()
                    * 0.5
                    + 0.5
            };
            let at = y * WIDTH + x;
            if polygon.attr & (1 << 14) != 0 {
                if (depth - depths[at]).abs() > if w_buffer { 0.125 } else { 0.000125 } {
                    continue;
                }
            } else if depth >= depths[at] {
                continue;
            }
            let interpolate = |get: &dyn Fn(Vertex) -> f64| {
                (0..3)
                    .map(|i| weights[i] * get(vertices[i]) / vertices[i].clip[3])
                    .sum::<f64>()
                    / reciprocal_w
            };
            let vertex_color: [f64; 3] = std::array::from_fn(|i| interpolate(&|v| v.color[i]));
            let uv = std::array::from_fn(|i| interpolate(&|v| v.uv[i]));
            let tex = if control & 1 != 0 {
                texture_sample(vram, polygon.texture, polygon.palette, uv)
            } else {
                Pixel {
                    color: 0x7fff,
                    alpha: 31,
                }
            };
            let texture_color = rgb(u32::from(tex.color));
            let mode = (polygon.attr >> 4) & 3;
            let alpha = if mode == 1 {
                polygon_alpha
            } else {
                (((u32::from(tex.alpha) + 1) * (u32::from(polygon_alpha) + 1) - 1) / 32) as u8
            };
            if alpha == 0 || control & 4 != 0 && alpha <= io.get8(0x0400_0340) {
                continue;
            }
            let color = match mode {
                1 => std::array::from_fn(|i| {
                    (texture_color[i] * f64::from(tex.alpha)
                        + vertex_color[i] * f64::from(31 - tex.alpha))
                        / 31.0
                }),
                2 => {
                    let toon = rgb(u32::from(
                        io.get16(0x0400_0380 + vertex_color[0].clamp(0.0, 31.0) as u32 * 2),
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
                        (color[i] * f64::from(alpha + 1) + old[i] * f64::from(31 - alpha)) / 32.0
                    }));
                    pixels[at].alpha = pixels[at].alpha.max(alpha);
                } else {
                    pixels[at] = Pixel {
                        color: pack(color),
                        alpha,
                    };
                }
            } else {
                pixels[at] = Pixel {
                    color: pack(color),
                    alpha,
                };
            }
            if alpha < 31 {
                ids[at] = id;
            } else {
                ids[at] = 64;
            }
            if alpha == 31 || polygon.attr & (1 << 11) != 0 {
                depths[at] = depth;
            }
        }
    }
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
        assert!(g.displayed.is_empty());
        triangle(&mut g, 31 << 5, 0, 31, 0);
        command(&mut g, 0x50, &[0]);
        g.vblank();
        assert_eq!(g.displayed.len(), 1);
        assert_eq!(g.displayed[0].vertices[0].color, [31.0, 0.0, 0.0]);
        assert_eq!(g.building.len(), 1);
        g.vblank();
        assert_eq!(g.displayed[0].vertices[0].color, [0.0, 31.0, 0.0]);
        g.vblank();
        assert_eq!(g.displayed.len(), 1);
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
        g.displayed.push(Polygon {
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
