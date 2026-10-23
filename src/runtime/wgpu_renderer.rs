use crate::Result;
use crate::game::bw2::field::FieldSession;
use crate::render::SceneTriangle;
use bytemuck::{Pod, Zeroable};
use egui::Context;
use egui_wgpu::ScreenDescriptor;
use glam::{Mat4, Vec3, Vec4};
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
    pipeline: wgpu::RenderPipeline,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    map_buffer: wgpu::Buffer,
    map_vertex_count: u32,
    player_buffer: wgpu::Buffer,
    depth_view: wgpu::TextureView,
    egui_renderer: egui_wgpu::Renderer,
    bounds_min: Vec3,
    bounds_max: Vec3,
}

impl WgpuRenderer {
    pub async fn new(
        window: Arc<Window>,
        session: &FieldSession,
        scene: &[SceneTriangle],
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

        let vertices = make_map_vertices(scene);
        let map_vertex_count = u32::try_from(vertices.len())
            .map_err(|_| "map vertex count exceeds the GPU draw limit".to_owned())?;
        let map_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Aspertia map model vertices"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let bounds = scene_bounds(scene)?;
        let initial_player = make_player_vertices(session, bounds.0, bounds.1);
        let player_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("field player marker vertices"),
            contents: bytemuck::cast_slice(&initial_player),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });

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
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("field map shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("map.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("field render pipeline layout"),
            bind_group_layouts: &[Some(&camera_layout)],
            immediate_size: 0,
        });
        const ATTRIBUTES: [wgpu::VertexAttribute; 2] =
            wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4];
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &ATTRIBUTES,
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("field map and player pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState {
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth24Plus,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
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
            pipeline,
            camera_buffer,
            camera_bind_group,
            map_buffer,
            map_vertex_count,
            player_buffer,
            depth_view,
            egui_renderer,
            bounds_min: bounds.0,
            bounds_max: bounds.1,
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
        let player_vertices = make_player_vertices(session, self.bounds_min, self.bounds_max);
        self.queue.write_buffer(
            &self.player_buffer,
            0,
            bytemuck::cast_slice(&player_vertices),
        );
        self.write_camera(size);

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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.camera_bind_group, &[]);
            pass.set_vertex_buffer(0, self.map_buffer.slice(..));
            pass.draw(0..self.map_vertex_count, 0..1);
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

    fn write_camera(&self, size: PhysicalSize<u32>) {
        let center = (self.bounds_min + self.bounds_max) * 0.5;
        let extent = self.bounds_max - self.bounds_min;
        let radius = extent.length().max(1.0) * 0.5;
        let eye = center + Vec3::new(radius * 1.15, radius * 1.2, radius * 1.35);
        let view = look_at_rh(eye, center, Vec3::Y);
        let aspect = size.width as f32 / size.height.max(1) as f32;
        let projection =
            perspective_rh(48.0f32.to_radians(), aspect, radius * 0.005, radius * 12.0);
        let uniform = CameraUniform {
            view_projection: (projection * view).to_cols_array_2d(),
        };
        self.queue
            .write_buffer(&self.camera_buffer, 0, bytemuck::bytes_of(&uniform));
    }
}

fn make_map_vertices(scene: &[SceneTriangle]) -> Vec<GpuVertex> {
    scene
        .iter()
        .flat_map(|triangle| {
            (0..3).map(|index| GpuVertex {
                position: triangle.positions[index],
                color: triangle.colors[index],
            })
        })
        .collect()
}

fn scene_bounds(scene: &[SceneTriangle]) -> Result<(Vec3, Vec3)> {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for triangle in scene {
        for point in triangle.positions {
            let point = Vec3::from_array(point);
            min = min.min(point);
            max = max.max(point);
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return Err("map geometry has no finite bounds".to_owned());
    }
    Ok((min, max))
}

fn make_player_vertices(session: &FieldSession, min: Vec3, max: Vec3) -> Vec<GpuVertex> {
    let width = (max.x - min.x) / f32::from(session.terrain.width);
    let depth = (max.z - min.z) / f32::from(session.terrain.height);
    let cell = width.min(depth).max(0.05);
    let ground_y = min.y + (max.y - min.y).max(cell) * 0.015;
    let center = Vec3::new(
        min.x + (session.player.x as f32 + 0.5) * width,
        ground_y,
        min.z + (session.player.z as f32 + 0.5) * depth,
    );
    let torso = box_vertices(
        center + Vec3::Y * (cell * 0.42),
        Vec3::new(cell * 0.36, cell * 0.72, cell * 0.3),
        [0.12, 0.40, 0.92, 1.0],
    );
    let head = box_vertices(
        center + Vec3::Y * (cell * 0.98),
        Vec3::splat(cell * 0.22),
        [1.0, 0.73, 0.48, 1.0],
    );
    torso.into_iter().chain(head).collect()
}

fn box_vertices(center: Vec3, half_size: Vec3, color: [f32; 4]) -> Vec<GpuVertex> {
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
        })
        .collect()
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
    egui::Panel::left("field_debug")
        .resizable(false)
        .frame(egui::Frame::new().fill(egui::Color32::from_black_alpha(190)))
        .show(root, |ui| {
            ui.set_min_width(240.0);
            ui.set_max_width(280.0);
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
            ui.label("WASD / arrow keys · controller D-pad");
            ui.label("Gray cells block movement; unknown terrain types stay solid.");
            draw_collision_minimap(ui, session);
        });
}

fn draw_collision_minimap(ui: &mut egui::Ui, session: &FieldSession) {
    let cell_size = 6.0;
    let size = egui::vec2(
        f32::from(session.terrain.width) * cell_size,
        f32::from(session.terrain.height) * cell_size,
    );
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    for z in 0..usize::from(session.terrain.height) {
        for x in 0..usize::from(session.terrain.width) {
            let tile = session.terrain.tile(x, z);
            let color = if (session.player.x, session.player.z) == (x, z) {
                egui::Color32::from_rgb(240, 75, 66)
            } else if tile.is_some_and(|tile| tile.is_walkable()) {
                egui::Color32::from_rgb(102, 157, 133)
            } else {
                egui::Color32::from_rgb(62, 70, 83)
            };
            let tile_rect = egui::Rect::from_min_size(
                rect.min + egui::vec2(x as f32 * cell_size, z as f32 * cell_size),
                egui::vec2(cell_size - 1.0, cell_size - 1.0),
            );
            painter.rect_filled(tile_rect, 0.0, color);
        }
    }
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
