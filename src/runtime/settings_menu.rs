//! Controller-friendly PC options, painted above the game rather than into DS VRAM.
use super::screen::Layout;
use super::settings::{ACTION_COUNT, ACTION_NAMES, Settings};
use crate::nds::input;

pub const WIDTH: f32 = 512.0;
pub const HEIGHT: f32 = 384.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Page {
    Main,
    Keyboard,
    Controller,
}

pub struct Menu {
    pub open: bool,
    pub page: Page,
    pub selected: usize,
    pub capture: Option<usize>,
    pub status: String,
    pub restore_confirm: bool,
}

impl Default for Menu {
    fn default() -> Self {
        Self {
            open: false,
            page: Page::Main,
            selected: 0,
            capture: None,
            status: String::new(),
            restore_confirm: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Effect {
    None,
    Changed,
    Close,
}

impl Menu {
    pub fn show(&mut self) {
        *self = Self {
            open: true,
            ..Self::default()
        };
    }

    pub fn back(&mut self) -> Effect {
        if self.capture.take().is_some() || std::mem::take(&mut self.restore_confirm) {
            return Effect::None;
        }
        if self.page != Page::Main {
            self.page = Page::Main;
            self.selected = 0;
            return Effect::None;
        }
        Effect::Close
    }

    pub fn rows(&self) -> usize {
        if self.page == Page::Main {
            15
        } else {
            ACTION_COUNT
        }
    }

    pub fn buttons(&mut self, pressed: u32, settings: &mut Settings) -> Effect {
        if pressed & input::B != 0 {
            return self.back();
        }
        if self.capture.is_some() {
            return Effect::None;
        }
        if pressed & input::UP != 0 {
            self.selected = (self.selected + self.rows() - 1) % self.rows();
            self.restore_confirm = false;
        }
        if pressed & input::DOWN != 0 {
            self.selected = (self.selected + 1) % self.rows();
            self.restore_confirm = false;
        }
        if pressed & (input::LEFT | input::RIGHT | input::A) == 0 {
            return Effect::None;
        }
        let direction = if pressed & input::LEFT != 0 { -1 } else { 1 };
        self.activate(direction, settings)
    }

    pub fn activate(&mut self, direction: i32, settings: &mut Settings) -> Effect {
        if self.page != Page::Main {
            self.capture = Some(self.selected);
            self.status.clear();
            return Effect::None;
        }
        match self.selected {
            0 => {
                let n = settings.scale.unwrap_or(0) as i32;
                let n = (n + direction).rem_euclid(9) as u32;
                settings.scale = (n != 0).then_some(n);
            }
            1 => settings.widescreen = !settings.widescreen,
            2 => {
                settings.layout = match (settings.layout, direction > 0) {
                    (Layout::Stacked, true) | (Layout::Split, false) => Layout::Single,
                    (Layout::Single, true) | (Layout::Stacked, false) => Layout::Split,
                    _ => Layout::Stacked,
                }
            }
            3 => settings.fullscreen = !settings.fullscreen,
            4 => settings.smart_bottom = !settings.smart_bottom,
            5 => settings.fast_forward = !settings.fast_forward,
            6 => settings.speed = (settings.speed as i32 + direction).clamp(2, 8) as u32,
            7 => settings.interpolate = !settings.interpolate,
            8 => {
                settings.stylus_sensitivity =
                    (settings.stylus_sensitivity + direction as f32 * 0.25).clamp(0.25, 3.0)
            }
            9 => {
                settings.stick_dead_zone =
                    (settings.stick_dead_zone + direction as f32 * 0.1).clamp(0.1, 0.9)
            }
            10 | 11 => {
                self.page = if self.selected == 10 {
                    Page::Keyboard
                } else {
                    Page::Controller
                };
                self.selected = 0;
                return Effect::None;
            }
            12 => settings.auto_continue = !settings.auto_continue,
            13 => {
                if !self.restore_confirm {
                    self.restore_confirm = true;
                    self.status = "Press confirm again to restore defaults".into();
                    return Effect::None;
                }
                *settings = Settings::default();
                self.restore_confirm = false;
            }
            _ => return Effect::Close,
        }
        Effect::Changed
    }

    pub fn bind(&mut self, name: String, settings: &mut Settings) -> Effect {
        let Some(action) = self.capture.take() else {
            return Effect::None;
        };
        settings
            .bindings
            .bind(self.page == Page::Controller, action, name);
        Effect::Changed
    }

    pub fn click(&mut self, x: f32, y: f32, settings: &mut Settings) -> Effect {
        if (28.0..=76.0).contains(&x) && (24.0..=44.0).contains(&y) {
            return self.back();
        }
        if self.capture.is_some() {
            return Effect::None;
        }
        let first = self.first_row();
        if (40.0..472.0).contains(&x) && (62.0..314.0).contains(&y) {
            let row = ((y - 62.0) / 18.0) as usize + first;
            if row < self.rows() {
                self.selected = row;
                return self.activate(if x < 270.0 { -1 } else { 1 }, settings);
            }
        }
        Effect::None
    }

    fn first_row(&self) -> usize {
        self.selected.saturating_sub(13)
    }

    /// `scale` is the multiple frames are drawn at now, which Auto resolves
    /// for the monitor, and `wide` whether the top screen is 16:9.
    fn row(&self, index: usize, s: &Settings, scale: u32, wide: bool) -> (String, String) {
        if self.page != Page::Main {
            let bindings = if self.page == Page::Keyboard {
                &s.bindings.keyboard
            } else {
                &s.bindings.controller
            };
            let value = &bindings[index];
            return (
                ACTION_NAMES[index].into(),
                if self.capture == Some(index) {
                    "Press a button...".into()
                } else if value.is_empty() {
                    "Unbound".into()
                } else {
                    value.clone()
                },
            );
        }
        let on = |b| if b { "On".into() } else { "Off".into() };
        match index {
            0 => ("Render resolution".into(), resolution(s.scale, scale, wide)),
            1 => (
                "Aspect ratio".into(),
                if s.widescreen { "16:9" } else { "4:3" }.into(),
            ),
            2 => (
                "Screen layout".into(),
                match s.layout {
                    Layout::Single => "Single",
                    Layout::Stacked => "Stacked",
                    Layout::Split => "Separate windows",
                }
                .into(),
            ),
            3 => ("Fullscreen".into(), on(s.fullscreen)),
            4 => (
                "Bottom screen".into(),
                if s.smart_bottom { "Smart" } else { "Manual" }.into(),
            ),
            5 => ("Fast-forward".into(), on(s.fast_forward)),
            6 => ("Fast-forward speed".into(), format!("{}x", s.speed)),
            7 => ("3D interpolation".into(), on(s.interpolate)),
            8 => (
                "Stylus sensitivity".into(),
                format!("{:.2}x", s.stylus_sensitivity),
            ),
            9 => (
                "Stick dead zone".into(),
                format!("{:.0}%", s.stick_dead_zone * 100.0),
            ),
            10 => ("Keyboard mappings".into(), ">".into()),
            11 => ("Controller mappings".into(), ">".into()),
            12 => ("Start in saved game".into(), on(s.auto_continue)),
            13 => (
                "Restore defaults".into(),
                if self.restore_confirm {
                    "Confirm?"
                } else {
                    ">"
                }
                .into(),
            ),
            _ => ("Back to game".into(), ">".into()),
        }
    }

    /// `scale` and `wide` are the resolution multiple and aspect ratio the
    /// game is drawn with now (see `row`).
    pub fn paint(&self, s: &Settings, scale: u32, wide: bool) -> Vec<Vertex> {
        let mut paint = Paint(Vec::new());
        paint.rect(0., 0., WIDTH, HEIGHT, [0.02, 0.04, 0.08, 0.75]);
        paint.rect(24., 18., 464., 350., [0.20, 0.25, 0.31, 1.]);
        paint.rect(28., 22., 456., 342., [0.94, 0.96, 0.97, 1.]);
        paint.rect(28., 22., 456., 28., [0.12, 0.17, 0.23, 1.]);
        paint.text(36., 32., "< Back", [1., 1., 1., 1.]);
        paint.text(
            166.,
            32.,
            match self.page {
                Page::Main => "PC SETTINGS",
                Page::Keyboard => "KEYBOARD MAPPINGS",
                Page::Controller => "CONTROLLER MAPPINGS",
            },
            [1., 1., 1., 1.],
        );
        let first = self.first_row();
        for index in first..(first + 14).min(self.rows()) {
            let y = 62. + (index - first) as f32 * 18.;
            let selected = index == self.selected;
            if selected {
                paint.rect(36., y, 440., 17., [0.23, 0.46, 0.65, 1.]);
            }
            let color = if selected {
                [1., 1., 1., 1.]
            } else {
                [0.10, 0.15, 0.20, 1.]
            };
            let (label, value) = self.row(index, s, scale, wide);
            paint.text(44., y + 5., &label, color);
            paint.text(284., y + 5., &value, color);
        }
        paint.text(
            40.,
            326.,
            "D-pad: choose   Left/Right: change   A: confirm",
            [0.12, 0.17, 0.23, 1.],
        );
        paint.text(
            40.,
            340.,
            "B/Esc: back   F1: PC settings   Del: clear binding",
            [0.12, 0.17, 0.23, 1.],
        );
        if !self.status.is_empty() {
            paint.text(40., 354., &self.status, [0.30, 0.22, 0.12, 1.]);
        }
        paint.0
    }
}

/// The render resolution row: the chosen multiple (or Auto and the multiple
/// it picked) and the size of the top screen drawn at it, 256 or 342 columns
/// wide (16:9) and 192 lines per multiple.
fn resolution(chosen: Option<u32>, scale: u32, wide: bool) -> String {
    use crate::nds::gpu2d::{HEIGHT, WIDE_EXTRA, WIDTH};
    let width = (WIDTH + if wide { 2 * WIDE_EXTRA } else { 0 }) as u32 * scale;
    let height = HEIGHT as u32 * scale;
    let auto = if chosen.is_none() { "Auto: " } else { "" };
    format!("{auto}{scale}x ({width} x {height})")
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

pub struct Paint(pub Vec<Vertex>);
impl Paint {
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        for (x, y) in [
            (x, y),
            (x + w, y),
            (x, y + h),
            (x, y + h),
            (x + w, y),
            (x + w, y + h),
        ] {
            self.0.push(Vertex {
                position: [x / WIDTH * 2. - 1., 1. - y / HEIGHT * 2.],
                color,
            });
        }
    }
    pub fn text(&mut self, x: f32, y: f32, text: &str, color: [f32; 4]) {
        self.text_scaled(x, y, text, color, 1.);
    }
    pub fn text_scaled(&mut self, x: f32, y: f32, text: &str, color: [f32; 4], scale: f32) {
        for (i, ch) in text.chars().take(72).enumerate() {
            for (row, bits) in glyph(ch.to_ascii_uppercase()).iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        self.rect(
                            x + (i as f32 * 6. + col as f32) * scale,
                            y + row as f32 * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
        }
    }
}

// Original 5x7 glyphs keep the overlay sharp at DS-like sizes, with no external font assets.
fn glyph(c: char) -> [u8; 7] {
    match c {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 27, 17],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        '.' => [0, 0, 0, 0, 0, 6, 6],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '/' => [1, 2, 2, 4, 8, 8, 16],
        '>' => [16, 8, 4, 2, 4, 8, 16],
        '<' => [1, 2, 4, 8, 4, 2, 1],
        '%' => [17, 2, 4, 8, 16, 17, 0],
        '?' => [14, 17, 1, 2, 4, 0, 4],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        _ => [0; 7],
    }
}

pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
}
impl Renderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("PC settings"),
            source: wgpu::ShaderSource::Wgsl(
                r#"
struct Out { @builtin(position) position: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> Out {
    var out: Out; out.position = vec4<f32>(position, 0.0, 1.0); out.color = color; return out;
}
@fragment fn fs(in: Out) -> @location(0) vec4<f32> { return in.color; }
"#
                .into(),
            ),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("PC settings"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                })],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline }
    }
    pub fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: (u32, u32),
        vertices: &[Vertex],
    ) {
        self.encode_at(device, encoder, view, viewport(size), vertices);
    }
    pub fn encode_at(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        rect: (f32, f32, f32, f32),
        vertices: &[Vertex],
    ) {
        use wgpu::util::DeviceExt;
        if vertices.is_empty() {
            return;
        }
        let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("PC settings vertices"),
            contents: bytemuck::cast_slice(vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("PC settings"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        let (x, y, w, h) = rect;
        pass.set_viewport(x, y, w, h, 0., 1.);
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buffer.slice(..));
        pass.draw(0..vertices.len() as u32, 0..1);
    }
}

pub fn viewport(size: (u32, u32)) -> (f32, f32, f32, f32) {
    let scale = (size.0 as f32 / WIDTH).min(size.1 as f32 / HEIGHT);
    let (w, h) = (WIDTH * scale, HEIGHT * scale);
    ((size.0 as f32 - w) / 2., (size.1 as f32 - h) / 2., w, h)
}
pub fn point(size: (u32, u32), point: (f64, f64)) -> Option<(f32, f32)> {
    let (x, y, w, h) = viewport(size);
    if w <= 0. || h <= 0. {
        return None;
    }
    let (px, py) = (
        (point.0 as f32 - x) * WIDTH / w,
        (point.1 as f32 - y) * HEIGHT / h,
    );
    ((0. ..WIDTH).contains(&px) && (0. ..HEIGHT).contains(&py)).then_some((px, py))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires a graphics adapter; also writes UI previews under target"]
    fn settings_pages_render_on_gpu() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let renderer = Renderer::new(&device, format);
        let output = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("PC settings preview"),
            size: wgpu::Extent3d {
                width: 1024,
                height: 768,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut menu = Menu::default();
        menu.show();
        for (page, name) in [
            (Page::Main, "main"),
            (Page::Keyboard, "keyboard"),
            (Page::Controller, "controller"),
        ] {
            menu.page = page;
            let view = output.create_view(&Default::default());
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
            }
            renderer.encode(
                &device,
                &mut encoder,
                &view,
                (1024, 768),
                &menu.paint(&Settings::default(), 1, false),
            );
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: 1024 * 768 * 4,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                output.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(1024 * 4),
                        rows_per_image: Some(768),
                    },
                },
                output.size(),
            );
            queue.submit([encoder.finish()]);
            let (send, recv) = std::sync::mpsc::channel();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    send.send(result).unwrap()
                });
            device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            recv.recv().unwrap().unwrap();
            let bytes = buffer.slice(..).get_mapped_range().unwrap();
            let at = |x: usize, y: usize| &bytes[(y * 1024 + x) * 4..(y * 1024 + x) * 4 + 4];
            assert!(at(60, 110)[0] > 220, "menu background is visible");
            assert!(at(80, 130)[2] > at(80, 130)[0], "selection is blue");
            std::fs::write(
                format!("target/pc-settings-{name}.png"),
                crate::nds::gpu2d::png(1024, 768, &bytes),
            )
            .unwrap();
        }
    }
    #[test]
    fn menu_edits_wraps_and_requires_restore_confirmation() {
        let mut m = Menu::default();
        let mut s = Settings::default();
        m.show();
        assert_eq!(m.buttons(input::LEFT, &mut s), Effect::Changed);
        assert_eq!(s.scale, None);
        m.selected = 13;
        m.activate(1, &mut s);
        assert_eq!(s.scale, None);
        m.activate(1, &mut s);
        assert_eq!(s, Settings::default());
        m.selected = 11;
        m.activate(1, &mut s);
        assert_eq!(m.page, Page::Controller);
        m.selected = 14;
        m.activate(1, &mut s);
        m.bind("Select".into(), &mut s);
        assert_eq!(s.bindings.controller[14], "Select");
        assert!(s.bindings.controller[2].is_empty());
        assert_eq!(m.back(), Effect::None);
        assert_eq!(m.back(), Effect::Close);
    }
    #[test]
    fn render_resolution_shows_the_top_screen_size_it_draws() {
        assert_eq!(resolution(Some(4), 4, false), "4x (1024 x 768)");
        assert_eq!(resolution(Some(1), 1, true), "1x (342 x 192)");
        assert_eq!(resolution(None, 6, true), "Auto: 6x (2052 x 1152)");
        assert_eq!(resolution(None, 6, false), "Auto: 6x (1536 x 1152)");
    }
    #[test]
    fn touch_uses_the_same_letterboxed_canvas_as_rendering() {
        assert_eq!(point((1920, 1080), (960., 540.)), Some((256., 192.)));
        assert!(point((1920, 1080), (0., 540.)).is_none());
        assert!(point((0, 0), (0., 0.)).is_none());
    }
}
