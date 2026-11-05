// Both screens composed over the scaled 3D layer. Follows
// `nds::gpu2d::Composed::resolve`; 2D pixels repeat to fill the scale.


struct Params {
    scale: u32,
    // The screen engine A's 3D layer is on (0 top, 1 bottom), or -1.
    screen: i32,
    // Engine A's BG0 horizontal scroll.
    scroll: u32,
    _pad: u32,
    // MASTER_BRIGHT of the top and bottom screens.
    brightness: vec4<u32>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var composed: texture_2d<u32>;
@group(0) @binding(2) var layer_color: texture_2d<f32>;
@group(0) @binding(3) var layer_alpha: texture_2d<f32>;

@vertex
fn screen_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
}

fn channel(c: u32, shift: u32) -> u32 {
    return (c >> shift) & 31u;
}

fn mix_colors(first: u32, second: u32, eva: u32, evb: u32) -> u32 {
    var out = 0u;
    for (var shift = 0u; shift < 15u; shift = shift + 5u) {
        out = out | (min((channel(first, shift) * eva + channel(second, shift) * evb) / 16u, 31u) << shift);
    }
    return out;
}

fn brighten(c: u32, evy: u32) -> u32 {
    var out = 0u;
    for (var shift = 0u; shift < 15u; shift = shift + 5u) {
        let v = channel(c, shift);
        out = out | (min(v + (31u - v) * evy / 16u, 31u) << shift);
    }
    return out;
}

fn darken(c: u32, evy: u32) -> u32 {
    var out = 0u;
    for (var shift = 0u; shift < 15u; shift = shift + 5u) {
        let v = channel(c, shift);
        out = out | (min(v - v * evy / 16u, 31u) << shift);
    }
    return out;
}

fn resolve(record: vec4<u32>, color: u32, alpha: u32) -> u32 {
    let kind = (record.x >> 16u) & 3u;
    let other = record.y & 0x7FFFu;
    let eva = (record.z >> 8u) & 31u;
    let evb = (record.z >> 16u) & 31u;
    let evy = (record.z >> 24u) & 31u;
    if kind == 0u {
        return other;
    }
    if kind == 2u {
        return mix_colors(other, color, eva, evb);
    }
    if alpha < 31u && (record.z & 1u) != 0u {
        let a = alpha + 1u;
        var out = 0u;
        for (var shift = 0u; shift < 15u; shift = shift + 5u) {
            out = out | (((channel(color, shift) * a + channel(other, shift) * (32u - a)) / 32u) << shift);
        }
        return out;
    }
    switch (record.z >> 1u) & 3u {
        case 1u: {
            return mix_colors(color, other, eva, evb);
        }
        case 2u: {
            return brighten(color, evy);
        }
        case 3u: {
            return darken(color, evy);
        }
        default: {
            return color;
        }
    }
}

@fragment
fn screen_fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let scale = params.scale;
    let x = u32(position.x);
    let y = u32(position.y);
    let record = textureLoad(composed, vec2<u32>(x / scale, y / scale), 0);
    let screen = (y / scale) / 192u;
    var color = record.x & 0x7FFFu;
    if i32(screen) == params.screen {
        let sx = (x + params.scroll * scale) % (512u * scale);
        if sx < 256u * scale {
            let at = vec2<u32>(sx, y - screen * 192u * scale);
            let alpha = u32(round(textureLoad(layer_alpha, at, 0).x * 31.0));
            if alpha != 0u {
                let layer = textureLoad(layer_color, at, 0);
                let c = vec3<u32>(clamp(round(layer.xyz / max(layer.w, 1e-6) * 31.0), vec3<f32>(0.0), vec3<f32>(31.0)));
                color = resolve(record, c.x | (c.y << 5u) | (c.z << 10u), alpha);
            }
        }
    }
    let bright = params.brightness[screen];
    let factor = min(bright & 31u, 16u);
    var rgb = vec3<u32>(color & 31u, (color >> 5u) & 31u, (color >> 10u) & 31u);
    switch bright >> 14u {
        case 1u: {
            rgb = rgb + (vec3<u32>(31u) - rgb) * factor / 16u;
        }
        case 2u: {
            rgb = rgb - rgb * factor / 16u;
        }
        default: {}
    }
    return vec4<f32>(vec3<f32>((rgb << vec3<u32>(3u)) | (rgb >> vec3<u32>(2u))) / 255.0, 1.0);
}
