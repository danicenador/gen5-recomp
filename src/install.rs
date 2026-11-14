//! The commands a launcher drives: identify a ROM, install a game from it in
//! one step, bring an install up to date after updating gen5, and say what
//! state an install is in. `docs/launcher.md` describes the contract.

use crate::Result;
use crate::formats::nds::NdsRom;
use crate::importer::{self, title::Gen5Title};
use crate::recomp::compile::{self, BuildOptions, CANCELLED};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// gen5's version, with the commit it was built from when known
/// (`0.1.0+1a2b3c4d5e`).
pub fn version() -> String {
    match env!("GEN5_COMMIT") {
        "" => env!("CARGO_PKG_VERSION").to_owned(),
        commit => format!("{}+{commit}", env!("CARGO_PKG_VERSION")),
    }
}

/// The copy of the ROM an install keeps, so the game runs after the
/// original file is moved or deleted.
pub const ROM_FILE: &str = "rom.nds";
/// Written after a successful build; says which gen5 built the library.
pub const STAMP_FILE: &str = "build.json";
const STAMP_FORMAT: &str = "gen5-build-v1";

/// How far a ROM is expected to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// English Black 2 / White 2: what the project is tested with.
    Supported,
    /// Another Gen V game or region: it installs, but is untested.
    Experimental,
    /// Not a Gen V game.
    Unsupported,
}

#[derive(Debug, Serialize)]
pub struct RomInfo {
    pub path: PathBuf,
    pub title: String,
    pub game_code: String,
    /// The cache key (`pokemon-white-2`), or None for another title.
    pub game: Option<&'static str>,
    pub game_name: Option<&'static str>,
    pub revision: u8,
    pub size: u64,
    pub rom_crc32: String,
    pub support: Support,
}

/// How far a game made from these four game-code bytes is expected to work.
pub fn support(game_code: &[u8; 4]) -> Support {
    match importer::title::identify(game_code) {
        // The fourth byte is the region; `O` is the English international one.
        Some(Gen5Title::Black2 | Gen5Title::White2) if game_code[3] == b'O' => Support::Supported,
        Some(_) => Support::Experimental,
        None => Support::Unsupported,
    }
}

/// Checks a ROM and says which game it is and whether it is supported.
pub fn inspect(path: &Path) -> Result<RomInfo> {
    let mut rom = NdsRom::open(path)?;
    let game = importer::title::identify(&rom.header.game_code);
    Ok(RomInfo {
        path: path.to_owned(),
        title: rom.header.title.clone(),
        game_code: String::from_utf8_lossy(&rom.header.game_code).into_owned(),
        game: game.map(Gen5Title::cache_key),
        game_name: game.map(Gen5Title::display_name),
        revision: rom.header.version,
        size: rom.byte_len,
        rom_crc32: format!("{:08X}", rom.crc32()?),
        support: support(&rom.header.game_code),
    })
}

/// Where an install stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// No imported game in the directory.
    Missing,
    /// Imported, but the game library was never built.
    NotBuilt,
    /// Built by another gen5 (or one that left no stamp): run `update`.
    Outdated,
    /// Built by this gen5; ready to play.
    Ready,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub cache: PathBuf,
    pub state: State,
    pub game: Option<String>,
    pub game_name: Option<&'static str>,
    pub game_code: Option<String>,
    pub rom_crc32: Option<String>,
    /// Whether the install keeps its own copy of the ROM (`rom.nds`), so
    /// `run` and `update` need no ROM path.
    pub has_rom: bool,
    /// The gen5 that built the game library, when it left a stamp.
    pub built_with: Option<String>,
    pub current: String,
}

#[derive(Deserialize)]
struct Manifest {
    game: String,
    game_code: String,
    rom_crc32: String,
}

#[derive(Serialize, Deserialize)]
struct Stamp {
    format: String,
    gen5_version: String,
}

/// What state the install in `cache` is in.
pub fn status(cache: &Path) -> Status {
    let manifest = fs::read_to_string(cache.join("manifest.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Manifest>(&text).ok());
    let stamp = fs::read_to_string(cache.join(STAMP_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Stamp>(&text).ok())
        .filter(|stamp| stamp.format == STAMP_FORMAT);
    let library = cache
        .join("recomp")
        .join("build")
        .join(compile::LIBRARY_NAME);
    let current = version();
    let built_with = stamp.map(|stamp| stamp.gen5_version);
    let state = if manifest.is_none() || !cache.join("code").is_dir() {
        State::Missing
    } else if !library.is_file() {
        State::NotBuilt
    } else if built_with.as_deref() == Some(current.as_str()) {
        State::Ready
    } else {
        State::Outdated
    };
    let game_name = manifest.as_ref().and_then(|manifest| {
        Gen5Title::ALL
            .into_iter()
            .find(|title| title.cache_key() == manifest.game)
            .map(Gen5Title::display_name)
    });
    Status {
        cache: cache.to_owned(),
        state,
        game_name,
        game: manifest.as_ref().map(|manifest| manifest.game.clone()),
        game_code: manifest.as_ref().map(|manifest| manifest.game_code.clone()),
        rom_crc32: manifest.map(|manifest| manifest.rom_crc32),
        has_rom: cache.join(ROM_FILE).is_file(),
        built_with,
        current,
    }
}

/// One step of an install or update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Step {
    Import,
    Analyze,
    Codegen,
    Build,
}

impl Step {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Import => "Copying the ROM's files and code",
            Self::Analyze => "Finding the game's functions",
            Self::Codegen => "Translating the code to C",
            Self::Build => "Compiling the game library",
        }
    }
}

/// What an install or update reports as it goes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// A step starts: `number` of `steps`, counted from 1.
    Step {
        step: Step,
        number: usize,
        steps: usize,
        title: &'static str,
    },
    /// Files compiled (or reused) so far in the build step.
    Progress {
        step: Step,
        done: usize,
        total: usize,
    },
}

/// What `install` and `update` need besides their main argument.
pub struct Options<'a> {
    /// The C compiler for the build step.
    pub compiler: &'a str,
    /// Set from another thread to stop; the step running stops as soon as it
    /// can and the call fails with `CANCELLED`.
    pub cancel: &'a AtomicBool,
    pub report: &'a (dyn Fn(Event) + Sync),
}

/// The error a cancelled `install` or `update` returns.
pub fn is_cancelled(error: &str) -> bool {
    error == CANCELLED
}

/// Installs the game in `rom` under `data_dir`: imports it (keeping a copy of
/// the ROM), translates its code to C and builds the game library. Returns
/// the install's directory, `<data_dir>/<game>/<rom-crc32>`. Running it again
/// on an install brings it up to date and only redoes what changed.
pub fn install(rom: &Path, data_dir: &Path, options: &Options) -> Result<PathBuf> {
    let steps = [Step::Import, Step::Analyze, Step::Codegen, Step::Build];
    start(options, &steps, Step::Import)?;
    let cache = import(rom, data_dir)?;
    check(options)?;
    build(&cache, options, &steps)?;
    Ok(cache)
}

/// Brings an install up to date with this gen5: runs the import again from
/// the install's ROM copy when it has one (which adds what older imports
/// lack), then translates and builds again, reusing whatever did not change.
pub fn update(cache: &Path, options: &Options) -> Result<()> {
    if status(cache).state == State::Missing {
        return Err(format!(
            "'{}' is not an installed game (no manifest.json and code/)",
            cache.display()
        ));
    }
    let rom = cache.join(ROM_FILE);
    if rom.is_file() {
        let steps = [Step::Import, Step::Analyze, Step::Codegen, Step::Build];
        start(options, &steps, Step::Import)?;
        let parent = cache
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| format!("'{}' is not an install directory", cache.display()))?;
        let imported = import(&rom, parent)?;
        if !same_directory(&imported, cache) {
            return Err(format!(
                "the ROM copy in '{}' belongs to '{}'",
                cache.display(),
                imported.display()
            ));
        }
        check(options)?;
        build(cache, options, &steps)
    } else {
        build(cache, options, &[Step::Analyze, Step::Codegen, Step::Build])
    }
}

fn same_directory(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn check(options: &Options) -> Result<()> {
    if options.cancel.load(Ordering::Relaxed) {
        Err(CANCELLED.to_owned())
    } else {
        Ok(())
    }
}

fn start(options: &Options, steps: &[Step], step: Step) -> Result<()> {
    check(options)?;
    let number = steps.iter().position(|&other| other == step).unwrap() + 1;
    (options.report)(Event::Step {
        step,
        number,
        steps: steps.len(),
        title: step.title(),
    });
    Ok(())
}

/// Imports the ROM and keeps a copy of it in the install.
fn import(rom: &Path, data_dir: &Path) -> Result<PathBuf> {
    let report = importer::import_rom(NdsRom::open(rom)?, data_dir)?;
    let cache = report.cache_path;
    let copy = cache.join(ROM_FILE);
    let size = fs::metadata(rom)
        .map_err(|error| format!("could not read '{}': {error}", rom.display()))?
        .len();
    if same_directory(rom, &copy) || fs::metadata(&copy).is_ok_and(|meta| meta.len() == size) {
        return Ok(cache);
    }
    let partial = cache.join(format!("{ROM_FILE}.partial"));
    fs::copy(rom, &partial)
        .and_then(|_| fs::rename(&partial, &copy))
        .map_err(|error| {
            let _ = fs::remove_file(&partial);
            format!("could not copy the ROM into '{}': {error}", cache.display())
        })?;
    Ok(cache)
}

/// Analysis, code generation and the build, then the stamp.
fn build(cache: &Path, options: &Options, steps: &[Step]) -> Result<()> {
    // A half-done update must not look ready.
    let stamp = cache.join(STAMP_FILE);
    if stamp.exists() {
        fs::remove_file(&stamp)
            .map_err(|error| format!("could not remove '{}': {error}", stamp.display()))?;
    }
    start(options, steps, Step::Analyze)?;
    let program = crate::recomp::program::Program::load(&cache.join("code"))?;
    let analysis = crate::recomp::analysis::analyze(&program);
    start(options, steps, Step::Codegen)?;
    crate::recomp::codegen::generate(&program, &analysis, &cache.join("recomp"))?;
    drop((program, analysis));
    start(options, steps, Step::Build)?;
    let progress = |done, total| {
        (options.report)(Event::Progress {
            step: Step::Build,
            done,
            total,
        })
    };
    compile::build_library_with(
        &cache.join("recomp"),
        &BuildOptions {
            compiler: options.compiler,
            cancel: Some(options.cancel),
            progress: Some(&progress),
        },
    )?;
    let text = serde_json::to_string_pretty(&Stamp {
        format: STAMP_FORMAT.to_owned(),
        gen5_version: version(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(&stamp, text)
        .map_err(|error| format!("could not write '{}': {error}", stamp.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_follows_game_and_region() {
        assert_eq!(support(b"IRDO"), Support::Supported);
        assert_eq!(support(b"IREO"), Support::Supported);
        assert_eq!(support(b"IRDJ"), Support::Experimental);
        assert_eq!(support(b"IRAO"), Support::Experimental);
        assert_eq!(support(b"ADAE"), Support::Unsupported);
    }

    #[test]
    fn status_tells_missing_unbuilt_outdated_and_ready_apart() {
        let root = std::env::temp_dir().join(format!("gen5-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(status(&root).state, State::Missing);
        fs::create_dir_all(root.join("code")).unwrap();
        fs::write(
            root.join("manifest.json"),
            r#"{"format":"gen5-rom-cache-v1","game":"pokemon-white-2","title":"POKEMON W2",
                "game_code":"IRDO","rom_size":1,"rom_crc32":"95AB5437","files":[]}"#,
        )
        .unwrap();
        let unbuilt = status(&root);
        assert_eq!(unbuilt.state, State::NotBuilt);
        assert_eq!(unbuilt.game_name, Some("Pokémon White Version 2"));
        assert!(!unbuilt.has_rom);
        let build = root.join("recomp").join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join(compile::LIBRARY_NAME), b"").unwrap();
        assert_eq!(status(&root).state, State::Outdated);
        let stamp = |version: &str| {
            fs::write(
                root.join(STAMP_FILE),
                serde_json::to_string(&Stamp {
                    format: STAMP_FORMAT.to_owned(),
                    gen5_version: version.to_owned(),
                })
                .unwrap(),
            )
            .unwrap()
        };
        stamp("0.0.1");
        let outdated = status(&root);
        assert_eq!(outdated.state, State::Outdated);
        assert_eq!(outdated.built_with.as_deref(), Some("0.0.1"));
        stamp(&version());
        assert_eq!(status(&root).state, State::Ready);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn events_serialize_as_tagged_json_lines() {
        let step = Event::Step {
            step: Step::Build,
            number: 4,
            steps: 4,
            title: Step::Build.title(),
        };
        assert_eq!(
            serde_json::to_string(&step).unwrap(),
            r#"{"event":"step","step":"build","number":4,"steps":4,"title":"Compiling the game library"}"#
        );
        let progress = Event::Progress {
            step: Step::Build,
            done: 3,
            total: 260,
        };
        assert_eq!(
            serde_json::to_string(&progress).unwrap(),
            r#"{"event":"progress","step":"build","done":3,"total":260}"#
        );
    }
}
