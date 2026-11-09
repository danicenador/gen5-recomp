//! Game controllers as DS buttons. Face buttons map by position, as on a
//! DS: the bottom one is B, the right one A, the top one X and the left one
//! Y, whatever the controller prints on them. The shoulder buttons are L
//! and R, Start and Select (Back, View, Share, -) are themselves, and the
//! D-pad or the left stick are the D-pad. The right trigger turns
//! fast-forward on and off, like Tab. In single-screen mode the left trigger
//! raises and lowers the bottom screen, like Space, and on the raised bottom
//! screen the right stick moves a stylus that pressing the stick touches
//! with.

use super::settings::{ACTION_COUNT, Bindings, FAST_FORWARD, FULLSCREEN, MENU, PANEL, TOUCH};
use crate::nds::input;
use gilrs::{Axis, Button, Gilrs};

/// How far the stick must lean to press a direction.
#[cfg(test)]
const STICK_THRESHOLD: f32 = 0.5;
const ALL_BUTTONS: [Button; 19] = [
    Button::South,
    Button::East,
    Button::North,
    Button::West,
    Button::C,
    Button::Z,
    Button::LeftTrigger,
    Button::LeftTrigger2,
    Button::RightTrigger,
    Button::RightTrigger2,
    Button::Select,
    Button::Start,
    Button::Mode,
    Button::LeftThumb,
    Button::RightThumb,
    Button::DPadUp,
    Button::DPadDown,
    Button::DPadLeft,
    Button::DPadRight,
];

pub struct Gamepads {
    gilrs: Gilrs,
    fast_forward_held: bool,
    panel_held: bool,
    actions_held: u32,
}

/// What the controllers are doing right now.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct State {
    pub held_actions: u32,
    /// DS buttons held, using the `input` constants.
    pub buttons: u32,
    /// The fast-forward button was pressed since the last poll.
    pub toggle_fast_forward: bool,
    /// The bottom-screen button (left trigger) was pressed since the last
    /// poll.
    pub toggle_panel: bool,
    /// The right stick, x right and y up, each -1 to 1.
    pub right_stick: (f32, f32),
    /// The right stick is pressed in: the stylus touches.
    pub touch: bool,
    pub toggle_menu: bool,
    pub toggle_fullscreen: bool,
    pub capture: Option<Button>,
    pub menu_buttons: u32,
}

const BUTTONS: [(Button, u32); 12] = [
    (Button::East, input::A),
    (Button::South, input::B),
    (Button::North, input::X),
    (Button::West, input::Y),
    (Button::LeftTrigger, input::L),
    (Button::RightTrigger, input::R),
    (Button::Start, input::START),
    (Button::Select, input::SELECT),
    (Button::DPadUp, input::UP),
    (Button::DPadDown, input::DOWN),
    (Button::DPadLeft, input::LEFT),
    (Button::DPadRight, input::RIGHT),
];

impl Gamepads {
    /// None when the system offers no controller support; the game is
    /// still played with the keyboard then.
    pub fn new() -> Option<Self> {
        match Gilrs::new() {
            Ok(gilrs) => {
                for (_, gamepad) in gilrs.gamepads() {
                    tracing::info!("controller connected: {}", gamepad.name());
                }
                Some(Self {
                    gilrs,
                    fast_forward_held: false,
                    panel_held: false,
                    actions_held: 0,
                })
            }
            Err(error) => {
                tracing::warn!("controllers are unavailable: {error}");
                None
            }
        }
    }

    pub fn poll(&mut self, bindings: &Bindings, dead_zone: f32) -> State {
        let mut capture = None;
        while let Some(event) = self.gilrs.next_event() {
            match event.event {
                gilrs::EventType::Connected => tracing::info!(
                    "controller connected: {}",
                    self.gilrs.gamepad(event.id).name()
                ),
                gilrs::EventType::Disconnected => tracing::info!("controller disconnected"),
                gilrs::EventType::ButtonPressed(button, _) if ALL_BUTTONS.contains(&button) => {
                    capture = Some(button)
                }
                _ => {}
            }
        }
        let mut buttons = 0;
        let mut fast_forward = false;
        let mut panel = false;
        let mut right_stick = (0.0f32, 0.0f32);
        let mut touch = false;
        let mut actions = 0u32;
        let mut menu_buttons = 0;
        for (_, gamepad) in self.gilrs.gamepads() {
            for button in ALL_BUTTONS {
                if !gamepad.is_pressed(button) {
                    continue;
                }
                let name = format!("{button:?}");
                for (action, bound) in bindings.controller.iter().enumerate().take(ACTION_COUNT) {
                    if *bound == name {
                        actions |= 1 << action;
                    }
                }
            }
            let directions = stick_with_threshold(
                gamepad.value(Axis::LeftStickX),
                gamepad.value(Axis::LeftStickY),
                dead_zone,
            );
            buttons |= directions;
            menu_buttons |= directions;
            // Menu navigation always has a recovery path, independent of gameplay bindings.
            for (button, bit) in BUTTONS {
                if gamepad.is_pressed(button) {
                    menu_buttons |= bit;
                }
            }
            // The controller leaning furthest steers.
            let (x, y) = (
                gamepad.value(Axis::RightStickX),
                gamepad.value(Axis::RightStickY),
            );
            if x.hypot(y) > right_stick.0.hypot(right_stick.1) {
                right_stick = (x, y);
            }
        }
        buttons |= actions & 0xFFF;
        fast_forward |= actions & (1 << FAST_FORWARD) != 0;
        panel |= actions & (1 << PANEL) != 0;
        touch |= actions & (1 << TOUCH) != 0;
        let newly_pressed = actions & !self.actions_held;
        self.actions_held = actions;
        let toggle_fast_forward = fast_forward && !self.fast_forward_held;
        self.fast_forward_held = fast_forward;
        let toggle_panel = panel && !self.panel_held;
        self.panel_held = panel;
        State {
            held_actions: actions,
            buttons,
            toggle_fast_forward,
            toggle_panel,
            right_stick,
            touch,
            toggle_menu: newly_pressed & (1 << MENU) != 0,
            toggle_fullscreen: newly_pressed & (1 << FULLSCREEN) != 0,
            capture,
            menu_buttons,
        }
    }
}

/// D-pad bits for a stick position (y up is positive).
#[cfg(test)]
fn stick(x: f32, y: f32) -> u32 {
    stick_with_threshold(x, y, STICK_THRESHOLD)
}

fn stick_with_threshold(x: f32, y: f32, threshold: f32) -> u32 {
    let mut buttons = 0;
    if x <= -threshold {
        buttons |= input::LEFT;
    }
    if x >= threshold {
        buttons |= input::RIGHT;
    }
    if y >= threshold {
        buttons |= input::UP;
    }
    if y <= -threshold {
        buttons |= input::DOWN;
    }
    buttons
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_presses_directions_past_the_threshold() {
        assert_eq!(stick(0.0, 0.0), 0);
        assert_eq!(stick(0.3, -0.4), 0);
        assert_eq!(stick(-0.9, 0.0), input::LEFT);
        assert_eq!(stick(0.7, 0.7), input::RIGHT | input::UP);
        assert_eq!(stick(0.0, -1.0), input::DOWN);
    }
}
