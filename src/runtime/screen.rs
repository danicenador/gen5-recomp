//! Windows for the recompiled game: DS screens stacked or separate, scaled to
//! fit with square pixels, keyboard or controller buttons and the mouse as
//! the stylus. Esc asks before quitting; F11 or Alt+Enter switches full
//! screen. In widescreen the top screen is 16:9 and the bottom one sits
//! centered under it. In single-screen mode the top screen fills the window
//! and L2 or Space slides the bottom one up over it, blurring the top one.
//! The game runs on its own threads and hands frames over through
//! `Screens`; this side presents them and records input. With an `Fps`
//! other than `Ds` it draws between game frames too (see `pacing`).

use super::gamepad::Gamepads;
use super::pacing::{self, Fps, FrameTime, Pick};
use crate::Result;
use crate::nds::gpu2d::{Frame, HEIGHT, WIDTH};
use crate::nds::gpu3d::{DisplayList, Matched};
use crate::nds::input::{self, Input};
use crate::nds::machine::LoadedOverlays;
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
/// `layout` says how the screens are shown; `auto_scale` lets single-screen
/// mode pick the resolution multiple for the monitor.
/// `widescreen` makes the top screen 16:9; H turns it off and on.
#[allow(clippy::too_many_arguments)]
pub fn run(
    title: &str,
    screens: Arc<Screens>,
    input: Arc<Input>,
    scale: Arc<AtomicU32>,
    fps: Fps,
    speed: Arc<Speed>,
    layout: Layout,
    auto_scale: bool,
    widescreen: Arc<AtomicBool>,
    overlays: Arc<LoadedOverlays>,
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
        layout,
        auto_scale,
        panel: Panel::new(Instant::now()),
        overlays,
        scene: SceneWatch::new(Instant::now()),
        x_menu: false,
        buttons: 0,
        pad_cursor: PadCursor::new(Instant::now()),
        alt: false,
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
    layout: Layout,
    auto_scale: bool,
    /// The bottom screen raised over the top one, in single-screen mode.
    panel: Panel,
    /// The overlays in RAM, which tell scenes apart.
    overlays: Arc<LoadedOverlays>,
    /// Raises and lowers the bottom screen as scenes change.
    scene: SceneWatch,
    /// Whether the X menu is open, as far as the keys tell (`x_menu_after`).
    x_menu: bool,
    /// The DS buttons held, keyboard and controllers together.
    buttons: u32,
    /// The stylus the right stick moves on the raised bottom screen.
    pad_cursor: PadCursor,
    /// Alt held, for Alt+Enter.
    alt: bool,
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

/// How the DS screens are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// One window, the top screen over the bottom one.
    Stacked,
    /// Each screen in its own window.
    Split,
    /// One window, normally full screen, that the top screen fills. L2 or
    /// Space slides the bottom screen up over it and back down; while it is
    /// up the top screen is blurred behind it.
    Single,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Stacked,
    Top,
    Bottom,
    /// The top screen, with the bottom one raised over it on demand.
    Single,
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
            Self::Top | Self::Single => [0.0, 0.0, 1.0, 0.5],
            Self::Bottom => [extra as f32 / total, 0.5, WIDTH as f32 / total, 0.5],
        }
    }

    /// The DS touch-screen point under `cursor`. Single-screen windows
    /// touch through the raised bottom screen instead (`Panel`).
    fn touch(
        self,
        size: winit::dpi::PhysicalSize<u32>,
        cursor: PhysicalPosition<f64>,
        extra: usize,
    ) -> Option<(u8, u8)> {
        if matches!(self, Self::Top | Self::Single) || size.width == 0 || size.height == 0 {
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

/// How long the bottom screen takes to rise all the way, or to drop.
const PANEL_TIME: Duration = Duration::from_millis(180);

/// The raised bottom screen's height, as a share of the top screen's: 720
/// lines over a 1080-line top screen.
const PANEL_HEIGHT: f32 = 2.0 / 3.0;

/// The bottom screen in single-screen mode: down out of sight, or raised
/// over the top screen, and how far along it is in between.
#[derive(Clone, Copy, Debug)]
struct Panel {
    /// Where it is heading: up or down.
    up: bool,
    /// How far up it is, from 0 (out of sight) to 1 (raised), in time:
    /// it moves by `PANEL_TIME` per unit.
    travel: f32,
    /// When `travel` was last brought up to date.
    at: Instant,
}

impl Panel {
    fn new(now: Instant) -> Self {
        Self {
            up: false,
            travel: 0.0,
            at: now,
        }
    }

    /// Move it on to `now`.
    fn advance(&mut self, now: Instant) {
        let step = now.saturating_duration_since(self.at).as_secs_f32() / PANEL_TIME.as_secs_f32();
        self.at = now;
        self.travel = if self.up {
            (self.travel + step).min(1.0)
        } else {
            (self.travel - step).max(0.0)
        };
    }

    /// Send it the other way, from wherever it is.
    fn toggle(&mut self, now: Instant) {
        self.advance(now);
        self.up = !self.up;
    }

    /// How much of it shows, from 0 to 1. It eases out as it rises and in as
    /// it drops, and turning back midway carries on from where it is.
    fn raised(&self) -> f32 {
        1.0 - (1.0 - self.travel).powi(3)
    }

    /// The stylus only reaches it once it has fully risen.
    fn touchable(&self) -> bool {
        self.up && self.travel >= 1.0
    }
}

/// The overworld's overlay: in RAM whenever the player walks around (but
/// not for the moment a map transition takes).
const FIELD_OVERLAY: u16 = 36;

/// Overlays in RAM throughout a battle, from the moment the screen goes black
/// after the trainer's challenge until it goes black again at the end.
const BATTLE_OVERLAYS: &[u16] = &[167, 168, 169, 170];

/// Overlays of menus played on the bottom screen: the bag (142), the party
/// (165), a Pokémon's summary (207, over the party) and the starter choice
/// (316). The naming keyboard's 280 is left out: it stays in RAM through the
/// whole opening, whose speeches are on the top screen.
const MENU_OVERLAYS: &[u16] = &[142, 165, 207, 316];

/// What the game is doing, going by the overlays in RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scene {
    /// Walking around, or talking, on the overworld. The X menu opens over
    /// it without overlays of its own.
    Field,
    Battle,
    Menu,
    /// The title screen, the opening, a map transition: no telling.
    Unknown,
}

fn scene(overlays: &[u16]) -> Scene {
    let any = |ids: &[u16]| overlays.iter().any(|id| ids.contains(id));
    if any(BATTLE_OVERLAYS) {
        Scene::Battle
    } else if any(MENU_OVERLAYS) {
        Scene::Menu
    } else if overlays.contains(&FIELD_OVERLAY) {
        Scene::Field
    } else {
        Scene::Unknown
    }
}

/// Whether the bottom screen should be up in `scene`: in battles and menus,
/// and on the field while the X menu is open.
fn scene_wants_panel(scene: Scene, x_menu: bool) -> Option<bool> {
    match scene {
        Scene::Battle | Scene::Menu => Some(true),
        Scene::Field => Some(x_menu),
        Scene::Unknown => None,
    }
}

/// Whether the X menu is open after the DS buttons in `pressed` were just
/// pressed in `scene`. The overlays cannot tell, so this follows the keys:
/// on the field X opens the menu and closes it, and B closes it. Menus
/// opened from it come back to it, and a battle closes it.
fn x_menu_after(open: bool, scene: Scene, pressed: u32) -> bool {
    match scene {
        Scene::Field if pressed & input::X != 0 => !open,
        Scene::Field if pressed & input::B != 0 => false,
        Scene::Battle => false,
        _ => open,
    }
}

/// How long a scene must hold before the bottom screen follows it.
const SCENE_SETTLE: Duration = Duration::from_millis(100);

/// Raises and lowers the bottom screen when the scene changes. It acts only
/// on changes, so L2 can still move the screen in any scene until the next
/// change; scenes it cannot tell apart leave the screen alone.
#[derive(Clone, Copy, Debug)]
struct SceneWatch {
    /// What it last told the bottom screen to do.
    decided: Option<bool>,
    /// What the scene wants now, and since when.
    wanted: Option<bool>,
    since: Instant,
}

impl SceneWatch {
    fn new(now: Instant) -> Self {
        Self {
            decided: None,
            wanted: None,
            since: now,
        }
    }

    /// Given what the scene wants at `now`, whether the bottom screen should
    /// now go up (`Some(true)`) or down (`Some(false)`).
    fn update(&mut self, now: Instant, wanted: Option<bool>) -> Option<bool> {
        if wanted != self.wanted {
            self.wanted = wanted;
            self.since = now;
            return None;
        }
        let wanted = wanted?;
        if self.decided == Some(wanted) || now.duration_since(self.since) < SCENE_SETTLE {
            return None;
        }
        self.decided = Some(wanted);
        Some(wanted)
    }
}

/// How fast the stick moves the stylus at full lean, in DS pixels a second.
const CURSOR_SPEED: f32 = 320.0;
/// How far the stick must lean before the stylus moves.
const CURSOR_DEAD_ZONE: f32 = 0.2;
/// How long the stylus stays in sight after the stick last moved it.
const CURSOR_LINGER: Duration = Duration::from_secs(3);

/// The stylus the right stick moves over the raised bottom screen and
/// touches with when pressed in.
#[derive(Clone, Copy, Debug)]
struct PadCursor {
    /// Where it points, in DS pixels.
    x: f32,
    y: f32,
    touching: bool,
    /// Until when it is in sight.
    shown_until: Option<Instant>,
    /// When it was last moved on.
    at: Instant,
}

impl PadCursor {
    fn new(now: Instant) -> Self {
        Self {
            x: WIDTH as f32 / 2.0,
            y: HEIGHT as f32 / 2.0,
            touching: false,
            shown_until: None,
            at: now,
        }
    }

    /// Move it on to `now` with the stick (x right, y up) and stick button
    /// as they are. It only moves and touches on a raised bottom screen
    /// (`active`).
    fn update(&mut self, now: Instant, stick: (f32, f32), pressed: bool, active: bool) {
        let elapsed = now
            .saturating_duration_since(self.at)
            .as_secs_f32()
            .min(0.1);
        self.at = now;
        if !active {
            self.touching = false;
            self.shown_until = None;
            return;
        }
        let lean = stick.0.hypot(stick.1);
        if lean > CURSOR_DEAD_ZONE {
            // Gentle near the middle for aiming, fast at the edge.
            let speed = CURSOR_SPEED
                * ((lean - CURSOR_DEAD_ZONE) / (1.0 - CURSOR_DEAD_ZONE))
                    .min(1.0)
                    .powi(2);
            let step = speed * elapsed / lean;
            self.x = (self.x + stick.0 * step).clamp(0.0, WIDTH as f32 - 0.01);
            self.y = (self.y - stick.1 * step).clamp(0.0, HEIGHT as f32 - 0.01);
        }
        self.touching = pressed;
        if lean > CURSOR_DEAD_ZONE || pressed {
            self.shown_until = Some(now + CURSOR_LINGER);
        }
    }

    fn shown(&self, now: Instant) -> bool {
        self.shown_until.is_some_and(|until| now < until)
    }

    /// The DS touch-screen point it is on.
    fn point(&self) -> (u8, u8) {
        (self.x as u8, self.y as u8)
    }
}

/// Where the bottom screen is drawn in a single-screen window of this size,
/// `raised` (0 to 1) of the way up: x, y, width, height. Its lower edge
/// starts at the top screen's and it rises its own height; whatever lies
/// below the top screen is not shown.
fn panel_rect(width: u32, height: u32, extra: usize, raised: f32) -> [f32; 4] {
    let (x, y, scale) = placement(width, height, View::Single, extra);
    let top_width = View::Single.width(extra) as f32 * scale;
    let top_height = HEIGHT as f32 * scale;
    let panel_height = (top_height * PANEL_HEIGHT).round();
    let panel_width = (panel_height * WIDTH as f32 / HEIGHT as f32).round();
    [
        (x + (top_width - panel_width) / 2.0).round(),
        (y + top_height - raised * panel_height).round(),
        panel_width,
        panel_height,
    ]
}

/// The DS touch-screen point under `cursor` on a raised bottom screen.
fn panel_touch(
    size: winit::dpi::PhysicalSize<u32>,
    cursor: PhysicalPosition<f64>,
    extra: usize,
) -> Option<(u8, u8)> {
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let [x, y, width, height] = panel_rect(size.width, size.height, extra, 1.0);
    let px = ((cursor.x as f32 - x) / width * WIDTH as f32).floor();
    let py = ((cursor.y as f32 - y) / height * HEIGHT as f32).floor();
    ((0.0..WIDTH as f32).contains(&px) && (0.0..HEIGHT as f32).contains(&py))
        .then_some((px as u8, py as u8))
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
        match self.layout {
            Layout::Stacked => View::Stacked,
            Layout::Single => View::Single,
            Layout::Split if index == 0 => View::Top,
            Layout::Split => View::Bottom,
        }
    }

    /// L2 or Space: slide the bottom screen up, or back down.
    fn toggle_panel(&mut self) {
        if self.layout != Layout::Single {
            return;
        }
        self.panel.toggle(Instant::now());
        // A dropping screen lets go of the stylus at once.
        self.update_touch();
    }

    /// F11 or Alt+Enter: switch window `index` between full screen and a
    /// window.
    fn toggle_fullscreen(&self, index: usize) {
        if let Some(window) = self.windows.get(index) {
            let fullscreen = window
                .fullscreen()
                .is_none()
                .then_some(winit::window::Fullscreen::Borderless(None));
            window.set_fullscreen(fullscreen);
        }
    }

    fn update_buttons(&mut self) {
        let buttons = self.pressed | self.pad_pressed;
        let pressed = buttons & !self.buttons;
        self.buttons = buttons;
        if pressed != 0 && self.layout == Layout::Single {
            self.x_menu = x_menu_after(self.x_menu, scene(&self.overlays.get()), pressed);
        }
        self.input.set_buttons(buttons);
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
        if state.toggle_panel {
            self.toggle_panel();
        }
        if self.layout == Layout::Single {
            let touching = self.pad_cursor.touching;
            let point = self.pad_cursor.point();
            self.pad_cursor.update(
                Instant::now(),
                state.right_stick,
                state.touch,
                self.panel.touchable(),
            );
            if self.pad_cursor.touching != touching
                || self.pad_cursor.touching && self.pad_cursor.point() != point
            {
                self.update_touch();
            }
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
            let index = usize::from(self.layout == Layout::Split);
            let size = self.windows.get(index)?.inner_size();
            match self.view(index) {
                View::Single => self
                    .panel
                    .touchable()
                    .then(|| panel_touch(size, cursor, extra))
                    .flatten(),
                view => view.touch(size, cursor, extra),
            }
        });
        // The mouse first, then the stick's stylus.
        let point = point.or_else(|| {
            (self.pad_cursor.touching && self.panel.touchable()).then(|| self.pad_cursor.point())
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
        if self.gpu.is_none() {
            return Ok(());
        }
        let now = Instant::now();
        let touchable = self.panel.touchable();
        let scene = scene(&self.overlays.get());
        if scene == Scene::Battle {
            self.x_menu = false;
        }
        if self.layout == Layout::Single
            && let Some(up) = self
                .scene
                .update(now, scene_wants_panel(scene, self.x_menu))
            && up != self.panel.up
        {
            self.panel.toggle(now);
        }
        self.panel.advance(now);
        if self.panel.touchable() != touchable {
            self.update_touch();
        }
        let Some(gpu) = &mut self.gpu else {
            return Ok(());
        };
        gpu.panel = self.panel.raised();
        gpu.cursor = (self.panel.touchable() && self.pad_cursor.shown(now)).then_some((
            (self.pad_cursor.x, self.pad_cursor.y),
            self.pad_cursor.touching,
        ));
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
            View::Stacked | View::Single => title,
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
        // Single-screen mode draws at least as many lines as the monitor
        // has (6x for 1080 lines), then shrinks to fit.
        if self.layout == Layout::Single
            && self.auto_scale
            && let Some(monitor) = event_loop.primary_monitor()
        {
            let scale = monitor.size().height.div_ceil(HEIGHT as u32);
            self.scale
                .store(scale.clamp(1, MAX_SCALE), Ordering::Relaxed);
        }
        // Big enough to show the chosen resolution, within the monitor.
        let scale = self.scale.load(Ordering::Relaxed);
        let extra = extra_columns(self.widescreen.load(Ordering::Relaxed));
        for index in 0..if self.layout == Layout::Split { 2 } else { 1 } {
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
            if view == View::Single {
                attributes =
                    attributes.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
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
        let views: Vec<View> = (0..self.windows.len())
            .map(|index| self.view(index))
            .collect();
        match pollster::block_on(Gpu::new(&self.windows, &views, self.fps)) {
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
                    if code == KeyCode::F11 || (code == KeyCode::Enter && self.alt) {
                        if event.state == ElementState::Pressed && !event.repeat {
                            self.toggle_fullscreen(index);
                        }
                        return;
                    }
                    if code == KeyCode::Space && self.layout == Layout::Single {
                        if event.state == ElementState::Pressed && !event.repeat {
                            self.toggle_panel();
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
            WindowEvent::ModifiersChanged(modifiers) => {
                self.alt = modifiers.state().alt_key();
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
                self.alt = false;
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
    rect_layout: wgpu::BindGroupLayout,
    /// The windows' colour format, which drawing in between uses too.
    format: wgpu::TextureFormat,
    /// Bilinear both ways, for the blur.
    smooth: wgpu::Sampler,
    /// Draws like `pipeline`, mixed over what is there by the blend
    /// constant: the blurred top screen over the sharp one.
    blend_pipeline: wgpu::RenderPipeline,
    /// How far the bottom screen is raised in single-screen windows, 0 to 1.
    panel: f32,
    /// The stick's stylus on the raised bottom screen, in DS pixels, and
    /// whether it touches.
    cursor: Option<((f32, f32), bool)>,
    /// Draw the stylus, up and touching.
    cursor_pipelines: [wgpu::RenderPipeline; 2],
    /// The single-screen window's blur, for its current size.
    blur: Option<Blur>,
}

struct SurfaceTarget {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view: View,
    /// What part of the screens the window shows (`View::rect`).
    rect: wgpu::Buffer,
    rect_bind_group: wgpu::BindGroup,
    /// The bottom screen's part, raised over the top one in single-screen
    /// windows.
    panel_rect: wgpu::Buffer,
    panel_bind_group: wgpu::BindGroup,
}

/// The stick's stylus: a light ring with a dark edge, filled while it
/// touches, drawn over a square viewport.
const CURSOR_SHADER: &str = r"
struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) at: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Out {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: Out;
    out.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    // -1 to 1 across the viewport.
    out.at = uv * 2.0 - 1.0;
    return out;
}

fn cursor(at: vec2<f32>, inner: f32) -> vec4<f32> {
    let r = length(at);
    let edge = fwidth(r);
    let alpha = 0.9 * (1.0 - smoothstep(1.0 - edge, 1.0, r)) * smoothstep(inner - edge, inner, r);
    // Dark at the rim and round the hole, light between.
    let rim = smoothstep(0.75 - edge, 0.75, r);
    let hole = select(0.0, 1.0 - smoothstep(inner + 0.2 - edge, inner + 0.2, r), inner > 0.0);
    let shade = mix(vec3<f32>(0.08), vec3<f32>(1.0), 1.0 - max(rim, hole));
    return vec4<f32>(shade * alpha, alpha);
}

@fragment
fn fs_up(in: Out) -> @location(0) vec4<f32> {
    return cursor(in.at, 0.45);
}

@fragment
fn fs_touching(in: Out) -> @location(0) vec4<f32> {
    return cursor(in.at, 0.0);
}
";

/// The stick's stylus's radius, in DS pixels.
const CURSOR_RADIUS: f32 = 5.0;

/// Lines the top screen is blurred at, at most; the blur then halves this
/// `BLUR_LEVELS` times and doubles back up, so it looks the same at any
/// window size from 540 lines up.
const BLUR_HEIGHT: u32 = 540;
const BLUR_LEVELS: usize = 2;

const BLUR_SHADER: &str = r"
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

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
// How far apart the taps are, in texture coordinates (x and y).
@group(0) @binding(2) var<uniform> spread: vec4<f32>;

fn tap(uv: vec2<f32>) -> vec4<f32> {
    return textureSample(source, source_sampler, uv);
}

// Halve: the middle and four diagonal taps, each bilinear.
@fragment
fn fs_down(in: Out) -> @location(0) vec4<f32> {
    let d = spread.xy;
    let sum = tap(in.uv) * 4.0 + tap(in.uv - d) + tap(in.uv + d)
        + tap(in.uv + vec2<f32>(d.x, -d.y)) + tap(in.uv + vec2<f32>(-d.x, d.y));
    return sum / 8.0;
}

// Double: a ring of eight taps, the diagonal ones weighed twice.
@fragment
fn fs_up(in: Out) -> @location(0) vec4<f32> {
    let d = spread.xy;
    var sum = tap(in.uv + vec2<f32>(-2.0 * d.x, 0.0)) + tap(in.uv + vec2<f32>(2.0 * d.x, 0.0))
        + tap(in.uv + vec2<f32>(0.0, -2.0 * d.y)) + tap(in.uv + vec2<f32>(0.0, 2.0 * d.y));
    sum += 2.0 * (tap(in.uv + d) + tap(in.uv - d)
        + tap(in.uv + vec2<f32>(d.x, -d.y)) + tap(in.uv + vec2<f32>(-d.x, d.y)));
    return sum / 12.0;
}
";

/// The top screen blurred behind the raised bottom screen: drawn into the
/// first of a chain of textures, each half the size of the one before,
/// halved down the chain and doubled back up it (a dual Kawase blur), so
/// the first ends up blurred.
struct Blur {
    /// The first texture's size.
    size: (u32, u32),
    levels: Vec<wgpu::TextureView>,
    down: wgpu::RenderPipeline,
    up: wgpu::RenderPipeline,
    /// Reading level `i`, to draw level `i + 1`.
    down_groups: Vec<wgpu::BindGroup>,
    /// Reading level `i + 1`, to draw level `i`.
    up_groups: Vec<wgpu::BindGroup>,
    /// The first level, for drawing over the window.
    shown: wgpu::BindGroup,
    /// All of the first level (`View::rect` for it).
    whole: wgpu::BindGroup,
}

impl Blur {
    fn new(gpu: &Gpu, size: (u32, u32)) -> Self {
        let device = &gpu.device;
        let mut levels = Vec::new();
        let mut sizes = Vec::new();
        for level in 0..=BLUR_LEVELS {
            let (width, height) = ((size.0 >> level).max(1), (size.1 >> level).max(1));
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("top screen blur"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: gpu.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            levels.push(texture.create_view(&wgpu::TextureViewDescriptor::default()));
            sizes.push((width as f32, height as f32));
        }
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("top screen blur layout"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let group = |source: usize, spread: f32| {
            let (width, height) = sizes[source];
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("top screen blur spread"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.queue.write_buffer(
                &buffer,
                0,
                bytemuck::cast_slice(&[spread / width, spread / height, 0.0, 0.0]),
            );
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("top screen blur bind group"),
                layout: &layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&levels[source]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&gpu.smooth),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: buffer.as_entire_binding(),
                    },
                ],
            })
        };
        // Down a texel from the middle of each 2x2 block; back up half a
        // texel of the smaller level.
        let down_groups = (0..BLUR_LEVELS).map(|level| group(level, 1.0)).collect();
        let up_groups = (0..BLUR_LEVELS)
            .map(|level| group(level + 1, 0.5))
            .collect();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("top screen blur shader"),
            source: wgpu::ShaderSource::Wgsl(BLUR_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("top screen blur pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("top screen blur pipeline"),
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
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: gpu.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let shown = screens_bind_group(device, &gpu.layout, &levels[0], &gpu.smooth);
        let (whole_rect, whole) = rect_bind_group(device, &gpu.rect_layout);
        gpu.queue.write_buffer(
            &whole_rect,
            0,
            bytemuck::cast_slice(&[0.0f32, 0.0, 1.0, 1.0]),
        );
        Self {
            size,
            down: pipeline("fs_down"),
            up: pipeline("fs_up"),
            levels,
            down_groups,
            up_groups,
            shown,
            whole,
        }
    }

    /// The first level's size for a top screen drawn `width` by `height`.
    fn size_for(width: f32, height: f32) -> (u32, u32) {
        let lines = (height.round() as u32).clamp(1, BLUR_HEIGHT);
        let columns = ((width * lines as f32 / height).round() as u32).max(1);
        (columns, lines)
    }

    /// Blur what has been drawn into the first level.
    fn run(&self, encoder: &mut wgpu::CommandEncoder) {
        let pass = |encoder: &mut wgpu::CommandEncoder,
                    pipeline: &wgpu::RenderPipeline,
                    group: &wgpu::BindGroup,
                    target: &wgpu::TextureView| {
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blur top screen"),
                color_attachments: &colors,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, group, &[]);
            pass.draw(0..3, 0..1);
        };
        for level in 0..BLUR_LEVELS {
            pass(
                encoder,
                &self.down,
                &self.down_groups[level],
                &self.levels[level + 1],
            );
        }
        for level in (0..BLUR_LEVELS).rev() {
            pass(
                encoder,
                &self.up,
                &self.up_groups[level],
                &self.levels[level],
            );
        }
    }
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
    /// Draw into `windows`, each showing its `views` entry.
    async fn new(windows: &[Arc<Window>], views: &[View], fps: Fps) -> Result<Self> {
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
            let (panel_rect, panel_bind_group) = rect_bind_group(&device, &rect_layout);
            let (rect, rect_bind_group) = rect_bind_group(&device, &rect_layout);
            targets.push(SurfaceTarget {
                surface,
                config: target_config,
                view: views[index],
                rect,
                rect_bind_group,
                panel_rect,
                panel_bind_group,
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
        let smooth = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("DS screens blur sampler"),
            mag_filter: wgpu::FilterMode::Linear,
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
        let pipeline = |blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let blend_pipeline = pipeline(Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Constant,
                dst_factor: wgpu::BlendFactor::OneMinusConstant,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::REPLACE,
        }));
        let pipeline = pipeline(None);
        let cursor_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("stylus shader"),
            source: wgpu::ShaderSource::Wgsl(CURSOR_SHADER.into()),
        });
        let cursor_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("stylus pipeline layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let cursor_pipelines = ["fs_up", "fs_touching"].map(|entry| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("stylus pipeline"),
                layout: Some(&cursor_layout),
                vertex: wgpu::VertexState {
                    module: &cursor_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &cursor_shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: config.format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
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
            format: config.format,
            smooth,
            blend_pipeline,
            panel: 0.0,
            cursor: None,
            cursor_pipelines,
            blur: None,
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

    /// The screens as last drawn, sampled to suit showing them at `shown`
    /// window pixels per DS pixel.
    fn screens_group(&self, shown: f32) -> &wgpu::BindGroup {
        match &self.scaled_view {
            // Shrinking a large output: average texels rather than skip them.
            Some(((scale, _), nearest, linear)) if self.showing_scaled => {
                if (*scale as f32) > shown {
                    linear
                } else {
                    nearest
                }
            }
            _ => &self.bind_group,
        }
    }

    fn draw(&mut self, index: usize, size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        self.prepare(index, size);
        let target = &mut self.targets[index];
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
        self.encode(index, &view, size, &mut encoder);
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }

    /// Bring window `index`'s rects up to date, and the blur when a
    /// single-screen window shows it, for drawing at `size`.
    fn prepare(&mut self, index: usize, size: winit::dpi::PhysicalSize<u32>) {
        let target = &self.targets[index];
        let view = target.view;
        self.queue.write_buffer(
            &target.rect,
            0,
            bytemuck::cast_slice(&view.rect(self.extra)),
        );
        if view != View::Single {
            return;
        }
        self.queue.write_buffer(
            &target.panel_rect,
            0,
            bytemuck::cast_slice(&View::Bottom.rect(self.extra)),
        );
        if self.panel > 0.0 {
            let (_, _, scale) = placement(size.width, size.height, view, self.extra);
            let wanted = Blur::size_for(
                view.width(self.extra) as f32 * scale,
                view.height() as f32 * scale,
            );
            if self.blur.as_ref().is_none_or(|blur| blur.size != wanted) {
                self.blur = Some(Blur::new(self, wanted));
            }
        }
    }

    /// Record drawing window `index`, `size` big, into `output`.
    fn encode(
        &self,
        index: usize,
        output: &wgpu::TextureView,
        size: winit::dpi::PhysicalSize<u32>,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        let target = &self.targets[index];
        let (x, y, scale) = placement(size.width, size.height, target.view, self.extra);
        let width = target.view.width(self.extra) as f32 * scale;
        let height = target.view.height() as f32 * scale;
        let blur = self
            .blur
            .as_ref()
            .filter(|_| target.view == View::Single && self.panel > 0.0);
        if let Some(blur) = blur {
            // The top screen, small, into the blur's first level.
            let colors = [Some(wgpu::RenderPassColorAttachment {
                view: &blur.levels[0],
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shrink top screen for blur"),
                color_attachments: &colors,
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(
                0,
                self.screens_group(blur.size.1 as f32 / HEIGHT as f32),
                &[],
            );
            pass.set_bind_group(1, &target.rect_bind_group, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
            blur.run(encoder);
        }
        let colors = [Some(wgpu::RenderPassColorAttachment {
            view: output,
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
        pass.set_viewport(x, y, width, height, 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, self.screens_group(scale), &[]);
        pass.set_bind_group(1, &target.rect_bind_group, &[]);
        pass.draw(0..3, 0..1);
        let Some(blur) = blur else {
            return;
        };
        // The blurred top screen over the sharp one, as much as the bottom
        // screen has risen.
        let amount = f64::from(self.panel);
        pass.set_pipeline(&self.blend_pipeline);
        pass.set_blend_constant(wgpu::Color {
            r: amount,
            g: amount,
            b: amount,
            a: amount,
        });
        pass.set_bind_group(0, &blur.shown, &[]);
        pass.set_bind_group(1, &blur.whole, &[]);
        pass.draw(0..3, 0..1);
        // The bottom screen, cut off at the top screen's lower edge.
        let [panel_x, panel_y, panel_width, panel_height] =
            panel_rect(size.width, size.height, self.extra, self.panel);
        let left = (panel_x.max(0.0) as u32).min(size.width);
        let right = ((panel_x + panel_width).max(0.0) as u32).min(size.width);
        let top = (panel_y.max(0.0) as u32).min(size.height);
        let bottom = ((y + height).round().max(0.0) as u32).min(size.height);
        if right <= left || bottom <= top {
            return;
        }
        pass.set_scissor_rect(left, top, right - left, bottom - top);
        pass.set_viewport(panel_x, panel_y, panel_width, panel_height, 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, self.screens_group(panel_height / HEIGHT as f32), &[]);
        pass.set_bind_group(1, &target.panel_bind_group, &[]);
        pass.draw(0..3, 0..1);
        // The stick's stylus, its middle on the point it touches.
        if let Some(((cursor_x, cursor_y), touching)) = self.cursor {
            let scale = panel_height / HEIGHT as f32;
            let radius = (CURSOR_RADIUS * scale).round();
            let x = panel_x + (cursor_x.floor() + 0.5) * scale;
            let y = panel_y + (cursor_y.floor() + 0.5) * scale;
            pass.set_viewport(x - radius, y - radius, 2.0 * radius, 2.0 * radius, 0.0, 1.0);
            pass.set_pipeline(&self.cursor_pipelines[usize::from(touching)]);
            pass.draw(0..3, 0..1);
        }
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
        // Single-screen windows touch only through the raised bottom screen.
        assert_eq!(View::Single.touch(size, point, 43), None);
    }

    #[test]
    fn bottom_screen_rises_eases_and_turns_back() {
        let start = Instant::now();
        let mut panel = Panel::new(start);
        panel.advance(start + PANEL_TIME);
        assert_eq!(panel.raised(), 0.0);
        panel.toggle(start + PANEL_TIME);
        // Halfway through in time, it has risen most of the way.
        panel.advance(start + PANEL_TIME + PANEL_TIME / 2);
        assert!((panel.raised() - 0.875).abs() < 1e-4, "{}", panel.raised());
        assert!(!panel.touchable());
        // Turning back carries on from where it is, without a jump.
        let before = panel.raised();
        panel.toggle(start + PANEL_TIME + PANEL_TIME / 2);
        assert_eq!(panel.raised(), before);
        panel.advance(start + 3 * PANEL_TIME);
        assert_eq!(panel.raised(), 0.0);
        // All the way up, and only then, the stylus reaches it.
        panel.toggle(start + 3 * PANEL_TIME);
        panel.advance(start + 5 * PANEL_TIME);
        assert_eq!(panel.raised(), 1.0);
        assert!(panel.touchable());
        // Dropping lets go at once.
        panel.toggle(start + 5 * PANEL_TIME);
        assert!(!panel.touchable());
    }

    #[test]
    fn scenes_raise_and_lower_the_bottom_screen_once_settled() {
        let start = Instant::now();
        let mut watch = SceneWatch::new(start);
        let field = Some(false);
        // Nothing until a scene has held for a moment.
        assert_eq!(watch.update(start, field), None);
        assert_eq!(watch.update(start + SCENE_SETTLE / 2, field), None);
        assert_eq!(watch.update(start + SCENE_SETTLE, field), Some(false));
        // Then only on a change.
        assert_eq!(watch.update(start + 2 * SCENE_SETTLE, field), None);
        // A map transition (no telling) is too short to count, and coming
        // back to the field changes nothing.
        let t = start + 3 * SCENE_SETTLE;
        assert_eq!(watch.update(t, None), None);
        assert_eq!(watch.update(t + SCENE_SETTLE * 3, None), None);
        assert_eq!(watch.update(t + SCENE_SETTLE * 4, field), None);
        assert_eq!(watch.update(t + SCENE_SETTLE * 6, field), None);
        // A battle raises it, and the field after it lowers it.
        let t = t + SCENE_SETTLE * 10;
        assert_eq!(watch.update(t, Some(true)), None);
        assert_eq!(watch.update(t + SCENE_SETTLE, Some(true)), Some(true));
        assert_eq!(watch.update(t + SCENE_SETTLE * 2, field), None);
        assert_eq!(watch.update(t + SCENE_SETTLE * 3, field), Some(false));
    }

    #[test]
    fn overlays_tell_the_field_from_scenes_on_the_bottom_screen() {
        // Sets seen replaying White 2 to the first battle.
        let field = [12, 27, 33, 35, 36, 66, 74, 80, 112, 279, 281];
        assert_eq!(scene(&field), Scene::Field);
        // The trainer's challenge still runs on the field.
        let challenge = [12, 27, 33, 35, 36, 66, 74, 80, 112, 146, 147, 279];
        assert_eq!(scene(&challenge), Scene::Field);
        let battle = [12, 27, 33, 146, 147, 167, 168, 169, 170, 338];
        assert_eq!(scene(&battle), Scene::Battle);
        let party = [12, 27, 33, 35, 66, 74, 80, 112, 165, 279, 281];
        assert_eq!(scene(&party), Scene::Menu);
        let summary = [12, 27, 33, 35, 66, 74, 80, 112, 165, 207, 279, 281];
        assert_eq!(scene(&summary), Scene::Menu);
        let bag = [12, 27, 33, 35, 66, 74, 80, 112, 142, 279, 281];
        assert_eq!(scene(&bag), Scene::Menu);
        let starter = [12, 27, 33, 35, 66, 74, 80, 112, 279, 281, 316];
        assert_eq!(scene(&starter), Scene::Menu);
        // A map transition, the opening (with the naming keyboard's 280).
        assert_eq!(
            scene(&[12, 27, 29, 139, 162, 280, 281, 294, 331]),
            Scene::Unknown
        );
        assert_eq!(
            scene(&[12, 27, 33, 35, 66, 74, 80, 112, 279, 281]),
            Scene::Unknown
        );
        assert_eq!(scene(&[12, 27, 29, 331]), Scene::Unknown);

        assert_eq!(scene_wants_panel(Scene::Field, false), Some(false));
        assert_eq!(scene_wants_panel(Scene::Field, true), Some(true));
        assert_eq!(scene_wants_panel(Scene::Battle, false), Some(true));
        assert_eq!(scene_wants_panel(Scene::Menu, false), Some(true));
        assert_eq!(scene_wants_panel(Scene::Unknown, true), None);
    }

    #[test]
    fn keys_tell_when_the_x_menu_is_open() {
        // X on the field opens it; menus from it come back to it.
        let open = x_menu_after(false, Scene::Field, input::X);
        assert!(open);
        assert!(x_menu_after(open, Scene::Field, input::A | input::DOWN));
        assert!(x_menu_after(open, Scene::Menu, input::B));
        // B or X closes it; B alone on the field (running) keeps it shut.
        assert!(!x_menu_after(open, Scene::Field, input::B));
        assert!(!x_menu_after(open, Scene::Field, input::X));
        assert!(!x_menu_after(false, Scene::Field, input::B));
        // Keys elsewhere do not open it, and a battle closes it.
        assert!(!x_menu_after(false, Scene::Menu, input::X));
        assert!(!x_menu_after(false, Scene::Unknown, input::X));
        assert!(!x_menu_after(true, Scene::Battle, input::A));
    }

    #[test]
    fn stick_moves_and_touches_with_the_stylus() {
        let start = Instant::now();
        let mut cursor = PadCursor::new(start);
        assert_eq!(cursor.point(), (128, 96));
        assert!(!cursor.shown(start));
        // Inside the dead zone it stays put and out of sight.
        cursor.update(start + Duration::from_millis(50), (0.1, 0.1), false, true);
        assert_eq!(cursor.point(), (128, 96));
        assert!(!cursor.shown(start + Duration::from_millis(50)));
        // Full lean right moves it at full speed; up is up on screen.
        let t = start + Duration::from_millis(100);
        cursor.update(t, (1.0, 0.0), false, true);
        assert_eq!(cursor.point(), (144, 96));
        assert!(cursor.shown(t));
        cursor.update(t + Duration::from_millis(50), (0.0, 1.0), false, true);
        assert_eq!(cursor.point(), (144, 80));
        // It stops at the edges.
        for step in 1..=20 {
            cursor.update(
                t + Duration::from_millis(50 + 100 * step),
                (-1.0, -1.0),
                false,
                true,
            );
        }
        assert_eq!(cursor.point(), (0, 191));
        // Pressing the stick touches, and keeps it in sight.
        let t = t + Duration::from_secs(10);
        cursor.update(t, (0.0, 0.0), true, true);
        assert!(cursor.touching && cursor.shown(t));
        assert!(!cursor.shown(t + CURSOR_LINGER));
        // With the bottom screen down it lets go and hides.
        cursor.update(t, (1.0, 0.0), true, false);
        assert!(!cursor.touching && !cursor.shown(t));
        assert_eq!(cursor.point(), (0, 191));
    }

    #[test]
    fn bottom_screen_rises_from_under_the_top_one() {
        // 1080p: the top screen fills the width; the bottom one is 720
        // lines high (give or take rounding of the 16:9 top screen),
        // centered, its lower edge on the top screen's.
        assert_eq!(
            panel_rect(1920, 1080, 43, 1.0),
            [481.0, 360.0, 959.0, 719.0]
        );
        // At rest it waits under the top screen, whose 342x192 leaves a
        // line of black above and below at 1920x1080.
        assert_eq!(panel_rect(1920, 1080, 43, 0.0)[1], 1079.0);
        // Twice the DS size: the top screen fills a 684x384 window.
        assert_eq!(panel_rect(684, 384, 43, 1.0), [172.0, 128.0, 341.0, 256.0]);
        assert_eq!(panel_rect(684, 384, 43, 0.5), [172.0, 256.0, 341.0, 256.0]);
        // A 4:3 top screen in a 16:9 window: centered under it.
        assert_eq!(panel_rect(684, 384, 0, 1.0), [172.0, 128.0, 341.0, 256.0]);
    }

    #[test]
    fn stylus_maps_the_raised_bottom_screen() {
        let size = winit::dpi::PhysicalSize::new(684, 384);
        let touch = |x, y| panel_touch(size, PhysicalPosition::new(x, y), 43);
        assert_eq!(touch(172.0, 128.0), Some((0, 0)));
        assert_eq!(touch(342.5, 256.0), Some((128, 96)));
        assert_eq!(touch(512.9, 383.9), Some((255, 191)));
        for (x, y) in [
            (171.9, 200.0),
            (513.0, 200.0),
            (300.0, 127.9),
            (300.0, 384.0),
        ] {
            assert_eq!(touch(x, y), None, "{x}, {y}");
        }
        assert_eq!(
            panel_touch(
                winit::dpi::PhysicalSize::new(0, 0),
                PhysicalPosition::new(0.0, 0.0),
                43
            ),
            None
        );
    }

    /// The single-screen window at rest, halfway and raised: the bottom
    /// screen (green) shows only as far as it has risen, cut off at the
    /// window's lower edge, and the top screen (red) stays red through
    /// the blur.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires a Windows desktop and graphics adapter"]
    fn single_screen_raises_the_bottom_screen_over_the_blurred_top() {
        use winit::platform::windows::EventLoopBuilderExtWindows;

        struct Smoke;
        impl ApplicationHandler for Smoke {
            fn resumed(&mut self, event_loop: &ActiveEventLoop) {
                let size = winit::dpi::PhysicalSize::new(512, 384);
                let window = Arc::new(
                    event_loop
                        .create_window(
                            Window::default_attributes()
                                .with_title("Single test")
                                .with_inner_size(size),
                        )
                        .unwrap(),
                );
                let mut gpu =
                    pollster::block_on(Gpu::new(&[window], &[View::Single], Fps::Ds)).unwrap();
                let mut frame = Frame::blank();
                for pixel in frame.top.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[255, 0, 0, 255]);
                }
                for pixel in frame.bottom.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&[0, 255, 0, 255]);
                }
                gpu.upload(&frame);
                // A 4:3 top screen fills 512x384; the bottom screen is
                // 341x256 at x 86, its top at 384 - 256 * raised.
                for (raised, panel_top) in [(0.0, 384), (0.5, 256), (1.0, 128)] {
                    gpu.panel = raised;
                    let pixels = render_window(&mut gpu, size);
                    for (index, actual) in pixels.iter().enumerate() {
                        let (x, y) = (index % 512, index / 512);
                        let panel = (86..427).contains(&x) && y >= panel_top;
                        let expected = if panel {
                            [0, 255, 0, 255]
                        } else {
                            [255, 0, 0, 255]
                        };
                        let close = actual
                            .iter()
                            .zip(expected)
                            .all(|(&a, e)| a.abs_diff(e) <= 2);
                        assert!(
                            close,
                            "raised {raised}, pixel {x}, {y}: {actual:?}, expected {expected:?}"
                        );
                    }
                }
                // The stick's stylus touching the middle of the bottom
                // screen (4/3 window pixels per DS pixel): light in the
                // middle, dark at the rim, the bottom screen around it.
                gpu.cursor = Some(((128.0, 96.0), true));
                let pixels = render_window(&mut gpu, size);
                let at = |x: usize, y: usize| pixels[y * 512 + x];
                let middle = at(257, 256);
                assert!(middle[0] > 200 && middle[2] > 200, "{middle:?}");
                let rim = at(263, 256);
                assert!(rim[0] < 100 && rim[1] < 200, "{rim:?}");
                assert_eq!(at(257 + 12, 256), [0, 255, 0, 255]);
                event_loop.exit();
            }

            fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
        }

        let event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
        event_loop.run_app(&mut Smoke).unwrap();
    }

    /// Draw window 0 into a texture `size` big and read it back as RGBA.
    #[cfg(target_os = "windows")]
    fn render_window(gpu: &mut Gpu, size: winit::dpi::PhysicalSize<u32>) -> Vec<[u8; 4]> {
        let format = gpu.targets[0].config.format;
        let output = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("window test"),
            size: wgpu::Extent3d {
                width: size.width,
                height: size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("window test readback"),
            size: u64::from(size.width * size.height * 4),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        gpu.prepare(0, size);
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        gpu.encode(
            0,
            &output.create_view(&Default::default()),
            size,
            &mut encoder,
        );
        encoder.copy_texture_to_buffer(
            output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: Some(size.height),
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
        data.chunks_exact(4)
            .map(|pixel| match format {
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                    [pixel[2], pixel[1], pixel[0], pixel[3]]
                }
                wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb => {
                    [pixel[0], pixel[1], pixel[2], pixel[3]]
                }
                _ => panic!("unexpected test surface format: {format:?}"),
            })
            .collect()
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
                let mut gpu =
                    pollster::block_on(Gpu::new(&windows, &[View::Top, View::Bottom], Fps::Ds))
                        .unwrap();
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
