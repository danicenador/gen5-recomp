//! DS geometry commands and a frame-based software rasterizer.
//!
//! Register formats and matrix conventions follow GBATEK's DS 3D chapters:
//! https://mgba-emu.github.io/gbatek/#ds-3d-video
//! Commands execute synchronously, except SWAP_BUFFERS which waits for vblank.
//! This is a functional renderer, not a cycle-accurate FIFO or rasterizer.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
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
    /// The submitted model-space coordinates, before camera/animation matrices.
    model: [i32; 3],
    /// Submitted UVs identify the mesh even when a texture matrix animates it.
    raw_uv: [f64; 2],
    clip: [f64; 4],
    color: [f64; 3],
    uv: [f64; 2],
}

impl Vertex {
    fn lerp(self, other: Self, t: f64) -> Self {
        Self {
            model: self.model,
            raw_uv: self.raw_uv,
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
    /// The id without the texture coordinates (`PolygonId::geometry` of
    /// the model-space vertices only, no occurrence). A sprite drawn from a
    /// sheet, such as the walking player, keeps it when its animation frame
    /// changes.
    shape: PolygonId,
    /// The model-to-clip matrix (position times projection) its vertices
    /// were transformed with. A static object's matrix changes between two
    /// lists only by the camera, which is how interpolation finds the
    /// camera's motion (`camera_motion`).
    matrix: Matrix,
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
    /// Model-space vertices and texture coordinates, independent of the camera.
    geometry: u64,
    occurrence: u32,
}

/// The attr bits that identify a polygon: everything but its alpha.
const ID_ATTR_MASK: u32 = !(31 << 16);

/// A swapped list: the polygons the rasterizer draws until the next swap.
#[derive(Clone, Default)]
pub struct DisplayList {
    polygons: Vec<Polygon>,
    /// Submission counts include culled polygons. If repeated geometry changes
    /// count, occurrence numbers cannot reliably identify the same object.
    submitted: HashMap<PolygonId, u32>,
    /// Submission counts by `Polygon::shape`, culled polygons included.
    shapes: HashMap<PolygonId, u32>,
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
        let Some(next) = next.filter(|next| alpha > 0.0 && self.render_w == next.render_w) else {
            return (Cow::Borrowed(&self.polygons), Matched { matched: 0, total });
        };
        let alpha = alpha.min(1.0);
        let identity = |id: PolygonId| PolygonId {
            occurrence: 0,
            ..id
        };
        let mut earlier: HashMap<PolygonId, Vec<&Polygon>> = HashMap::new();
        let mut later: HashMap<PolygonId, Vec<&Polygon>> = HashMap::new();
        for polygon in &self.polygons {
            earlier
                .entry(identity(polygon.id))
                .or_default()
                .push(polygon);
        }
        for polygon in &next.polygons {
            later.entry(identity(polygon.id)).or_default().push(polygon);
        }
        // Meshes submitted once to each list pair up directly, and give the
        // camera's motion between the lists.
        let unique = |polygon: &Polygon, id: &PolygonId| {
            earlier[id].len() == 1
                && later.get(id).is_some_and(|after| after.len() == 1)
                && self.submitted.get(id) == Some(&1)
                && next.submitted.get(id) == Some(&1)
                && pairs_with(polygon, later[id][0])
        };
        let direct: Vec<_> = self
            .polygons
            .iter()
            .filter_map(|polygon| {
                let id = identity(polygon.id);
                unique(polygon, &id).then(|| (polygon, later[&id][0]))
            })
            .collect();
        let camera = camera_motion(&direct);
        let back = camera.and_then(invert);
        // Without it, fall back on the screen motion of unique meshes.
        // Repeated fence segments, grass and shore tiles can enter/leave the
        // submitted list or change order; their occurrence number is not an
        // object identity.
        let mut motion = [Vec::new(), Vec::new()];
        for (id, before) in &earlier {
            if before.len() == 1
                && let Some(after) = later.get(id).filter(|after| after.len() == 1)
                && pairs_with(before[0], after[0])
            {
                let delta = screen_motion(before[0], after[0]);
                for axis in 0..2 {
                    motion[axis].push(delta[axis]);
                }
            }
        }
        let shift = (!motion[0].is_empty()).then(|| {
            std::array::from_fn(|axis| {
                motion[axis].sort_by(f64::total_cmp);
                motion[axis][motion[axis].len() / 2]
            })
        });
        let partners = object_partners(self, next);
        let pairs: Vec<_> = self
            .polygons
            .iter()
            .map(|polygon| {
                let id = identity(polygon.id);
                let before = &earlier[&id];
                let placed = later.get(&id).and_then(|candidates| {
                    if unique(polygon, &id) {
                        return Some(candidates[0]);
                    }
                    // Repeated meshes (stepping stones, flags, fence posts)
                    // pair with the copy found where the camera's motion
                    // takes them, if exactly one is there.
                    if let (Some(camera), Some(back)) = (&camera, &back) {
                        let candidate = predicted_polygon(polygon, candidates, camera)?;
                        let reverse = predicted_polygon(candidate, before, back)?;
                        return (reverse.id == polygon.id).then_some(candidate);
                    }
                    // Submitted once to both lists but not paired above:
                    // it jumped or changed shape.
                    if self.submitted.get(&id) == Some(&1) && next.submitted.get(&id) == Some(&1) {
                        return None;
                    }
                    if shift.is_none() && self.submitted.get(&id) != next.submitted.get(&id) {
                        return None;
                    }
                    let tolerance = shift.map(|_| 3.0);
                    let shift = shift.unwrap_or([0.0; 2]);
                    let candidate = nearest_polygon(polygon, candidates, shift, tolerance)?;
                    let reverse = nearest_polygon(candidate, before, shift.map(|v| -v), tolerance)?;
                    (reverse.id == polygon.id).then_some(candidate)
                });
                // Repeated meshes neither placed: the copy in the object
                // the polygon's object became (`object_partners`).
                placed.or_else(|| {
                    partners
                        .get(&polygon.matrix)
                        .and_then(|object| object.get(&id))
                        .copied()
                })
            })
            .collect();
        // A sprite drawn from a sheet picks its animation frame by texture
        // coordinates, so a new frame (each step of the walking player) is a
        // new id. Moved with the camera instead, the player slid with the
        // scenery for the in-between frames and snapped back at the next
        // list, a ghost that fast-forward, whose shown lists are a few game
        // frames apart, showed on nearly every frame. What is left pairs
        // with the polygon of the same shape in the next list, provided
        // each list submitted just one.
        let mut shaped: HashMap<PolygonId, &Polygon> = HashMap::new();
        for polygon in &next.polygons {
            if next.shapes.get(&polygon.shape) == Some(&1) {
                shaped.insert(polygon.shape, polygon);
            }
        }
        let pairs: Vec<_> = self
            .polygons
            .iter()
            .zip(pairs)
            .map(|(polygon, pair)| {
                pair.or_else(|| {
                    let candidate = *shaped.get(&polygon.shape)?;
                    (self.shapes.get(&polygon.shape) == Some(&1)
                        && candidate.id != polygon.id
                        && moves_to(polygon, candidate))
                    .then_some(candidate)
                })
            })
            .collect();
        // What could not be paired still moves: with the paired polygons
        // drawn through the same matrix (the rest of that object), or else
        // with the camera. Left where it was, it would stand still for the
        // in-between frames while everything around it moves on.
        let mut objects: HashMap<Matrix, Option<Matrix>> = HashMap::new();
        for (polygon, after) in self.polygons.iter().zip(&pairs) {
            if let Some(after) = after {
                let entry = objects.entry(polygon.matrix).or_insert(Some(after.matrix));
                if *entry != Some(after.matrix) {
                    *entry = None;
                }
            }
        }
        let follow = |polygon: &Polygon| -> Option<Vec<[f64; 4]>> {
            let object = match objects.get(&polygon.matrix) {
                Some(Some(after)) => motion_between(&polygon.matrix, after),
                Some(None) => None,
                None => camera,
            }?;
            polygon
                .vertices
                .iter()
                .map(|vertex| followed(vertex.clip, &object))
                .collect()
        };
        // Different materials can share a geometric boundary. If just one
        // material changes between frames, leaving its edge behind opens a
        // crack (notably between a battle's ground and sky). Move shared
        // endpoints together, provided every paired neighbor agrees.
        let key = |polygon: &Polygon, vertex: &Vertex| {
            (
                polygon.viewport,
                vertex
                    .clip
                    .map(|value| if value == 0.0 { 0 } else { value.to_bits() }),
            )
        };
        let shape = |polygon: &Polygon| {
            let mut vertices: Vec<_> = polygon
                .vertices
                .iter()
                .map(|vertex| key(polygon, vertex).1)
                .collect();
            vertices.sort_unstable();
            (polygon.viewport, vertices)
        };
        let mut shared = HashMap::new();
        let mut matched_shapes = std::collections::HashSet::new();
        for (polygon, after) in self.polygons.iter().zip(&pairs) {
            if let Some(after) = after {
                matched_shapes.insert(shape(polygon));
                for (vertex, after) in polygon.vertices.iter().zip(&after.vertices) {
                    let target = shared
                        .entry(key(polygon, vertex))
                        .or_insert(Some(after.clip));
                    if target.is_some_and(|position| position != after.clip) {
                        *target = None;
                    }
                }
            }
        }
        let matched = pairs.iter().filter(|pair| pair.is_some()).count();
        let polygons = self
            .polygons
            .iter()
            .zip(pairs)
            .map(|(before, after)| {
                let mut polygon = before.clone();
                if let Some(after) = after {
                    for (vertex, after) in polygon.vertices.iter_mut().zip(&after.vertices) {
                        let moved = vertex.lerp(*after, alpha);
                        vertex.clip = moved.clip;
                        vertex.color = moved.color;
                    }
                } else {
                    // Complete coincident meshes can be separate objects;
                    // only their material-independent boundaries are shared.
                    let boundaries = !matched_shapes.contains(&shape(before));
                    let follows = follow(before);
                    for (index, vertex) in polygon.vertices.iter_mut().enumerate() {
                        let target = match shared.get(&key(before, vertex)) {
                            Some(Some(target)) if boundaries => Some(*target),
                            _ => follows.as_ref().map(|targets| targets[index]),
                        };
                        if let Some(target) = target {
                            vertex.clip = std::array::from_fn(|axis| {
                                vertex.clip[axis] + (target[axis] - vertex.clip[axis]) * alpha
                            });
                        }
                    }
                }
                polygon
            })
            .collect();
        (Cow::Owned(polygons), Matched { matched, total })
    }
}

/// A motion in clip space between two lists: a vertex at `clip` in the
/// first is at `clip · motion` in the second (row vectors, as `transform`).
type Motion = [[f64; 4]; 4];

fn to_motion(m: &Matrix) -> Motion {
    std::array::from_fn(|r| std::array::from_fn(|c| f64::from(m[r * 4 + c]) / 4096.0))
}

fn compose(a: &Motion, b: &Motion) -> Motion {
    std::array::from_fn(|r| std::array::from_fn(|c| (0..4).map(|k| a[r][k] * b[k][c]).sum()))
}

fn moved(v: [f64; 4], m: &Motion) -> [f64; 4] {
    std::array::from_fn(|c| (0..4).map(|k| v[k] * m[k][c]).sum())
}

/// Gauss-Jordan elimination with partial pivoting.
fn invert(m: Motion) -> Option<Motion> {
    let mut a = m;
    let mut inverse: Motion =
        std::array::from_fn(|r| std::array::from_fn(|c| if r == c { 1.0 } else { 0.0 }));
    let scale = m.iter().flatten().fold(0.0f64, |max, v| max.max(v.abs()));
    if scale == 0.0 || !scale.is_finite() {
        return None;
    }
    for column in 0..4 {
        let pivot =
            (column..4).max_by(|&x, &y| a[x][column].abs().total_cmp(&a[y][column].abs()))?;
        if a[pivot][column].abs() <= scale * 1e-9 {
            return None;
        }
        a.swap(column, pivot);
        inverse.swap(column, pivot);
        let divisor = a[column][column];
        for c in 0..4 {
            a[column][c] /= divisor;
            inverse[column][c] /= divisor;
        }
        for row in 0..4 {
            if row != column {
                let factor = a[row][column];
                for c in 0..4 {
                    a[row][c] -= factor * a[column][c];
                    inverse[row][c] -= factor * inverse[column][c];
                }
            }
        }
    }
    Some(inverse)
}

/// How a mesh drawn through `before` in one list and `after` in the next
/// moved: exactly the camera's motion if the mesh itself stood still,
/// since its model matrix cancels out.
fn motion_between(before: &Matrix, after: &Matrix) -> Option<Motion> {
    Some(compose(&invert(to_motion(before))?, &to_motion(after)))
}

/// A clip-space position in DS pixels from the screen's center, if it is
/// in front of the camera.
fn on_screen(clip: [f64; 4]) -> Option<[f64; 2]> {
    (clip[3] > 0.0).then(|| [clip[0] / clip[3] * 128.0, clip[1] / clip[3] * 96.0])
}

/// Where `motion` takes a vertex, unless it goes behind the camera or
/// jumps further than a cut (`SNAP_PIXELS`).
fn followed(clip: [f64; 4], motion: &Motion) -> Option<[f64; 4]> {
    let target = moved(clip, motion);
    let (from, to) = (on_screen(clip)?, on_screen(target)?);
    ((to[0] - from[0]).abs() <= SNAP_PIXELS && (to[1] - from[1]).abs() <= SNAP_PIXELS)
        .then_some(target)
}

/// A camera motion predicts a paired vertex within this many DS pixels.
const CAMERA_TOLERANCE: f64 = 0.5;

/// The camera's motion between two lists, from meshes paired between them
/// (`direct`). Each mesh drawn through one matrix proposes the motion that
/// takes its matrix to its partner's; the proposal that also explains the
/// most other meshes wins, so characters and animated parts that move on
/// their own are outvoted by the scenery. At least two meshes must agree.
fn camera_motion(direct: &[(&Polygon, &Polygon)]) -> Option<Motion> {
    type Samples = Vec<([f64; 4], [f64; 4])>;
    // Per matrix of the first list: its partner matrix (None if its
    // polygons disagree) and some vertex positions to check proposals on.
    let mut order = Vec::new();
    let mut objects: HashMap<Matrix, (Option<Matrix>, Samples)> = HashMap::new();
    for (before, after) in direct {
        let object = objects.entry(before.matrix).or_insert_with(|| {
            order.push(before.matrix);
            (Some(after.matrix), Vec::new())
        });
        if object.0 != Some(after.matrix) {
            object.0 = None;
        }
        for (a, b) in before.vertices.iter().zip(&after.vertices) {
            if object.1.len() < 8 {
                object.1.push((a.clip, b.clip));
            }
        }
    }
    let proposals: Vec<_> = order
        .iter()
        .filter_map(|matrix| {
            let (after, samples) = &objects[matrix];
            Some((motion_between(matrix, after.as_ref()?)?, samples))
        })
        .collect();
    let explains = |motion: &Motion, samples: &Samples| {
        samples.iter().all(|&(before, after)| {
            let (Some(predicted), Some(actual)) =
                (on_screen(moved(before, motion)), on_screen(after))
            else {
                return false;
            };
            (predicted[0] - actual[0]).abs() <= CAMERA_TOLERANCE
                && (predicted[1] - actual[1]).abs() <= CAMERA_TOLERANCE
        })
    };
    let mut best: Option<(usize, Motion)> = None;
    for (motion, _) in &proposals {
        let support = proposals
            .iter()
            .filter(|(_, samples)| explains(motion, samples))
            .count();
        if best.is_none_or(|(most, _)| support > most) {
            best = Some((support, *motion));
        }
    }
    best.filter(|&(support, _)| support >= 2)
        .map(|(_, motion)| motion)
}

/// The one candidate whose vertices are where `motion` takes `polygon`'s,
/// within `CAMERA_TOLERANCE` and with no other candidate as close.
fn predicted_polygon<'a>(
    polygon: &Polygon,
    candidates: &[&'a Polygon],
    motion: &Motion,
) -> Option<&'a Polygon> {
    let predicted: Vec<_> = polygon
        .vertices
        .iter()
        .map(|vertex| on_screen(moved(vertex.clip, motion)))
        .collect::<Option<_>>()?;
    let mut best = None;
    let mut distance = f64::INFINITY;
    let mut ambiguous = false;
    for &candidate in candidates {
        if !pairs_with(polygon, candidate) {
            continue;
        }
        let Some(score) = candidate
            .vertices
            .iter()
            .zip(&predicted)
            .map(|(vertex, predicted)| {
                on_screen(vertex.clip).map(|actual| {
                    (actual[0] - predicted[0])
                        .abs()
                        .max((actual[1] - predicted[1]).abs())
                })
            })
            .try_fold(0.0f64, |worst, error| Some(worst.max(error?)))
        else {
            continue;
        };
        if score + 1e-6 < distance {
            ambiguous = false;
            best = Some(candidate);
            distance = score;
        } else if (score - distance).abs() < 1e-6 {
            ambiguous = true;
        }
    }
    (!ambiguous && distance <= CAMERA_TOLERANCE)
        .then_some(best)
        .flatten()
}

fn screen_motion(before: &Polygon, after: &Polygon) -> [f64; 2] {
    std::array::from_fn(|axis| {
        before
            .vertices
            .iter()
            .zip(&after.vertices)
            .map(|(a, b)| {
                (b.clip[axis] / b.clip[3] - a.clip[axis] / a.clip[3])
                    * if axis == 0 { 128.0 } else { 96.0 }
            })
            .sum::<f64>()
            / before.vertices.len() as f64
    })
}

fn nearest_polygon<'a>(
    polygon: &Polygon,
    candidates: &[&'a Polygon],
    camera: [f64; 2],
    tolerance: Option<f64>,
) -> Option<&'a Polygon> {
    let mut best = None;
    let mut distance = f64::INFINITY;
    let mut ambiguous = false;
    for &candidate in candidates {
        if !pairs_with(polygon, candidate) {
            continue;
        }
        let delta = screen_motion(polygon, candidate);
        let score = (delta[0] - camera[0]).powi(2) + (delta[1] - camera[1]).powi(2);
        if score + 1e-8 < distance {
            best = Some(candidate);
            distance = score;
            ambiguous = false;
        } else if (score - distance).abs() < 1e-8 {
            ambiguous = true;
        }
    }
    (!ambiguous && tolerance.is_none_or(|limit| distance <= limit * limit))
        .then_some(best)
        .flatten()
}

/// Objects (the polygons drawn through one matrix) of one list paired with
/// those of the next, for meshes that repeat across objects: a battle draws
/// both platforms from one mesh, so each piece of a platform has a copy in
/// the other. The camera's motion places such copies, but a battle's
/// backdrop moves with the camera, so the motion most meshes agree on can
/// be a Pokémon sprite growing as it comes out of its ball, which places
/// neither platform; moved with it instead, a platform slid over the ground
/// without its depth following and sank under it, so fast-forward, whose
/// shown lists are a few game frames apart, lost the floor on frames in
/// between. Each object pairs with the object of the next list holding the
/// most of its meshes within reach (`pairs_with`), the nearest on screen
/// among those holding as many, provided no other is nearly as near and no
/// other object pairs with the same one. Objects that draw a mesh twice are
/// left out, their copies told apart by the camera if at all, and so are
/// those with a mesh drawn more or fewer times in the second list: with a
/// copy added or gone, which one went where is anyone's guess. The map
/// gives, per matrix of the first list, its meshes' polygons in the second.
fn object_partners<'a>(
    first: &'a DisplayList,
    second: &'a DisplayList,
) -> HashMap<Matrix, HashMap<PolygonId, &'a Polygon>> {
    let (earlier, later) = (&first.polygons, &second.polygons);
    let identity = |id: PolygonId| PolygonId {
        occurrence: 0,
        ..id
    };
    // Per matrix, each mesh's polygon, or None for a mesh drawn twice.
    let group = |polygons: &'a [Polygon]| {
        let mut objects: HashMap<Matrix, HashMap<PolygonId, Option<&'a Polygon>>> = HashMap::new();
        for polygon in polygons {
            objects
                .entry(polygon.matrix)
                .or_default()
                .entry(identity(polygon.id))
                .and_modify(|slot| *slot = None)
                .or_insert(Some(polygon));
        }
        objects
    };
    let before = group(earlier);
    let after = group(later);
    let mut holders: HashMap<PolygonId, Vec<Matrix>> = HashMap::new();
    for (matrix, meshes) in &after {
        for id in meshes.keys() {
            holders.entry(*id).or_default().push(*matrix);
        }
    }
    let mut chosen: HashMap<Matrix, Matrix> = HashMap::new();
    let mut taken: HashMap<Matrix, usize> = HashMap::new();
    for (matrix, meshes) in &before {
        if meshes.values().any(Option::is_none)
            || meshes
                .keys()
                .any(|id| first.submitted.get(id) != second.submitted.get(id))
        {
            continue;
        }
        let mut candidates: Vec<Matrix> = meshes
            .keys()
            .filter_map(|id| holders.get(id))
            .flatten()
            .copied()
            .collect();
        candidates.sort_unstable();
        candidates.dedup();
        // (meshes within reach, mean distance on screen, matrix)
        let mut scores: Vec<(usize, f64, Matrix)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let mut reached = 0;
                let mut distance = 0.0;
                for (id, polygon) in meshes {
                    let (Some(polygon), Some(Some(other))) = (polygon, after[&candidate].get(id))
                    else {
                        continue;
                    };
                    if pairs_with(polygon, other) {
                        let delta = screen_motion(polygon, other);
                        reached += 1;
                        distance += delta[0].hypot(delta[1]);
                    }
                }
                (reached > 0).then(|| (reached, distance / reached as f64, candidate))
            })
            .collect();
        scores.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.total_cmp(&b.1)));
        let Some(&(reached, distance, best)) = scores.first() else {
            continue;
        };
        let close_second = scores.get(1).is_some_and(|&(other, other_distance, _)| {
            other == reached && other_distance <= distance * 2.0 + 1.0
        });
        if 2 * reached < meshes.len() || close_second {
            continue;
        }
        chosen.insert(*matrix, best);
        *taken.entry(best).or_default() += 1;
    }
    chosen
        .into_iter()
        .filter(|(_, partner)| taken[partner] == 1)
        .map(|(matrix, partner)| {
            let meshes = after[&partner]
                .iter()
                .filter_map(|(id, polygon)| Some((*id, (*polygon)?)))
                .collect();
            (matrix, meshes)
        })
        .collect()
}

/// Whether `before` can be drawn part way to `after`: the same shape, in
/// front of the camera both times, and not too far apart on screen.
fn pairs_with(before: &Polygon, after: &Polygon) -> bool {
    moves_to(before, after)
        && before
            .vertices
            .iter()
            .zip(&after.vertices)
            .all(|(a, b)| a.raw_uv == b.raw_uv)
}

/// `pairs_with` whatever the texture coordinates: the same model-space
/// shape, in front of the camera both times, and not too far apart.
fn moves_to(before: &Polygon, after: &Polygon) -> bool {
    if before.vertices.len() != after.vertices.len() || before.viewport != after.viewport {
        return false;
    }
    before.vertices.iter().zip(&after.vertices).all(|(a, b)| {
        if a.model != b.model {
            return false;
        }
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
    /// The same by `Polygon::shape`.
    shapes: HashMap<PolygonId, u32>,
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
            shapes: HashMap::new(),
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
                submitted: self.occurrences.clone(),
                shapes: std::mem::take(&mut self.shapes),
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
        let matrix = self.clip_matrix();
        let clip = transform(self.position, matrix);
        self.vertices.push(Vertex {
            model: std::array::from_fn(|i| (self.position[i] * 4096.0) as i32),
            raw_uv: self.raw_uv,
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
        let mut geometry = std::collections::hash_map::DefaultHasher::new();
        let mut model = std::collections::hash_map::DefaultHasher::new();
        for vertex in &vertices {
            vertex.model.hash(&mut geometry);
            vertex.raw_uv.map(f64::to_bits).hash(&mut geometry);
            vertex.model.hash(&mut model);
        }
        let mut id = PolygonId {
            texture: self.texture,
            palette: self.palette,
            attr: self.attr & ID_ATTR_MASK,
            vertices: count as u8,
            geometry: geometry.finish(),
            occurrence: 0,
        };
        let shape = PolygonId {
            geometry: model.finish(),
            ..id
        };
        let seen = self.occurrences.entry(id).or_default();
        id.occurrence = *seen;
        *seen += 1;
        *self.shapes.entry(shape).or_default() += 1;
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
                    shape,
                    matrix,
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
            let _ = write!(trace, " #{} model", id.occurrence);
            for v in &vertices {
                let _ = write!(
                    trace,
                    " [{} {} {} uv {} {}]",
                    v.model[0], v.model[1], v.model[2], v.raw_uv[0], v.raw_uv[1]
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
        let pixels = self.render_list(io, vram, false);
        self.render_nanos
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        pixels
    }

    /// `render` for widescreen: rows `WIDTH + 2 * WIDE_EXTRA` wide, the
    /// DS's columns in the middle, for display capture to keep what lies
    /// beside them.
    pub fn render_wide(&self, io: &Io, vram: &Vram) -> Vec<Pixel> {
        let start = std::time::Instant::now();
        let pixels = self.render_list(io, vram, true);
        self.render_nanos
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        pixels
    }

    /// Host time spent rendering since the last call, for the frame-time report.
    pub fn take_render_time(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.render_nanos.swap(0, Ordering::Relaxed))
    }

    fn render_list(&self, io: &Io, vram: &Vram, wide: bool) -> Vec<Pixel> {
        let columns = if wide { WIDTH + 2 * WIDE_EXTRA } else { WIDTH };
        let clear = io.get32(0x0400_0350);
        let mut pixels = vec![
            Pixel {
                color: clear as u16 & 0x7fff,
                alpha: ((clear >> 16) & 31) as u8
            };
            columns * HEIGHT
        ];
        let clear_depth = f64::from(io.get16(0x0400_0354) & 0x7fff) / 32767.0;
        let list = &*self.displayed;
        let mut depths = vec![
            if list.render_w {
                clear_depth * 4096.0
            } else {
                clear_depth
            };
            columns * HEIGHT
        ];
        // Only a previously drawn translucent polygon suppresses another
        // translucent polygon with the same ID; opaque IDs do not. Each
        // pixel's fog flag (`FOG_FLAG`) is kept with its ID; the rear
        // plane's is CLEAR_COLOR bit 15.
        let clear_fog = if clear & 0x8000 != 0 { FOG_FLAG } else { 0 };
        let mut ids = vec![64 | clear_fog; columns * HEIGHT];
        let state = RenderState::read(io, list.render_w);
        // The DS's own view: display capture and native-resolution screens
        // show exactly the screen's 256 columns. A wide render keeps what
        // widescreen shows beside them too.
        let mut triangles = triangles(&list.polygons, list.manual_sort, wide);
        if wide {
            triangles = triangles.into_iter().map(Triangle::widened).collect();
        }
        // Every pixel only depends on the polygons drawn over it, in order,
        // so bands of rows render in parallel with the same result.
        if !triangles.is_empty() {
            pixels
                .par_chunks_mut(BAND_ROWS * columns)
                .zip(depths.par_chunks_mut(BAND_ROWS * columns))
                .zip(ids.par_chunks_mut(BAND_ROWS * columns))
                .enumerate()
                .for_each(|(index, ((pixels, depths), ids))| {
                    let top = index * BAND_ROWS;
                    let bottom = top + pixels.len() / columns;
                    for triangle in &triangles {
                        let rows = triangle.min_y.max(top)..triangle.max_y.min(bottom);
                        if !rows.is_empty() {
                            triangle.draw(rows, top, &state, vram, pixels, depths, ids);
                        }
                    }
                });
        }
        let fog = Fog::read(io);
        if fog.enabled() {
            // The W-buffer holds w, which fills its 24 bits at 4096.
            let scale = if list.render_w { 1.0 / 4096.0 } else { 1.0 };
            for ((pixel, &depth), &id) in pixels.iter_mut().zip(&depths).zip(&ids) {
                if id & FOG_FLAG != 0 {
                    *pixel = fog.apply(*pixel, depth * scale);
                }
            }
        }
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
            clear_fog: clear & 0x8000 != 0,
            fog: Fog::read(io),
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
/// widescreen view (see `reach`) instead of the DS's, or, for a flat
/// picture drawn across the screen (`flat_across_screen`), shows its
/// reflection in the extra columns.
fn triangles(polygons: &[Polygon], manual_sort: bool, wide: bool) -> Vec<Triangle<'_>> {
    // Opaque polygons precede translucent ones. The hardware sorts
    // opaque polygons, and translucent ones unless SWAP_BUFFERS asked
    // for manual order, by their lowest then highest screen Y. The sort
    // is stable, like submission order within equal keys.
    let mut triangles = Vec::new();
    let mirrored = wide && flat_across_screen(polygons);
    fn fan<'a>(
        triangles: &mut Vec<Triangle<'a>>,
        vertices: &[Vertex],
        polygon: &'a Polygon,
        translucent: bool,
    ) {
        for i in 1..vertices.len().saturating_sub(1) {
            triangles.extend(Triangle::new(
                [vertices[0], vertices[i], vertices[i + 1]],
                polygon,
                translucent,
            ));
        }
    }
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
            if mirrored {
                for part in mirrored_parts(&polygon.vertices) {
                    fan(&mut triangles, &part, polygon, translucent);
                }
            } else {
                let vertices = clip_polygon(&polygon.vertices, reach(polygon.viewport, wide));
                fan(&mut triangles, &vertices, polygon, translucent);
            }
        }
    }
    triangles
}

/// Whether a list is a flat picture drawn across the whole screen, such as
/// the curtain that falls over the field as a wild battle starts in a cave
/// or the sewers: every polygon through the full-width viewport and at the
/// same depth from the eye (an orthographic projection, which no 3D scene
/// uses: even sprites facing the camera in a scene are at different
/// distances), reaching past both sides of the screen. Drawing what lies
/// beside the screen does not widen such a picture, as the game draws it
/// for the DS's 256 columns, so widescreen's extra columns show it
/// reflected at the screen's edges instead (`mirrored_parts`), as they do
/// the 2D layers under the 3D.
fn flat_across_screen(polygons: &[Polygon]) -> bool {
    let Some(w) = polygons
        .first()
        .and_then(|polygon| polygon.vertices.first())
        .map(|vertex| vertex.clip[3])
    else {
        return false;
    };
    let (mut left, mut right) = (false, false);
    for polygon in polygons {
        if polygon.viewport[0] != 0 || polygon.viewport[2] != 255 {
            return false;
        }
        for vertex in &polygon.vertices {
            if vertex.clip[3] != w {
                return false;
            }
            left |= vertex.clip[0] <= -w;
            right |= vertex.clip[0] >= w;
        }
    }
    w > 0.0 && left && right
}

/// A polygon of a picture drawn across the screen (`flat_across_screen`),
/// clipped to the DS's view, and reflected at its left and right edges and
/// clipped to the widescreen columns beyond them: column -1 - i shows
/// column i, column 256 + i column 255 - i.
fn mirrored_parts(vertices: &[Vertex]) -> [Vec<Vertex>; 3] {
    let reach = reach([0, 0, 255, 191], true);
    // `side` is -1 for the left edge (x = -w) and 1 for the right (x = w).
    let reflected = |side: f64| {
        // Reversed, so that the reflection faces the same way.
        let vertices: Vec<Vertex> = vertices
            .iter()
            .rev()
            .map(|vertex| {
                let mut vertex = *vertex;
                vertex.clip[0] = side * 2.0 * vertex.clip[3] - vertex.clip[0];
                vertex
            })
            .collect();
        let vertices = clip_polygon(&vertices, reach);
        clip_plane(&vertices, |clip| side * clip[0] - clip[3])
    };
    [clip_polygon(vertices, 1.0), reflected(-1.0), reflected(1.0)]
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
            let start = vertices.len() as u32;
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
            let end = vertices.len() as u32;
            // Alpha textures are sorted as translucent polygons, but their
            // opaque texels still write depth and bypass the translucent ID
            // test. Separate fragment passes keep those per-pixel semantics.
            let format = (triangle.polygon.texture >> 26) & 7;
            let split = triangle.translucent && (attr >> 16) & 31 == 31 && matches!(format, 1 | 6);
            let passes: &[(bool, AlphaTest)] = if split {
                &[(false, AlphaTest::Opaque), (true, AlphaTest::Translucent)]
            } else {
                &[(triangle.translucent, AlphaTest::All)]
            };
            for &(translucent, alpha_test) in passes {
                let batch = Batch {
                    range: start..end,
                    translucent,
                    alpha_test,
                    depth_equal: attr & (1 << 14) != 0,
                    depth_write: !translucent || attr & (1 << 11) != 0,
                    id: if translucent {
                        ((attr >> 24) & 63) as u8
                    } else {
                        64
                    },
                };
                match batches.last_mut() {
                    Some(last) if last.same_state(&batch) && last.range.end == start => {
                        last.range.end = end;
                    }
                    _ => batches.push(batch),
                }
            }
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
    /// Whether fog covers the rear plane (CLEAR_COLOR bit 15).
    pub clear_fog: bool,
    pub fog: Fog,
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

/// Which texel alpha values a GPU draw accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AlphaTest {
    All = 0,
    Opaque = 1,
    Translucent = 2,
}

/// Consecutive triangles drawn with the same blending, depth and stencil
/// state. `id` is the translucent polygon ID (64 for opaque polygons).
#[derive(Clone, Debug)]
pub struct Batch {
    pub range: std::ops::Range<u32>,
    pub translucent: bool,
    pub alpha_test: AlphaTest,
    pub depth_equal: bool,
    pub depth_write: bool,
    pub id: u8,
}

impl Batch {
    fn same_state(&self, other: &Self) -> bool {
        (
            self.translucent,
            self.alpha_test,
            self.depth_equal,
            self.depth_write,
            self.id,
        ) == (
            other.translucent,
            other.alpha_test,
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

/// A pixel's fog flag, kept beside its polygon ID while rasterizing: set by
/// an opaque polygon with fog enabled (POLYGON_ATTR bit 15), cleared by an
/// opaque or translucent polygon without it.
const FOG_FLAG: u32 = 1 << 8;

/// Fog (DISP3DCNT bit 7), applied to the finished 3D layer: each pixel
/// whose fog flag is set moves toward FOG_COLOR, and its alpha toward
/// FOG_COLOR's (only the alpha with DISP3DCNT bit 6), by a density read
/// from its depth. Gyms use it for darkness: Driftveil's is fogged black
/// all over but for a disc around the player, whose translucent polygon
/// has fog disabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fog {
    /// DISP3DCNT: enable (bit 7), alpha only (bit 6), FOG_SHIFT (8-11).
    pub control: u16,
    /// FOG_COLOR: the color, and the alpha in bits 16-20.
    pub color: u32,
    /// FOG_OFFSET, in the depth buffer's top 15 bits.
    pub offset: u16,
    /// FOG_TABLE: densities from 0 to 127.
    pub table: [u8; 32],
}

impl Fog {
    fn read(io: &Io) -> Self {
        Self {
            control: io.get16(0x0400_0060),
            color: io.get32(0x0400_0358),
            offset: io.get16(0x0400_035C) & 0x7fff,
            table: std::array::from_fn(|i| io.get8(0x0400_0360 + i as u32) & 0x7f),
        }
    }

    pub fn enabled(&self) -> bool {
        self.control & 0x80 != 0
    }

    /// The density at `depth` (0 near to 1 far, as the depth buffer
    /// stores it): 0 for none to 128 for full. The table's entries are at
    /// depths FOG_OFFSET + FOG_STEP * (1 to 32), FOG_STEP being 0x400 >>
    /// FOG_SHIFT, with linear steps between them; nearer than the first
    /// the first applies, beyond the last the last.
    pub fn density(&self, depth: f64) -> u32 {
        let shift = (self.control >> 8) & 15;
        let position =
            (depth * 32767.0 - f64::from(self.offset)) * f64::from(1u32 << shift) / 1024.0;
        let entry = |index: f64| f64::from(self.table[index.clamp(0.0, 31.0) as usize]);
        let density = if position < 0.0 {
            entry(0.0)
        } else if position >= 32.0 {
            entry(31.0)
        } else {
            let index = position.floor();
            let fraction = position - index;
            entry(index - 1.0) * (1.0 - fraction) + entry(index) * fraction
        } as u32;
        if density >= 127 { 128 } else { density }
    }

    /// A pixel at `depth` (see `density`) with fog.
    pub fn apply(&self, pixel: Pixel, depth: f64) -> Pixel {
        let density = self.density(depth);
        let mix = |fog: u32, value: u32| (fog * density + value * (128 - density)) / 128;
        let color = if self.control & 0x40 != 0 {
            pixel.color
        } else {
            [0, 5, 10].into_iter().fold(0, |color, shift| {
                let channel = mix(
                    self.color >> shift & 31,
                    u32::from(pixel.color >> shift & 31),
                );
                color | (channel as u16) << shift
            })
        };
        Pixel {
            color,
            alpha: mix(self.color >> 16 & 31, u32::from(pixel.alpha)) as u8,
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
            let y = 192.0
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
        if vertices.is_empty() {
            break;
        }
        vertices = clip_plane(&vertices, |clip| plane_distance(clip, plane, reach));
    }
    vertices
}

/// The part of a polygon on the side of a plane where `distance` is not
/// negative.
fn clip_plane(input: &[Vertex], distance: impl Fn([f64; 4]) -> f64) -> Vec<Vertex> {
    let mut vertices = Vec::with_capacity(input.len() + 1);
    let Some(&last) = input.last() else {
        return vertices;
    };
    let mut previous = last;
    let mut pd = distance(previous.clip);
    for &current in input {
        let cd = distance(current.clip);
        if (pd < 0.0) != (cd < 0.0) {
            vertices.push(previous.lerp(current, pd / (pd - cd)));
        }
        if cd >= 0.0 {
            vertices.push(current);
        }
        previous = current;
        pd = cd;
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
    /// The width of the rows it is drawn into: `WIDTH`, or the widescreen
    /// width for `widened` triangles.
    columns: usize,
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
                192.0
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
            columns: WIDTH,
        })
    }

    /// The triangle drawn into rows `WIDTH + 2 * WIDE_EXTRA` wide, whose
    /// first column is DS column `-WIDE_EXTRA`.
    fn widened(mut self) -> Self {
        let columns = WIDTH + 2 * WIDE_EXTRA;
        for point in &mut self.screen {
            point[0] += WIDE_EXTRA as f64;
        }
        let low = self
            .screen
            .iter()
            .map(|p| p[0])
            .fold(f64::INFINITY, f64::min);
        let high = self
            .screen
            .iter()
            .map(|p| p[0])
            .fold(f64::NEG_INFINITY, f64::max);
        self.min_x = low.floor().clamp(0.0, columns as f64) as usize;
        self.max_x = high.ceil().clamp(0.0, columns as f64) as usize;
        self.columns = columns;
        self
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
        let fog = if polygon.attr & (1 << 15) != 0 {
            FOG_FLAG
        } else {
            0
        };
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
                let at = (y - top) * self.columns + x;
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
                    if ids[at] & !FOG_FLAG == id {
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
                    // A translucent polygon only keeps the fog flag of
                    // what is under it, or clears it.
                    ids[at] = id | (ids[at] & fog);
                } else {
                    pixels[at] = Pixel {
                        color: pack(color),
                        alpha,
                    };
                    ids[at] = 64 | fog;
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
                    model: [0; 3],
                    raw_uv: [0.0, 0.0],
                    clip: [-1.0, -1.0, 0.0, 1.0],
                    uv: [0.0, 0.0],
                    color: [31.0; 3],
                },
                Vertex {
                    model: [0; 3],
                    raw_uv: [8.0, 0.0],
                    clip: [2.0, -2.0, 0.0, 2.0],
                    uv: [8.0, 0.0],
                    color: [31.0; 3],
                },
                Vertex {
                    model: [0; 3],
                    raw_uv: [0.0, 0.0],
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
            shape: PolygonId::default(),
            matrix: identity(),
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
    fn inserting_a_repeated_object_does_not_morph_its_neighbors() {
        let shown = list_at(&[0, 819]);
        let next = list_at(&[-819, 205, 1024]);
        let half = shown.build(Some(&next), 0.5, false);
        // The repeated mesh changed count, making occurrence-based pairing
        // ambiguous. Keep the shown geometry instead of pulling it sideways.
        assert_eq!(half.matched.matched, 0);
        assert_eq!(
            left_edges(&half),
            left_edges(&shown.build(None, 0.0, false))
        );
    }

    #[test]
    fn copies_of_a_mesh_follow_their_own_objects_when_the_camera_misleads() {
        // Two copies of one mesh far apart (a battle's two platforms), which
        // move 205 to the right, and two unique meshes that agree on moving
        // 600 (a sprite growing), so the camera's motion places neither copy.
        let list = |copies: &[i32], anchors: &[i32]| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            for &x in copies {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[x as u32, 0, 0]);
                triangle(&mut g, 31, 0, 31, 0);
            }
            for (index, &x) in anchors.iter().enumerate() {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[x as u32, 0, 0]);
                triangle(&mut g, 31, 0, 31, 1 + index as u32);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed.clone()
        };
        let shown = list(&[-1300, 1300], &[100, -100]);
        let next = list(&[-1095, 1505], &[700, 500]);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 4);
        let mut edges = left_edges(&half);
        edges.sort_by(f32::total_cmp);
        // Each copy half way to itself, not moved with the anchors.
        for (edge, expected) in edges.iter().zip([26.58, 70.25, 76.5, 107.83]) {
            assert!((edge - expected).abs() < 0.1, "{edges:?}");
        }
    }

    #[test]
    fn reordered_fence_segments_follow_their_positions_instead_of_submission_order() {
        let shown = list_at(&[0, 819]);
        let next = list_at(&[1024, 205]);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 2);
        let mut edges = left_edges(&half);
        edges.sort_by(f32::total_cmp);
        assert!((edges[0] - 67.2).abs() < 0.1, "{edges:?}");
        assert!((edges[1] - 92.8).abs() < 0.1, "{edges:?}");
    }

    #[test]
    fn replacing_a_tile_with_the_same_submission_count_does_not_pair_its_neighbor() {
        let list = |offsets: &[i32], camera: i32| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            for &offset in offsets {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[(offset + camera) as u32, 0, 0]);
                triangle(&mut g, 31, 0, 31, 0);
            }
            command(&mut g, 0x15, &[]);
            command(&mut g, 0x1c, &[camera as u32, 0, 0]);
            triangle(&mut g, 31, 0, 31, 1); // unique camera anchor
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed
        };
        let shown = list(&[0, 819], 0);
        let next = list(&[-819, 819], 205);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 2); // anchor and retained tile
        let mut edges = left_edges(&half);
        edges.sort_by(f32::total_cmp);
        // The replaced tile pairs with neither neighbor; drawn through the
        // anchor's matrix, it moves on with the anchor instead.
        assert!((edges[0] - 67.2).abs() < 0.1, "{edges:?}");
        assert!((edges[1] - 67.2).abs() < 0.1, "{edges:?}");
        assert!((edges[2] - 92.8).abs() < 0.1, "{edges:?}");
    }

    #[test]
    fn different_meshes_with_the_same_material_keep_their_pairs_when_reordered() {
        let list = |shapes: &[(i32, i16)]| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            for &(offset, size) in shapes {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[offset as u32, 0, 0]);
                command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
                command(&mut g, 0x40, &[0]);
                vertex(&mut g, -size, -size, 0);
                vertex(&mut g, size, -size, 0);
                vertex(&mut g, 0, size, 0);
                command(&mut g, 0x41, &[]);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed
        };
        let shown = list(&[(0, 2048), (819, 1024)]);
        let next = list(&[(1024, 1024), (205, 2048)]);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 2);
        let mut edges = left_edges(&half);
        edges.sort_by(f32::total_cmp);
        assert!((edges[0] - 67.2).abs() < 0.1, "{edges:?}");
        assert!((edges[1] - 124.8).abs() < 0.1, "{edges:?}");
    }

    #[test]
    fn changed_texture_coordinates_and_viewports_are_not_interpolated() {
        let shown = list_at(&[0]);
        let mut next = list_at(&[205]);
        Arc::make_mut(&mut next).polygons[0].vertices[0].raw_uv[0] = 8.0;
        assert_eq!(shown.build(Some(&next), 0.5, false).matched.matched, 0);
        let mut next = list_at(&[205]);
        Arc::make_mut(&mut next).polygons[0].viewport = [0, 0, 127, 191];
        assert_eq!(shown.build(Some(&next), 0.5, false).matched.matched, 0);
    }

    #[test]
    fn animated_texture_matrices_do_not_detach_flags_from_the_moving_scene() {
        let list = |offset: i32, texture_offset: i32| {
            let mut g = Geometry::default();
            command(&mut g, 0x2a, &[(1 << 30) | (4 << 26)]);
            command(&mut g, 0x10, &[3]);
            command(&mut g, 0x1c, &[texture_offset as u32, 0, 0]);
            command(&mut g, 0x22, &[0]);
            command(&mut g, 0x10, &[2]);
            command(&mut g, 0x1c, &[offset as u32, 0, 0]);
            triangle(&mut g, 0x7fff, 0, 31, 0);
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed
        };
        let shown = list(0, 0);
        let next = list(410, 65536);
        assert_ne!(
            shown.polygons[0].vertices[0].uv,
            next.polygons[0].vertices[0].uv
        );
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 1);
        assert!((left_edges(&half)[0] - 70.4).abs() < 0.1);
        // The current frame's texture is sampled at its own UVs.
        assert_eq!(
            half.vertices[0].uv,
            shown.build(None, 0.0, false).vertices[0].uv
        );
    }

    /// A list of triangles, each `(offset, size, texture_v)` drawn through
    /// `camera`, a zoom `[scale]` or a translation `[x, y, z]` applied after
    /// the object's own translation, as a view matrix is.
    fn camera_list(camera: &[u32], shapes: &[(i32, i16, u32)]) -> Arc<DisplayList> {
        let mut g = Geometry::default();
        command(&mut g, 0x10, &[2]);
        for &(offset, size, texture_v) in shapes {
            command(&mut g, 0x15, &[]);
            // One value zooms, three translate.
            match camera {
                [scale] => command(&mut g, 0x1b, &[*scale, *scale, 4096]),
                _ => command(&mut g, 0x1c, camera),
            }
            command(&mut g, 0x1c, &[offset as u32, 0, 0]);
            command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
            command(&mut g, 0x40, &[0]);
            command(&mut g, 0x22, &[texture_v << 16]);
            vertex(&mut g, -size, -size, 0);
            vertex(&mut g, size, -size, 0);
            vertex(&mut g, 0, size, 0);
            command(&mut g, 0x41, &[]);
        }
        command(&mut g, 0x50, &[0]);
        g.vblank();
        g.displayed.clone()
    }

    fn sorted_edges(triangles: &Triangles) -> Vec<f32> {
        let mut edges = left_edges(triangles);
        edges.sort_by(f32::total_cmp);
        edges
    }

    #[test]
    fn repeated_stepping_stones_follow_the_camera_where_screen_shifts_differ() {
        // A camera that zooms in by 10% moves things near the edge of the
        // screen further than those in the middle, so one median screen
        // shift cannot pair the two identical stones on the right. The
        // camera's motion, from the two unique meshes, predicts both.
        let shapes = [(-2048, 400, 0), (0, 300, 0), (2048, 200, 0), (2867, 200, 0)];
        let shown = camera_list(&[4096], &shapes);
        let next = camera_list(&[4506], &shapes);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 4);
        let (before, after) = (
            sorted_edges(&shown.build(None, 0.0, false)),
            sorted_edges(&next.build(None, 0.0, false)),
        );
        for ((half, before), after) in sorted_edges(&half).iter().zip(&before).zip(&after) {
            assert!(
                (half - (before + after) / 2.0).abs() < 0.05,
                "{half} {before} {after}"
            );
        }
    }

    #[test]
    fn a_flag_that_cannot_be_paired_moves_on_with_the_camera() {
        // The flag's cloth changes shape every list (a new mesh each time),
        // so it never pairs; it must not stand still while its pole and the
        // building around it move.
        let shown = camera_list(&[0, 0, 0], &[(-2048, 400, 0), (0, 300, 0), (2048, 200, 0)]);
        let next = camera_list(
            &[410, 0, 0],
            &[(-2048, 400, 0), (0, 300, 0), (2048, 220, 0)],
        );
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 2);
        let before = sorted_edges(&shown.build(None, 0.0, false));
        // 410 is 0.1 of the screen's half width: 12.8 pixels, 6.4 halfway.
        for (half, before) in sorted_edges(&half).iter().zip(&before) {
            assert!((half - before - 6.4).abs() < 0.05, "{half} {before}");
        }
    }

    #[test]
    fn an_unpaired_part_moves_with_the_rest_of_its_own_object() {
        // A character walks right on its own while the camera stays: its
        // body pairs, its face (a new mesh) does not, and the face goes with
        // the body rather than with the scenery.
        let list = |walk: i32, face: i16| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            for (offset, size, v) in [
                (-2048, 400, 0u32),
                (0, 300, 0),
                (2048 + walk, 200, 0),
                (2048 + walk, face, 0),
            ] {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[offset as u32, 0, 0]);
                command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
                command(&mut g, 0x40, &[0]);
                command(&mut g, 0x22, &[v << 16]);
                vertex(&mut g, -size, -size, 0);
                vertex(&mut g, size, -size, 0);
                vertex(&mut g, 0, size, 0);
                command(&mut g, 0x41, &[]);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed.clone()
        };
        let shown = list(0, 100);
        let next = list(410, 120);
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 3);
        let before = left_edges(&shown.build(None, 0.0, false));
        let half = left_edges(&half);
        // By where they started, left to right: the two pieces of scenery,
        // then the body and face.
        let mut moved: Vec<_> = before.iter().zip(&half).map(|(b, h)| (*b, h - b)).collect();
        moved.sort_by(|a, b| a.0.total_cmp(&b.0));
        let moved: Vec<_> = moved.into_iter().map(|(_, moved)| moved).collect();
        assert!(moved[0].abs() < 0.05 && moved[1].abs() < 0.05, "{moved:?}");
        assert!(
            (moved[2] - 6.4).abs() < 0.05 && (moved[3] - 6.4).abs() < 0.05,
            "{moved:?}"
        );
    }

    #[test]
    fn a_sprite_that_changes_animation_frame_stays_with_the_camera_that_follows_it() {
        // The camera follows the walking player, who stays in the middle of
        // the screen while the scenery scrolls. Its next walking frame comes
        // from the same sheet at other texture coordinates: it must still
        // pair, and stay where it is, rather than slide with the scenery.
        let shown = camera_list(&[0, 0, 0], &[(-2048, 400, 0), (2048, 300, 0), (0, 200, 0)]);
        let next = camera_list(
            &[410, 0, 0],
            &[(-2048, 400, 0), (2048, 300, 0), (-410, 200, 16)],
        );
        let half = shown.build(Some(&next), 0.5, false);
        assert_eq!(half.matched.matched, 3);
        let before = left_edges(&shown.build(None, 0.0, false));
        let mut moved: Vec<_> = left_edges(&half)
            .iter()
            .zip(&before)
            .map(|(half, before)| (*before, half - before))
            .collect();
        moved.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Left to right: scenery, the player, scenery.
        let moved: Vec<_> = moved.into_iter().map(|(_, moved)| moved).collect();
        assert!(
            (moved[0] - 6.4).abs() < 0.05 && (moved[2] - 6.4).abs() < 0.05,
            "{moved:?}"
        );
        assert!(moved[1].abs() < 0.05, "{moved:?}");
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
    fn the_viewport_covers_every_row_and_column() {
        // Clip space from -1 to 1 maps onto rows 0 to 191 and columns 0 to
        // 255, all of them: the bottom row is not left to the clear color.
        for (reach, rows, columns) in [(4096, 0..192, 0..256), (2048, 48..144, 64..192)] {
            let mut g = Geometry::default();
            command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
            command(&mut g, 0x40, &[1]);
            for (x, y) in [
                (-reach, -reach),
                (reach, -reach),
                (reach, reach),
                (-reach, reach),
            ] {
                vertex(&mut g, x, y, 0);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            let pixels = g.render(&io(), &Vram::new());
            for (index, pixel) in pixels.iter().enumerate() {
                let (x, y) = (index % 256, index / 256);
                let inside = rows.contains(&y) && columns.contains(&x);
                assert_eq!(pixel.alpha != 0, inside, "pixel {x},{y} of a {reach} quad");
            }
        }
    }

    #[test]
    fn opaque_alpha_texture_pixels_write_depth_and_do_not_share_translucent_ids() {
        let mut gpu = crate::runtime::scaled::Headless::new().unwrap();
        for format in [1, 6] {
            let mut io = io();
            let mut memory = super::super::memory::Memory::new();
            memory.vram = texture_vram();
            // White texels with maximum alpha in both A3I5 and A5I3.
            memory.vram.banks[..64].fill(if format == 1 { 0xe3 } else { 0xfb });
            command(&mut io.geometry, 0x2a, &[format << 26]);
            triangle(&mut io.geometry, 31, 1024, 31, 2);
            triangle(&mut io.geometry, 31 << 5, 0, 31, 2);
            triangle(&mut io.geometry, 31 << 10, 2048, 31, 3);
            command(&mut io.geometry, 0x50, &[0]);
            io.advance_to_vblank();
            io.set16(0x0400_0304, 0x800a);
            io.set32(0x0400_0000, 0x10108);
            io.set16(0x0400_0060, 9);
            let software = super::super::gpu2d::render(&io, &memory);
            let scaled = super::super::gpu2d::render_scaled(&io, &memory);
            for scale in [1, 2] {
                let (width, _, pixels) = gpu.render(&scaled, scale).unwrap();
                let at = ((96 * scale * width + 128 * scale) * 4) as usize;
                let expected = &software.top[(96 * WIDTH + 128) * 4..][..4];
                assert_eq!(expected, &[0, 255, 0, 255]);
                assert_eq!(
                    &pixels[at..at + 4],
                    expected,
                    "format {format}, scale {scale}"
                );
            }
        }
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

    /// Fog over everything: DISP3DCNT with alpha blending, FOG_SHIFT 10,
    /// full density from the first step, in `color` with alpha 31.
    fn full_fog(io: &mut Io, color: u16) {
        io.set16(0x0400_0060, 0x80 | 8 | (10 << 8));
        io.set32(0x0400_0358, u32::from(color) | (31 << 16));
        io.set16(0x0400_035C, 0);
        for i in 0..32 {
            io.set8(0x0400_0360 + i, 127);
        }
    }

    /// An opaque red triangle with fog at z 0.25, and a translucent blue
    /// one (alpha 15) in front of its left part, with fog if `fogged`.
    fn fog_scene(g: &mut Geometry, fogged: bool) {
        command(g, 0x29, &[(31 << 16) | (1 << 15) | 0xc0]);
        command(g, 0x40, &[0]);
        command(g, 0x20, &[31]);
        vertex(g, -2048, -2048, 1024);
        vertex(g, 2048, -2048, 1024);
        vertex(g, 0, 2048, 1024);
        command(g, 0x41, &[]);
        let fog = if fogged { 1 << 15 } else { 0 };
        command(g, 0x29, &[(15 << 16) | fog | 0xc0 | (1 << 24)]);
        command(g, 0x40, &[0]);
        command(g, 0x20, &[31 << 10]);
        vertex(g, -4096, -1024, 0);
        vertex(g, 1024, -1024, 0);
        vertex(g, 1024, 3072, 0);
        command(g, 0x41, &[]);
        command(g, 0x50, &[0]);
        g.vblank();
    }

    #[test]
    fn fog_density_follows_the_table_between_its_steps() {
        let fog = |shift: u16, offset: u16, table: [u8; 32]| Fog {
            control: 0x80 | (shift << 8),
            color: 0,
            offset,
            table,
        };
        let ramp: [u8; 32] = std::array::from_fn(|i| 4 * i as u8);
        // FOG_SHIFT 0: the entries are 1024 apart, the first at 1024.
        let depth = |depth15: f64| depth15 / 32767.0;
        assert_eq!(fog(0, 0, ramp).density(0.0), 0);
        // 2.6 steps in: 40% of entry 1 (4), 60% of entry 2 (8).
        assert_eq!(fog(0, 0, ramp).density(depth(2.6 * 1024.0)), 6);
        // Beyond the last entry (FOG_SHIFT 1 ends half way), the last one.
        assert_eq!(fog(1, 0, ramp).density(1.0), 124);
        // Nearer than FOG_OFFSET, the first entry.
        assert_eq!(fog(0, 1000, [9; 32]).density(depth(500.0)), 9);
        // FOG_SHIFT 10: a step of 1; 127 is full fog.
        assert_eq!(fog(10, 0, [127; 32]).density(0.5), 128);
    }

    #[test]
    fn fog_covers_fogged_pixels_unless_a_translucent_polygon_without_it_is_in_front() {
        for (fogged, rear) in [(false, false), (true, false), (false, true)] {
            let mut g = Geometry::default();
            fog_scene(&mut g, fogged);
            let mut io = io();
            full_fog(&mut io, 31 << 5);
            // A transparent rear plane, with fog if `rear`.
            io.set32(0x0400_0350, if rear { 0x8000 } else { 0 });
            let pixels = g.render(&io, &Vram::new());
            let at = |x: usize, y: usize| {
                let pixel = pixels[y * WIDTH + x];
                (pixel.color, pixel.alpha)
            };
            let green = (31 << 5, 31);
            // The red triangle alone.
            assert_eq!(at(165, 110), green);
            // Under the translucent triangle.
            if fogged {
                assert_eq!(at(120, 110), green);
            } else {
                let (color, alpha) = at(120, 110);
                assert_eq!((color >> 5 & 31, alpha), (0, 31));
            }
            // The rear plane.
            assert_eq!(at(20, 20), if rear { green } else { (0, 0) });
        }
    }

    #[test]
    fn gpu_renderer_fogs_as_the_software_one_does() {
        let Ok(mut gpu) = crate::runtime::scaled::Headless::new() else {
            eprintln!("no graphics adapter; skipping");
            return;
        };
        for (fogged, rear) in [(false, false), (true, true)] {
            let mut io = io();
            let mut memory = super::super::memory::Memory::new();
            fog_scene(&mut io.geometry, fogged);
            io.advance_to_vblank();
            io.set16(0x0400_0304, 0x800a);
            io.set32(0x0400_0000, 0x10108);
            full_fog(&mut io, 0x4210);
            io.set32(0x0400_0350, if rear { 0x8000 } else { 0 });
            memory.palette[0..2].copy_from_slice(&(31u16 << 5).to_le_bytes());
            let software = super::super::gpu2d::render(&io, &memory);
            let scaled = super::super::gpu2d::render_scaled(&io, &memory);
            let (_, _, pixels) = gpu.render(&scaled, 1).unwrap();
            let differing = software
                .top
                .chunks(4)
                .zip(pixels.chunks(4))
                .filter(|(a, b)| (0..3).any(|i| a[i].abs_diff(b[i]) > 9))
                .count();
            assert_eq!(differing, 0, "fogged {fogged}, rear {rear}");
            // Fog shows: the red triangle alone is gray.
            let pixel = &pixels[(110 * 256 + 165) * 4..][..3];
            assert_eq!(pixel, [132, 132, 132]);
        }
    }

    #[test]
    fn fractional_resolutions_render_exact_sizes_without_stretching_geometry() {
        let Ok(mut gpu) = crate::runtime::scaled::Headless::new() else {
            return;
        };
        let mut io = io();
        let mut memory = super::super::memory::Memory::new();
        io.geometry.wide = true;
        triangle(&mut io.geometry, 31, 0, 31, 0);
        command(&mut io.geometry, 0x50, &[0]);
        io.advance_to_vblank();
        io.set16(0x0400_0304, 0x800a);
        io.set32(0x0400_0000, 0x10108);
        memory.palette[0..2].copy_from_slice(&(31u16 << 5).to_le_bytes());
        let frame = super::super::gpu2d::render_scaled(&io, &memory);
        for (width, height) in [(1024, 768), (1417, 907), (1920, 1080), (1920, 1200)] {
            let (w, h, pixels) = gpu.render_sized(&frame, (width, height)).unwrap();
            assert_eq!((w, h), (width, 2 * height));
            let mut bounds = [w, h, 0, 0];
            for (i, pixel) in pixels[..(w * height * 4) as usize]
                .as_chunks::<4>()
                .0
                .iter()
                .enumerate()
            {
                if pixel[0] > 200 && pixel[1] < 10 && pixel[2] < 10 {
                    let (x, y) = (i as u32 % w, i as u32 / w);
                    bounds = [
                        bounds[0].min(x),
                        bounds[1].min(y),
                        bounds[2].max(x),
                        bounds[3].max(y),
                    ];
                }
            }
            let ratio = (bounds[2] - bounds[0] + 1) as f32 / (bounds[3] - bounds[1] + 1) as f32;
            assert!(
                (ratio - 4.0 / 3.0).abs() < 0.02,
                "{width}x{height}: {bounds:?}"
            );
        }
    }

    #[test]
    fn the_battle_horizon_stays_closed_when_one_material_changes_during_camera_motion() {
        let list = |offset: i32, ground_texture: u32| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            command(&mut g, 0x1c, &[0, offset as u32, 0]);
            for (bottom, top, color, texture) in
                [(0, 4096, 31 << 10, 0), (-4096, 0, 31 << 5, ground_texture)]
            {
                command(&mut g, 0x2a, &[texture]);
                command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
                command(&mut g, 0x20, &[color]);
                command(&mut g, 0x40, &[1]);
                for (x, y) in [(-4096, bottom), (4096, bottom), (4096, top), (-4096, top)] {
                    vertex(&mut g, x, y, 0);
                }
                command(&mut g, 0x41, &[]);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g
        };
        let mut io = io();
        io.geometry = list(0, 0);
        io.set16(0x0400_0304, 0x800a);
        io.set32(0x0400_0000, 0x10108);
        let memory = super::super::memory::Memory::new();
        let frame = super::super::gpu2d::render_scaled(&io, &memory);
        let next = list(205, 1 << 26);
        let Ok(mut gpu) = crate::runtime::scaled::Headless::new() else {
            return;
        };
        for alpha in [0.25, 0.5, 0.75, 1.0] {
            let (width, _, pixels) = gpu
                .render_interpolated(&frame, 3, &next.displayed, alpha)
                .unwrap();
            for y in 88 * 3..100 * 3 {
                for x in 1..width - 1 {
                    let at = ((y * width + x) * 4) as usize;
                    assert!(
                        pixels[at + 1] != 0 || pixels[at + 2] != 0,
                        "black horizon at {x},{y}, alpha {alpha}"
                    );
                }
            }
        }
    }

    #[test]
    fn coincident_objects_with_conflicting_motion_do_not_drag_an_unmatched_object() {
        let list = |offsets: [i32; 3], change_last: bool| {
            let mut g = Geometry::default();
            command(&mut g, 0x10, &[2]);
            for (id, offset) in offsets.into_iter().enumerate() {
                command(&mut g, 0x15, &[]);
                command(&mut g, 0x1c, &[offset as u32, 0, 0]);
                command(
                    &mut g,
                    0x2a,
                    &[if change_last && id == 2 { 1 << 26 } else { 0 }],
                );
                triangle(&mut g, 31, 0, 31, id as u32);
            }
            command(&mut g, 0x50, &[0]);
            g.vblank();
            g.displayed
        };
        let mut shown = list([0, 0, 0], false);
        let third = &mut Arc::make_mut(&mut shown).polygons[2];
        third.vertices[1].clip[0] += 0.1;
        third.vertices[2].clip[1] += 0.1;
        let next = list([205, -205, 0], true);
        let (half, matched) = shown.interpolated(Some(&next), 0.5);
        assert_eq!(matched.matched, 2);
        for (before, after) in shown.polygons[2].vertices.iter().zip(&half[2].vertices) {
            assert_eq!(before.clip, after.clip);
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
    fn a_wide_software_render_draws_beside_the_screen_too() {
        let mut g = Geometry {
            wide: true,
            ..Default::default()
        };
        triangle(&mut g, 31, 0, 31, 0);
        beside_the_screen(&mut g);
        command(&mut g, 0x50, &[0]);
        g.vblank();
        let columns = WIDTH + 2 * WIDE_EXTRA;
        let narrow = g.render(&io(), &Vram::new());
        let wide = g.render_wide(&io(), &Vram::new());
        assert_eq!(wide.len(), columns * HEIGHT);
        // The DS's columns are the same picture.
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (a, b) = (wide[y * columns + WIDE_EXTRA + x], narrow[y * WIDTH + x]);
                assert_eq!((a.color, a.alpha), (b.color, b.alpha), "pixel {x},{y}");
            }
        }
        // The triangle beside the screen, DS columns 262 to 294, shows
        // only in the wide render.
        assert_eq!(wide[96 * columns + WIDE_EXTRA + 278].alpha, 31);
        assert_eq!(wide[96 * columns + WIDE_EXTRA + 300].alpha, 0);
    }

    #[test]
    fn widescreen_reflects_a_flat_picture_across_the_screen_at_its_edges() {
        let mut g = Geometry {
            wide: true,
            ..Default::default()
        };
        // A red quad, front faces only, across the screen and a little
        // past both sides (x -1.05 to 1.05), with nothing in perspective.
        command(&mut g, 0x29, &[(31 << 16) | 0x80]);
        command(&mut g, 0x40, &[1]);
        command(&mut g, 0x20, &[31]);
        vertex(&mut g, -4301, -4096, 0);
        vertex(&mut g, 4301, -4096, 0);
        vertex(&mut g, 4301, 4096, 0);
        vertex(&mut g, -4301, 4096, 0);
        command(&mut g, 0x41, &[]);
        // A blue triangle in front of it at the left edge: DS columns 0 to
        // 19 on row 96.
        command(&mut g, 0x29, &[(31 << 16) | 0xc0]);
        command(&mut g, 0x40, &[0]);
        command(&mut g, 0x20, &[31 << 10]);
        vertex(&mut g, -4096, -2048, -2048);
        vertex(&mut g, -3482, 0, -2048);
        vertex(&mut g, -4096, 2048, -2048);
        command(&mut g, 0x41, &[]);
        command(&mut g, 0x50, &[0]);
        g.vblank();
        let columns = WIDTH + 2 * WIDE_EXTRA;
        let narrow = g.render(&io(), &Vram::new());
        let wide = g.render_wide(&io(), &Vram::new());
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (a, b) = (wide[y * columns + WIDE_EXTRA + x], narrow[y * WIDTH + x]);
                assert_eq!((a.color, a.alpha), (b.color, b.alpha), "pixel {x},{y}");
            }
        }
        let side = |x: i32, y: usize| {
            let pixel = wide[y * columns + (WIDE_EXTRA as i32 + x) as usize];
            (pixel.color, pixel.alpha)
        };
        // Column -1 - i shows column i, and 256 + i column 255 - i, all
        // the way out, rather than the quad stopping 6 columns out.
        assert_eq!(side(-10, 96), (31 << 10, 31));
        assert_eq!(side(-40, 96), (31, 31));
        assert_eq!(side(-43, 0), (31, 31));
        assert_eq!(side(256 + 30, 96), (31, 31));
        assert_eq!(side(256 + 42, 191), (31, 31));
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
