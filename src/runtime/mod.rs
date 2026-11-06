pub mod audio;
pub mod pacing;
pub mod scaled;
pub mod screen;

use crate::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Play the recompiled game in a window: the game runs on its own threads,
/// paced to the DS frame rate, and the window shows its frames, drawn at
/// `scale` times the DS resolution (keys 1-8 change it). The window draws
/// as often as `fps` says, interpolating 3D between game frames.
pub fn play_recompiled(cache_root: &Path, rom: &Path, scale: u32, fps: pacing::Fps) -> Result<()> {
    use crate::nds::{input::Input, machine};
    let scale = Arc::new(std::sync::atomic::AtomicU32::new(
        scale.clamp(1, screen::MAX_SCALE),
    ));
    let screens = Arc::new(screen::Screens::default());
    let input = Arc::new(Input::default());
    let audio = Arc::new(crate::nds::sound::AudioQueue::default());
    // Kept alive for as long as the window is open.
    let _output = audio::start(audio.clone());
    let mut next = Instant::now();
    let sink = screens.clone();
    let on_frame: machine::FrameSink = Box::new(move |number, frame| {
        // Hold the game to 59.83 frames per second; after a long stall
        // (loading, a slow host), carry on from now instead of catching up.
        // The frame is due on screen when the game moves on: the window
        // times interpolation by these steady deadlines.
        next += screen::FRAME_TIME;
        let now = Instant::now();
        if now > next && now - next > screen::FRAME_TIME * 6 {
            next = now;
        }
        sink.publish(number, frame, next);
        if next > now {
            std::thread::sleep(next - now);
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
        scale: scale.clone(),
        gpu: fps.interpolates(),
    };
    let (cache_root, rom): (PathBuf, PathBuf) = (cache_root.into(), rom.into());
    std::thread::spawn(move || {
        if let Err(error) = machine::boot(&cache_root, &rom, options) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    });
    screen::run("Gen5 Recomp", screens, input, scale, fps)?;
    // The game threads never return; closing the window ends the program.
    std::process::exit(0);
}
