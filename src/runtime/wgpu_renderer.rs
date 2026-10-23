use crate::Result;
use crate::formats::nitro::texture::NitroTexture;
use crate::game::bw2::field::FieldSession;
use crate::render::{MapScene, SceneTriangle};
use bytemuck::{Pod, Zeroable};
use egui::Context;
use egui_wgpu::ScreenDescriptor;
use glam::{Mat4, Vec3, Vec4};
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::dpi::PhysicalSize;
use winit::window::Window;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 3],
    color: [f32; 4],
    uv: [f32; 2],
    atlas_rect: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CameraUniform {
    view_projection: [[f32; 4]; 4],
}

pub struct WgpuRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    map_pipeline: wgpu::RenderPipeline,
    collision_pipeline: wgpu::RenderPipeline,
    player_pipeline: wgpu::RenderPipeline,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    texture_bind_group: wgpu::BindGroup,
    map_buffer: wgpu::Buffer,
    map_vertex_count: u32,
    collision_buffer: wgpu::Buffer,
    collision_vertex_capacity: u32,
    player_buffer: wgpu::Buffer,
    npc_buffer: wgpu::Buffer,
    npc_vertex_capacity: u32,
    depth_view: wgpu::TextureView,
    egui_renderer: egui_wgpu::Renderer,
    fallback_rect: [f32; 4],
}

impl WgpuRenderer {
    pub async fn new(
        window: Arc<Window>,
        session: &FieldSession,
        scene: &MapScene,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| format!("could not create the game window surface: {error}"))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await
            .map_err(|error| format!("no compatible graphics adapter was found: {error}"))?;
        let adapter_info = adapter.get_info();
        tracing::info!(
            adapter = %adapter_info.name,
            backend = ?adapter_info.backend,
            "selected wgpu adapter"
        );
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gen5 desktop renderer"),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("could not create graphics device: {error}"))?;

        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "graphics surface has no supported configuration".to_owned())?;
        surface.configure(&device, &config);
        let depth_view = create_depth_view(&device, size);

        let atlas =
            build_texture_atlas(&scene.textures, adapter.limits().max_texture_dimension_2d)?;
        let vertices = make_map_vertices(&scene.triangles, &atlas.entries, atlas.fallback_rect);
        let map_vertex_count = u32::try_from(vertices.len())
            .map_err(|_| "map vertex count exceeds the GPU draw limit".to_owned())?;
        let map_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("BW2 map model vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let collision_capacity = collision_vertex_capacity(session);
        let collision_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("field collision overlay vertices"),
            size: u64::from(collision_capacity) * std::mem::size_of::<GpuVertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let initial_player = make_player_vertices(session, atlas.fallback_rect);
        let player_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("field player marker vertices"),
            contents: bytemuck::cast_slice(&initial_player),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let initial_npcs = make_npc_vertices(session, atlas.fallback_rect);
        let npc_vertex_capacity = u32::try_from(initial_npcs.len().max(1))
            .map_err(|_| "NPC vertex count exceeds the GPU draw limit".to_owned())?;
        let npc_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("field NPC marker vertices"),
            size: u64::from(npc_vertex_capacity) * std::mem::size_of::<GpuVertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if !initial_npcs.is_empty() {
            queue.write_buffer(&npc_buffer, 0, bytemuck::cast_slice(&initial_npcs));
        }

        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("field camera uniform"),
            contents: bytemuck::bytes_of(&CameraUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("field camera layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(std::mem::size_of::<CameraUniform>() as u64),
                },
                count: None,
            }],
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("field camera bind group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("BW2 map texture atlas"),
            size: wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width * 4),
                rows_per_image: Some(atlas.height),
            },
            wgpu::Extent3d {
                width: atlas.width,
                height: atlas.height,
                depth_or_array_layers: 1,
            },
        );
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let texture_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("BW2 map nearest sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("map atlas texture layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let texture_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("map atlas texture bind group"),
            layout: &texture_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&texture_sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("field map shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("map.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("field render pipeline layout"),
            bind_group_layouts: &[Some(&camera_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        const ATTRIBUTES: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
            0 => Float32x3,
            1 => Float32x4,
            2 => Float32x2,
            3 => Float32x4
        ];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBUTES,
        };
        let map_pipeline = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            config.format,
            true,
            wgpu::BlendState::ALPHA_BLENDING,
            "textured map pipeline",
            &vertex_layout,
        );
        let collision_pipeline = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            config.format,
            false,
            wgpu::BlendState::ALPHA_BLENDING,
            "collision overlay pipeline",
            &vertex_layout,
        );
        let player_pipeline = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            config.format,
            true,
            wgpu::BlendState::REPLACE,
            "field player pipeline",
            &vertex_layout,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            config.format,
            egui_wgpu::RendererOptions::default(),
        );

        Ok(Self {
            surface,
            device,
            queue,
            config,
            map_pipeline,
            collision_pipeline,
            player_pipeline,
            camera_buffer,
            camera_bind_group,
            texture_bind_group,
            map_buffer,
            map_vertex_count,
            collision_buffer,
            collision_vertex_capacity: collision_capacity,
            player_buffer,
            npc_buffer,
            npc_vertex_capacity,
            depth_view,
            egui_renderer,
            fallback_rect: atlas.fallback_rect,
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.depth_view = create_depth_view(&self.device, size);
    }

    pub fn render(
        &mut self,
        window: &Window,
        session: &FieldSession,
        egui_ctx: &Context,
        egui_platform: &mut egui_winit::State,
        egui_input: egui::RawInput,
        audio_available: bool,
    ) -> Result<()> {
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        self.resize(size);
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("wgpu rejected the current surface texture".to_owned());
            }
        };

        let mut output = egui_ctx.run_ui(egui_input, |ui| {
            debug_panel(ui, session, audio_available);
            dialogue_panel(ui, session);
        });
        egui_platform.handle_platform_output(window, output.platform_output);
        let texture_updates = std::mem::take(&mut output.textures_delta.set);
        let textures_to_free = std::mem::take(&mut output.textures_delta.free);
        for (id, deltas) in texture_updates {
            for delta in deltas {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, id, &delta);
            }
        }
        let paint_jobs = egui_ctx.tessellate(output.shapes, output.pixels_per_point);
        let screen = ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: output.pixels_per_point,
        };
        let player_vertices = make_player_vertices(session, self.fallback_rect);
        let npc_vertices = make_npc_vertices(session, self.fallback_rect);
        if npc_vertices.len() > self.npc_vertex_capacity as usize {
            return Err("NPC markers exceed their reserved vertex capacity".to_owned());
        }
        if !npc_vertices.is_empty() {
            self.queue
                .write_buffer(&self.npc_buffer, 0, bytemuck::cast_slice(&npc_vertices));
        }
        self.queue.write_buffer(
            &self.player_buffer,
            0,
            bytemuck::cast_slice(&player_vertices),
        );
        let collision_vertices = make_collision_vertices(session, self.fallback_rect);
        if collision_vertices.len() > self.collision_vertex_capacity as usize {
            return Err("collision overlay exceeds its reserved vertex capacity".to_owned());
        }
        if !collision_vertices.is_empty() {
            self.queue.write_buffer(
                &self.collision_buffer,
                0,
                bytemuck::cast_slice(&collision_vertices),
            );
        }
        self.write_camera(size, session);

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("field frame encoder"),
            });
        let callback_commands = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );
        {
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.06,
                        g: 0.08,
                        b: 0.12,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render BW2 map and player"),
                color_attachments: &colors,
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.map_pipeline);
            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            pass.set_bind_group(1, &self.texture_bind_group, &[]);
            pass.set_vertex_buffer(0, self.map_buffer.slice(..));
            pass.draw(0..self.map_vertex_count, 0..1);
            if !collision_vertices.is_empty() {
                pass.set_pipeline(&self.collision_pipeline);
                pass.set_vertex_buffer(0, self.collision_buffer.slice(..));
                pass.draw(0..collision_vertices.len() as u32, 0..1);
            }
            if !npc_vertices.is_empty() {
                pass.set_pipeline(&self.player_pipeline);
                pass.set_vertex_buffer(0, self.npc_buffer.slice(..));
                pass.draw(0..npc_vertices.len() as u32, 0..1);
            }
            pass.set_pipeline(&self.player_pipeline);
            pass.set_vertex_buffer(0, self.player_buffer.slice(..));
            pass.draw(0..player_vertices.len() as u32, 0..1);
        }
        {
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })];
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render egui debug interface"),
                color_attachments: &colors,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.egui_renderer
                .render(&mut pass.forget_lifetime(), &paint_jobs, &screen);
        }
        self.queue.submit(
            callback_commands
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        self.queue.present(frame);
        for id in &textures_to_free {
            self.egui_renderer.free_texture(id);
        }
        Ok(())
    }

    fn write_camera(&self, size: PhysicalSize<u32>, session: &FieldSession) {
        let center = player_world_position(session) + Vec3::Y * 0.75;
        let eye = center + Vec3::new(7.5, 9.5, 10.5);
        let view = look_at_rh(eye, center, Vec3::Y);
        let aspect = size.width as f32 / size.height.max(1) as f32;
        let projection = perspective_rh(48.0f32.to_radians(), aspect, 0.05, 160.0);
        let uniform = CameraUniform {
            view_projection: (projection * view).to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
    }
}

#[derive(Debug, Clone, Copy)]
struct AtlasEntry {
    origin: [f32; 2],
    scale: [f32; 2],
    width: f32,
    height: f32,
}

struct TextureAtlas {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    entries: HashMap<String, AtlasEntry>,
    fallback_rect: [f32; 4],
}

fn build_texture_atlas(textures: &[NitroTexture], max_dimension: u32) -> Result<TextureAtlas> {
    let max_dimension = max_dimension.max(64);
    let mut order = (0..textures.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| std::cmp::Reverse(textures[*index].height));
    let total_area = textures
        .iter()
        .map(|texture| u64::from(texture.width + 2) * u64::from(texture.height + 2))
        .sum::<u64>()
        .max(64);
    let largest_width = textures
        .iter()
        .map(|texture| u32::from(texture.width) + 2)
        .max()
        .unwrap_or(2);
    let target_width = ((total_area as f64).sqrt() * 1.35).ceil() as u32;
    let atlas_width = target_width
        .max(largest_width)
        .next_power_of_two()
        .clamp(64, max_dimension);
    if largest_width > atlas_width {
        return Err(format!(
            "map texture width {} exceeds the GPU atlas limit {atlas_width}",
            largest_width - 2
        ));
    }

    let mut positions = vec![(0u32, 0u32); textures.len()];
    let (mut x, mut y, mut shelf_height) = (2u32, 0u32, 0u32);
    for index in order {
        let width = u32::from(textures[index].width) + 2;
        let height = u32::from(textures[index].height) + 2;
        if x + width > atlas_width {
            y += shelf_height;
            x = 0;
            shelf_height = 0;
        }
        if y + height > max_dimension {
            return Err("BW2 map textures exceed the GPU atlas size limit".to_owned());
        }
        positions[index] = (x + 1, y + 1);
        x += width;
        shelf_height = shelf_height.max(height);
    }
    let needed_height = (y + shelf_height).max(1).next_power_of_two();
    if needed_height > max_dimension {
        return Err("BW2 map texture atlas height exceeds the GPU limit".to_owned());
    }
    let atlas_height = needed_height.max(64);
    let pixel_count = usize::try_from(u64::from(atlas_width) * u64::from(atlas_height))
        .map_err(|_| "map texture atlas is too large for this platform".to_owned())?;
    let mut pixels = vec![0u8; pixel_count * 4];
    pixels[..4].copy_from_slice(&[255, 255, 255, 255]);
    let fallback_rect = [
        0.5 / atlas_width as f32,
        0.5 / atlas_height as f32,
        0.0,
        0.0,
    ];
    let mut entries = HashMap::with_capacity(textures.len());
    for (index, texture) in textures.iter().enumerate() {
        let (origin_x, origin_y) = positions[index];
        let width = usize::from(texture.width);
        let height = usize::from(texture.height);
        if texture.rgba8.len() != width * height * 4 {
            return Err(format!(
                "decoded texture '{}' has an unexpected pixel buffer size",
                texture.name
            ));
        }
        for row in 0..height {
            let source_start = row * width * 4;
            let target_start =
                ((origin_y as usize + row) * atlas_width as usize + origin_x as usize) * 4;
            pixels[target_start..target_start + width * 4]
                .copy_from_slice(&texture.rgba8[source_start..source_start + width * 4]);
        }
        entries.insert(
            texture.name.clone(),
            AtlasEntry {
                origin: [
                    (origin_x as f32 + 0.5) / atlas_width as f32,
                    (origin_y as f32 + 0.5) / atlas_height as f32,
                ],
                scale: [
                    texture.width as f32 / atlas_width as f32,
                    texture.height as f32 / atlas_height as f32,
                ],
                width: f32::from(texture.width),
                height: f32::from(texture.height),
            },
        );
    }
    Ok(TextureAtlas {
        width: atlas_width,
        height: atlas_height,
        pixels,
        entries,
        fallback_rect,
    })
}

fn make_map_vertices(
    scene: &[SceneTriangle],
    atlas: &HashMap<String, AtlasEntry>,
    fallback_rect: [f32; 4],
) -> Vec<GpuVertex> {
    scene
        .iter()
        .flat_map(|triangle| {
            let texture = triangle
                .texture_name
                .as_ref()
                .and_then(|name| atlas.get(name));
            let rect = texture.map_or(fallback_rect, |texture| {
                [
                    texture.origin[0],
                    texture.origin[1],
                    texture.scale[0],
                    texture.scale[1],
                ]
            });
            (0..3).map(move |index| {
                let uv = texture.map_or([0.0, 0.0], |texture| {
                    [
                        triangle.uvs[index][0] / texture.width,
                        triangle.uvs[index][1] / texture.height,
                    ]
                });
                GpuVertex {
                    position: triangle.positions[index],
                    color: triangle.colors[index],
                    uv,
                    atlas_rect: rect,
                }
            })
        })
        .collect()
}

fn player_world_position(session: &FieldSession) -> Vec3 {
    let height = session
        .terrain
        .tile(session.player.x, session.player.z)
        .map_or(0.0, |tile| tile.height_world());
    Vec3::new(
        session.player.x as f32 * 0.25 - 4.0 + 0.125,
        height,
        session.player.z as f32 * 0.25 - 4.0 + 0.125,
    )
}

fn make_player_vertices(session: &FieldSession, fallback_rect: [f32; 4]) -> Vec<GpuVertex> {
    let center = player_world_position(session);
    let cell = 0.25;
    let torso = box_vertices(
        center + Vec3::Y * 0.22,
        Vec3::new(cell * 0.34, 0.22, cell * 0.28),
        [0.12, 0.40, 0.92, 1.0],
        fallback_rect,
    );
    let head = box_vertices(
        center + Vec3::Y * 0.53,
        Vec3::splat(cell * 0.29),
        [1.0, 0.73, 0.48, 1.0],
        fallback_rect,
    );
    torso.into_iter().chain(head).collect()
}

fn make_npc_vertices(session: &FieldSession, fallback_rect: [f32; 4]) -> Vec<GpuVertex> {
    let cell = 0.25;
    let mut vertices = Vec::new();
    for npc in &session.npcs {
        let Some(tile) = session.terrain.tile(npc.x, npc.z) else {
            continue;
        };
        let center = Vec3::new(
            npc.x as f32 * cell - 4.0 + cell * 0.5,
            tile.height_world(),
            npc.z as f32 * cell - 4.0 + cell * 0.5,
        );
        let torso = box_vertices(
            center + Vec3::Y * 0.22,
            Vec3::new(cell * 0.34, 0.22, cell * 0.28),
            [0.12, 0.68, 0.30, 1.0],
            fallback_rect,
        );
        let head = box_vertices(
            center + Vec3::Y * 0.53,
            Vec3::splat(cell * 0.29),
            [0.96, 0.78, 0.52, 1.0],
            fallback_rect,
        );
        vertices.extend(torso);
        vertices.extend(head);
    }
    vertices
}

fn box_vertices(
    center: Vec3,
    half_size: Vec3,
    color: [f32; 4],
    atlas_rect: [f32; 4],
) -> Vec<GpuVertex> {
    let p = [
        center + Vec3::new(-half_size.x, -half_size.y, -half_size.z),
        center + Vec3::new(half_size.x, -half_size.y, -half_size.z),
        center + Vec3::new(half_size.x, half_size.y, -half_size.z),
        center + Vec3::new(-half_size.x, half_size.y, -half_size.z),
        center + Vec3::new(-half_size.x, -half_size.y, half_size.z),
        center + Vec3::new(half_size.x, -half_size.y, half_size.z),
        center + Vec3::new(half_size.x, half_size.y, half_size.z),
        center + Vec3::new(-half_size.x, half_size.y, half_size.z),
    ];
    let faces = [
        [0, 2, 1, 0, 3, 2],
        [4, 5, 6, 4, 6, 7],
        [0, 1, 5, 0, 5, 4],
        [3, 7, 6, 3, 6, 2],
        [0, 4, 7, 0, 7, 3],
        [1, 2, 6, 1, 6, 5],
    ];
    faces
        .into_iter()
        .flatten()
        .map(|index| GpuVertex {
            position: p[index].to_array(),
            color,
            uv: [0.0; 2],
            atlas_rect,
        })
        .collect()
}

fn collision_vertex_capacity(session: &FieldSession) -> u32 {
    let width = usize::from(session.terrain.width).min(25);
    let height = usize::from(session.terrain.height).min(25);
    (width * height * 6) as u32
}

fn make_collision_vertices(session: &FieldSession, fallback_rect: [f32; 4]) -> Vec<GpuVertex> {
    const RADIUS: usize = 12;
    let min_x = session.player.x.saturating_sub(RADIUS);
    let min_z = session.player.z.saturating_sub(RADIUS);
    let max_x = (session.player.x + RADIUS + 1).min(usize::from(session.terrain.width));
    let max_z = (session.player.z + RADIUS + 1).min(usize::from(session.terrain.height));
    let mut vertices = Vec::with_capacity((max_x - min_x) * (max_z - min_z) * 6);
    for z in min_z..max_z {
        for x in min_x..max_x {
            let Some(tile) = session.terrain.tile(x, z) else {
                continue;
            };
            if tile.tile_type == u32::MAX {
                continue;
            }
            let color = if tile.is_walkable() {
                [0.12, 0.92, 0.42, 0.30]
            } else {
                [0.96, 0.18, 0.14, 0.34]
            };
            let x0 = x as f32 * 0.25 - 4.0;
            let z0 = z as f32 * 0.25 - 4.0;
            let y = tile.height_world() + 0.025;
            let corners = [
                Vec3::new(x0, y, z0),
                Vec3::new(x0 + 0.25, y, z0),
                Vec3::new(x0 + 0.25, y, z0 + 0.25),
                Vec3::new(x0, y, z0 + 0.25),
            ];
            for index in [0usize, 1, 2, 0, 2, 3] {
                vertices.push(GpuVertex {
                    position: corners[index].to_array(),
                    color,
                    uv: [0.0; 2],
                    atlas_rect: fallback_rect,
                });
            }
        }
    }
    vertices
}

fn create_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    depth_write_enabled: bool,
    blend: wgpu::BlendState,
    label: &'static str,
    vertex_layout: &wgpu::VertexBufferLayout<'_>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(vertex_layout.clone())],
        },
        primitive: wgpu::PrimitiveState {
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth24Plus,
            depth_write_enabled: Some(depth_write_enabled),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn create_depth_view(device: &wgpu::Device, size: PhysicalSize<u32>) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("field depth buffer"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth24Plus,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn debug_panel(root: &mut egui::Ui, session: &FieldSession, audio_available: bool) {
    egui::Area::new(egui::Id::unique("field_debug"))
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(14.0, 14.0))
        .show(root.ctx(), |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(188))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_max_width(260.0);
                    ui.label(format!(
                        "{} · zone {}",
                        session.zone.display_name(),
                        session.zone.zone_id
                    ));
                    ui.monospace(format!(
                        "Player: ({}, {})",
                        session.player.x, session.player.z
                    ));
                    ui.monospace(format!(
                        "Facing: {:?} · steps: {}",
                        session.player.facing, session.player.steps
                    ));
                    ui.label(format!(
                        "Terrain: {}×{}",
                        session.terrain.width, session.terrain.height
                    ));
                    ui.label(if audio_available {
                        "Audio backend: ready"
                    } else {
                        "Audio backend: unavailable"
                    });
                    ui.separator();
                    ui.label("WASD / arrows · controller D-pad");
                    ui.label("Enter / Space / controller A · talk / continue");
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::from_rgb(45, 230, 110), "Walkable");
                        ui.colored_label(egui::Color32::from_rgb(245, 70, 60), "Blocked");
                    });
                });
        });
}

fn dialogue_panel(root: &mut egui::Ui, session: &FieldSession) {
    let Some(dialogue) = session.dialogue() else {
        return;
    };
    egui::Area::new(egui::Id::unique("field_dialogue"))
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -22.0))
        .order(egui::Order::Foreground)
        .show(root.ctx(), |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(16, 20, 34, 238))
                .stroke(egui::Stroke::new(
                    2.0,
                    egui::Color32::from_rgb(236, 235, 220),
                ))
                .inner_margin(egui::Margin::same(18))
                .show(ui, |ui| {
                    ui.set_width(900.0);
                    ui.label(
                        egui::RichText::new(dialogue)
                            .size(22.0)
                            .color(egui::Color32::WHITE),
                    );
                    ui.add_space(8.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("Enter / Space / A")
                                .small()
                                .color(egui::Color32::from_rgb(190, 204, 225)),
                        );
                    });
                });
        });
}

fn look_at_rh(eye: Vec3, center: Vec3, up: Vec3) -> Mat4 {
    let forward = (center - eye).normalize();
    let right = forward.cross(up).normalize();
    let camera_up = right.cross(forward);
    Mat4::from_cols(
        Vec4::new(right.x, camera_up.x, -forward.x, 0.0),
        Vec4::new(right.y, camera_up.y, -forward.y, 0.0),
        Vec4::new(right.z, camera_up.z, -forward.z, 0.0),
        Vec4::new(-right.dot(eye), -camera_up.dot(eye), forward.dot(eye), 1.0),
    )
}

fn perspective_rh(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let focal_length = 1.0 / (fov_y * 0.5).tan();
    Mat4::from_cols(
        Vec4::new(focal_length / aspect, 0.0, 0.0, 0.0),
        Vec4::new(0.0, focal_length, 0.0, 0.0),
        Vec4::new(0.0, 0.0, far / (near - far), -1.0),
        Vec4::new(0.0, 0.0, far * near / (near - far), 0.0),
    )
}
