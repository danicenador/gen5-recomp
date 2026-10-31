use gen5::formats::nds::{Cpu, NdsRom};
use gen5::game::bw2::field;
use gen5::game::bw2::map;
use gen5::game::gen5 as gen5_game;
use gen5::importer;
use gen5::recomp::analysis::{self as recomp_analysis, Analysis};
use gen5::recomp::program::{ModuleId, Program, Resolution};
use gen5::render;
use gen5::runtime;
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
        Some("boot") if args.len() == 3 || args.len() == 4 => {
            let max_frames = args
                .get(3)
                .map(|value| {
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid frame count '{value}'"))
                })
                .transpose()?
                .unwrap_or(60);
            gen5::nds::machine::boot(
                Path::new(&args[1]),
                Path::new(&args[2]),
                gen5::nds::machine::Options {
                    max_frames,
                    seconds: 20,
                },
            )
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
        Some("map-preview") if (2..=4).contains(&args.len()) => {
            let zone_id = args
                .get(2)
                .map(|value| {
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid zone index '{value}'"))
                })
                .transpose()?
                .unwrap_or(map::ASPERTIA_CITY_ZONE);
            let cache_root = Path::new(&args[1]);
            let output = args.get(3).map_or_else(
                || {
                    cache_root
                        .join("previews")
                        .join(format!("zone-{zone_id}.svg"))
                },
                PathBuf::from,
            );
            map_preview(cache_root, zone_id, &output)
        }
        Some("walk") if (2..=5).contains(&args.len()) => {
            if args.len() == 4 {
                print_usage();
                return Err("provide both start coordinates or neither".to_owned());
            }
            let zone_id = args
                .get(2)
                .map(|value| {
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid zone index '{value}'"))
                })
                .transpose()?
                .unwrap_or(map::ASPERTIA_CITY_ZONE);
            let start = if args.len() == 5 {
                let x = args[3]
                    .parse::<usize>()
                    .map_err(|_| format!("invalid start x coordinate '{}'", args[3]))?;
                let z = args[4]
                    .parse::<usize>()
                    .map_err(|_| format!("invalid start z coordinate '{}'", args[4]))?;
                Some((x, z))
            } else {
                None
            };
            field_walk(Path::new(&args[1]), zone_id, start)
        }
        Some("play") if (2..=5).contains(&args.len()) => {
            if args.len() == 4 {
                print_usage();
                return Err("provide both start coordinates or neither".to_owned());
            }
            let zone_id = args
                .get(2)
                .map(|value| {
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid zone index '{value}'"))
                })
                .transpose()?
                .unwrap_or(map::ASPERTIA_CITY_ZONE);
            let start = if args.len() == 5 {
                let x = args[3]
                    .parse::<usize>()
                    .map_err(|_| format!("invalid start x coordinate '{}'", args[3]))?;
                let z = args[4]
                    .parse::<usize>()
                    .map_err(|_| format!("invalid start z coordinate '{}'", args[4]))?;
                Some((x, z))
            } else {
                None
            };
            field_play(Path::new(&args[1]), zone_id, start)
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
    let (_, analysis) = run_analysis(cache_root)?;
    let function = analysis
        .functions
        .values()
        .filter(|function| function.module == module)
        .find(|function| {
            function.address == address || function.instructions.contains_key(&address)
        })
        .ok_or_else(|| format!("no discovered function in {module} contains 0x{address:08X}"))?;
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

fn map_preview(cache_root: &Path, zone_id: u32, output: &Path) -> Result<(), String> {
    let zone = map::load_zone(cache_root, zone_id)?;
    let preview = render::render_map_svg(cache_root, &zone, output)?;
    println!("Rendered {}", zone.display_name());
    println!(
        "Zone {}, area {}, matrix {} ({}×{}), map models: {}",
        zone.zone_id,
        zone.area_id,
        zone.matrix_id,
        zone.width,
        zone.height,
        preview
            .model_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("Geometry: {} triangles", preview.triangle_count);
    println!("Preview: {}", preview.output.display());
    Ok(())
}

fn field_walk(
    cache_root: &Path,
    zone_id: u32,
    start: Option<(usize, usize)>,
) -> Result<(), String> {
    let zone = map::load_zone(cache_root, zone_id)?;
    field::run_terminal(cache_root, zone, start)
}

fn field_play(
    cache_root: &Path,
    zone_id: u32,
    start: Option<(usize, usize)>,
) -> Result<(), String> {
    let zone = map::load_zone(cache_root, zone_id)?;
    runtime::run(cache_root, zone, start)
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
        "gen5 — native Pokémon Gen V ROM explorer/importer\n\n\
         Usage:\n\
         gen5 inspect <rom.nds>\n\
         gen5 list <rom.nds> [path-prefix]\n\
         gen5 code-info <rom.nds>\n\
         gen5 analyze <import-cache-directory>\n\
         gen5 codegen <import-cache-directory>\n\
         gen5 build <import-cache-directory> [c-compiler]\n\
         gen5 boot <import-cache-directory> <rom.nds> [frames]\n\
         gen5 disasm <import-cache-directory> <main|autoloadN|ovN> <address>\n\
         gen5 import <rom.nds> [cache-directory]\n\
         gen5 map-preview <import-cache-directory> [zone-index] [output.svg]\n\
         gen5 play <import-cache-directory> [zone-index [start-x start-z]]\n\
         gen5 walk <import-cache-directory> [zone-index [start-x start-z]]\n\
         gen5 help"
    );
}
