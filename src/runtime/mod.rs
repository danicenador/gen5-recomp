pub mod audio;
mod desktop;
pub mod screen;
mod wgpu_renderer;

use crate::Result;
use crate::game::bw2::field::FieldSession;
use crate::game::bw2::map::ZoneMap;
use crate::render;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Launch the native desktop field slice using the imported map geometry.
pub fn run(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<()> {
    let session = FieldSession::load(cache_root, zone.clone(), start)?;
    let scene = render::load_map_scene(cache_root, &zone)?;
    desktop::run(cache_root, session, scene)
}

/// Play the recompiled game in a window: the game runs on its own threads,
/// paced to the DS frame rate, and the window shows its frames.
pub fn play_recompiled(cache_root: &Path, rom: &Path) -> Result<()> {
    use crate::nds::{input::Input, machine};
    let screens = Arc::new(screen::Screens::default());
    let input = Arc::new(Input::default());
    let audio = Arc::new(crate::nds::sound::AudioQueue::default());
    // Kept alive for as long as the window is open.
    let _output = audio::start(audio.clone());
    let mut next = Instant::now();
    let sink = screens.clone();
    let on_frame: machine::FrameSink = Box::new(move |number, frame| {
        sink.publish(number, frame);
        // Hold the game to 59.83 frames per second; after a long stall
        // (loading, a slow host), carry on from now instead of catching up.
        next += screen::FRAME_TIME;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else if now - next > screen::FRAME_TIME * 6 {
            next = now;
        }
    });
    let options = machine::Options {
        max_frames: u64::MAX,
        seconds: 0,
        on_frame: Some(on_frame),
        render_every: 1,
        on_vblank: None,
        input: Some(input.clone()),
        audio: Some(audio.clone()),
    };
    let (cache_root, rom): (PathBuf, PathBuf) = (cache_root.into(), rom.into());
    std::thread::spawn(move || {
        if let Err(error) = machine::boot(&cache_root, &rom, options) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    });
    screen::run("Gen5 Recomp", screens, input)?;
    // The game threads never return; closing the window ends the program.
    std::process::exit(0);
}
