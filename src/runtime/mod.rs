pub mod audio;
pub mod gamepad;
pub mod pacing;
pub mod scaled;
pub mod screen;

use crate::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Play the recompiled game in a window: the game runs on its own threads,
/// paced to the DS frame rate (or a multiple of it while fast-forwarding,
/// Tab), and the window shows its frames, drawn at `scale` times the DS
/// resolution (keys 1-8 change it). The window draws as often as `fps`
/// says, interpolating 3D between game frames. `split_screens` puts each
/// DS screen in its own window.
pub fn play_recompiled(
    cache_root: &Path,
    rom: &Path,
    scale: u32,
    fps: pacing::Fps,
    split_screens: bool,
) -> Result<()> {
    use crate::nds::{input::Input, machine, speed::Speed};
    let scale = Arc::new(std::sync::atomic::AtomicU32::new(
        scale.clamp(1, screen::MAX_SCALE),
    ));
    let screens = Arc::new(screen::Screens::default());
    let input = Arc::new(Input::default());
    // GEN5_SPEED=N starts fast-forwarding at N times the game speed.
    let speed = Arc::new(match std::env::var("GEN5_SPEED") {
        Ok(value) => Speed::fixed(
            value
                .parse()
                .map_err(|_| format!("invalid GEN5_SPEED: {value}"))?,
        ),
        Err(_) => Speed::default(),
    });
    let audio = Arc::new(crate::nds::sound::AudioQueue::default());
    // Kept alive for as long as the window is open.
    let _output = audio::start(audio.clone());
    let sink = screens.clone();
    // The machine paces the game and says when each frame is due on screen.
    let on_frame: machine::FrameSink =
        Box::new(move |number, frame, deadline| sink.publish(number, frame, deadline));
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
        speed: Some(speed.clone()),
        real_time: true,
    };
    let (cache_root, rom): (PathBuf, PathBuf) = (cache_root.into(), rom.into());
    std::thread::spawn(move || {
        if let Err(error) = machine::boot(&cache_root, &rom, options) {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    });
    screen::run(
        "Gen5 Recomp",
        screens,
        input,
        scale,
        fps,
        speed,
        split_screens,
    )?;
    // The game threads never return; closing the window ends the program.
    std::process::exit(0);
}
