use gen5::formats::nds::{Cpu, NdsRom};
use gen5::game::bw2::field;
use gen5::game::bw2::map;
use gen5::game::gen5 as gen5_game;
use gen5::importer;
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
         gen5 import <rom.nds> [cache-directory]\n\
         gen5 map-preview <import-cache-directory> [zone-index] [output.svg]\n\
         gen5 play <import-cache-directory> [zone-index [start-x start-z]]\n\
         gen5 walk <import-cache-directory> [zone-index [start-x start-z]]\n\
         gen5 help"
    );
}
