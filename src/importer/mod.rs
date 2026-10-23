use crate::Result;
use crate::formats::nds::{NdsRom, RomFile};
use crate::game::gen5::{self, Gen5Title};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct ImportReport {
    pub cache_path: PathBuf,
    pub game: Gen5Title,
    pub files_written: usize,
    pub bytes_written: u64,
    pub rom_crc32: u32,
}

/// Import all files addressed by the NDS FNT/FAT tables into an immutable cache.
pub fn import_rom(mut rom: NdsRom, cache_root: &Path) -> Result<ImportReport> {
    let game = gen5::identify(&rom.header.game_code).ok_or_else(|| {
        format!(
            "game code '{}' is not a recognized Pokémon Gen V retail title",
            String::from_utf8_lossy(&rom.header.game_code)
        )
    })?;
    let rom_crc32 = rom.crc32()?;
    let final_path = cache_root
        .join(game.cache_key())
        .join(format!("{rom_crc32:08X}"));

    if final_path.exists() {
        let manifest = final_path.join("manifest.json");
        if manifest.is_file() {
            let manifest_text = fs::read_to_string(&manifest).map_err(|error| {
                format!(
                    "could not read existing cache manifest '{}': {error}",
                    manifest.display()
                )
            })?;
            let expected_crc = format!("\"rom_crc32\": \"{rom_crc32:08X}\"");
            if manifest_text.contains("\"format\": \"gen5-rom-cache-v1\"")
                && manifest_text.contains(&expected_crc)
                && final_path.join("romfs").is_dir()
            {
                return Ok(ImportReport {
                    cache_path: final_path,
                    game,
                    files_written: 0,
                    bytes_written: 0,
                    rom_crc32,
                });
            }
            return Err(format!(
                "cache destination '{}' has a manifest for a different or incomplete import",
                final_path.display()
            ));
        }
        return Err(format!(
            "cache destination '{}' already exists without a manifest; move it aside before importing",
            final_path.display()
        ));
    }

    let parent = final_path
        .parent()
        .ok_or_else(|| "cache path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create cache directory '{}': {error}",
            parent.display()
        )
    })?;
    let staging_root = cache_root.join(".staging");
    fs::create_dir_all(&staging_root).map_err(|error| {
        format!(
            "could not create cache staging directory '{}': {error}",
            staging_root.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging_path = staging_root.join(format!(
        "{}-{rom_crc32:08X}-{}-{nonce}",
        game.cache_key(),
        std::process::id()
    ));
    fs::create_dir(&staging_path).map_err(|error| {
        format!(
            "could not create staging directory '{}': {error}",
            staging_path.display()
        )
    })?;

    let result = write_cache(&mut rom, game, rom_crc32, &staging_path);
    let (files_written, bytes_written) = match result {
        Ok(counts) => counts,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_path);
            return Err(error);
        }
    };
    fs::rename(&staging_path, &final_path).map_err(|error| {
        let _ = fs::remove_dir_all(&staging_path);
        format!(
            "could not publish cache at '{}': {error}",
            final_path.display()
        )
    })?;

    Ok(ImportReport {
        cache_path: final_path,
        game,
        files_written,
        bytes_written,
        rom_crc32,
    })
}

fn write_cache(
    rom: &mut NdsRom,
    game: Gen5Title,
    rom_crc32: u32,
    staging_path: &Path,
) -> Result<(usize, u64)> {
    let content_root = staging_path.join("romfs");
    fs::create_dir_all(&content_root)
        .map_err(|error| format!("could not create extracted file root: {error}"))?;

    let files = rom.filesystem.files.clone();
    let mut manifest_entries = Vec::with_capacity(files.len());
    let mut bytes_written = 0_u64;
    for file in &files {
        let output_path = content_root.join(&file.path);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create extracted directory '{}': {error}",
                    parent.display()
                )
            })?;
        }
        let mut output = File::create(&output_path).map_err(|error| {
            format!(
                "could not create extracted file '{}': {error}",
                output_path.display()
            )
        })?;
        let file_crc32 = copy_file(rom, file, &mut output)?;
        manifest_entries.push(format!(
            "    {{\"id\":{},\"path\":{},\"size\":{},\"crc32\":\"{file_crc32:08X}\"}}",
            file.id,
            json_string(&file.path),
            file.size
        ));
        bytes_written += u64::from(file.size);
    }

    let manifest_path = staging_path.join("manifest.json");
    let mut manifest = File::create(&manifest_path)
        .map_err(|error| format!("could not create cache manifest: {error}"))?;
    writeln!(manifest, "{{").map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"format\": \"gen5-rom-cache-v1\",")
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"game\": {},", json_string(game.cache_key()))
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"title\": {},", json_string(&rom.header.title))
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(
        manifest,
        "  \"game_code\": {},",
        json_string(&String::from_utf8_lossy(&rom.header.game_code))
    )
    .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"rom_size\": {},", rom.byte_len)
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"rom_crc32\": \"{rom_crc32:08X}\",")
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    writeln!(manifest, "  \"files\": [")
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    for (index, entry) in manifest_entries.iter().enumerate() {
        let suffix = if index + 1 == manifest_entries.len() {
            ""
        } else {
            ","
        };
        writeln!(manifest, "{entry}{suffix}")
            .map_err(|error| format!("could not write cache manifest: {error}"))?;
    }
    writeln!(manifest, "  ]\n}}")
        .map_err(|error| format!("could not write cache manifest: {error}"))?;
    manifest
        .sync_all()
        .map_err(|error| format!("could not flush cache manifest: {error}"))?;

    Ok((files.len(), bytes_written))
}

fn copy_file(rom: &mut NdsRom, entry: &RomFile, output: &mut File) -> Result<u32> {
    let mut offset = u64::from(entry.offset);
    let mut remaining = entry.size as usize;
    let mut crc = crate::formats::nds::Crc32::new();
    const CHUNK_SIZE: usize = 1024 * 1024;
    while remaining > 0 {
        let count = remaining.min(CHUNK_SIZE);
        let bytes = rom.read_at(offset, count)?;
        output
            .write_all(&bytes)
            .map_err(|error| format!("could not write extracted file '{}': {error}", entry.path))?;
        crc.update(&bytes);
        offset += count as u64;
        remaining -= count;
    }
    output
        .sync_all()
        .map_err(|error| format!("could not flush extracted file '{}': {error}", entry.path))?;
    Ok(crc.finish())
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(&mut output, "\\u{:04X}", u32::from(character));
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}
