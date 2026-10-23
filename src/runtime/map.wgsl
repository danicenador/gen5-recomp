struct Camera {
    view_projection: mat4x4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(1) @binding(0)
var map_texture: texture_2d<f32>;
@group(1) @binding(1)
var map_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) color: vec4<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) atlas_rect: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) atlas_rect: vec4<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.color = input.color;
    output.uv = input.uv;
    output.atlas_rect = input.atlas_rect;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let atlas_uv = input.atlas_rect.xy + fract(input.uv) * input.atlas_rect.zw;
    let color = textureSample(map_texture, map_sampler, atlas_uv) * input.color;
    if color.a < 0.05 {
        discard;
    }
    return color;
}
