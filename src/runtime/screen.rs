//! A window for the recompiled game: both DS screens stacked, scaled to
//! fit with square pixels, keyboard buttons and the mouse as the stylus.
//! The game runs on its own threads and hands frames over through
//! `Screens`; this side presents them and records input. With an `Fps`
//! other than `Ds` it draws between game frames too (see `pacing`).

use super::pacing::{self, Fps, FrameTime, Pick};
use crate::Result;
use crate::nds::gpu2d::{Frame, HEIGHT, WIDTH};
use crate::nds::gpu3d::{DisplayList, Matched};
use crate::nds::input::{self, Input};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Frames from the game that the window has not taken yet.
#[derive(Default)]
pub struct Screens {
    queue: Mutex<VecDeque<Published>>,
}

/// A game frame, numbered, and when it is due on screen.
pub struct Published {
    pub number: u64,
    pub frame: Frame,
    pub deadline: Instant,
}

impl Published {
    fn list(&self) -> Option<&Arc<DisplayList>> {
        let layer = self.frame.scaled.as_ref()?.three_d.as_ref()?;
        Some(&layer.scene.list)
    }

    fn time(&self) -> FrameTime {
        FrameTime {
            deadline: self.deadline,
            list: self.list().map(|list| list.swap),
        }
    }
}

/// Frames held for a window that stopped taking them (minimized, say).
const QUEUE: usize = 8;

impl Screens {
    /// Hand over frame `number`, due on screen at `deadline`.
    pub fn publish(&self, number: u64, frame: &Frame, deadline: Instant) {
        let published = Published {
            number,
            frame: frame.clone(),
            deadline,
        };
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(published);
            while queue.len() > QUEUE {
                queue.pop_front();
            }
        }
    }

    fn take(&self) -> Vec<Published> {
        self.queue
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }
}

/// Presents and interpolation over the last half second, for the title.
struct Rate {
    since: Instant,
    presents: u32,
    matched: Matched,
    shown: String,
}

/// DS display refresh: 33.51 MHz / (355 * 263 * 6) ≈ 59.83 Hz.
pub const FRAME_TIME: Duration = Duration::from_nanos(16_715_646);

/// The largest resolution multiple keys 1-8 select.
pub const MAX_SCALE: u32 = 8;

/// Run the window until it is closed; then the process exits. `scale` is
/// the resolution multiple frames are drawn at; keys 1-8 change it. `fps`
/// is how often it draws; F turns interpolation off and on to compare.
pub fn run(
    title: &str,
    screens: Arc<Screens>,
    input: Arc<Input>,
    scale: Arc<AtomicU32>,
    fps: Fps,
) -> Result<()> {
    let event_loop =
        EventLoop::new().map_err(|error| format!("could not create event loop: {error}"))?;
    let mut app = App {
        title: title.to_owned(),
        screens,
        input,
        scale,
        window: None,
        gpu: None,
        pressed: 0,
        cursor: None,
        pen_down: false,
        shown: u64::MAX,
        error: None,
        fps,
        interpolate: fps.interpolates(),
        history: VecDeque::new(),
        drawn: None,
        next_present: Instant::now(),
        rate: Rate {
            since: Instant::now(),
            presents: 0,
            matched: Matched::default(),
            shown: String::new(),
        },
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
    scale: Arc<AtomicU32>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    pressed: u32,
    cursor: Option<PhysicalPosition<f64>>,
    pen_down: bool,
    shown: u64,
    error: Option<String>,
    fps: Fps,
    /// Drawing between game frames; F toggles it.
    interpolate: bool,
    /// Frames taken from the game, oldest first, that may still be shown.
    history: VecDeque<Published>,
    /// What the scaled renderer last drew: frame, next list, alpha, scale.
    drawn: Option<(u64, Option<u64>, u64, u32)>,
    /// With a frame-rate cap, when to present next.
    next_present: Instant,
    rate: Rate,
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
        self.history.extend(self.screens.take());
        let scale = self.scale.load(Ordering::Relaxed);
        if !self.history.is_empty() {
            let pick = if self.interpolate {
                let times: Vec<FrameTime> = self.history.iter().map(Published::time).collect();
                pacing::pick(&times, Instant::now())
            } else {
                Pick::newest(self.history.len())
            };
            let shown = &self.history[pick.shown];
            if shown.number != self.shown {
                self.shown = shown.number;
                gpu.upload(&shown.frame);
            }
            if gpu.showing_scaled {
                let next = pick.next.and_then(|next| self.history[next].list());
                let key = (
                    self.shown,
                    next.map(|list| list.swap),
                    pick.alpha.to_bits(),
                    scale,
                );
                if self.drawn != Some(key) {
                    self.drawn = Some(key);
                    let matched = gpu.render_scaled(scale, next.map(|list| &**list), pick.alpha);
                    if pick.next.is_some() {
                        self.rate.matched.matched += matched.matched;
                        self.rate.matched.total += matched.total;
                    }
                }
            }
            self.history.drain(..pick.keep_from);
        }
        gpu.draw(window.inner_size())?;
        self.rate.presents += 1;
        let elapsed = self.rate.since.elapsed();
        if self.fps.interpolates() && elapsed >= Duration::from_millis(500) {
            let presents = f64::from(self.rate.presents) / elapsed.as_secs_f64();
            let matched = self.rate.matched;
            let shown = if !self.interpolate {
                format!("{presents:.0} fps, interpolation off (F)")
            } else if let Some(percent) = (matched.matched * 100).checked_div(matched.total) {
                format!("{presents:.0} fps, {percent}% of 3D interpolated")
            } else {
                format!("{presents:.0} fps")
            };
            self.rate = Rate {
                since: Instant::now(),
                presents: 0,
                matched: Matched::default(),
                shown,
            };
            let title = self.window_title();
            if let Some(window) = &self.window {
                window.set_title(&title);
            }
        }
        Ok(())
    }

    fn window_title(&self) -> String {
        let mut title = match self.scale.load(Ordering::Relaxed) {
            1 => self.title.clone(),
            scale => format!("{} - {scale}x resolution", self.title),
        };
        if !self.rate.shown.is_empty() {
            title = format!("{title} - {}", self.rate.shown);
        }
        title
    }
}

/// Keys 1-8 select the resolution multiple.
fn scale_key(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::Digit1 => 1,
        KeyCode::Digit2 => 2,
        KeyCode::Digit3 => 3,
        KeyCode::Digit4 => 4,
        KeyCode::Digit5 => 5,
        KeyCode::Digit6 => 6,
        KeyCode::Digit7 => 7,
        KeyCode::Digit8 => 8,
        _ => return None,
    })
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Big enough to show the chosen resolution, within the monitor.
        let scale = self.scale.load(Ordering::Relaxed);
        let mut attributes = Window::default_attributes()
            .with_title(self.window_title())
            .with_inner_size(winit::dpi::LogicalSize::new(512.0, 768.0));
        if scale > 2 {
            let mut size =
                winit::dpi::PhysicalSize::new(WIDTH as u32 * scale, 2 * HEIGHT as u32 * scale);
            if let Some(monitor) = event_loop.primary_monitor() {
                let limit = monitor.size().height * 9 / 10;
                if size.height > limit {
                    size = winit::dpi::PhysicalSize::new(
                        limit * WIDTH as u32 / (2 * HEIGHT as u32),
                        limit,
                    );
                }
            }
            attributes = attributes.with_inner_size(size);
        }
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.error = Some(format!("could not create window: {error}"));
                event_loop.exit();
                return;
            }
        };
        match pollster::block_on(Gpu::new(window.clone(), self.fps)) {
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
                    if code == KeyCode::KeyF
                        && event.state == ElementState::Pressed
                        && !event.repeat
                        && self.fps.interpolates()
                    {
                        self.interpolate = !self.interpolate;
                        return;
                    }
                    if let Some(scale) = scale_key(code)
                        && event.state == ElementState::Pressed
                    {
                        self.scale.store(scale, Ordering::Relaxed);
                        if let Some(window) = &self.window {
                            window.set_title(&self.window_title());
                        }
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
        let Some(window) = &self.window else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_TIME / 2));
            return;
        };
        match self.fps {
            Fps::Ds => {
                window.request_redraw();
                event_loop
                    .set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_TIME / 2));
            }
            // Vsync, or nothing, paces the presents.
            Fps::Display | Fps::Uncapped => {
                window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Poll);
            }
            Fps::Cap(rate) => {
                let now = Instant::now();
                if now >= self.next_present {
                    self.next_present = pacing::next_present(self.next_present, now, rate);
                    window.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_present));
            }
        }
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
    /// Software frames, at the DS resolution.
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    nearest: wgpu::Sampler,
    linear: wgpu::Sampler,
    /// Frames drawn at a multiple of the DS resolution.
    scaled: Option<super::scaled::Renderer>,
    /// The scaled output's scale, sampled nearest and linear (for windows
    /// smaller than the output), while it is what is shown.
    scaled_view: Option<(u32, wgpu::BindGroup, wgpu::BindGroup)>,
    showing_scaled: bool,
}

fn screens_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("DS screens bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

impl Gpu {
    async fn new(window: Arc<Window>, fps: Fps) -> Result<Self> {
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
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "graphics surface has no supported configuration".to_owned())?;
        config.present_mode = if fps.vsync() {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
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
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("DS screens linear sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
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
            layout,
            nearest: sampler,
            linear,
            scaled: None,
            scaled_view: None,
            showing_scaled: false,
        })
    }

    /// Take the frame to show. A scaled frame is drawn by `render_scaled`.
    fn upload(&mut self, frame: &Frame) {
        self.showing_scaled = frame.scaled.is_some();
        if let Some(scaled) = &frame.scaled {
            self.scaled
                .get_or_insert_with(|| super::scaled::Renderer::new(&self.device, &self.queue))
                .set_frame(scaled);
            return;
        }
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

    /// Draw the uploaded scaled frame, its 3D moved `alpha` of the way to
    /// `next`.
    fn render_scaled(&mut self, scale: u32, next: Option<&DisplayList>, alpha: f64) -> Matched {
        let scale = scale.clamp(1, MAX_SCALE);
        let Some(renderer) = self.scaled.as_mut() else {
            return Matched::default();
        };
        let (output, matched) = renderer.draw(scale, next, alpha);
        if self
            .scaled_view
            .as_ref()
            .is_none_or(|(shown, _, _)| *shown != scale)
        {
            // Sample through the same format as software frames, so
            // sRGB surfaces pass colors through unchanged.
            let view = output.create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.texture.format()),
                ..Default::default()
            });
            let nearest = screens_bind_group(&self.device, &self.layout, &view, &self.nearest);
            let linear = screens_bind_group(&self.device, &self.layout, &view, &self.linear);
            self.scaled_view = Some((scale, nearest, linear));
        }
        matched
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
            let bind_group = match &self.scaled_view {
                // Shrinking a large output: average texels rather than skip them.
                Some((shown, nearest, linear)) if self.showing_scaled => {
                    if (*shown as f32) > scale {
                        linear
                    } else {
                        nearest
                    }
                }
                _ => &self.bind_group,
            };
            pass.set_bind_group(0, bind_group, &[]);
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
