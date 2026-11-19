// Both screens composed over the scaled 3D layer. Follows
// `nds::gpu2d::Composed::resolve`; 2D pixels repeat to fill the scale.
// In widescreen everything is `extra` DS columns wider on each side; the
// bottom screen keeps its 256 columns, with black around them.


struct Params {
    width: u32,
    height: u32,
    // The screen engine A's 3D layer is on (0 top, 1 bottom), or -1.
    screen: i32,
    // Engine A's BG0 horizontal scroll.
    scroll: u32,
    // Widescreen columns on each side (0 without).
    extra: u32,
    span: f32,
    padding: vec2<u32>,
    // MASTER_BRIGHT of the top and bottom screens.
    brightness: vec4<u32>,
    // The 3D layer's DISP3DCNT, FOG_COLOR and FOG_OFFSET.
    fog: vec4<u32>,
    // FOG_TABLE, four entries to a word.
    fog_table: array<vec4<u32>, 2>,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var composed: texture_2d<u32>;
@group(0) @binding(2) var layer_color: texture_2d<f32>;
@group(0) @binding(3) var layer_alpha: texture_2d<f32>;
@group(0) @binding(4) var layer_fog: texture_2d<f32>;
@group(0) @binding(5) var layer_depth: texture_depth_2d;

fn fog_entry(index: f32) -> f32 {
    let n = u32(clamp(index, 0.0, 31.0));
    return f32((params.fog_table[n / 16u][(n / 4u) % 4u] >> ((n % 4u) * 8u)) & 0x7Fu);
}

// Follows `nds::gpu3d::Fog::density`: 0 for none to 128 for full.
fn fog_density(depth: f32) -> u32 {
    let shift = (params.fog.x >> 8u) & 15u;
    let position = (depth * 32767.0 - f32(params.fog.z)) * f32(1u << shift) / 1024.0;
    var density: f32;
    if position < 0.0 {
        density = fog_entry(0.0);
    } else if position >= 32.0 {
        density = fog_entry(31.0);
    } else {
        let index = floor(position);
        let fraction = position - index;
        density = fog_entry(index - 1.0) * (1.0 - fraction) + fog_entry(index) * fraction;
    }
    let d = u32(density);
    if d >= 127u {
        return 128u;
    }
    return d;
}

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
    // Over a second target 3D pixels blend by their own alpha, never by
    // EVA/EVB; an opaque one (alpha 31) comes out as it is.
    if (record.z & 1u) != 0u {
        let a = alpha + 1u;
        var out = 0u;
        for (var shift = 0u; shift < 15u; shift = shift + 5u) {
            out = out | (((channel(color, shift) * a + channel(other, shift) * (32u - a)) / 32u) << shift);
        }
        return out;
    }
    switch (record.z >> 1u) & 3u {
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
    let extra = params.extra;
    let x = u32(position.x);
    let y = u32(position.y);
    let screen = min(y / params.height, 1u);
    let scale = f32(params.width) / params.span;
    let ds_x = position.x / scale + (f32(256u + 2u * extra) - params.span) * 0.5;
    let ds_y = f32(y % params.height) * 192.0 / f32(params.height);
    if screen == 1u && (ds_x < f32(extra) || ds_x >= f32(extra + 256u)) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let source_x = u32(clamp(floor(ds_x), 0.0, f32(255u + 2u * extra)));
    let record = textureLoad(composed, vec2<u32>(source_x, u32(ds_y) + screen * 192u), 0);
    var color = record.x & 0x7FFFu;
    if i32(screen) == params.screen {
        // The 3D layer's column, in scaled DS pixels from the screen's
        // left edge, after BG0's scroll (-256 to 255).
        var scroll = i32(params.scroll);
        if scroll >= 256 {
            scroll = scroll - 512;
        }
        let sx = i32(floor(position.x + f32(scroll) * scale));
        if sx >= 0 && sx < i32(params.width) {
            let at = vec2<u32>(u32(sx), y % params.height);
            var alpha = u32(round(textureLoad(layer_alpha, at, 0).x * 31.0));
            let layer = textureLoad(layer_color, at, 0);
            var c = vec3<u32>(clamp(round(layer.xyz / max(layer.w, 1e-6) * 31.0), vec3<f32>(0.0), vec3<f32>(31.0)));
            if (params.fog.x & 0x80u) != 0u && textureLoad(layer_fog, at, 0).x > 0.5 {
                let density = fog_density(textureLoad(layer_depth, at, 0));
                let fog = params.fog.y;
                if (params.fog.x & 0x40u) == 0u {
                    let fog_rgb = vec3<u32>(fog & 31u, (fog >> 5u) & 31u, (fog >> 10u) & 31u);
                    c = (fog_rgb * density + c * (128u - density)) / 128u;
                }
                alpha = (((fog >> 16u) & 31u) * density + alpha * (128u - density)) / 128u;
            }
            if alpha != 0u {
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
