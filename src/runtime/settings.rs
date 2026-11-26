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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub fn parse(text: &str) -> Result<Self> {
        let (width, height) = text
            .split_once(['x', 'X'])
            .ok_or("resolution must be WIDTHxHEIGHT")?;
        let resolution = Self {
            width: width.parse().map_err(|_| "invalid resolution width")?,
            height: height.parse().map_err(|_| "invalid resolution height")?,
        };
        if !(64..=8192).contains(&resolution.width) || !(48..=4320).contains(&resolution.height) {
            return Err("resolution must be between 64x48 and 8192x4320".into());
        }
        Ok(resolution)
    }

    /// The game fits 4:3 through 16:9 without stretching. Wider or taller
    /// output modes leave bars outside this content rectangle.
    pub fn content(self, wide: bool) -> (u32, u32) {
        let aspect = if wide {
            (self.width as f64 / self.height as f64).clamp(4.0 / 3.0, 16.0 / 9.0)
        } else {
            4.0 / 3.0
        };
        let height = (self.height as f64).min(self.width as f64 / aspect);
        (
            (height * aspect).round().max(1.0) as u32,
            height.round().max(1.0) as u32,
        )
    }

    pub fn choices(monitor: Option<Self>) -> Vec<Self> {
        let mut choices = vec![
            (800, 600),
            (1024, 768),
            (1280, 720),
            (1280, 800),
            (1366, 768),
            (1600, 900),
            (1920, 1080),
            (1920, 1200),
            (2560, 1440),
            (2560, 1600),
            (3840, 2160),
        ]
        .into_iter()
        .map(|(width, height)| Self { width, height })
        .collect::<Vec<_>>();
        if let Some(monitor) = monitor.filter(|monitor| !choices.contains(monitor)) {
            choices.push(monitor);
        }
        choices.sort_by_key(|resolution| (resolution.width, resolution.height));
        choices
    }
}

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
    pub resolution: Option<Resolution>,
    /// None selects a monitor-appropriate render scale.
    pub scale: Option<u32>,
    pub widescreen: bool,
    pub layout: Layout,
    pub fullscreen: bool,
    pub smart_bottom: bool,
    pub fast_forward: bool,
    pub speed: u32,
    pub interpolate: bool,
    pub auto_continue: bool,
    pub stick_dead_zone: f32,
    pub stylus_sensitivity: f32,
    pub bindings: Bindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution: None,
            scale: None,
            widescreen: true,
            layout: Layout::Single,
            fullscreen: true,
            smart_bottom: true,
            fast_forward: false,
            speed: 3,
            interpolate: false,
            auto_continue: false,
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
        if let Some(resolution) = &mut self.resolution {
            resolution.width = resolution.width.clamp(64, 8192);
            resolution.height = resolution.height.clamp(48, 4320);
        }
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

/// Where the preferences are saved, and what this launch overrode in them.
///
/// Launch flags and environment variables apply to one launch. Saving keeps
/// the file's value for each setting they overrode until the player changes
/// that setting while playing; from then on the player's choice is saved.
pub struct Store {
    path: PathBuf,
    /// What the file held at launch.
    saved: Settings,
    /// What this launch started with, overrides included.
    launched: Settings,
    /// Settings the player has changed since launch.
    changed: Vec<&'static str>,
}

impl Store {
    pub fn new(path: PathBuf, saved: Settings, launched: Settings) -> Self {
        Self {
            path,
            saved,
            launched,
            changed: Vec::new(),
        }
    }

    /// Save `current`, without the overrides the player has left alone.
    pub fn save(&mut self, current: &Settings) -> Result<()> {
        self.persisted(current).save(&self.path)
    }

    fn persisted(&mut self, current: &Settings) -> Settings {
        let mut persisted = current.clone();
        macro_rules! one_launch {
            ($($field:ident),* $(,)?) => {
                // Without `..`, a field added to Settings must be listed here.
                let Settings { $($field: _),* } = current;
                $(
                    let name = stringify!($field);
                    if current.$field != self.launched.$field && !self.changed.contains(&name) {
                        self.changed.push(name);
                    }
                    if !self.changed.contains(&name) {
                        persisted.$field = self.saved.$field.clone();
                    }
                )*
            };
        }
        one_launch!(
            resolution,
            scale,
            widescreen,
            layout,
            fullscreen,
            smart_bottom,
            fast_forward,
            speed,
            interpolate,
            auto_continue,
            stick_dead_zone,
            stylus_sensitivity,
            bindings,
        );
        persisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_overrides_are_not_saved_until_the_player_changes_them() {
        let saved = Settings {
            widescreen: false,
            auto_continue: false,
            layout: Layout::Split,
            speed: 4,
            ..Settings::default()
        };
        // `--continue --widescreen --single-screen`.
        let launched = Settings {
            widescreen: true,
            auto_continue: true,
            layout: Layout::Single,
            ..saved.clone()
        };
        let mut store = Store::new(PathBuf::new(), saved.clone(), launched.clone());

        // The player changes the speed: only that is saved.
        let mut current = Settings {
            speed: 6,
            ..launched.clone()
        };
        let persisted = store.persisted(&current);
        assert_eq!(
            persisted,
            Settings {
                speed: 6,
                ..saved.clone()
            }
        );

        // Turning widescreen off and back on makes it the player's choice.
        current.widescreen = false;
        assert!(!store.persisted(&current).widescreen);
        current.widescreen = true;
        let persisted = store.persisted(&current);
        assert!(persisted.widescreen);
        assert!(!persisted.auto_continue);
        assert_eq!(persisted.layout, Layout::Split);
    }

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
            auto_continue: true,
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
        assert!(!settings.auto_continue);
        assert_eq!(settings.bindings.keyboard.len(), ACTION_COUNT);
        settings.bindings.bind(false, PANEL, "KeyX".into());
        assert_eq!(settings.bindings.keyboard_action("KeyX"), Some(PANEL));
        assert!(settings.bindings.keyboard[0].is_empty());
    }

    #[test]
    fn arbitrary_resolutions_keep_aspect_and_bar_ultrawide_monitors() {
        assert_eq!(
            Resolution::parse("1920x1080").unwrap().content(true),
            (1920, 1080)
        );
        assert_eq!(
            Resolution::parse("1920x1200").unwrap().content(true),
            (1920, 1200)
        );
        assert_eq!(
            Resolution::parse("1417x907").unwrap().content(true),
            (1417, 907)
        );
        assert_eq!(
            Resolution::parse("3440x1440").unwrap().content(true),
            (2560, 1440)
        );
        assert_eq!(
            Resolution::parse("1920x1080").unwrap().content(false),
            (1440, 1080)
        );
        assert!(Resolution::parse("0x1080").is_err());
        assert!(Resolution::parse("1920").is_err());
        let monitor = Resolution {
            width: 2256,
            height: 1504,
        };
        assert_eq!(
            Resolution::choices(Some(monitor))
                .iter()
                .filter(|&&item| item == monitor)
                .count(),
            1
        );
        assert_eq!(Settings::default().layout, Layout::Single);
    }
}
