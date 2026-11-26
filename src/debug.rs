//! Isolated debug sessions and portable, ROM-bound in-game save checkpoints.
use crate::{Result, formats::nds::NdsRom};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
struct Metadata {
    format: String,
    rom_crc32: String,
    game_code: String,
    description: String,
    reproduction: String,
    source: Option<String>,
    frame: Option<u64>,
    status: String,
}

fn io(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// A path as printed and recorded: forward slashes only, since Windows paths
/// built with `join("debug/runs")` would otherwise mix both separators.
fn slashed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// The part of a checkpoint's source path that names the run, from `debug/runs/`
/// on, so committed checkpoints never carry the absolute path of a user's checkout.
fn portable_source(path: &Path) -> String {
    let path = slashed(path);
    if let Some(start) = path.find("debug/runs/") {
        return path[start..].into();
    }
    // Checked by hand: on Windows `/x` is not absolute, on Unix `C:/x` is not.
    if !path.starts_with('/') && path.get(1..2) != Some(":") {
        return path;
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    parts[parts.len().saturating_sub(3)..].join("/")
}

fn read_metadata(path: &Path) -> Result<Metadata> {
    let meta: Metadata =
        serde_json::from_slice(&fs::read(path.join("checkpoint.json")).map_err(io)?).map_err(io)?;
    if meta.format != "gen5-debug-save-v1" {
        return Err("unsupported checkpoint format".into());
    }
    Ok(meta)
}

fn write_metadata(path: &Path, meta: &Metadata) -> Result<()> {
    fs::write(
        path.join("checkpoint.json"),
        serde_json::to_vec_pretty(meta).map_err(io)?,
    )
    .map_err(io)
}

/// Create a destination exactly once. Never overwrite an existing checkpoint.
fn write_checkpoint(path: &Path, meta: &Metadata, save: &[u8], firmware: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    fs::create_dir(path).map_err(|error| format!("cannot create '{}': {error}", path.display()))?;
    fs::write(path.join("save.bin"), save).map_err(io)?;
    fs::write(path.join("firmware.bin"), firmware).map_err(io)?;
    write_metadata(path, meta)
}

fn read_state(path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let save = fs::read(path.join("save.bin")).map_err(io)?;
    let firmware = fs::read(path.join("firmware.bin")).map_err(io)?;
    if save.len() != 512 * 1024 || firmware.len() != 256 * 1024 {
        return Err("checkpoint requires a 512 KiB save and 256 KiB firmware".into());
    }
    Ok((save, firmware))
}

fn create_session(state: &Path, meta: &Metadata, checkpoint: Option<&Path>) -> Result<()> {
    let seed = checkpoint
        .map(|path| {
            let seed_meta = read_metadata(path)?;
            if seed_meta.rom_crc32 != meta.rom_crc32 || seed_meta.game_code != meta.game_code {
                return Err("checkpoint belongs to a different ROM".into());
            }
            read_state(path)
        })
        .transpose()?;
    fs::create_dir_all(state.parent().ok_or("session needs a parent directory")?).map_err(io)?;
    fs::create_dir(state).map_err(io)?;
    write_metadata(state, meta)?;
    if let Some((save, firmware)) = seed {
        write_checkpoint(&state.join("history/initial"), meta, &save, &firmware)?;
        fs::write(state.join("save.bin"), save).map_err(io)?;
        fs::write(state.join("firmware.bin"), firmware).map_err(io)?;
    }
    Ok(())
}

/// `debug run|boot <cache> <rom> [--checkpoint=<directory>] [normal arguments]`.
/// A child process supplies isolated environment variables without changing the
/// parent's environment or the window/runtime interfaces.
pub fn launch(args: &[String]) -> Result<()> {
    if args.len() < 3 || !matches!(args[0].as_str(), "run" | "boot") {
        return Err(
            "usage: debug run|boot <cache> <rom> [--checkpoint=<directory>] [arguments]".into(),
        );
    }
    let mut checkpoint = None;
    let mut forwarded = Vec::new();
    for arg in args {
        if let Some(path) = arg.strip_prefix("--checkpoint=") {
            if path.is_empty() || checkpoint.replace(PathBuf::from(path)).is_some() {
                return Err("provide exactly one nonempty --checkpoint directory".into());
            }
        } else {
            forwarded.push(arg);
        }
    }
    let cache = Path::new(&args[1]);
    let mut rom = NdsRom::open(Path::new(&args[2]))?;
    let crc = format!("{:08X}", rom.crc32()?);
    let cache_meta: serde_json::Value =
        serde_json::from_slice(&fs::read(cache.join("manifest.json")).map_err(io)?).map_err(io)?;
    if cache_meta["rom_crc32"].as_str() != Some(crc.as_str()) {
        return Err("debug ROM does not match the imported cache".into());
    }
    let meta = Metadata {
        format: "gen5-debug-save-v1".into(),
        rom_crc32: crc,
        game_code: String::from_utf8_lossy(&rom.header.game_code).into_owned(),
        description: "Debug session".into(),
        reproduction: String::new(),
        source: checkpoint.as_deref().map(slashed),
        frame: None,
        status: "working-copy".into(),
    };
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io)?
        .as_nanos();
    let state = std::env::current_dir()
        .map_err(io)?
        .join("debug/runs")
        .join(format!("{id}-{}", std::process::id()));
    create_session(&state, &meta, checkpoint.as_deref())?;
    println!("Debug state: {}", slashed(&state));
    let status = Command::new(std::env::current_exe().map_err(io)?)
        .args(forwarded)
        .env("GEN5_STATE_DIR", &state)
        .env("GEN5_DEBUG_HISTORY", state.join("history"))
        .status()
        .map_err(io)?;
    if !status.success() {
        return Err(format!("debug game process exited with {status}"));
    }
    Ok(())
}

/// Promote a stopped session or immutable history entry to a named checkpoint.
pub fn promote(
    source: &Path,
    destination: &Path,
    description: &str,
    reproduction: &str,
) -> Result<()> {
    if description.trim().is_empty() || reproduction.trim().is_empty() {
        return Err("checkpoint description and reproduction steps must be nonempty".into());
    }
    let mut meta = read_metadata(source)?;
    let (save, firmware) = read_state(source)?;
    meta.description = description.into();
    meta.reproduction = reproduction.into();
    meta.source = Some(portable_source(source));
    meta.status = "manual-checkpoint; verify by loading Continue".into();
    write_checkpoint(destination, &meta, &save, &firmware)
}

/// Flash writes are not whole game saves. Wait for two seconds of DS frames
/// without writes; these candidates still need an in-game reload check.
pub struct History {
    root: PathBuf,
    meta: Metadata,
    last_write: Option<u64>,
    previous: Vec<u8>,
}

impl History {
    pub fn open(root: PathBuf, initial: &[u8]) -> Result<Self> {
        let meta = read_metadata(root.parent().ok_or("history needs a parent directory")?)?;
        Ok(Self {
            root,
            meta,
            last_write: None,
            previous: initial.to_vec(),
        })
    }
    pub fn changed(&mut self, frame: u64) {
        self.last_write = Some(frame);
    }
    pub fn capture(&mut self, frame: u64, save: &[u8], firmware: &[u8]) -> Result<()> {
        let Some(last) = self.last_write else {
            return Ok(());
        };
        if frame.saturating_sub(last) < 120 {
            return Ok(());
        }
        if save != self.previous {
            let mut meta = self.meta.clone();
            meta.frame = Some(frame);
            meta.status = "unverified; 120 frames without flash writes".into();
            let path = self.root.join(format!("frame-{frame:010}"));
            write_checkpoint(&path, &meta, save, firmware)?;
            eprintln!("Debug save candidate: {}", slashed(&path));
            self.previous = save.to_vec();
        }
        self.last_write = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/debug-save-tests")
            .join(format!("{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn metadata() -> Metadata {
        Metadata {
            format: "gen5-debug-save-v1".into(),
            rom_crc32: "95AB5437".into(),
            game_code: "IREO".into(),
            description: "Test".into(),
            reproduction: "Continue".into(),
            source: None,
            frame: None,
            status: "test".into(),
        }
    }

    #[test]
    fn printed_paths_use_forward_slashes() {
        assert_eq!(
            slashed(Path::new(
                r"D:\gen5\debug/runs\1-2\history\frame-0000084621"
            )),
            "D:/gen5/debug/runs/1-2/history/frame-0000084621"
        );
    }

    #[test]
    fn checkpoint_sources_keep_only_the_run() {
        assert_eq!(
            portable_source(Path::new(
                r"D:\gen5\debug\runs\1-2\history\frame-0000084621"
            )),
            "debug/runs/1-2/history/frame-0000084621"
        );
        assert_eq!(
            portable_source(Path::new("debug/runs/1-2/history/frame-0000084621")),
            "debug/runs/1-2/history/frame-0000084621"
        );
        assert_eq!(
            portable_source(Path::new("/elsewhere/1-2/history/frame-0000084621")),
            "1-2/history/frame-0000084621"
        );
    }

    #[test]
    fn history_waits_after_last_write_and_keeps_distinct_images() {
        let root = directory();
        write_metadata(&root, &metadata()).unwrap();
        let history_path = root.join("history");
        let mut history = History::open(history_path.clone(), &[0]).unwrap();
        history.changed(10);
        history.capture(129, &[1], &[2]).unwrap();
        assert!(!history_path.exists());
        history.changed(129);
        history.capture(130, &[1], &[2]).unwrap();
        assert!(!history_path.exists());
        history.capture(249, &[1], &[2]).unwrap();
        let first = history_path.join("frame-0000000249");
        assert_eq!(fs::read(first.join("save.bin")).unwrap(), [1]);
        assert_eq!(fs::read(first.join("firmware.bin")).unwrap(), [2]);
        let meta = read_metadata(&first).unwrap();
        assert_eq!(meta.frame, Some(249));
        assert!(meta.status.starts_with("unverified"));
        history.changed(300);
        history.capture(420, &[1], &[3]).unwrap();
        assert_eq!(fs::read_dir(&history_path).unwrap().count(), 1);
        history.changed(421);
        history.capture(541, &[4], &[3]).unwrap();
        assert_eq!(fs::read_dir(&history_path).unwrap().count(), 2);
        assert_eq!(fs::read(first.join("save.bin")).unwrap(), [1]);
    }

    #[test]
    fn promotion_copies_state_metadata_and_never_overwrites() {
        let root = directory();
        let source = root.join("source");
        let destination = root.join("named");
        let save = vec![42; 512 * 1024];
        let firmware = vec![7; 256 * 1024];
        write_checkpoint(&source, &metadata(), &save, &firmware).unwrap();
        promote(
            &source,
            &destination,
            "Before battle",
            "Continue, talk to rival",
        )
        .unwrap();
        assert_eq!(
            read_state(&destination).unwrap(),
            (save.clone(), firmware.clone())
        );
        let meta = read_metadata(&destination).unwrap();
        assert_eq!(meta.rom_crc32, "95AB5437");
        assert_eq!(meta.description, "Before battle");
        assert_eq!(meta.reproduction, "Continue, talk to rival");
        fs::write(source.join("save.bin"), vec![0; save.len()]).unwrap();
        assert!(promote(&source, &destination, "Overwrite", "Continue").is_err());
        assert_eq!(read_state(&destination).unwrap(), (save, firmware));
    }

    #[test]
    fn promotion_rejects_missing_truncated_or_unknown_state() {
        let root = directory();
        let dest = root.join("bad");
        assert!(promote(&root, &dest, "Test", "Continue").is_err());
        write_metadata(&root, &metadata()).unwrap();
        fs::write(root.join("save.bin"), [0; 3]).unwrap();
        fs::write(root.join("firmware.bin"), [0; 3]).unwrap();
        assert!(promote(&root, &dest, "Test", "Continue").is_err());
        assert!(!dest.exists());
        let mut meta = metadata();
        meta.format = "future-format".into();
        write_metadata(&root, &meta).unwrap();
        assert!(read_metadata(&root).is_err());
        assert!(promote(&root, &dest, " ", "Continue").is_err());
    }

    #[test]
    fn sessions_copy_checkpoints_and_reject_other_roms() {
        let root = directory();
        let source = root.join("checkpoint");
        let state = root.join("session");
        let save = vec![42; 512 * 1024];
        let firmware = vec![7; 256 * 1024];
        write_checkpoint(&source, &metadata(), &save, &firmware).unwrap();
        let mut other_rom = metadata();
        other_rom.rom_crc32 = "00000000".into();
        assert!(create_session(&state, &other_rom, Some(&source)).is_err());
        assert!(!state.exists());
        other_rom = metadata();
        other_rom.game_code = "OTHER".into();
        assert!(create_session(&state, &other_rom, Some(&source)).is_err());
        create_session(&state, &metadata(), Some(&source)).unwrap();
        assert_eq!(
            read_state(&state).unwrap(),
            (save.clone(), firmware.clone())
        );
        fs::write(state.join("save.bin"), vec![0; save.len()]).unwrap();
        assert_eq!(
            read_state(&source).unwrap(),
            (save.clone(), firmware.clone())
        );
        assert_eq!(
            read_state(&state.join("history/initial")).unwrap(),
            (save, firmware)
        );
        assert!(create_session(&state, &metadata(), Some(&source)).is_err());
        let fresh = root.join("fresh");
        create_session(&fresh, &metadata(), None).unwrap();
        assert!(!fresh.join("save.bin").exists());
        assert!(!fresh.join("firmware.bin").exists());
    }
}
