//! Both DS screens at a multiple of their resolution, on the GPU.
//!
//! The game thread composes the 2D layers at native resolution, leaving
//! engine A's 3D layer open (`gpu2d::ScaledFrame`), and hands over the 3D
//! list. Here the list is rasterized at `scale` times the resolution and the
//! screens are composed over it, so 2D pixels repeat cleanly and only the 3D
//! layer gains detail.
//!
//! DS translucency blends with what is below only where something has been
//! drawn; elsewhere the polygon's color is stored as is. The color target
//! keeps each color times a weight, with the weights accumulated in alpha
//! (`One`, `OneMinusSrcAlpha`), and the compositor divides by the weight.
//! Over a drawn pixel (weight 1) that is the hardware's blend; stacked
//! translucent polygons over nothing come out as a weighted average. The
//! DS alpha (the most opaque polygon drawn, as on hardware) is a second
//! target blended with `Max`. Translucent polygon IDs live in the stencil
//! buffer: opaque pixels store 64, translucent ones their ID, and a
//! translucent polygon does not draw over its own ID.
//!
//! In widescreen the 3D layer and the top screen are `extra` columns wider
//! on each side (`ScaledFrame::extra`), and the output is that wide, with
//! the bottom screen in the middle of its half.

use crate::Result;
use crate::nds::gpu2d::{HEIGHT, ScaledFrame, WIDTH};
use crate::nds::gpu3d::{Batch, DisplayList, Matched, Scene, SceneVertex};
use std::collections::HashMap;
use std::sync::Arc;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const ALPHA_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
/// Both screens, stacked. Written raw; the window samples it through an
/// sRGB view when its surface is sRGB, as it does with software frames.
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SceneUniform {
    control: u32,
    alpha_ref: u32,
    w_buffer: u32,
    extra: u32,
    toon: [u32; 32],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ScreenUniform {
    scale: u32,
    screen: i32,
    scroll: u32,
    extra: u32,
    brightness: [u32; 4],
}

/// What `set_frame` keeps for drawing the frame, perhaps several times.
#[derive(Default)]
struct Shown {
    brightness: [u16; 2],
    extra: usize,
    layer: Option<ShownLayer>,
}

struct ShownLayer {
    screen: usize,
    scroll: u32,
    list: Arc<DisplayList>,
    clear_color: u16,
    clear_alpha: u8,
    clear_depth: f32,
}

/// Everything sized by the scale and the width.
struct Targets {
    scale: u32,
    extra: usize,
    color: wgpu::TextureView,
    alpha: wgpu::TextureView,
    depth: wgpu::TextureView,
    output: wgpu::Texture,
    output_view: wgpu::TextureView,
    screen_bind_group: wgpu::BindGroup,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    scene_pipeline_layout: wgpu::PipelineLayout,
    scene_shader: wgpu::ShaderModule,
    scene_uniform: wgpu::Buffer,
    texture_memory: wgpu::Buffer,
    palette_memory: wgpu::Buffer,
    scene_bind_group: wgpu::BindGroup,
    /// By (translucent, depth equal, depth write).
    pipelines: HashMap<(bool, bool, bool), wgpu::RenderPipeline>,
    vertices: Option<(wgpu::Buffer, u64)>,
    screen_layout: wgpu::BindGroupLayout,
    screen_pipeline: wgpu::RenderPipeline,
    screen_uniform: wgpu::Buffer,
    /// The composed 2D pixels, `extra` columns wider on each side.
    composed: wgpu::Texture,
    composed_view: wgpu::TextureView,
    composed_extra: usize,
    targets: Option<Targets>,
    shown: Shown,
}

fn composed_texture(device: &wgpu::Device, extra: usize) -> (wgpu::Texture, wgpu::TextureView) {
    let composed = texture(
        device,
        "composed 2D pixels",
        ((WIDTH + 2 * extra) as u32, 2 * HEIGHT as u32),
        wgpu::TextureFormat::Rgba32Uint,
        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        &[],
    );
    let view = composed.create_view(&Default::default());
    (composed, view)
}

fn texture(
    device: &wgpu::Device,
    label: &str,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
    view_formats: &[wgpu::TextureFormat],
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats,
    })
}

fn entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty,
        count: None,
    }
}

/// Registers, read by both stages: the 3D layer's vertices need its width.
fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ..entry(
            binding,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
        )
    }
}

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn texture_entry(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

impl Renderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let scene_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scaled 3D shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scaled_scene.wgsl").into()),
        });
        let screen_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scaled screen shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scaled_screen.wgsl").into()),
        });
        let scene_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scaled 3D layout"),
            entries: &[uniform_entry(0), storage_entry(1), storage_entry(2)],
        });
        let scene_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("scaled 3D pipeline layout"),
                bind_group_layouts: &[Some(&scene_layout)],
                immediate_size: 0,
            });
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let scene_uniform = buffer(
            "scaled 3D registers",
            std::mem::size_of::<SceneUniform>() as u64,
            wgpu::BufferUsages::UNIFORM,
        );
        let texture_memory = buffer("texture memory", 0x8_0000, wgpu::BufferUsages::STORAGE);
        let palette_memory = buffer(
            "texture palette memory",
            0x2_0000,
            wgpu::BufferUsages::STORAGE,
        );
        let scene_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scaled 3D bind group"),
            layout: &scene_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: scene_uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: texture_memory.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: palette_memory.as_entire_binding(),
                },
            ],
        });

        let screen_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scaled screen layout"),
            entries: &[
                uniform_entry(0),
                texture_entry(1, wgpu::TextureSampleType::Uint),
                texture_entry(2, wgpu::TextureSampleType::Float { filterable: false }),
                texture_entry(3, wgpu::TextureSampleType::Float { filterable: false }),
            ],
        });
        let screen_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("scaled screen pipeline layout"),
                bind_group_layouts: &[Some(&screen_layout)],
                immediate_size: 0,
            });
        let screen_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scaled screen pipeline"),
            layout: Some(&screen_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &screen_shader,
                entry_point: Some("screen_vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &screen_shader,
                entry_point: Some("screen_fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: OUTPUT_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let screen_uniform = buffer(
            "scaled screen parameters",
            std::mem::size_of::<ScreenUniform>() as u64,
            wgpu::BufferUsages::UNIFORM,
        );
        let (composed, composed_view) = composed_texture(device, 0);
        Self {
            device: device.clone(),
            queue: queue.clone(),
            scene_pipeline_layout,
            scene_shader,
            scene_uniform,
            texture_memory,
            palette_memory,
            scene_bind_group,
            pipelines: HashMap::new(),
            vertices: None,
            screen_layout,
            screen_pipeline,
            screen_uniform,
            composed,
            composed_view,
            composed_extra: 0,
            targets: None,
            shown: Shown::default(),
        }
    }

    fn targets(&mut self, scale: u32) -> &Targets {
        let extra = self.composed_extra;
        if self
            .targets
            .as_ref()
            .is_none_or(|targets| (targets.scale, targets.extra) != (scale, extra))
        {
            let device = &self.device;
            let size = ((WIDTH + 2 * extra) as u32 * scale, HEIGHT as u32 * scale);
            let attachment =
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
            let color = texture(
                device,
                "scaled 3D color",
                size,
                COLOR_FORMAT,
                attachment,
                &[],
            )
            .create_view(&Default::default());
            let alpha = texture(
                device,
                "scaled 3D alpha",
                size,
                ALPHA_FORMAT,
                attachment,
                &[],
            )
            .create_view(&Default::default());
            let depth = texture(
                device,
                "scaled 3D depth",
                size,
                DEPTH_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                &[],
            )
            .create_view(&Default::default());
            let output = texture(
                device,
                "scaled screens",
                (size.0, size.1 * 2),
                OUTPUT_FORMAT,
                attachment | wgpu::TextureUsages::COPY_SRC,
                &[wgpu::TextureFormat::Rgba8UnormSrgb],
            );
            let output_view = output.create_view(&Default::default());
            let screen_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("scaled screen bind group"),
                layout: &self.screen_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.screen_uniform.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&self.composed_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&color),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::TextureView(&alpha),
                    },
                ],
            });
            self.targets = Some(Targets {
                scale,
                extra,
                color,
                alpha,
                depth,
                output,
                output_view,
                screen_bind_group,
            });
        }
        self.targets.as_ref().unwrap()
    }

    fn pipeline(&mut self, batch: &Batch) -> &wgpu::RenderPipeline {
        let key = (batch.translucent, batch.depth_equal, batch.depth_write);
        let (device, layout, shader) = (
            &self.device,
            &self.scene_pipeline_layout,
            &self.scene_shader,
        );
        self.pipelines.entry(key).or_insert_with(|| {
            let (translucent, depth_equal, depth_write) = key;
            const ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
                0 => Float32x4,
                1 => Float32x3,
                2 => Float32x2,
                3 => Uint32x3,
            ];
            let stencil = wgpu::StencilFaceState {
                compare: if translucent {
                    wgpu::CompareFunction::NotEqual
                } else {
                    wgpu::CompareFunction::Always
                },
                fail_op: wgpu::StencilOperation::Keep,
                depth_fail_op: wgpu::StencilOperation::Keep,
                pass_op: wgpu::StencilOperation::Replace,
            };
            let (color_blend, alpha_blend) = if translucent {
                let over = wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                };
                let max = wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Max,
                };
                (
                    Some(wgpu::BlendState {
                        color: over,
                        alpha: over,
                    }),
                    Some(wgpu::BlendState {
                        color: max,
                        alpha: max,
                    }),
                )
            } else {
                (None, None)
            };
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("scaled 3D pipeline"),
                layout: Some(layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("scene_vertex"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<SceneVertex>() as wgpu::BufferAddress,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &ATTRIBUTES,
                    })],
                },
                // Facing was decided when the list was built.
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(if depth_equal {
                        wgpu::CompareFunction::LessEqual
                    } else {
                        wgpu::CompareFunction::Less
                    }),
                    stencil: wgpu::StencilState {
                        front: stencil,
                        back: stencil,
                        read_mask: 0xFF,
                        write_mask: 0xFF,
                    },
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some("scene_fragment"),
                    compilation_options: Default::default(),
                    targets: &[
                        Some(wgpu::ColorTargetState {
                            format: COLOR_FORMAT,
                            blend: color_blend,
                            write_mask: wgpu::ColorWrites::ALL,
                        }),
                        Some(wgpu::ColorTargetState {
                            format: ALPHA_FORMAT,
                            blend: alpha_blend,
                            write_mask: wgpu::ColorWrites::ALL,
                        }),
                    ],
                }),
                multiview_mask: None,
                cache: None,
            })
        })
    }

    fn upload_scene(&mut self, scene: &Scene, extra: usize) {
        let mut toon = [0; 32];
        for (out, &color) in toon.iter_mut().zip(&scene.toon) {
            *out = u32::from(color);
        }
        let uniform = SceneUniform {
            control: u32::from(scene.control),
            alpha_ref: u32::from(scene.alpha_ref),
            w_buffer: u32::from(scene.w_buffer),
            extra: extra as u32,
            toon,
        };
        self.queue
            .write_buffer(&self.scene_uniform, 0, bytemuck::bytes_of(&uniform));
        self.queue
            .write_buffer(&self.texture_memory, 0, &scene.texture);
        self.queue
            .write_buffer(&self.palette_memory, 0, &scene.texture_palette);
    }

    fn upload_vertices(&mut self, vertices: &[SceneVertex]) {
        let bytes: &[u8] = bytemuck::cast_slice(vertices);
        let size = (bytes.len() as u64).max(64);
        if self
            .vertices
            .as_ref()
            .is_none_or(|(_, capacity)| *capacity < size)
        {
            let capacity = size.next_power_of_two();
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scaled 3D vertices"),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vertices = Some((buffer, capacity));
        }
        if !bytes.is_empty() {
            self.queue
                .write_buffer(&self.vertices.as_ref().unwrap().0, 0, bytes);
        }
    }

    /// Draw `frame` at `scale` times the DS resolution into the output
    /// texture: both screens stacked, `(256 + 2 * frame.extra) * scale` by
    /// `384 * scale`.
    pub fn render(&mut self, frame: &ScaledFrame, scale: u32) -> &wgpu::Texture {
        self.set_frame(frame);
        self.draw(scale, None, 0.0).0
    }

    /// Take a new frame from the game: its 2D layers, registers and texture
    /// memory. `draw` then draws it, as often as the window presents.
    pub fn set_frame(&mut self, frame: &ScaledFrame) {
        if frame.extra != self.composed_extra {
            (self.composed, self.composed_view) = composed_texture(&self.device, frame.extra);
            self.composed_extra = frame.extra;
            self.targets = None;
        }
        let width = (WIDTH + 2 * frame.extra) as u32;
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.composed,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&frame.pixels),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 16),
                rows_per_image: Some(2 * HEIGHT as u32),
            },
            wgpu::Extent3d {
                width,
                height: 2 * HEIGHT as u32,
                depth_or_array_layers: 1,
            },
        );
        if let Some(layer) = &frame.three_d {
            self.upload_scene(&layer.scene, frame.extra);
        }
        self.shown = Shown {
            brightness: frame.brightness,
            extra: frame.extra,
            layer: frame.three_d.as_ref().map(|layer| ShownLayer {
                screen: layer.screen,
                scroll: layer.scroll,
                list: layer.scene.list.clone(),
                clear_color: layer.scene.clear_color,
                clear_alpha: layer.scene.clear_alpha,
                clear_depth: layer.scene.clear_depth,
            }),
        };
    }

    /// Draw the frame `set_frame` took, its 3D list moved `alpha` of the
    /// way to `next` (see `DisplayList::build`).
    pub fn draw(
        &mut self,
        scale: u32,
        next: Option<&DisplayList>,
        alpha: f64,
    ) -> (&wgpu::Texture, Matched) {
        let scale = scale.max(1);
        let layer = self.shown.layer.as_ref();
        let extra = self.shown.extra;
        let uniform = ScreenUniform {
            scale,
            screen: layer.map_or(-1, |layer| layer.screen as i32),
            scroll: layer.map_or(0, |layer| layer.scroll),
            extra: extra as u32,
            brightness: [
                u32::from(self.shown.brightness[0]),
                u32::from(self.shown.brightness[1]),
                0,
                0,
            ],
        };
        self.queue
            .write_buffer(&self.screen_uniform, 0, bytemuck::bytes_of(&uniform));
        let triangles = layer.map(|layer| layer.list.build(next, alpha, extra > 0));
        if let Some(triangles) = &triangles {
            self.upload_vertices(&triangles.vertices);
            for batch in &triangles.batches {
                self.pipeline(batch);
            }
        }
        self.targets(scale);
        let targets = self.targets.as_ref().unwrap();
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scaled frame"),
            });
        if let (Some(scene), Some(triangles)) = (self.shown.layer.as_ref(), &triangles) {
            let clear = if scene.clear_alpha != 0 {
                let c = scene.clear_color;
                let channel = |shift: u16| f64::from((c >> shift) & 31) / 31.0;
                wgpu::Color {
                    r: channel(0),
                    g: channel(5),
                    b: channel(10),
                    a: 1.0,
                }
            } else {
                wgpu::Color::TRANSPARENT
            };
            let alpha = f64::from(scene.clear_alpha) / 31.0;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scaled 3D"),
                color_attachments: &[
                    Some(wgpu::RenderPassColorAttachment {
                        view: &targets.color,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                    Some(wgpu::RenderPassColorAttachment {
                        view: &targets.alpha,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: alpha,
                                g: 0.0,
                                b: 0.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    }),
                ],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(scene.clear_depth),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(64),
                        store: wgpu::StoreOp::Discard,
                    }),
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some((vertices, _)) = &self.vertices
                && !triangles.vertices.is_empty()
            {
                pass.set_vertex_buffer(0, vertices.slice(..));
                pass.set_bind_group(0, &self.scene_bind_group, &[]);
                for batch in &triangles.batches {
                    let key = (batch.translucent, batch.depth_equal, batch.depth_write);
                    pass.set_pipeline(&self.pipelines[&key]);
                    pass.set_stencil_reference(u32::from(batch.id));
                    pass.draw(batch.range.clone(), 0..1);
                }
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scaled screens"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.screen_pipeline);
            pass.set_bind_group(0, &targets.screen_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        let matched = triangles
            .map(|triangles| triangles.matched)
            .unwrap_or_default();
        (&targets.output, matched)
    }
}

/// A renderer without a window, for screenshots of headless runs.
pub struct Headless {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
}

impl Headless {
    pub fn new() -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .map_err(|error| format!("no graphics adapter was found: {error}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gen5 headless screens"),
            ..Default::default()
        }))
        .map_err(|error| format!("could not create graphics device: {error}"))?;
        let renderer = Renderer::new(&device, &queue);
        Ok(Self {
            device,
            queue,
            renderer,
        })
    }

    /// Both screens as RGBA8, `(256 + 2 * frame.extra) * scale` by
    /// `384 * scale`, the bottom screen centered in its half.
    pub fn render(&mut self, frame: &ScaledFrame, scale: u32) -> Result<(u32, u32, Vec<u8>)> {
        let output = self.renderer.render(frame, scale);
        let (width, height) = (output.width(), output.height());
        let row = width * 4;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screenshot readback"),
            size: u64::from(padded * height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            output.size(),
        );
        self.queue.submit(std::iter::once(encoder.finish()));
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|error| format!("could not read the screens back: {error}"))?;
        let data = slice
            .get_mapped_range()
            .map_err(|error| format!("could not read the screens back: {error}"))?;
        let mut pixels = Vec::with_capacity((row * height) as usize);
        for line in data.chunks(padded as usize) {
            pixels.extend_from_slice(&line[..row as usize]);
        }
        Ok((width, height, pixels))
    }
}
