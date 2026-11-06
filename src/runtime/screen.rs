//! Windows for the recompiled game: DS screens stacked or separate, scaled to
//! fit with square pixels, keyboard or controller buttons and the mouse as
//! the stylus. Esc asks before quitting. In widescreen the top screen is
//! 16:9 and the bottom one sits centered under it.
//! The game runs on its own threads and hands frames over through
//! `Screens`; this side presents them and records input. With an `Fps`
//! other than `Ds` it draws between game frames too (see `pacing`).

use super::gamepad::Gamepads;
use super::pacing::{self, Fps, FrameTime, Pick};
use crate::Result;
use crate::nds::gpu2d::{Frame, HEIGHT, WIDTH};
use crate::nds::gpu3d::{DisplayList, Matched};
use crate::nds::input::{self, Input};
use crate::nds::speed::Speed;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, KeyCode, PhysicalKey};
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

pub use crate::nds::speed::FRAME_TIME;

/// The largest resolution multiple keys 1-8 select.
pub const MAX_SCALE: u32 = 8;

/// Run until either window is closed; then the process exits. `scale` is
/// the resolution multiple frames are drawn at; keys 1-8 change it. `fps`
/// is how often it draws; F turns interpolation off and on to compare.
/// Tab turns fast-forward on and off, and + and - change its speed.
/// `split_screens` gives each screen its own window instead of stacking them.
/// `widescreen` makes the top screen 16:9; H turns it off and on.
#[allow(clippy::too_many_arguments)]
pub fn run(
    title: &str,
    screens: Arc<Screens>,
    input: Arc<Input>,
    scale: Arc<AtomicU32>,
    fps: Fps,
    speed: Arc<Speed>,
    split_screens: bool,
    widescreen: Arc<AtomicBool>,
) -> Result<()> {
    let event_loop =
        EventLoop::new().map_err(|error| format!("could not create event loop: {error}"))?;
    let mut app = App {
        title: title.to_owned(),
        screens,
        input,
        scale,
        speed,
        windows: Vec::new(),
        split_screens,
        widescreen,
        redraw_pending: false,
        gpu: None,
        pressed: 0,
        gamepads: Gamepads::new(),
        pad_pressed: 0,
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
    speed: Arc<Speed>,
    windows: Vec<Arc<Window>>,
    split_screens: bool,
    /// Whether the game draws the top screen 16:9; frames say how wide
    /// they are, so this only decides new frames and window sizes.
    widescreen: Arc<AtomicBool>,
    redraw_pending: bool,
    gpu: Option<Gpu>,
    /// Buttons held on the keyboard.
    pressed: u32,
    gamepads: Option<Gamepads>,
    /// Buttons held on controllers.
    pad_pressed: u32,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Stacked,
    Top,
    Bottom,
}

impl View {
    fn height(self) -> usize {
        if self == Self::Stacked {
            2 * HEIGHT
        } else {
            HEIGHT
        }
    }

    /// The width shown, in DS pixels, when the top screen has `extra`
    /// widescreen columns on each side.
    fn width(self, extra: usize) -> usize {
        if self == Self::Bottom {
            WIDTH
        } else {
            WIDTH + 2 * extra
        }
    }

    /// The part of the stacked screens shown, in texture coordinates:
    /// x, y, width, height. Frames are `WIDTH + 2 * extra` wide, the bottom
    /// screen in the middle of its half.
    fn rect(self, extra: usize) -> [f32; 4] {
        let total = (WIDTH + 2 * extra) as f32;
        match self {
            Self::Stacked => [0.0, 0.0, 1.0, 1.0],
            Self::Top => [0.0, 0.0, 1.0, 0.5],
            Self::Bottom => [extra as f32 / total, 0.5, WIDTH as f32 / total, 0.5],
        }
    }

    fn touch(
        self,
        size: winit::dpi::PhysicalSize<u32>,
        cursor: PhysicalPosition<f64>,
        extra: usize,
    ) -> Option<(u8, u8)> {
        if self == Self::Top || size.width == 0 || size.height == 0 {
            return None;
        }
        let (x, y, scale) = placement(size.width, size.height, self, extra);
        let px = ((cursor.x as f32 - x) / scale).floor()
            - if self == Self::Stacked {
                extra as f32
            } else {
                0.0
            };
        let py = ((cursor.y as f32 - y) / scale).floor()
            - if self == Self::Stacked {
                HEIGHT as f32
            } else {
                0.0
            };
        ((0.0..WIDTH as f32).contains(&px) && (0.0..HEIGHT as f32).contains(&py))
            .then_some((px as u8, py as u8))
    }
}

/// Where the screens are drawn in a window of this size, with `extra`
/// widescreen columns on each side of the top screen: x, y, scale.
fn placement(width: u32, height: u32, view: View, extra: usize) -> (f32, f32, f32) {
    let shown = view.width(extra) as f32;
    let scale = (width as f32 / shown).min(height as f32 / view.height() as f32);
    let x = (width as f32 - shown * scale) / 2.0;
    let y = (height as f32 - view.height() as f32 * scale) / 2.0;
    (x, y, scale)
}

/// The widescreen columns on each side of the top screen.
fn extra_columns(widescreen: bool) -> usize {
    if widescreen {
        crate::nds::gpu2d::WIDE_EXTRA
    } else {
        0
    }
}

impl App {
    fn view(&self, index: usize) -> View {
        if !self.split_screens {
            View::Stacked
        } else if index == 0 {
            View::Top
        } else {
            View::Bottom
        }
    }

    fn update_buttons(&self) {
        self.input.set_buttons(self.pressed | self.pad_pressed);
    }

    fn poll_gamepads(&mut self) {
        let Some(gamepads) = &mut self.gamepads else {
            return;
        };
        let state = gamepads.poll();
        if state.toggle_fast_forward {
            self.speed.toggle();
            self.update_title();
        }
        if state.buttons != self.pad_pressed {
            self.pad_pressed = state.buttons;
            self.update_buttons();
        }
    }

    /// Ask whether to quit, over window `index`. Keys and buttons held when
    /// the dialog opened are let go, as their releases go to the dialog.
    fn confirm_quit(&mut self, index: usize) -> bool {
        self.pressed = 0;
        self.update_buttons();
        let mut dialog = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title(&self.title)
            .set_description("Quit the game? Progress since your last save will be lost.")
            .set_buttons(rfd::MessageButtons::YesNo);
        if let Some(window) = self.windows.get(index) {
            dialog = dialog.set_parent(&**window);
        }
        dialog.show() == rfd::MessageDialogResult::Yes
    }

    fn update_touch(&self) {
        // Where the screens are is decided by the frame shown.
        let extra = self.gpu.as_ref().map_or(0, |gpu| gpu.extra);
        let point = self.cursor.filter(|_| self.pen_down).and_then(|cursor| {
            let index = usize::from(self.split_screens);
            self.view(index)
                .touch(self.windows.get(index)?.inner_size(), cursor, extra)
        });
        self.input.set_touch(point);
    }

    /// H: switch the top screen between 16:9 and 4:3. Windows keep their
    /// height and change width to match, unless maximized or full screen.
    fn toggle_widescreen(&mut self) {
        let wide = !self.widescreen.load(Ordering::Relaxed);
        self.widescreen.store(wide, Ordering::Relaxed);
        for (index, window) in self.windows.iter().enumerate() {
            if window.is_maximized() || window.fullscreen().is_some() {
                continue;
            }
            let view = self.view(index);
            let size = window.inner_size();
            let width = size.height as usize * view.width(extra_columns(wide)) / view.height();
            let _ =
                window.request_inner_size(winit::dpi::PhysicalSize::new(width as u32, size.height));
        }
        self.update_title();
    }

    fn redraw(&mut self) -> Result<()> {
        let Some(gpu) = &mut self.gpu else {
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
        for (index, window) in self.windows.iter().enumerate() {
            gpu.draw(index, window.inner_size())?;
        }
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
            self.update_title();
        }
        Ok(())
    }

    fn window_title(&self) -> String {
        let mut title = match self.scale.load(Ordering::Relaxed) {
            1 => self.title.clone(),
            scale => format!("{} - {scale}x resolution", self.title),
        };
        if self.widescreen.load(Ordering::Relaxed) {
            title = format!("{title} - 16:9");
        }
        if self.speed.is_fast() {
            title = format!("{title} - fast-forward {}x", self.speed.factor());
        }
        if !self.rate.shown.is_empty() {
            title = format!("{title} - {}", self.rate.shown);
        }
        title
    }

    fn update_title(&self) {
        for (index, window) in self.windows.iter().enumerate() {
            window.set_title(&self.output_title(index));
        }
    }

    fn output_title(&self, index: usize) -> String {
        let title = self.window_title();
        match self.view(index) {
            View::Stacked => title,
            View::Top => format!("{title} - Top screen"),
            View::Bottom => format!("{title} - Bottom screen"),
        }
    }

    fn request_redraw(&mut self) {
        self.redraw_pending = true;
        for window in &self.windows {
            window.request_redraw();
        }
    }

    /// Tab turns fast-forward on and off; + and - change its speed (and
    /// turn it on, so the change can be seen). + and - are matched by what
    /// the key types, so they work on any keyboard layout (and = counts as
    /// +, being + unshifted on US keyboards), as do the numpad keys.
    fn speed_key(&self, event: &KeyEvent) -> bool {
        let typed = match &event.logical_key {
            Key::Character(text) => text.as_str(),
            _ => "",
        };
        let code = match event.physical_key {
            PhysicalKey::Code(code) => Some(code),
            PhysicalKey::Unidentified(_) => None,
        };
        let factor = self.speed.factor();
        let new_factor = if code == Some(KeyCode::Tab) {
            self.speed.toggle();
            return true;
        } else if code == Some(KeyCode::NumpadAdd) || typed == "+" || typed == "=" {
            factor + 1
        } else if code == Some(KeyCode::NumpadSubtract) || typed == "-" {
            factor.saturating_sub(1)
        } else {
            return false;
        };
        self.speed.set_factor(new_factor);
        if !self.speed.is_fast() {
            self.speed.toggle();
        }
        true
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
        if !self.windows.is_empty() {
            return;
        }
        // Big enough to show the chosen resolution, within the monitor.
        let scale = self.scale.load(Ordering::Relaxed);
        let extra = extra_columns(self.widescreen.load(Ordering::Relaxed));
        for index in 0..if self.split_screens { 2 } else { 1 } {
            let view = self.view(index);
            let height = view.height() as u32;
            let width = view.width(extra) as u32;
            let mut attributes = Window::default_attributes()
                .with_title(self.output_title(index))
                .with_inner_size(winit::dpi::LogicalSize::new(
                    f64::from(width * 2),
                    f64::from(height * 2),
                ));
            if scale > 2 {
                let mut size = winit::dpi::PhysicalSize::new(width * scale, height * scale);
                if let Some(monitor) = event_loop.primary_monitor() {
                    let limit = monitor.size().height * 9 / 10;
                    if size.height > limit {
                        size = winit::dpi::PhysicalSize::new(limit * width / height, limit);
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
            self.windows.push(window);
        }
        match pollster::block_on(Gpu::new(&self.windows, self.fps)) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
                return;
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(index) = self.windows.iter().position(|window| window.id() == id) else {
            return;
        };
        let touch_window = self.view(index) != View::Top;
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if code == KeyCode::Escape {
                        if event.state == ElementState::Pressed
                            && !event.repeat
                            && self.confirm_quit(index)
                        {
                            event_loop.exit();
                        }
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
                    if code == KeyCode::KeyH {
                        if event.state == ElementState::Pressed && !event.repeat {
                            self.toggle_widescreen();
                        }
                        return;
                    }
                    if let Some(scale) = scale_key(code)
                        && event.state == ElementState::Pressed
                    {
                        self.scale.store(scale, Ordering::Relaxed);
                        self.update_title();
                        return;
                    }
                    if event.state == ElementState::Pressed
                        && !event.repeat
                        && self.speed_key(&event)
                    {
                        self.update_title();
                        return;
                    }
                    if let Some(bit) = button(code) {
                        match event.state {
                            ElementState::Pressed => self.pressed |= bit,
                            ElementState::Released => self.pressed &= !bit,
                        }
                        self.update_buttons();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } if touch_window => {
                self.cursor = Some(position);
                self.update_touch();
            }
            WindowEvent::CursorLeft { .. } if touch_window => {
                self.cursor = None;
                self.update_touch();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } if touch_window => {
                self.pen_down = state == ElementState::Pressed;
                self.update_touch();
            }
            WindowEvent::Focused(false) => {
                self.pressed = 0;
                self.update_buttons();
                if touch_window {
                    self.pen_down = false;
                    self.cursor = None;
                    self.update_touch();
                }
            }
            WindowEvent::Resized(_) if touch_window => {
                self.update_touch();
            }
            WindowEvent::RedrawRequested => {
                // Both windows request the same presentation. Draw once so
                // they share a frame and interpolation point.
                if !self.redraw_pending {
                    return;
                }
                self.redraw_pending = false;
                if let Err(error) = self.redraw() {
                    self.error = Some(error);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_gamepads();
        if self.windows.is_empty() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_TIME / 2));
            return;
        }
        match self.fps {
            Fps::Ds => {
                self.request_redraw();
                event_loop
                    .set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME_TIME / 2));
            }
            // Vsync, or nothing, paces the presents.
            Fps::Display | Fps::Uncapped => {
                self.request_redraw();
                event_loop.set_control_flow(ControlFlow::Poll);
            }
            Fps::Cap(rate) => {
                let now = Instant::now();
                if now >= self.next_present {
                    self.next_present = pacing::next_present(self.next_present, now, rate);
                    self.request_redraw();
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

// The part of the stacked screens this window shows: offset and size.
struct Rect {
    offset: vec2<f32>,
    size: vec2<f32>,
};

@group(1) @binding(0) var<uniform> rect: Rect;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Out {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: Out;
    out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = rect.offset + uv * rect.size;
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
    targets: Vec<SurfaceTarget>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Software frames, at the DS resolution.
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    nearest: wgpu::Sampler,
    linear: wgpu::Sampler,
    /// Frames drawn at a multiple of the DS resolution.
    scaled: Option<super::scaled::Renderer>,
    /// The scaled output's scale and width, sampled nearest and linear (for
    /// windows smaller than the output), while it is what is shown.
    scaled_view: Option<((u32, u32), wgpu::BindGroup, wgpu::BindGroup)>,
    showing_scaled: bool,
    /// The widescreen columns on each side of the shown frame's top screen.
    extra: usize,
    /// For the screen tests, which draw views no window has.
    #[cfg_attr(not(test), allow(dead_code))]
    rect_layout: wgpu::BindGroupLayout,
}

struct SurfaceTarget {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view: View,
    /// What part of the screens the window shows (`View::rect`).
    rect: wgpu::Buffer,
    rect_bind_group: wgpu::BindGroup,
}

fn rect_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("DS screens rect"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("DS screens rect bind group"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    (buffer, bind_group)
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
    async fn new(windows: &[Arc<Window>], fps: Fps) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surfaces = windows
            .iter()
            .map(|window| {
                instance
                    .create_surface(window.clone())
                    .map_err(|error| format!("could not create the window surface: {error}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: surfaces.first(),
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
        let size = windows[0].inner_size();
        let config = surfaces[0]
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "graphics surface has no supported configuration".to_owned())?;
        let present_mode = if fps.vsync() {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::AutoNoVsync
        };
        let rect_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("DS screens rect layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let mut targets = Vec::new();
        for (index, (surface, window)) in surfaces.into_iter().zip(windows).enumerate() {
            let size = window.inner_size();
            let capabilities = surface.get_capabilities(&adapter);
            if !capabilities.formats.contains(&config.format) {
                return Err(
                    "graphics adapter does not support a shared format for both windows".to_owned(),
                );
            }
            let mut target_config = surface
                .get_default_config(&adapter, size.width.max(1), size.height.max(1))
                .ok_or_else(|| "graphics surface has no supported configuration".to_owned())?;
            target_config.format = config.format;
            target_config.present_mode = present_mode;
            surface.configure(&device, &target_config);
            let (rect, rect_bind_group) = rect_bind_group(&device, &rect_layout);
            targets.push(SurfaceTarget {
                surface,
                config: target_config,
                view: if windows.len() == 1 {
                    View::Stacked
                } else if index == 0 {
                    View::Top
                } else {
                    View::Bottom
                },
                rect,
                rect_bind_group,
            });
        }

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
            bind_group_layouts: &[Some(&layout), Some(&rect_layout)],
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
            targets,
            device,
            queue,
            texture,
            bind_group,
            pipeline,
            layout,
            nearest: sampler,
            linear,
            scaled: None,
            scaled_view: None,
            showing_scaled: false,
            extra: 0,
            rect_layout,
        })
    }

    /// Take the frame to show. A scaled frame is drawn by `render_scaled`.
    fn upload(&mut self, frame: &Frame) {
        self.showing_scaled = frame.scaled.is_some();
        self.extra = frame.scaled.as_ref().map_or(0, |scaled| scaled.extra);
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
        let id = (scale, output.width());
        if self
            .scaled_view
            .as_ref()
            .is_none_or(|(shown, _, _)| *shown != id)
        {
            // Sample through the same format as software frames, so
            // sRGB surfaces pass colors through unchanged.
            let view = output.create_view(&wgpu::TextureViewDescriptor {
                format: Some(self.texture.format()),
                ..Default::default()
            });
            let nearest = screens_bind_group(&self.device, &self.layout, &view, &self.nearest);
            let linear = screens_bind_group(&self.device, &self.layout, &view, &self.linear);
            self.scaled_view = Some((id, nearest, linear));
        }
        matched
    }

    fn draw(&mut self, index: usize, size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let target = &mut self.targets[index];
        self.queue.write_buffer(
            &target.rect,
            0,
            bytemuck::cast_slice(&target.view.rect(self.extra)),
        );
        if (size.width, size.height) != (target.config.width, target.config.height) {
            target.config.width = size.width;
            target.config.height = size.height;
            target.surface.configure(&self.device, &target.config);
        }
        let frame = match target.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                target.surface.configure(&self.device, &target.config);
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
            let (x, y, scale) = placement(size.width, size.height, target.view, self.extra);
            pass.set_viewport(
                x,
                y,
                target.view.width(self.extra) as f32 * scale,
                target.view.height() as f32 * scale,
                0.0,
                1.0,
            );
            pass.set_pipeline(&self.pipeline);
            let bind_group = match &self.scaled_view {
                // Shrinking a large output: average texels rather than skip them.
                Some(((shown, _), nearest, linear)) if self.showing_scaled => {
                    if (*shown as f32) > scale {
                        linear
                    } else {
                        nearest
                    }
                }
                _ => &self.bind_group,
            };
            pass.set_bind_group(0, bind_group, &[]);
            pass.set_bind_group(1, &target.rect_bind_group, &[]);
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
        assert_eq!(placement(512, 768, View::Stacked, 0), (0.0, 0.0, 2.0));
        // A wide window letterboxes horizontally.
        let (x, y, scale) = placement(1000, 384, View::Stacked, 0);
        assert_eq!((y, scale), (0.0, 1.0));
        assert_eq!(x, (1000.0 - 256.0) / 2.0);
        // Widescreen stacks a 342-wide top screen over the bottom one.
        assert_eq!(placement(684, 768, View::Stacked, 43), (0.0, 0.0, 2.0));
        assert_eq!(placement(1000, 384, View::Stacked, 43).0, 329.0);
    }

    #[test]
    fn separate_screens_fit_with_square_pixels() {
        for view in [View::Top, View::Bottom] {
            assert_eq!(placement(512, 384, view, 0), (0.0, 0.0, 2.0));
            assert_eq!(placement(1000, 384, view, 0), (244.0, 0.0, 2.0));
            assert_eq!(placement(256, 400, view, 0), (0.0, 104.0, 1.0));
        }
        // Only the top screen widens.
        assert_eq!(placement(684, 384, View::Top, 43), (0.0, 0.0, 2.0));
        assert_eq!(placement(1000, 384, View::Bottom, 43), (244.0, 0.0, 2.0));
    }

    #[test]
    fn widescreen_shows_the_bottom_screen_from_the_middle() {
        assert_eq!(View::Stacked.rect(43), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(View::Top.rect(43), [0.0, 0.0, 1.0, 0.5]);
        assert_eq!(View::Bottom.rect(0), [0.0, 0.5, 1.0, 0.5]);
        let [x, y, width, height] = View::Bottom.rect(43);
        assert_eq!(
            (x * 342.0, y, width * 342.0, height),
            (43.0, 0.5, 256.0, 0.5)
        );
    }

    #[test]
    fn stylus_only_maps_the_bottom_screen() {
        let size = winit::dpi::PhysicalSize::new(512, 768);
        let point = PhysicalPosition::new(256.0, 576.0);
        assert_eq!(View::Stacked.touch(size, point, 0), Some((128, 96)));
        assert_eq!(
            View::Stacked.touch(size, PhysicalPosition::new(256.0, 192.0), 0),
            None
        );
        assert_eq!(View::Top.touch(size, point, 0), None);
        let size = winit::dpi::PhysicalSize::new(1000, 384);
        assert_eq!(
            View::Bottom.touch(size, PhysicalPosition::new(500.0, 192.0), 0),
            Some((128, 96))
        );
        for (x, y) in [(243.0, 0.0), (756.0, 0.0), (244.0, -1.0), (244.0, 384.0)] {
            assert_eq!(
                View::Bottom.touch(size, PhysicalPosition::new(x, y), 0),
                None
            );
        }
        assert_eq!(
            View::Bottom.touch(size, PhysicalPosition::new(244.0, 0.0), 0),
            Some((0, 0))
        );
        assert_eq!(
            View::Bottom.touch(size, PhysicalPosition::new(755.0, 383.0), 0),
            Some((255, 191))
        );
        assert_eq!(
            View::Bottom.touch(winit::dpi::PhysicalSize::new(0, 0), point, 0),
            None
        );
        // Widescreen: the bottom screen is centered under the wider top.
        let size = winit::dpi::PhysicalSize::new(684, 768);
        assert_eq!(
            View::Stacked.touch(size, PhysicalPosition::new(342.0, 576.0), 43),
            Some((128, 96))
        );
        for x in [85.0, 598.0] {
            assert_eq!(
                View::Stacked.touch(size, PhysicalPosition::new(x, 576.0), 43),
                None
            );
        }
        assert_eq!(
            View::Stacked.touch(size, PhysicalPosition::new(86.0, 384.0), 43),
            Some((0, 0))
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires a Windows desktop and graphics adapter"]
    fn split_windows_present_native_and_scaled_frames() {
        use winit::platform::windows::EventLoopBuilderExtWindows;

        struct Smoke;
        impl ApplicationHandler for Smoke {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                let windows: Vec<_> = ["Top test", "Bottom test"]
                    .into_iter()
                    .map(|title| {
                        Arc::new(
                            event_loop
                                .create_window(
                                    Window::default_attributes()
                                        .with_title(title)
                                        .with_inner_size(winit::dpi::PhysicalSize::new(512, 384)),
                                )
                                .unwrap(),
                        )
                    })
                    .collect();
                let mut gpu = pollster::block_on(Gpu::new(&windows, Fps::Ds)).unwrap();
                assert_eq!(gpu.targets.len(), 2);
                assert_eq!(gpu.targets[0].view, View::Top);
                assert_eq!(gpu.targets[1].view, View::Bottom);
                let mut frame = Frame::blank();
                for pixel in frame.top.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[255, 0, 0, 255]);
                }
                for pixel in frame.bottom.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[0, 255, 0, 255]);
                }
                gpu.upload(&frame);
                verify_screen_pixels(&gpu);
                for (index, window) in windows.iter().enumerate() {
                    gpu.draw(index, window.inner_size()).unwrap();
                    gpu.draw(index, winit::dpi::PhysicalSize::new(320, 240))
                        .unwrap();
                    gpu.draw(index, winit::dpi::PhysicalSize::new(0, 0))
                        .unwrap();
                }
                use crate::nds::gpu2d::{Composed, ScaledFrame};
                let mut pixels = vec![Composed::fixed(31).pack(); WIDTH * HEIGHT];
                pixels.extend(vec![Composed::fixed(31 << 5).pack(); WIDTH * HEIGHT]);
                frame.scaled = Some(Box::new(ScaledFrame {
                    pixels,
                    extra: 0,
                    brightness: [0; 2],
                    three_d: None,
                }));
                gpu.upload(&frame);
                for scale in [1, 2, 4] {
                    gpu.render_scaled(scale, None, 0.0);
                    verify_screen_pixels(&gpu);
                    for (index, window) in windows.iter().enumerate() {
                        gpu.draw(index, window.inner_size()).unwrap();
                    }
                }
                event_loop.exit();
            }

            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
        }

        let event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
        event_loop.run_app(&mut Smoke).unwrap();
    }

    #[cfg(target_os = "windows")]
    fn verify_screen_pixels(gpu: &Gpu) {
        let format = gpu.targets[0].config.format;
        for view in [View::Stacked, View::Top, View::Bottom] {
            let height = view.height() as u32;
            let output = gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("screen selection test"),
                size: wgpu::Extent3d {
                    width: WIDTH as u32,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let output_view = output.create_view(&Default::default());
            let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("screen selection readback"),
                size: u64::from(WIDTH as u32 * height * 4),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            {
                let colors = [Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &colors,
                    ..Default::default()
                });
                pass.set_pipeline(&gpu.pipeline);
                let bind_group = if gpu.showing_scaled {
                    &gpu.scaled_view.as_ref().unwrap().1
                } else {
                    &gpu.bind_group
                };
                pass.set_bind_group(0, bind_group, &[]);
                let (rect, rect_group) = rect_bind_group(&gpu.device, &gpu.rect_layout);
                gpu.queue
                    .write_buffer(&rect, 0, bytemuck::cast_slice(&view.rect(0)));
                pass.set_bind_group(1, &rect_group, &[]);
                pass.draw(0..3, 0..1);
            }
            encoder.copy_texture_to_buffer(
                output.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(WIDTH as u32 * 4),
                        rows_per_image: Some(height),
                    },
                },
                output.size(),
            );
            gpu.queue.submit(std::iter::once(encoder.finish()));
            let (sender, receiver) = std::sync::mpsc::channel();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    sender.send(result).unwrap();
                });
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap();
            receiver.recv().unwrap().unwrap();
            let data = buffer.slice(..).get_mapped_range().unwrap();
            for (index, pixel) in data.chunks_exact(4).enumerate() {
                let bottom =
                    view == View::Bottom || (view == View::Stacked && index >= WIDTH * HEIGHT);
                let expected = if bottom {
                    [0, 255, 0, 255]
                } else {
                    [255, 0, 0, 255]
                };
                let actual = match format {
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                        [pixel[2], pixel[1], pixel[0], pixel[3]]
                    }
                    wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => {
                        [pixel[0], pixel[1], pixel[2], pixel[3]]
                    }
                    _ => panic!("unexpected test surface format: {format:?}"),
                };
                assert_eq!(actual, expected, "{view:?}, pixel {index}");
            }
        }
    }
}
