use gen5::formats::nds::{Cpu, NdsRom};
use gen5::importer::{self, title as gen5_game};
use gen5::recomp::analysis::{self as recomp_analysis, Analysis};
use gen5::recomp::program::{ModuleId, Program, Resolution};
use gen5::runtime::screen::MAX_SCALE;
use std::env;
use std::path::{Path, PathBuf};

mod help;

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gen5=info"));
    // With --json, stdout carries only JSON lines, so logs go to stderr.
    let json = env::args().any(|arg| arg == "--json");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(move || -> Box<dyn std::io::Write> {
            if json {
                Box::new(std::io::stderr())
            } else {
                Box::new(std::io::stdout())
            }
        })
        .try_init();
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        println!("gen5 {}", gen5::install::version());
        return Ok(());
    }
    if let Some(help) = help::requested(&args) {
        println!("{}", help?);
        return Ok(());
    }
    if matches!(args.first().map(String::as_str), Some("run" | "boot"))
        && !gen5::runtime::reset::is_worker()
    {
        return gen5::runtime::reset::supervise(&args);
    }
    match args.first().map(String::as_str) {
        Some("debug") => gen5::debug::launch(&args[1..]),
        Some("checkpoint") if args.len() == 5 => {
            gen5::debug::promote(Path::new(&args[1]), Path::new(&args[2]), &args[3], &args[4])
        }
        Some("inspect") if args.len() == 2 => inspect(Path::new(&args[1])),
        Some("inspect") if args.len() == 3 && args[2] == "--json" => {
            print_json(&gen5::install::inspect(Path::new(&args[1]))?)
        }
        Some("version") if args.len() == 1 => {
            println!("gen5 {}", gen5::install::version());
            Ok(())
        }
        Some("version") if args.len() == 2 && args[1] == "--json" => {
            print_json(&serde_json::json!({ "version": gen5::install::version() }))
        }
        Some("status") if args.len() == 2 || (args.len() == 3 && args[2] == "--json") => {
            let status = gen5::install::status(Path::new(&args[1]));
            if args.len() == 3 {
                print_json(&status)
            } else {
                print_status(&status);
                Ok(())
            }
        }
        Some("install" | "update") if args.len() >= 2 => install_command(&args),
        Some("code-info") if args.len() == 2 => code_info(Path::new(&args[1])),
        Some("analyze") if args.len() == 2 => analyze(Path::new(&args[1])),
        Some("codegen") if args.len() == 2 => codegen(Path::new(&args[1])),
        Some("build") if args.len() == 2 || args.len() == 3 => {
            let compiler = args.get(2).map_or("gcc", String::as_str);
            build(Path::new(&args[1]), compiler)
        }
        Some("boot") if (2..=5).contains(&args.len()) => {
            let (rom, rest) = rom_argument(Path::new(&args[1]), &args[2..])?;
            if rest.len() > 2 {
                return Err(format!(
                    "wrong arguments for 'boot'\n\nUsage: {}",
                    help::command("boot").unwrap().usage
                ));
            }
            let max_frames = rest
                .first()
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid frame count '{value}'"))
                })
                .transpose()?
                .unwrap_or(60);
            // GEN5_INPUT=first-last:button+button,... holds buttons over frame
            // ranges, for scripted runs.
            let mut script = std::env::var("GEN5_INPUT")
                .map(|spec| parse_input_script(&spec))
                .unwrap_or(Ok(Vec::new()))?;
            let input_file = std::env::var_os("GEN5_INPUT_FILE").map(PathBuf::from);
            let mut input_text = String::new();
            if let Some(path) = &input_file {
                input_text = std::fs::read_to_string(path).map_err(|error| {
                    format!("could not read input script '{}': {error}", path.display())
                })?;
                script = parse_input_file(&input_text)?;
            }
            let touch_script = std::env::var("GEN5_TOUCH")
                .map(|spec| parse_touch_script(&spec))
                .unwrap_or(Ok(Vec::new()))?;
            let screenshot_every = std::env::var("GEN5_SCREEN_EVERY")
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| "invalid GEN5_SCREEN_EVERY".to_owned())
                })
                .unwrap_or(Ok(30))?;
            let input = std::sync::Arc::new(gen5::nds::input::Input::default());
            input.set_buttons(
                script
                    .iter()
                    .filter(|(range, _)| range.contains(&1))
                    .fold(0, |buttons, (_, held)| buttons | held),
            );
            input.set_touch(
                touch_script
                    .iter()
                    .rev()
                    .find(|(range, _)| range.contains(&1))
                    .map(|(_, point)| *point),
            );
            let scripted = input.clone();
            let fps = std::env::var("GEN5_BOOT_FPS")
                .map(|value| {
                    value
                        .parse::<u32>()
                        .map_err(|_| "invalid GEN5_BOOT_FPS".to_owned())
                })
                .unwrap_or(Ok(0))?;
            let interval =
                (fps != 0).then(|| std::time::Duration::from_secs_f64(1.0 / f64::from(fps)));
            let mut next = std::time::Instant::now();
            // GEN5_AUDIO_WAV: record the mixed sound of a headless run.
            let audio = std::env::var_os("GEN5_AUDIO_WAV")
                .map(|path| -> Result<_, String> {
                    let writer = gen5::runtime::audio::WavWriter::create(Path::new(&path))
                        .map_err(|error| format!("could not create audio file: {error}"))?;
                    Ok((
                        std::sync::Arc::new(gen5::nds::sound::AudioQueue::default()),
                        writer,
                    ))
                })
                .transpose()?;
            let queue = audio.as_ref().map(|(queue, _)| queue.clone());
            let mut wav = audio.map(|(_, writer)| writer);
            let wav_queue = queue.clone();
            let on_vblank = Some(Box::new(move |number: u64| {
                if let (Some(writer), Some(queue)) = (&mut wav, &wav_queue)
                    && let Err(error) = writer.drain(queue)
                {
                    eprintln!("could not write audio: {error}");
                }
                if let Some(interval) = interval {
                    next += interval;
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else if now - next > interval * 6 {
                        next = now;
                    }
                }
                // Reload at a frame boundary so a long diagnostic playthrough
                // can be extended without restarting the introduction.
                if number.is_multiple_of(60)
                    && let Some(path) = &input_file
                {
                    match std::fs::read_to_string(path) {
                        Ok(text) if text != input_text => {
                            match parse_input_file(&text) {
                                Ok(updated) => {
                                    script = updated;
                                    eprintln!("input script reloaded at frame {number}");
                                }
                                Err(error) => eprintln!("{error}; keeping previous inputs"),
                            }
                            input_text = text;
                        }
                        Err(error) => eprintln!("could not reload input script: {error}"),
                        _ => {}
                    }
                }
                let held = script
                    .iter()
                    .filter(|(range, _)| range.contains(&(number + 1)))
                    .fold(0, |buttons, (_, held)| buttons | held);
                scripted.set_buttons(held);
                scripted.set_touch(
                    touch_script
                        .iter()
                        .rev()
                        .find(|(range, _)| range.contains(&(number + 1)))
                        .map(|(_, point)| *point),
                );
            }) as Box<dyn FnMut(u64) + Send>);
            let directory = rest.get(1).map(std::path::PathBuf::from);
            // GEN5_SCALE: screenshots at a multiple of the DS resolution,
            // drawn by the GPU renderer.
            let resolution = std::env::var("GEN5_RESOLUTION")
                .ok()
                .map(|value| gen5::runtime::settings::Resolution::parse(&value))
                .transpose()?;
            let scale = resolution.map_or_else(scale_from_env, |resolution| {
                Ok(resolution.height.div_ceil(192).clamp(1, 8))
            })?;
            let mut gpu: Option<gen5::runtime::scaled::Headless> = None;
            // GEN5_INTERP_SHOTS=1: when a frame shows a new 3D list, also
            // save the frame before it drawn half way to that list
            // (`frame-NNNNN-half.png`), as the window's 3D interpolation
            // draws it, to find polygons that tear or stay behind.
            let interp_shots = std::env::var("GEN5_INTERP_SHOTS").is_ok_and(|value| value == "1");
            let mut shown: Option<gen5::nds::gpu2d::ScaledFrame> = None;
            let on_frame = directory.map(|directory| {
                Box::new(move |number: u64, frame: &gen5::nds::gpu2d::Frame, _| {
                    let (width, height, pixels) = match &frame.scaled {
                        Some(scaled) => {
                            if gpu.is_none() {
                                match gen5::runtime::scaled::Headless::new() {
                                    Ok(renderer) => gpu = Some(renderer),
                                    Err(error) => {
                                        eprintln!("could not start the GPU renderer: {error}");
                                        std::process::exit(1);
                                    }
                                }
                            }
                            if interp_shots
                                && let Some(before) = shown.take()
                                && let (Some(old), Some(new)) = (&before.three_d, &scaled.three_d)
                                && old.scene.list.swap != new.scene.list.swap
                            {
                                let gpu = gpu.as_mut().unwrap();
                                let half = match resolution {
                                    Some(resolution) => gpu.render_interpolated_sized(
                                        &before,
                                        resolution.content(before.extra > 0),
                                        &new.scene.list,
                                        0.5,
                                    ),
                                    None => gpu.render_interpolated(
                                        &before,
                                        scale,
                                        &new.scene.list,
                                        0.5,
                                    ),
                                };
                                if let Ok((width, height, pixels)) = half {
                                    let image = gen5::nds::gpu2d::png(
                                        width as usize,
                                        height as usize,
                                        &pixels,
                                    );
                                    let path =
                                        directory.join(format!("frame-{number:05}-half.png"));
                                    let _ = std::fs::create_dir_all(&directory)
                                        .and_then(|()| std::fs::write(&path, image));
                                }
                            }
                            if interp_shots {
                                shown = Some((**scaled).clone());
                            }
                            let rendered = match resolution {
                                Some(resolution) => gpu
                                    .as_mut()
                                    .unwrap()
                                    .render_sized(scaled, resolution.content(scaled.extra > 0)),
                                None => gpu.as_mut().unwrap().render(scaled, scale),
                            };
                            match rendered {
                                Ok(image) => image,
                                Err(error) => {
                                    eprintln!("{error}");
                                    return;
                                }
                            }
                        }
                        None => {
                            let mut pixels = frame.top.clone();
                            pixels.extend_from_slice(&frame.bottom);
                            (256, 384, pixels)
                        }
                    };
                    let image = gen5::nds::gpu2d::png(width as usize, height as usize, &pixels);
                    let path = directory.join(format!("frame-{number:05}.png"));
                    if let Err(error) = std::fs::create_dir_all(&directory)
                        .and_then(|()| std::fs::write(&path, image))
                    {
                        eprintln!("could not write {}: {error}", path.display());
                    }
                }) as gen5::nds::machine::FrameSink
            });
            gen5::nds::machine::boot(
                Path::new(&args[1]),
                &rom,
                gen5::nds::machine::Options {
                    auto_continue: std::env::var("GEN5_AUTO_CONTINUE")
                        .is_ok_and(|value| value == "1"),
                    max_frames,
                    seconds: std::env::var("GEN5_BOOT_SECONDS")
                        .map(|value| {
                            value
                                .parse::<u64>()
                                .map_err(|_| "invalid GEN5_BOOT_SECONDS".to_owned())
                        })
                        .unwrap_or(Ok(20))?,
                    on_frame,
                    render_every: screenshot_every,
                    on_vblank,
                    input: Some(input),
                    audio: queue,
                    scale: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(scale)),
                    gpu: resolution.is_some(),
                    // GEN5_WIDESCREEN=1: 16:9 top-screen screenshots.
                    widescreen: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                        widescreen_from_env(),
                    )),
                    // GEN5_SPEED=N: fast-forward at N times the game speed,
                    // with sound and key repeat counted at normal speed.
                    speed: std::env::var("GEN5_SPEED")
                        .ok()
                        .map(|value| {
                            value
                                .parse::<u32>()
                                .map_err(|_| "invalid GEN5_SPEED".to_owned())
                        })
                        .transpose()?
                        .map(|factor| std::sync::Arc::new(gen5::nds::speed::Speed::fixed(factor))),
                    real_time: false,
                    overlays: None,
                },
            )
        }
        Some("run") if args.len() >= 2 => {
            let (rom, rest) = rom_argument(Path::new(&args[1]), &args[2..])?;
            use gen5::runtime::screen::Layout;
            use gen5::runtime::settings::Settings;
            let settings_path = Settings::path(Path::new(&args[1]));
            let mut settings = Settings::load(&settings_path)?;
            // Flags and environment variables below apply to this launch only.
            let saved = settings.clone();
            if let Ok(value) = std::env::var("GEN5_RESOLUTION") {
                settings.resolution = Some(gen5::runtime::settings::Resolution::parse(&value)?);
                settings.scale = None;
            }
            if std::env::var_os("GEN5_AUTO_CONTINUE").is_some() {
                settings.auto_continue =
                    std::env::var("GEN5_AUTO_CONTINUE").is_ok_and(|value| value == "1");
            }
            let mut scale = settings.scale;
            let mut explicit_scale = false;
            let mut fps = None;
            let mut layout = settings.layout;
            let mut chosen_layout = None;
            let mut widescreen = settings.widescreen;
            if std::env::var_os("GEN5_WIDESCREEN").is_some() {
                widescreen = widescreen_from_env();
            }
            if std::env::var_os("GEN5_SCALE").is_some() {
                scale = Some(scale_from_env()?);
                settings.resolution = None;
            }
            for value in rest {
                let chosen = match value.as_str() {
                    "--split-screens" => Some(Layout::Split),
                    "--single-screen" => Some(Layout::Single),
                    _ => None,
                };
                if let Some(chosen) = chosen {
                    if chosen_layout.is_some_and(|layout| layout != chosen) {
                        return Err(
                            "--split-screens and --single-screen cannot be combined".to_owned()
                        );
                    }
                    layout = chosen;
                    chosen_layout = Some(chosen);
                    if layout == Layout::Single {
                        settings.fullscreen = true;
                        widescreen = true;
                    }
                } else if let Some(value) = value.strip_prefix("--resolution=") {
                    settings.resolution = Some(gen5::runtime::settings::Resolution::parse(value)?);
                    scale = None;
                    explicit_scale = true;
                } else if value == "--continue" {
                    settings.auto_continue = true;
                } else if value == "--widescreen" {
                    widescreen = true;
                } else if let Some(mode) = value.strip_prefix("--fps=") {
                    fps = Some(gen5::runtime::pacing::Fps::parse(mode)?);
                } else if let Some(text) = scale_argument(value) {
                    explicit_scale = true;
                    settings.resolution = None;
                    scale = Some(parse_scale(text).ok_or_else(|| {
                        format!("invalid scale {text}: expected 1 to {MAX_SCALE}")
                    })?);
                } else {
                    return Err(format!(
                        "unknown option '{value}' for 'run'\n\nUsage: {}\n\nRun `gen5 help run` \
                         for more.",
                        help::command("run").map_or("", |command| command.usage)
                    ));
                }
            }
            if chosen_layout == Some(Layout::Single)
                && !explicit_scale
                && std::env::var_os("GEN5_SCALE").is_none()
            {
                scale = None;
            }
            settings.scale = scale;
            settings.layout = layout;
            settings.widescreen = widescreen;
            let fps = match fps {
                Some(fps) => {
                    settings.interpolate = fps.interpolates();
                    fps
                }
                None if std::env::var_os("GEN5_FPS").is_some() => {
                    let fps = fps_from_env()?;
                    settings.interpolate = fps.interpolates();
                    fps
                }
                None if settings.interpolate => gen5::runtime::pacing::Fps::Display,
                None => gen5::runtime::pacing::Fps::Ds,
            };
            gen5::runtime::play_recompiled(Path::new(&args[1]), &rom, fps, saved, settings)
        }
        Some("disasm") if args.len() == 4 => {
            let module = args[2].parse::<ModuleId>()?;
            let address = parse_address(&args[3])?;
            disasm(Path::new(&args[1]), module, address)
        }
        Some("list") if args.len() == 2 || args.len() == 3 => {
            let prefix = args.get(2).map(String::as_str).unwrap_or("");
            list(Path::new(&args[1]), prefix)
        }
        Some("import") if args.len() == 2 || args.len() == 3 => {
            let cache_path = args
                .get(2)
                .map_or_else(|| Path::new(".gen5-cache"), |path| Path::new(path));
            import(Path::new(&args[1]), cache_path)
        }
        Some(name) => match help::command(name) {
            Some(command) => Err(format!(
                "wrong arguments for '{name}'\n\nUsage: {}\n\nRun `gen5 help {name}` for more.",
                command.usage
            )),
            None => Err(format!("unknown command '{name}'\n\n{}", help::usage())),
        },
        None => unreachable!("no arguments print the help"),
    }
}

fn inspect(path: &Path) -> Result<(), String> {
    let rom = NdsRom::open(path)?;
    let code = String::from_utf8_lossy(&rom.header.game_code);
    let game = gen5_game::identify(&rom.header.game_code);
    println!("ROM: {}", path.display());
    println!("Title: {}", rom.header.title);
    println!("Game code: {code}");
    println!(
        "Game: {}",
        game.map_or("unrecognized Nintendo DS title", |game| game.display_name())
    );
    println!("Revision: {}", rom.header.version);
    println!("Unit code: 0x{:02X}", rom.header.unit_code);
    println!(
        "ROM size: {} bytes ({})",
        rom.byte_len,
        human_size(rom.byte_len)
    );
    println!(
        "Header declared size: {} bytes ({})",
        rom.header.declared_rom_size,
        human_size(u64::from(rom.header.declared_rom_size))
    );
    println!(
        "Checksums: logo 0x{:04X} OK, header 0x{:04X} OK",
        rom.header.calculated_logo_crc16, rom.header.calculated_header_crc16
    );
    println!("Files: {}", rom.filesystem.files.len());
    println!(
        "Maker code: {}",
        String::from_utf8_lossy(&rom.header.maker_code)
    );
    println!("Header size: 0x{:X}", rom.header.declared_header_size);
    println!("Banner offset: 0x{:08X}", rom.header.banner_offset);
    for section in [
        rom.header.arm9,
        rom.header.arm7,
        rom.header.fnt,
        rom.header.fat,
        rom.header.arm9_overlay,
        rom.header.arm7_overlay,
    ] {
        println!(
            "{:<20} offset 0x{:08X}, size 0x{:08X}",
            section.name, section.offset, section.size
        );
    }
    Ok(())
}

/// The ROM argument of `run` and `boot`, and the arguments after it. The ROM
/// path may be left out for an install that keeps its own copy (`rom.nds`);
/// a number or an option in its place is taken as the next argument.
fn rom_argument<'a>(cache: &Path, rest: &'a [String]) -> Result<(PathBuf, &'a [String]), String> {
    match rest.first() {
        Some(first) if !first.starts_with("--") && first.parse::<u64>().is_err() => {
            Ok((PathBuf::from(first), &rest[1..]))
        }
        _ => {
            let copy = cache.join(gen5::install::ROM_FILE);
            if copy.is_file() {
                Ok((copy, rest))
            } else {
                Err(format!(
                    "no ROM given, and '{}' has no copy of it (pass the ROM's path, or install \
                     the game with `gen5 install`)",
                    cache.display()
                ))
            }
        }
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn print_status(status: &gen5::install::Status) {
    use gen5::install::State;
    println!("Install: {}", status.cache.display());
    if let (Some(game), Some(crc)) = (&status.game, &status.rom_crc32) {
        let name = status.game_name.unwrap_or(game.as_str());
        let code = status.game_code.as_deref().unwrap_or("?");
        println!("Game: {name} ({code}, ROM CRC-32 {crc})");
    }
    println!(
        "ROM copy: {}",
        if status.has_rom {
            "yes"
        } else {
            "no (`run` needs the ROM's path)"
        }
    );
    println!(
        "Built with: {}",
        status.built_with.as_deref().unwrap_or("unknown")
    );
    println!("This gen5: {}", status.current);
    println!(
        "State: {}",
        match status.state {
            State::Missing => "not installed (run `gen5 install <rom.nds>`)",
            State::NotBuilt => "imported but not built (run `gen5 update <install>`)",
            State::Outdated => "built by another gen5 (run `gen5 update <install>`)",
            State::Ready => "ready to play",
        }
    );
}

/// `install <rom.nds> [--data-dir=<dir>] [--cc=<compiler>] [--json]` and
/// `update <install> [--cc=<compiler>] [--json]`.
fn install_command(args: &[String]) -> Result<(), String> {
    use gen5::install::{self, Event};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let updating = args[0] == "update";
    let usage = || {
        format!(
            "Usage: {}\n\nRun `gen5 help {}` for more.",
            help::command(&args[0]).unwrap().usage,
            args[0]
        )
    };
    let mut data_dir = PathBuf::from(".gen5-cache");
    let mut compiler = "gcc".to_owned();
    let mut json = false;
    let mut target = None;
    for arg in &args[1..] {
        if let Some(value) = arg.strip_prefix("--data-dir=").filter(|_| !updating) {
            data_dir = PathBuf::from(value);
        } else if let Some(value) = arg.strip_prefix("--cc=") {
            compiler = value.to_owned();
        } else if arg == "--json" {
            json = true;
        } else if !arg.starts_with("--") && target.is_none() {
            target = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unexpected argument '{arg}'\n\n{}", usage()));
        }
    }
    let Some(target) = target else {
        return Err(format!("wrong arguments for '{}'\n\n{}", args[0], usage()));
    };
    let cancel: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    if json {
        // The launcher asks to stop by writing `cancel` on a line of stdin.
        std::thread::spawn(move || {
            for line in std::io::stdin().lines() {
                match line {
                    Ok(line) if line.trim() == "cancel" => cancel.store(true, Ordering::Relaxed),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    }
    let shown = AtomicUsize::new(usize::MAX);
    let report = |event: Event| {
        if json {
            if let Ok(line) = serde_json::to_string(&event) {
                println!("{line}");
            }
            return;
        }
        match event {
            Event::Step {
                number,
                steps,
                title,
                ..
            } => println!("[{number}/{steps}] {title}"),
            Event::Progress { done, total, .. } => {
                // Every tenth, and the end.
                let tenth = done * 10 / total.max(1);
                if shown.swap(tenth, Ordering::Relaxed) != tenth {
                    println!("      {done} of {total} files");
                }
            }
        }
    };
    let options = install::Options {
        compiler: &compiler,
        cancel,
        report: &report,
    };
    let started = std::time::Instant::now();
    let result = if updating {
        install::update(&target, &options).map(|()| target.clone())
    } else {
        install::install(&target, &data_dir, &options)
    };
    match result {
        Ok(cache) => {
            let status = install::status(&cache);
            if json {
                let mut value = serde_json::to_value(&status).map_err(|error| error.to_string())?;
                value["event"] = "finished".into();
                print_json(&value)
            } else {
                println!(
                    "Done in {:.0}s. Play with: gen5 run \"{}\"",
                    started.elapsed().as_secs_f32(),
                    cache.display()
                );
                Ok(())
            }
        }
        Err(error) if install::is_cancelled(&error) => {
            if json {
                print_json(&serde_json::json!({ "event": "cancelled" }))?;
            } else {
                eprintln!("cancelled");
            }
            std::process::exit(2)
        }
        Err(error) if json => {
            print_json(&serde_json::json!({ "event": "failed", "message": error }))?;
            std::process::exit(1)
        }
        Err(error) => Err(error),
    }
}

/// GEN5_WIDESCREEN=1: draw the top screen 16:9.
fn widescreen_from_env() -> bool {
    std::env::var("GEN5_WIDESCREEN").is_ok_and(|value| value == "1")
}

/// GEN5_SCALE: the resolution multiple to draw at (default 1).
fn scale_from_env() -> Result<u32, String> {
    match std::env::var("GEN5_SCALE") {
        Ok(value) => parse_scale(&value)
            .ok_or_else(|| format!("invalid GEN5_SCALE {value}: expected 1 to {MAX_SCALE}")),
        Err(_) => Ok(1),
    }
}

/// The scale a `run` argument gives, as `--scale=N` or a bare `N`; `None`
/// for any other option, so a mistyped one is reported as unknown.
fn scale_argument(value: &str) -> Option<&str> {
    value
        .strip_prefix("--scale=")
        .or_else(|| (!value.starts_with('-')).then_some(value))
}

/// A resolution multiple, 1 to `MAX_SCALE`.
fn parse_scale(text: &str) -> Option<u32> {
    text.parse()
        .ok()
        .filter(|scale| (1..=MAX_SCALE).contains(scale))
}

/// GEN5_FPS: how often the game window draws (default `ds`, once per game
/// frame); `display`, `uncapped` or a number interpolate 3D in between.
fn fps_from_env() -> Result<gen5::runtime::pacing::Fps, String> {
    match std::env::var("GEN5_FPS") {
        Ok(value) => gen5::runtime::pacing::Fps::parse(&value),
        Err(_) => Ok(gen5::runtime::pacing::Fps::Ds),
    }
}

fn parse_input_file(text: &str) -> Result<Vec<(std::ops::RangeInclusive<u64>, u32)>, String> {
    let entries: Vec<_> = text
        .lines()
        .map(|line| line.split('#').next().unwrap().trim())
        .filter(|line| !line.is_empty())
        .collect();
    parse_input_script(&entries.join(","))
}

fn parse_input_script(spec: &str) -> Result<Vec<(std::ops::RangeInclusive<u64>, u32)>, String> {
    use gen5::nds::input;
    spec.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let invalid = || format!("invalid GEN5_INPUT entry '{entry}'");
            let (frames, buttons) = entry.split_once(':').ok_or_else(invalid)?;
            let (first, last) = frames.split_once('-').unwrap_or((frames, frames));
            let first = first.trim().parse::<u64>().map_err(|_| invalid())?;
            let last = last.trim().parse::<u64>().map_err(|_| invalid())?;
            if first == 0 || last < first {
                return Err(invalid());
            }
            let mut held = 0;
            for name in buttons.split('+') {
                held |= match name.trim().to_ascii_lowercase().as_str() {
                    "a" => input::A,
                    "b" => input::B,
                    "x" => input::X,
                    "y" => input::Y,
                    "l" => input::L,
                    "r" => input::R,
                    "start" => input::START,
                    "select" => input::SELECT,
                    "up" => input::UP,
                    "down" => input::DOWN,
                    "left" => input::LEFT,
                    "right" => input::RIGHT,
                    _ => return Err(invalid()),
                };
            }
            Ok((first..=last, held))
        })
        .collect()
}

/// `first-last:x:y,...`; coordinates are pixels on the 256 x 192 touch screen.
fn parse_touch_script(
    spec: &str,
) -> Result<Vec<(std::ops::RangeInclusive<u64>, (u8, u8))>, String> {
    spec.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let invalid = || format!("invalid GEN5_TOUCH entry '{entry}'");
            let mut parts = entry.split(':');
            let frames = parts.next().ok_or_else(invalid)?;
            let (first, last) = frames.split_once('-').unwrap_or((frames, frames));
            let first = first.trim().parse::<u64>().map_err(|_| invalid())?;
            let last = last.trim().parse::<u64>().map_err(|_| invalid())?;
            let x = parts
                .next()
                .ok_or_else(invalid)?
                .trim()
                .parse::<u8>()
                .map_err(|_| invalid())?;
            let y = parts
                .next()
                .ok_or_else(invalid)?
                .trim()
                .parse::<u8>()
                .map_err(|_| invalid())?;
            if first == 0 || last < first || y >= 192 || parts.next().is_some() {
                return Err(invalid());
            }
            Ok((first..=last, (x, y)))
        })
        .collect()
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    #[test]
    fn run_scales_are_checked_and_other_options_are_not_scales() {
        assert_eq!(scale_argument("3"), Some("3"));
        assert_eq!(scale_argument("--scale=4"), Some("4"));
        assert_eq!(scale_argument("--contine"), None);
        assert_eq!(scale_argument("-x"), None);
        assert_eq!(parse_scale("8"), Some(8));
        assert_eq!(parse_scale("0"), None);
        assert_eq!(parse_scale("99"), None);
        assert_eq!(parse_scale("big"), None);
    }

    #[test]
    fn replay_ranges_and_touch_coordinates_are_validated() {
        assert_eq!(
            parse_input_file("# comment\n10-15:a # hold\n\n").unwrap(),
            vec![(10..=15, gen5::nds::input::A)]
        );
        assert!(parse_input_script("").unwrap().is_empty());
        assert!(parse_input_script("20-10:a").is_err());
        assert!(parse_input_script("0:a").is_err());
        assert!(parse_input_script("10:unknown").is_err());
        assert_eq!(
            parse_touch_script("10-15:255:191").unwrap(),
            vec![(10..=15, (255, 191))]
        );
        for invalid in ["0:0:0", "15-10:0:0", "10:256:0", "10:0:192", "10:0:0:1"] {
            assert!(parse_touch_script(invalid).is_err(), "{invalid}");
        }
    }
}

fn parse_address(text: &str) -> Result<u32, String> {
    let digits = text.trim_start_matches("0x").trim_start_matches("0X");
    u32::from_str_radix(digits, 16).map_err(|_| format!("invalid hexadecimal address '{text}'"))
}

fn run_analysis(cache_root: &Path) -> Result<(Program, Analysis), String> {
    let program = Program::load(&cache_root.join("code"))?;
    let started = std::time::Instant::now();
    let analysis = recomp_analysis::analyze(&program);
    tracing::info!(
        "analyzed {} functions in {} rounds ({:.1}s)",
        analysis.functions.len(),
        analysis.rounds,
        started.elapsed().as_secs_f32()
    );
    Ok((program, analysis))
}

fn analyze(cache_root: &Path) -> Result<(), String> {
    let (program, analysis) = run_analysis(cache_root)?;
    let functions = analysis.functions.values();
    let count = |predicate: &dyn Fn(&recomp_analysis::Function) -> usize| {
        analysis.functions.values().map(predicate).sum::<usize>()
    };
    let thumb = functions.clone().filter(|function| function.thumb).count();
    println!(
        "Functions: {} ({thumb} Thumb, {} ARM)",
        analysis.functions.len(),
        analysis.functions.len() - thumb
    );
    let mut by_source = std::collections::BTreeMap::new();
    for function in functions.clone() {
        for source in &function.sources {
            *by_source.entry(format!("{source:?}")).or_insert(0) += 1;
        }
    }
    println!("Found via: {by_source:?}");
    println!(
        "Jump tables: {}, unresolved indirect jumps: {}, indirect calls: {}",
        count(&|function| function.jump_tables.len()),
        count(&|function| function.indirect_jumps.len()),
        count(&|function| function.indirect_calls.len())
    );
    let calls = || functions.clone().flat_map(|function| function.calls.iter());
    println!(
        "Calls: {}, ambiguous between overlays: {}, outside loaded code: {}",
        calls().count(),
        calls()
            .filter(|call| matches!(call.resolution, Resolution::Ambiguous(_)))
            .count(),
        calls()
            .filter(|call| call.resolution == Resolution::Outside)
            .count()
    );
    let problems = functions
        .clone()
        .filter(|function| !function.problems.is_empty())
        .count();
    println!(
        "Functions with problems: {problems}, ARM/Thumb conflicts: {}",
        analysis.state_conflicts.len()
    );

    println!("Coverage (bytes decoded as code):");
    let mut overlay_total = (0_u64, 0_u64, 0_usize);
    for module in &program.modules {
        let code = recomp_analysis::module_code_bytes(&analysis, module.id);
        let size = module.bytes.len() as u64;
        let function_count = functions
            .clone()
            .filter(|function| function.module == module.id)
            .count();
        if let ModuleId::Overlay(_) = module.id {
            overlay_total.0 += u64::from(code);
            overlay_total.1 += size;
            overlay_total.2 += function_count;
            continue;
        }
        println!(
            "  {:<10} 0x{:08X} {:>8} bytes, {:>5} functions, {:5.1}% code",
            module.id.to_string(),
            module.base,
            size,
            function_count,
            100.0 * f64::from(code) / size.max(1) as f64
        );
    }
    println!(
        "  overlays   {:>19} bytes, {:>5} functions, {:5.1}% code",
        overlay_total.1,
        overlay_total.2,
        100.0 * overlay_total.0 as f64 / overlay_total.1.max(1) as f64
    );

    let output_dir = cache_root.join("analysis");
    std::fs::create_dir_all(&output_dir)
        .map_err(|error| format!("could not create '{}': {error}", output_dir.display()))?;
    let output = output_dir.join("functions.json");
    let file = std::fs::File::create(&output)
        .map_err(|error| format!("could not create '{}': {error}", output.display()))?;
    let list: Vec<_> = analysis.functions.values().collect();
    serde_json::to_writer(std::io::BufWriter::new(file), &list)
        .map_err(|error| format!("could not write '{}': {error}", output.display()))?;
    println!("Wrote {}", output.display());
    Ok(())
}

fn codegen(cache_root: &Path) -> Result<(), String> {
    let (program, analysis) = run_analysis(cache_root)?;
    let out_dir = cache_root.join("recomp");
    let report = gen5::recomp::codegen::generate(&program, &analysis, &out_dir)?;
    println!(
        "Generated {} functions into {} files ({}) at {}",
        report.functions,
        report.files,
        human_size(report.bytes as u64),
        out_dir.display()
    );
    Ok(())
}

fn build(cache_root: &Path, compiler: &str) -> Result<(), String> {
    let started = std::time::Instant::now();
    let report = gen5::recomp::compile::build_library(&cache_root.join("recomp"), compiler)?;
    println!(
        "Built {} ({} files compiled, {} reused) in {:.0}s",
        report.library.display(),
        report.compiled,
        report.reused,
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn disasm(cache_root: &Path, module: ModuleId, address: u32) -> Result<(), String> {
    let (program, analysis) = run_analysis(cache_root)?;
    let Some(function) = analysis
        .functions
        .values()
        .filter(|function| function.module == module)
        .find(|function| {
            function.address == address & !1 || function.instructions.contains_key(&address)
        })
    else {
        // Not part of a discovered function: decode linearly, in Thumb
        // when bit 0 of the address is set.
        let code = program
            .module(module)
            .ok_or_else(|| format!("no module {module}"))?;
        println!("{module}:0x{address:08X} is not in a discovered function; linear decode:");
        let thumb = address & 1 != 0;
        let mut at = address & !1;
        for _ in 0..48 {
            let Some(ins) = recomp_analysis::decode_at(code, at, thumb) else {
                break;
            };
            println!("  {:08X}: {ins}", ins.address);
            at = ins.end();
        }
        return Ok(());
    };
    println!(
        "{}:0x{:08X} ({}, {} instructions, found via {:?})",
        function.module,
        function.address,
        if function.thumb { "Thumb" } else { "ARM" },
        function.instruction_count,
        function.sources
    );
    for problem in &function.problems {
        println!("  ; problem: {problem}");
    }
    let targets: std::collections::HashSet<u32> = function
        .instructions
        .values()
        .filter_map(|ins| match ins.op {
            gen5::recomp::arm::Op::Branch {
                target,
                link: false,
                ..
            } => Some(target),
            _ => None,
        })
        .chain(
            function
                .jump_tables
                .iter()
                .flat_map(|table| table.targets.iter().copied()),
        )
        .collect();
    let mut previous_end = None;
    for ins in function.instructions.values() {
        if previous_end.is_some_and(|end| end != ins.address) {
            println!("  ; gap");
        }
        if targets.contains(&ins.address) {
            println!("loc_{:08X}:", ins.address);
        }
        let raw = if ins.size == 2 {
            format!("{:04X}    ", ins.raw)
        } else if ins.thumb {
            format!("{:04X}{:04X}", ins.raw & 0xFFFF, ins.raw >> 16)
        } else {
            format!("{:08X}", ins.raw)
        };
        let mut note = String::new();
        if let gen5::recomp::arm::Op::Branch {
            target, link: true, ..
        } = ins.op
            && let Some(call) = function.calls.iter().find(|call| call.from == ins.address)
        {
            note = match &call.resolution {
                Resolution::Module(module) => format!(" ; {module}:0x{target:08X}"),
                Resolution::Ambiguous(modules) => format!(" ; ambiguous: {modules:?}"),
                Resolution::Outside => " ; outside loaded code".to_owned(),
            };
        }
        println!("  {:08X}: {raw}  {ins}{note}", ins.address);
        previous_end = Some(ins.end());
    }
    for table in &function.jump_tables {
        println!(
            "  ; jump table at 0x{:08X} (from 0x{:08X}): {} entries",
            table.table,
            table.jump,
            table.targets.len()
        );
    }
    Ok(())
}

fn code_info(path: &Path) -> Result<(), String> {
    let mut rom = NdsRom::open(path)?;
    let arm9 = rom.arm9_image()?;
    println!(
        "ARM9: load 0x{:08X}, entry 0x{:08X}, {} bytes ({}), SDK 0x{:08X}",
        arm9.load_address,
        arm9.entry,
        arm9.bytes.len(),
        if arm9.was_compressed {
            "BLZ-compressed in ROM"
        } else {
            "stored uncompressed"
        },
        arm9.params.sdk_version
    );
    println!(
        "  static BSS 0x{:08X}..0x{:08X}",
        arm9.params.static_bss_start, arm9.params.static_bss_end
    );
    for block in &arm9.autoloads {
        println!(
            "  autoload 0x{:08X}: {} bytes + {} BSS",
            block.ram_address, block.size, block.bss_size
        );
    }
    println!(
        "ARM7: load 0x{:08X}, entry 0x{:08X}, {} bytes",
        rom.header.arm7_load, rom.header.arm7_entry, rom.header.arm7.size
    );
    for cpu in [Cpu::Arm9, Cpu::Arm7] {
        let overlays = rom.overlay_table(cpu)?;
        println!("{cpu:?} overlays: {}", overlays.len());
        for entry in &overlays {
            let image = rom.overlay_image(entry)?;
            println!(
                "  {:4} file {:4}: 0x{:08X}..0x{:08X} ({} bytes{}, {} BSS)",
                entry.id,
                entry.file_id,
                entry.ram_address,
                entry.ram_end(),
                image.len(),
                if entry.compressed { ", BLZ" } else { "" },
                entry.bss_size
            );
        }
    }
    Ok(())
}

fn list(path: &Path, prefix: &str) -> Result<(), String> {
    let rom = NdsRom::open(path)?;
    let files = rom.files_under(prefix);
    if files.is_empty() {
        return Err(format!("no ROM files found under path '{prefix}'"));
    }
    for file in files {
        println!(
            "{:>5}  {:>10}  0x{:08X}  {}",
            file.id, file.size, file.offset, file.path
        );
    }
    Ok(())
}

fn import(path: &Path, cache_path: &Path) -> Result<(), String> {
    let rom = NdsRom::open(path)?;
    let report = importer::import_rom(rom, cache_path)?;
    println!("Imported {}", report.game.display_name());
    println!("ROM CRC-32: {:08X}", report.rom_crc32);
    println!("Cache: {}", report.cache_path.display());
    if report.files_written == 0 {
        println!("Already imported; nothing was written");
    } else {
        println!(
            "Wrote {} original files ({}) to {}",
            report.files_written,
            human_size(report.bytes_written),
            report.cache_path.join("romfs").display()
        );
        println!(
            "Manifest: {}",
            report.cache_path.join("manifest.json").display()
        );
    }
    println!("Next: gen5 analyze \"{}\"", report.cache_path.display());
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}
