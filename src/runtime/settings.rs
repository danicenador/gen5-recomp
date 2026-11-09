//! PC preferences, independent of the cartridge save and shared by every UI.

use super::screen::Layout;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const ACTION_COUNT: usize = 17;
pub const FAST_FORWARD: usize = 12;
pub const PANEL: usize = 13;
pub const MENU: usize = 14;
pub const FULLSCREEN: usize = 15;
pub const TOUCH: usize = 16;
pub const ACTION_NAMES: [&str; ACTION_COUNT] = [
    "A",
    "B",
    "Select",
    "Start",
    "Right",
    "Left",
    "Up",
    "Down",
    "R",
    "L",
    "X",
    "Y",
    "Fast-forward",
    "Bottom screen",
    "PC settings",
    "Fullscreen",
    "Stylus touch",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bindings {
    pub keyboard: Vec<String>,
    pub controller: Vec<String>,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            keyboard: [
                "KeyX",
                "KeyZ",
                "ShiftRight",
                "Enter",
                "ArrowRight",
                "ArrowLeft",
                "ArrowUp",
                "ArrowDown",
                "KeyW",
                "KeyQ",
                "KeyS",
                "KeyA",
                "Tab",
                "Space",
                "F1",
                "F11",
                "",
            ]
            .map(str::to_owned)
            .to_vec(),
            controller: [
                "East",
                "South",
                "Select",
                "Start",
                "DPadRight",
                "DPadLeft",
                "DPadUp",
                "DPadDown",
                "RightTrigger",
                "LeftTrigger",
                "North",
                "West",
                "RightTrigger2",
                "LeftTrigger2",
                "LeftThumb",
                "",
                "RightThumb",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
}

impl Bindings {
    pub fn keyboard_action(&self, key: &str) -> Option<usize> {
        self.keyboard
            .iter()
            .position(|bound| !bound.is_empty() && bound == key)
    }

    /// A physical button has one meaning. Rebinding transfers it from its old action.
    pub fn bind(&mut self, controller: bool, action: usize, key: String) {
        let bindings = if controller {
            &mut self.controller
        } else {
            &mut self.keyboard
        };
        for bound in bindings.iter_mut() {
            if *bound == key {
                bound.clear();
            }
        }
        bindings[action] = key;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// None selects a monitor-appropriate render scale.
    pub scale: Option<u32>,
    pub widescreen: bool,
    pub layout: Layout,
    pub fullscreen: bool,
    pub smart_bottom: bool,
    pub fast_forward: bool,
    pub speed: u32,
    pub interpolate: bool,
    pub stick_dead_zone: f32,
    pub stylus_sensitivity: f32,
    pub bindings: Bindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scale: Some(1),
            widescreen: false,
            layout: Layout::Stacked,
            fullscreen: false,
            smart_bottom: true,
            fast_forward: false,
            speed: 3,
            interpolate: false,
            stick_dead_zone: 0.5,
            stylus_sensitivity: 1.0,
            bindings: Bindings::default(),
        }
    }
}

impl Settings {
    pub fn path(cache: &Path) -> PathBuf {
        std::env::var_os("GEN5_SETTINGS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| cache.join("pc-settings.json"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("could not read PC settings: {error}")),
        };
        let mut settings: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid PC settings in '{}': {error}", path.display()))?;
        settings.normalize();
        Ok(settings)
    }

    fn normalize(&mut self) {
        self.scale = self
            .scale
            .map(|scale| scale.clamp(1, super::screen::MAX_SCALE));
        self.speed = self.speed.clamp(2, 8);
        if !self.stick_dead_zone.is_finite() {
            self.stick_dead_zone = 0.5;
        }
        self.stick_dead_zone = self.stick_dead_zone.clamp(0.1, 0.9);
        if !self.stylus_sensitivity.is_finite() {
            self.stylus_sensitivity = 1.0;
        }
        self.stylus_sensitivity = self.stylus_sensitivity.clamp(0.25, 3.0);
        let defaults = Bindings::default();
        for (bindings, default) in [
            (&mut self.bindings.keyboard, defaults.keyboard),
            (&mut self.bindings.controller, defaults.controller),
        ] {
            if bindings.len() != ACTION_COUNT {
                *bindings = default;
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create settings directory: {error}"))?;
        }
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(&temporary, bytes)
            .map_err(|error| format!("could not write PC settings: {error}"))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("could not save PC settings: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_without_touching_game_save() {
        let directory = std::env::temp_dir().join(format!("gen5-settings-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("pc-settings.json");
        std::fs::write(directory.join("save.bin"), b"cartridge").unwrap();
        let mut settings = Settings {
            layout: Layout::Single,
            scale: None,
            smart_bottom: false,
            ..Settings::default()
        };
        settings.bindings.bind(true, MENU, "Select".into());
        settings.save(&path).unwrap();
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), settings);
        assert_eq!(
            std::fs::read(directory.join("save.bin")).unwrap(),
            b"cartridge"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn old_files_get_defaults_and_invalid_ranges_are_clamped() {
        let mut settings: Settings =
            serde_json::from_str(r#"{"scale":99,"speed":0,"bindings":{"keyboard":[]}}"#).unwrap();
        settings.normalize();
        assert_eq!(settings.scale, Some(8));
        assert_eq!(settings.speed, 2);
        assert!(settings.smart_bottom);
        assert_eq!(settings.bindings.keyboard.len(), ACTION_COUNT);
        settings.bindings.bind(false, PANEL, "KeyX".into());
        assert_eq!(settings.bindings.keyboard_action("KeyX"), Some(PANEL));
        assert!(settings.bindings.keyboard[0].is_empty());
    }
}
