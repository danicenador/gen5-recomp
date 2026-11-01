//! A window for the recompiled game: both DS screens stacked, scaled to
//! fit with square pixels, keyboard buttons and the mouse as the stylus.
//! The game runs on its own threads and hands frames over through
//! `Screens`; this side only presents the latest one and records input.

use crate::Result;
use crate::nds::gpu2d::{Frame, HEIGHT, WIDTH};
use crate::nds::input::{self, Input};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// The latest frame from the game, with a count so unchanged frames are not
/// uploaded again.
#[derive(Default)]
pub struct Screens {
    latest: Mutex<Option<(u64, Frame)>>,
}

impl Screens {
    pub fn publish(&self, number: u64, frame: &Frame) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some((number, frame.clone()));
        }
    }

    fn take_newer(&self, than: u64) -> Option<(u64, Frame)> {
        let latest = self.latest.lock().ok()?;
        latest
            .as_ref()
            .filter(|(number, _)| *number != than)
            .cloned()
    }
}

/// DS display refresh: 33.51 MHz / (355 * 263 * 6) ≈ 59.83 Hz.
pub const FRAME_TIME: Duration = Duration::from_nanos(16_715_646);

/// Run the window until it is closed; then the process exits.
pub fn run(title: &str, screens: Arc<Screens>, input: Arc<Input>) -> Result<()> {
    let event_loop =
        EventLoop::new().map_err(|error| format!("could not create event loop: {error}"))?;
    let mut app = App {
        title: title.to_owned(),
        screens,
        input,
        window: None,
        gpu: None,
        pressed: 0,
        cursor: None,
        pen_down: false,
        shown: u64::MAX,
        error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|error| format!("window event loop failed: {error}"))?;
    app.error.map_or(Ok(()), Err)
}

struct App {
    title: String,
    screens: Arc<Screens>,
    input: Arc<Input>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    pressed: u32,
    cursor: Option<PhysicalPosition<f64>>,
    pen_down: bool,
    shown: u64,
    error: Option<String>,
}

fn button(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::KeyX => input::A,
        KeyCode::KeyZ => input::B,
        KeyCode::KeyS => input::X,
        KeyCode::KeyA => input::Y,
        KeyCode::KeyQ => input::L,
        KeyCode::KeyW => input::R,
        KeyCode::Enter => input::START,
        KeyCode::ShiftRight | KeyCode::Backspace => input::SELECT,
        KeyCode::ArrowUp => input::UP,
        KeyCode::ArrowDown => input::DOWN,
        KeyCode::ArrowLeft => input::LEFT,
        KeyCode::ArrowRight => input::RIGHT,
        _ => return None,
    })
}

/// Where the two screens are drawn in a window of this size: x, y, scale.
fn placement(width: u32, height: u32) -> (f32, f32, f32) {
    let scale = (width as f32 / WIDTH as f32).min(height as f32 / (2 * HEIGHT) as f32);
    let x = (width as f32 - WIDTH as f32 * scale) / 2.0;
    let y = (height as f32 - (2 * HEIGHT) as f32 * scale) / 2.0;
    (x, y, scale)
}

impl App {
    fn update_touch(&self) {
        let point = self.cursor.filter(|_| self.pen_down).and_then(|cursor| {
            let size = self.window.as_ref()?.inner_size();
            let (x, y, scale) = placement(size.width, size.height);
            let px = ((cursor.x as f32 - x) / scale).floor();
            let py = ((cursor.y as f32 - y) / scale).floor() - HEIGHT as f32;
            ((0.0..WIDTH as f32).contains(&px) && (0.0..HEIGHT as f32).contains(&py))
                .then_some((px as u8, py as u8))
        });
        self.input.set_touch(point);
    }

    fn redraw(&mut self) -> Result<()> {
        let (Some(window), Some(gpu)) = (&self.window, &mut self.gpu) else {
            return Ok(());
        };
        if let Some((number, frame)) = self.screens.take_newer(self.shown) {
            self.shown = number;
            gpu.upload(&frame);
        }
        gpu.draw(window.inner_size())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(winit::dpi::LogicalSize::new(512.0, 768.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.error = Some(format!("could not create window: {error}"));
                event_loop.exit();
                return;
            }
        };
        match pollster::block_on(Gpu::new(window.clone())) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if code == KeyCode::Escape {
                        event_loop.exit();
                        return;
                    }
                    if let Some(bit) = button(code) {
                        match event.state {
                            ElementState::Pressed => self.pressed |= bit,
                            ElementState::Released => self.pressed &= !bit,
                        }
                        self.input.set_buttons(self.pressed);
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                self.update_touch();
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.update_touch();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.pen_down = state == ElementState::Pressed;
                self.update_touch();
            }
            WindowEvent::Focused(false) => {
                self.pressed = 0;
                self.pen_down = false;
                self.input.set_buttons(0);
                self.update_touch();
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    self.error = Some(error);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_TIME / 2));
    }
}

const SHADER: &str = r"
struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Out {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: Out;
    out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var screen: texture_2d<f32>;
@group(0) @binding(1) var screen_sampler: sampler;

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    return textureSample(screen, screen_sampler, in.uv);
}
";

struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
}

impl Gpu {
    async fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|error| format!("could not create the window surface: {error}"))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("no compatible graphics adapter was found: {error}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("gen5 screen"),
                ..Default::default()
            })
            .await
            .map_err(|error| format!("could not create graphics device: {error}"))?;
        let size = window.inner_size();
        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "graphics surface has no supported configuration".to_owned())?;
        surface.configure(&device, &config);

        // Frames are sRGB colours; match the surface so they pass through.
        let format = if config.format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DS screens"),
            size: wgpu::Extent3d {
                width: WIDTH as u32,
                height: 2 * HEIGHT as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("DS screens nearest sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("DS screens layout"),
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DS screens bind group"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DS screens shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("DS screens pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("DS screens pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            texture,
            bind_group,
            pipeline,
        })
    }

    fn upload(&self, frame: &Frame) {
        let mut pixels = frame.top.clone();
        pixels.extend_from_slice(&frame.bottom);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH as u32 * 4),
                rows_per_image: Some(2 * HEIGHT as u32),
            },
            wgpu::Extent3d {
                width: WIDTH as u32,
                height: 2 * HEIGHT as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    fn draw(&mut self, size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        if (size.width, size.height) != (self.config.width, self.config.height) {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
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
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("DS screens frame"),
            });
        {
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("draw DS screens"),
                color_attachments: &colors,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let (x, y, scale) = placement(size.width, size.height);
            pass.set_viewport(
                x,
                y,
                WIDTH as f32 * scale,
                (2 * HEIGHT) as f32 * scale,
                0.0,
                1.0,
            );
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_fit_with_square_pixels() {
        assert_eq!(placement(512, 768), (0.0, 0.0, 2.0));
        // A wide window letterboxes horizontally.
        let (x, y, scale) = placement(1000, 384);
        assert_eq!((y, scale), (0.0, 1.0));
        assert_eq!(x, (1000.0 - 256.0) / 2.0);
    }
}
