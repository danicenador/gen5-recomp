// The DS 3D layer at a multiple of its resolution. Follows `nds::gpu3d`'s
// rasterizer.

struct Scene {
    // DISP3DCNT.
    control: u32,
    alpha_ref: u32,
    w_buffer: u32,
    _pad: u32,
    toon: array<vec4<u32>, 8>,
};

@group(0) @binding(0) var<uniform> scene: Scene;
// Texture image memory (512 KiB) and texture palette memory (128 KiB).
@group(0) @binding(1) var<storage, read> texture_memory: array<u32>;
@group(0) @binding(2) var<storage, read> palette_memory: array<u32>;

struct VertexIn {
    // DS screen x, y (pixels, y down), Z-buffer depth, w.
    @location(0) position: vec4<f32>,
    @location(1) color: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // Polygon attributes, texture parameters, palette base.
    @location(3) polygon: vec3<u32>,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) w: f32,
    @location(3) @interpolate(flat) polygon: vec3<u32>,
};

@vertex
fn scene_vertex(in: VertexIn) -> VertexOut {
    let w = in.position.w;
    let ndc = vec2<f32>(in.position.x / 128.0 - 1.0, 1.0 - in.position.y / 96.0);
    var out: VertexOut;
    out.position = vec4<f32>(ndc * w, in.position.z * w, w);
    out.color = in.color;
    out.uv = in.uv;
    out.w = w;
    out.polygon = in.polygon;
    return out;
}

fn texture_byte(at: u32) -> u32 {
    if at >= 0x80000u {
        return 0u;
    }
    return (texture_memory[at >> 2u] >> ((at & 3u) * 8u)) & 0xFFu;
}

fn texture_half(at: u32) -> u32 {
    let a = at & ~1u;
    return texture_byte(a) | (texture_byte(a + 1u) << 8u);
}

fn palette_half(at: u32) -> u32 {
    let a = at & ~1u;
    if a >= 0x20000u {
        return 0u;
    }
    return (palette_memory[a >> 2u] >> ((a & 2u) * 8u)) & 0xFFFFu;
}

fn rgb(c: u32) -> vec3<f32> {
    return vec3<f32>(f32(c & 31u), f32((c >> 5u) & 31u), f32((c >> 10u) & 31u));
}

fn pack(c: vec3<f32>) -> u32 {
    let v = vec3<u32>(clamp(round(c), vec3<f32>(0.0), vec3<f32>(31.0)));
    return v.x | (v.y << 5u) | (v.z << 10u);
}

fn coordinate(value: f32, size: u32, repeat: bool, flip: bool) -> u32 {
    let n = i32(floor(value));
    if !repeat {
        return u32(clamp(n, 0, i32(size) - 1));
    }
    var period = i32(size);
    if flip {
        period = period * 2;
    }
    let m = u32(((n % period) + period) % period);
    if m >= size {
        return size * 2u - 1u - m;
    }
    return m;
}

struct Texel {
    color: u32,
    alpha: u32,
};

fn sample(param: u32, palette: u32, uv: vec2<f32>) -> Texel {
    let format = (param >> 26u) & 7u;
    if format == 0u {
        return Texel(0x7FFFu, 31u);
    }
    let width = 8u << ((param >> 20u) & 7u);
    let height = 8u << ((param >> 23u) & 7u);
    let s = coordinate(uv.x, width, (param & (1u << 16u)) != 0u, (param & (1u << 18u)) != 0u);
    let t = coordinate(uv.y, height, (param & (1u << 17u)) != 0u, (param & (1u << 19u)) != 0u);
    let index = t * width + s;
    let base = (param & 0xFFFFu) * 8u;
    var palette_base = palette * 16u;
    if format == 2u {
        palette_base = palette * 8u;
    }
    if format == 7u {
        let c = texture_half(base + index * 2u);
        return Texel(c & 0x7FFFu, select(0u, 31u, (c & 0x8000u) != 0u));
    }
    if format == 5u {
        let block = (t / 4u) * (width / 4u) + s / 4u;
        let at = base + block * 4u;
        let texel = (texture_byte(at + t % 4u) >> (2u * (s % 4u))) & 3u;
        var slot = 0x20000u + (at & 0x1FFFFu) / 2u;
        if (at & 0x40000u) != 0u {
            slot = slot + 0x10000u;
        }
        let descriptor = texture_half(slot);
        let offset = palette_base + (descriptor & 0x3FFFu) * 4u;
        let mode = descriptor >> 14u;
        if mode <= 1u && texel == 3u {
            return Texel(0u, 0u);
        }
        if (mode == 1u && texel == 2u) || (mode == 3u && texel >= 2u) {
            var weight = 3.0;
            if mode == 1u {
                weight = 4.0;
            } else if texel == 2u {
                weight = 5.0;
            }
            let a = rgb(palette_half(offset) & 0x7FFFu);
            let b = rgb(palette_half(offset + 2u) & 0x7FFFu);
            return Texel(pack(floor((a * weight + b * (8.0 - weight)) / 8.0)), 31u);
        }
        return Texel(palette_half(offset + texel * 2u) & 0x7FFFu, 31u);
    }
    var color = 0u;
    var alpha = 31u;
    switch format {
        case 1u: {
            let b = texture_byte(base + index);
            color = b & 31u;
            let a = b >> 5u;
            alpha = a * 4u + a / 2u;
        }
        case 2u: {
            color = (texture_byte(base + index / 4u) >> ((index % 4u) * 2u)) & 3u;
        }
        case 3u: {
            color = (texture_byte(base + index / 2u) >> ((index % 2u) * 4u)) & 15u;
        }
        case 4u: {
            color = texture_byte(base + index);
        }
        default: {
            let b = texture_byte(base + index);
            color = b & 7u;
            alpha = b >> 3u;
        }
    }
    if format >= 2u && format <= 4u && color == 0u && (param & (1u << 29u)) != 0u {
        alpha = 0u;
    }
    return Texel(palette_half(palette_base + color * 2u) & 0x7FFFu, alpha);
}

struct FragmentOut {
    // Color times its weight, and the accumulated weight (see `Renderer`).
    @location(0) color: vec4<f32>,
    // The DS pixel alpha, out of 31.
    @location(1) alpha: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@fragment
fn scene_fragment(in: VertexOut) -> FragmentOut {
    let attr = in.polygon.x;
    let control = scene.control;
    var tex = Texel(0x7FFFu, 31u);
    if (control & 1u) != 0u {
        tex = sample(in.polygon.y, in.polygon.z, in.uv);
    }
    let mode = (attr >> 4u) & 3u;
    let polygon_alpha = (attr >> 16u) & 31u;
    var alpha = polygon_alpha;
    if mode != 1u {
        alpha = ((tex.alpha + 1u) * (polygon_alpha + 1u) - 1u) / 32u;
    }
    if alpha == 0u || ((control & 4u) != 0u && alpha <= scene.alpha_ref) {
        discard;
    }
    let tc = rgb(tex.color);
    let vc = in.color;
    var color: vec3<f32>;
    if mode == 1u {
        let ta = f32(tex.alpha);
        color = (tc * ta + vc * (31.0 - ta)) / 31.0;
    } else if mode == 2u {
        let index = u32(clamp(vc.x, 0.0, 31.0));
        let toon = rgb(scene.toon[index / 4u][index % 4u]);
        if (control & 2u) == 0u {
            color = tc * toon / 31.0;
        } else {
            color = min(tc * vc / 31.0 + toon, vec3<f32>(31.0));
        }
    } else {
        color = tc * vc / 31.0;
    }
    let c = clamp(round(color), vec3<f32>(0.0), vec3<f32>(31.0)) / 31.0;
    var weight = 1.0;
    if alpha < 31u && (control & 8u) != 0u {
        weight = f32(alpha + 1u) / 32.0;
    }
    var out: FragmentOut;
    out.color = vec4<f32>(c * weight, weight);
    out.alpha = vec4<f32>(f32(alpha) / 31.0, 0.0, 0.0, 0.0);
    if scene.w_buffer != 0u {
        out.depth = clamp(in.w / 4096.0, 0.0, 1.0);
    } else {
        out.depth = in.position.z;
    }
    return out;
}
