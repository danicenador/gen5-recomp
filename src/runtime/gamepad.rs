//! Game controllers as DS buttons. Face buttons map by position, as on a
//! DS: the bottom one is B, the right one A, the top one X and the left one
//! Y, whatever the controller prints on them. The shoulder buttons are L
//! and R, Start and Select (Back, View, Share, -) are themselves, and the
//! D-pad or the left stick are the D-pad. The right trigger turns
//! fast-forward on and off, like Tab.

use crate::nds::input;
use gilrs::{Axis, Button, Gilrs};

/// How far the stick must lean to press a direction.
const STICK_THRESHOLD: f32 = 0.5;

pub struct Gamepads {
    gilrs: Gilrs,
    fast_forward_held: bool,
}

/// What the controllers are doing right now.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    /// DS buttons held, using the `input` constants.
    pub buttons: u32,
    /// The fast-forward button was pressed since the last poll.
    pub toggle_fast_forward: bool,
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
                })
            }
            Err(error) => {
                tracing::warn!("controllers are unavailable: {error}");
                None
            }
        }
    }

    pub fn poll(&mut self) -> State {
        while let Some(event) = self.gilrs.next_event() {
            match event.event {
                gilrs::EventType::Connected => tracing::info!(
                    "controller connected: {}",
                    self.gilrs.gamepad(event.id).name()
                ),
                gilrs::EventType::Disconnected => tracing::info!("controller disconnected"),
                _ => {}
            }
        }
        let mut buttons = 0;
        let mut fast_forward = false;
        for (_, gamepad) in self.gilrs.gamepads() {
            for (button, bit) in BUTTONS {
                if gamepad.is_pressed(button) {
                    buttons |= bit;
                }
            }
            buttons |= stick(gamepad.value(Axis::LeftStickX), gamepad.value(Axis::LeftStickY));
            fast_forward |= gamepad.is_pressed(Button::RightTrigger2);
        }
        let toggle_fast_forward = fast_forward && !self.fast_forward_held;
        self.fast_forward_held = fast_forward;
        State {
            buttons,
            toggle_fast_forward,
        }
    }
}

/// D-pad bits for a stick position (y up is positive).
fn stick(x: f32, y: f32) -> u32 {
    let mut buttons = 0;
    if x <= -STICK_THRESHOLD {
        buttons |= input::LEFT;
    }
    if x >= STICK_THRESHOLD {
        buttons |= input::RIGHT;
    }
    if y >= STICK_THRESHOLD {
        buttons |= input::UP;
    }
    if y <= -STICK_THRESHOLD {
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
