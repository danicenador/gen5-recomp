use gen5::formats::nds::{Cpu, NdsRom};
use gen5::importer::{self, title as gen5_game};
use gen5::recomp::analysis::{self as recomp_analysis, Analysis};
use gen5::recomp::program::{ModuleId, Program, Resolution};
use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("gen5=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("inspect") if args.len() == 2 => inspect(Path::new(&args[1])),
        Some("code-info") if args.len() == 2 => code_info(Path::new(&args[1])),
        Some("analyze") if args.len() == 2 => analyze(Path::new(&args[1])),
        Some("codegen") if args.len() == 2 => codegen(Path::new(&args[1])),
        Some("build") if args.len() == 2 || args.len() == 3 => {
            let compiler = args.get(2).map_or("gcc", String::as_str);
            build(Path::new(&args[1]), compiler)
        }
        Some("boot") if (3..=5).contains(&args.len()) => {
            let max_frames = args
                .get(3)
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
            let directory = args.get(4).map(std::path::PathBuf::from);
            // GEN5_SCALE: screenshots at a multiple of the DS resolution,
            // drawn by the GPU renderer.
            let scale = scale_from_env()?;
            let mut gpu: Option<gen5::runtime::scaled::Headless> = None;
            let on_frame = directory.map(|directory| {
                Box::new(move |number: u64, frame: &gen5::nds::gpu2d::Frame| {
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
                            match gpu.as_mut().unwrap().render(scaled, scale) {
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
                Path::new(&args[2]),
                gen5::nds::machine::Options {
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
                    gpu: false,
                },
            )
        }
        Some("run") if (3..=5).contains(&args.len()) => {
            let mut scale = None;
            let mut fps = None;
            for value in &args[3..] {
                if let Some(mode) = value.strip_prefix("--fps=") {
                    fps = Some(gen5::runtime::pacing::Fps::parse(mode)?);
                } else {
                    scale = Some(
                        value
                            .trim_start_matches("--scale=")
                            .parse()
                            .map_err(|_| format!("invalid scale: {value}"))?,
                    );
                }
            }
            let scale = match scale {
                Some(scale) => scale,
                None => scale_from_env()?,
            };
            let fps = match fps {
                Some(fps) => fps,
                None => fps_from_env()?,
            };
            gen5::runtime::play_recompiled(Path::new(&args[1]), Path::new(&args[2]), scale, fps)
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
        Some("help" | "--help" | "-h") if args.len() == 1 => {
            print_usage();
            Ok(())
        }
        _ => {
            print_usage();
            Err("invalid command or arguments".to_owned())
        }
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

/// `first-last:button+button,...` into frame ranges and held buttons.
/// GEN5_SCALE: the resolution multiple to draw at (default 1).
fn scale_from_env() -> Result<u32, String> {
    match std::env::var("GEN5_SCALE") {
        Ok(value) => match value.parse::<u32>() {
            Ok(scale @ 1..=8) => Ok(scale),
            _ => Err(format!("invalid GEN5_SCALE {value}: expected 1 to 8")),
        },
        Err(_) => Ok(1),
    }
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
    if report.files_written == 0 {
        println!("Cache already exists at {}", report.cache_path.display());
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

fn print_usage() {
    eprintln!(
        "gen5 — Pokémon Gen V ROM importer and static recompiler\n\n\
         Usage:\n\
         gen5 inspect <rom.nds>\n\
         gen5 list <rom.nds> [path-prefix]\n\
         gen5 code-info <rom.nds>\n\
         gen5 analyze <import-cache-directory>\n\
         gen5 codegen <import-cache-directory>\n\
         gen5 build <import-cache-directory> [c-compiler]\n\
         gen5 boot <import-cache-directory> <rom.nds> [frames [png-directory]]\n\
         gen5 run <import-cache-directory> <rom.nds> [scale] [--fps=ds|display|uncapped|N]\n\
         gen5 disasm <import-cache-directory> <main|autoloadN|ovN> <address>\n\
         gen5 import <rom.nds> [cache-directory]\n\
         gen5 help"
    );
}
